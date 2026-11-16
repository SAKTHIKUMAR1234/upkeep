pub mod auth;
pub mod log_stream;
pub mod routes;

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::{TcpListener, UnixListener};

use crate::adapter::Catalog;
use crate::engine::Runner;
use crate::paths::Paths;
use crate::settings::Settings;
use crate::store::Store;
use auth::Token;

/// `upkeep serve`: resumes the active job, then serves the API on the unix socket and the optional TCP address.
pub async fn serve(paths: Paths) -> anyhow::Result<()> {
    for dir in [&paths.state_dir, &paths.log_dir] {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::set_permissions(&paths.state_dir, std::fs::Permissions::from_mode(0o700))?;
    let _lock = lock_instance(&paths)?;

    let settings = Settings::load(&paths.settings_file())?;
    let tcp = match settings.listen {
        Some(address) => Some((address, Token::load(&paths.token_file())?)),
        None => None,
    };
    report_catalog(&Catalog::load(&paths.adapters_dir()));

    let store = Store::open(&paths.database())?;
    let runner = Arc::new(Runner::new(paths.clone(), settings, store));
    runner.resume()?;

    let unix = bind_socket(&paths)?;
    tracing::info!(socket = %paths.socket().display(), "listening on unix socket");
    let local = axum::serve(unix, routes::local_router(runner.clone()));
    let remote = async {
        let Some((address, token)) = tcp else {
            return std::future::pending().await;
        };
        let listener = TcpListener::bind(address)
            .await
            .with_context(|| format!("cannot listen on {address}"))?;
        tracing::info!(%address, "listening on tcp");
        axum::serve(listener, routes::remote_router(runner.clone(), token)).await?;
        anyhow::Ok(())
    };

    tokio::select! {
        result = local => result?,
        result = remote => result?,
        () = shutdown_signal() => tracing::info!("shutting down; running steps continue on their own"),
    }
    Ok(())
}

/// Holds an exclusive lock for the life of the daemon, so only one runner serves this state.
fn lock_instance(paths: &Paths) -> anyhow::Result<File> {
    let file = File::create(paths.lock_file())?;
    file.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "another upkeep serve is running for {}",
            paths.state_dir.display()
        )
    })?;
    Ok(file)
}

fn bind_socket(paths: &Paths) -> anyhow::Result<UnixListener> {
    let socket = paths.socket();
    match std::fs::remove_file(&socket) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("cannot bind {}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn report_catalog(catalog: &Catalog) {
    tracing::info!(adapters = catalog.adapters.len(), "adapters loaded");
    for error in &catalog.errors {
        tracing::error!(path = %error.path.display(), error = %error.error, "invalid adapter");
    }
}

async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("cannot install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}
