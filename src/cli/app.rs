use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::app::auth::password;
use crate::app::store::{Activity, Db, normalize_email};
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
    /// Serve the REST service and the screens on 127.0.0.1, without the desktop window.
    Serve {
        #[arg(long, default_value_t = 7890)]
        port: u16,
    },
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
    let AppCommand::Serve { port } = args.command;
    let service = Service::bind(args.data.config(), port).await?;
    println!("Upkeep listens on {}", service.url());
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
    let valid = email.len() <= 254
        && !email
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.contains('@')
        });
    anyhow::ensure!(valid, "invalid email {email:?}");
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
    use super::*;

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
