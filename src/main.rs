use clap::Parser;
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
        .init();

    let paths = cli.paths();
    let result = match &cli.command {
        Command::Serve => upkeep::api::serve(paths).await,
        Command::StepExec { dir } => upkeep::engine::step_exec::execute(dir).await,
        command => run_client(command, &paths).await,
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("upkeep: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
