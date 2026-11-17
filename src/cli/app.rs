use std::io::{BufRead, IsTerminal};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::app::auth::password;
use crate::app::serve::{ServeConfig, ServeSettings};
use crate::app::store::{Activity, Db, is_valid_email, normalize_email};
use crate::app::{AppConfig, Service, default_data_dir};
use crate::time::{format_unix, now_millis};

#[derive(Debug, Args)]
pub struct DataDir {
    /// The desktop app's data folder [default: ~/.local/share/upkeep-app].
    #[arg(long, global = true, env = "UPKEEP_APP_DIR", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
}

impl DataDir {
    pub fn config(&self) -> AppConfig {
        AppConfig::new(self.data_dir.clone().unwrap_or_else(default_data_dir))
    }
}

#[derive(Debug, Args)]
pub struct AppArgs {
    #[command(flatten)]
    pub data: DataDir,
    #[command(subcommand)]
    pub command: AppCommand,
}

#[derive(Debug, Subcommand)]
pub enum AppCommand {
    /// Serve the REST service and the screens without the desktop window: on 127.0.0.1, or as a
    /// website behind a TLS proxy when a public URL is set.
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Read the settings from this TOML file. Flags win over it.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Listen on 127.0.0.1 with this port.
    #[arg(long, conflicts_with = "listen")]
    pub port: Option<u16>,
    /// The address and port to listen on [default: 127.0.0.1:7890].
    #[arg(long, value_name = "ADDR:PORT")]
    pub listen: Option<SocketAddr>,
    /// The site's URL, such as https://upkeep.example.com. Turns on website mode.
    #[arg(long, value_name = "URL")]
    pub public_url: Option<String>,
    /// The proxy whose X-Forwarded-For and X-Real-IP headers name the client.
    #[arg(long, value_name = "IP")]
    pub trusted_proxy: Option<IpAddr>,
    /// Lock a session after this time without use [default: 15m].
    #[arg(long, value_name = "DURATION")]
    pub idle_timeout: Option<String>,
    /// End a session this long after sign-in [default: 12h].
    #[arg(long, value_name = "DURATION")]
    pub session_lifetime: Option<String>,
}

impl ServeArgs {
    /// The flags over the config file.
    pub fn serve_config(&self) -> anyhow::Result<ServeConfig> {
        let flags = ServeSettings {
            listen: self
                .port
                .map(|port| SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
                .or(self.listen),
            public_url: self.public_url.clone(),
            trusted_proxy: self.trusted_proxy,
            idle_timeout: self.idle_timeout.clone(),
            session_lifetime: self.session_lifetime.clone(),
        };
        let file = match &self.config {
            Some(path) => ServeSettings::read(path)?,
            None => ServeSettings::default(),
        };
        Ok(ServeConfig::new(flags.or(file))?)
    }
}

#[derive(Debug, Args)]
pub struct UserArgs {
    #[command(flatten)]
    pub data: DataDir,
    #[command(subcommand)]
    pub command: UserCommand,
}

#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Add a user. Asks for the password twice.
    Add { email: String },
    /// Set a new password. Signs the user out everywhere.
    Passwd { email: String },
    /// Stop a user from signing in, and end their sessions.
    Disable { email: String },
    /// Let a disabled user sign in again.
    Enable { email: String },
    /// List the users.
    List,
}

/// `upkeep app serve`: serves until SIGTERM or Ctrl-C.
pub async fn serve(args: &AppArgs) -> anyhow::Result<()> {
    let AppCommand::Serve(serve) = &args.command;
    let serve = serve.serve_config()?;
    if serve.is_website() && serve.trusted_proxy.is_none() {
        tracing::warn!("no trusted proxy: sign-in limits count every client as the proxy address");
    }
    let website = serve.public_origin.clone();
    let service = Service::bind(args.data.config(), serve).await?;
    match website {
        Some(origin) => println!("Upkeep listens on {} for {origin}", service.address()),
        None => println!("Upkeep listens on {}", service.url()),
    }
    service.run(crate::api::shutdown_signal()).await
}

/// `upkeep user ...`.
pub fn user(args: &UserArgs) -> anyhow::Result<()> {
    let db = args.data.config().open_db()?;
    match &args.command {
        UserCommand::Add { email } => {
            let email = checked_email(email)?;
            let hash = password::hash(&read_new_password()?)?;
            anyhow::ensure!(
                db.add_user(&email, &hash, now_millis())?,
                "{email} exists already; use upkeep user passwd to change the password"
            );
            record(&db, Activity::UserAdd, &email)?;
            println!("{email} may sign in");
        }
        UserCommand::Passwd { email } => {
            let email = normalize_email(email);
            anyhow::ensure!(db.password_hash(&email)?.is_some(), "no user {email}");
            let hash = password::hash(&read_new_password()?)?;
            db.set_password(&email, &hash)?;
            db.clear_login_failures(&email)?;
            record(&db, Activity::UserPassword, &email)?;
            println!("password changed; {email} is signed out everywhere");
        }
        UserCommand::Disable { email } => {
            let email = normalize_email(email);
            anyhow::ensure!(db.set_disabled(&email, true)?, "no user {email}");
            record(&db, Activity::UserDisable, &email)?;
            println!("{email} is disabled and signed out everywhere");
        }
        UserCommand::Enable { email } => {
            let email = normalize_email(email);
            anyhow::ensure!(db.set_disabled(&email, false)?, "no user {email}");
            record(&db, Activity::UserEnable, &email)?;
            println!("{email} may sign in again");
        }
        UserCommand::List => {
            for user in db.users()? {
                let state = if user.disabled { "disabled" } else { "enabled" };
                let added = format_unix(user.created_at / 1000);
                println!("{state:<9} {:<40} added {added}", user.email);
            }
        }
    }
    Ok(())
}

fn record(db: &Db, action: Activity, email: &str) -> rusqlite::Result<()> {
    db.record(now_millis(), "cli", action, None, email)
}

/// A lower-case email with one `@` and no spaces.
fn checked_email(email: &str) -> anyhow::Result<String> {
    let email = normalize_email(email);
    anyhow::ensure!(is_valid_email(&email), "invalid email {email:?}");
    Ok(email)
}

/// Asks twice without echo on a terminal; reads two lines from stdin otherwise.
fn read_new_password() -> anyhow::Result<String> {
    let (first, second) = if std::io::stdin().is_terminal() {
        (
            rpassword::prompt_password("Password: ")?,
            rpassword::prompt_password("Repeat password: ")?,
        )
    } else {
        let mut lines = std::io::stdin().lock().lines();
        let mut next = || lines.next().transpose().map(Option::unwrap_or_default);
        (next()?, next()?)
    };
    anyhow::ensure!(first == second, password::PasswordError::Mismatch);
    password::check_new(&first)?;
    Ok(first)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Command {
        #[command(subcommand)]
        app: AppCommand,
    }

    fn serve_config(args: &[&str]) -> anyhow::Result<ServeConfig> {
        let command = Command::try_parse_from([&["upkeep", "serve"], args].concat())?;
        let AppCommand::Serve(serve) = command.app;
        serve.serve_config()
    }

    #[test]
    fn serve_refuses_a_public_address_without_a_public_url() {
        let error = serve_config(&["--listen", "0.0.0.0:7890"]).unwrap_err();
        assert!(error.to_string().contains("is not 127.0.0.1"), "{error}");
        let website = serve_config(&[
            "--listen",
            "0.0.0.0:7890",
            "--public-url",
            "https://upkeep.example.com",
        ])
        .unwrap();
        assert!(website.is_website());
        assert_eq!(
            serve_config(&["--port", "7000"]).unwrap().listen,
            "127.0.0.1:7000".parse().unwrap()
        );
        assert_eq!(
            serve_config(&[]).unwrap().listen,
            "127.0.0.1:7890".parse().unwrap()
        );
    }

    #[test]
    fn serve_flags_win_over_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.toml");
        std::fs::write(
            &file,
            "listen = \"0.0.0.0:8000\"\npublic_url = \"https://upkeep.example.com\"\n\
             trusted_proxy = \"127.0.0.1\"\nidle_timeout = \"10m\"\n",
        )
        .unwrap();
        let path = file.display().to_string();
        let config = serve_config(&["--config", &path, "--idle-timeout", "20m"]).unwrap();
        assert_eq!(config.listen, "0.0.0.0:8000".parse().unwrap());
        assert_eq!(config.trusted_proxy, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(config.sessions.idle_ms, 20 * 60_000);
        assert!(serve_config(&["--config", "/nonexistent/app.toml"]).is_err());
    }

    #[test]
    fn emails_are_checked_and_lower_case() {
        assert_eq!(
            checked_email(" Owner@Example.com ").unwrap(),
            "owner@example.com"
        );
        for bad in [
            "owner",
            "@example.com",
            "a@b",
            "a b@example.com",
            "a@b@example.com",
        ] {
            assert!(checked_email(bad).is_err(), "{bad}");
        }
    }
}
