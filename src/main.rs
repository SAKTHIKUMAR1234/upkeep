use std::io::IsTerminal;

use clap::Parser;
use nix::sys::signal::{SigHandler, Signal, signal};
use tracing_subscriber::EnvFilter;
use upkeep::cli::{Cli, Command, run_client};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let paths = cli.paths();
    let result = match &cli.command {
        Command::Serve => upkeep::api::serve(paths).await,
        Command::StepExec { dir } => upkeep::engine::step_exec::execute(dir).await,
        Command::App(args) => upkeep::cli::app::serve(args).await,
        Command::User(args) => upkeep::cli::app::user(args),
        _ => {
            // The client exits quietly when its output pipe closes, like `upkeep status | head`.
            // SAFETY: SIG_DFL runs no handler code, and the client installs no SIGPIPE handler.
            unsafe { signal(Signal::SIGPIPE, SigHandler::SigDfl) }.ok();
            run_client(&cli).await
        }
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("upkeep: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
