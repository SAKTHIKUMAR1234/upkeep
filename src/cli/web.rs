use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::paths::Paths;
use crate::time::format_unix;
use crate::web::settings::WebSettings;
use crate::web::users::{Role, WebDb, checked_email};

#[derive(Debug, Args)]
pub struct WebArgs {
    /// The web page settings [default: <config-dir>/web.toml].
    #[arg(long, value_name = "FILE")]
    pub settings: Option<PathBuf>,
    #[command(subcommand)]
    pub command: WebCommand,
}

#[derive(Debug, Subcommand)]
pub enum WebCommand {
    /// Serve the web page with Google sign-in.
    Serve,
    /// Manage who may sign in.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Allow a Google email, or change its role.
    Add {
        email: String,
        #[arg(long, value_enum)]
        role: Role,
    },
    /// Remove an email and end its sessions.
    Remove { email: String },
    /// List the allowed emails.
    List,
}

/// Runs `upkeep web ...`.
pub async fn run(args: &WebArgs, paths: &Paths) -> anyhow::Result<()> {
    let settings_file = args
        .settings
        .clone()
        .unwrap_or_else(|| paths.web_settings_file());
    let command = match &args.command {
        WebCommand::Serve => return crate::web::serve(&settings_file, &paths.state_dir).await,
        WebCommand::User { command } => command,
    };
    let database = WebSettings::load(&settings_file)?.database_path(&paths.state_dir);
    let db = WebDb::open(&database)?;
    user(&db, command)
}

fn user(db: &WebDb, command: &UserCommand) -> anyhow::Result<()> {
    match command {
        UserCommand::Add { email, role } => {
            let email =
                checked_email(email).ok_or_else(|| anyhow::anyhow!("invalid email {email:?}"))?;
            db.set_user(&email, *role, "cli")?;
            db.record(
                "cli",
                "user add",
                None,
                &format!("{email} as {}", role.as_str()),
            )?;
            println!("{email} may sign in as {}", role.as_str());
        }
        UserCommand::Remove { email } => {
            anyhow::ensure!(db.remove_user(email)?, "no user {email:?}");
            db.record("cli", "user remove", None, email)?;
            println!("{email} removed");
        }
        UserCommand::List => {
            for user in db.users()? {
                let added = format_unix(user.added_at);
                println!(
                    "{:<9} {:<40} added by {} at {added}",
                    user.role.as_str(),
                    user.email,
                    user.added_by
                );
            }
        }
    }
    Ok(())
}
