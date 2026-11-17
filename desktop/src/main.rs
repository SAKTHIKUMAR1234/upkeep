//! The Upkeep desktop window: the REST service in this process, and one window on it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::{WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tracing_subscriber::EnvFilter;
use upkeep::app::serve::ServeConfig;
use upkeep::app::{AppConfig, Service, default_data_dir};

/// A second close within this time quits while runs are active.
const CLOSE_AGAIN: Duration = Duration::from_secs(5);

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let data_dir = std::env::var_os("UPKEEP_APP_DIR").map_or_else(default_data_dir, Into::into);
    let runtime = tokio::runtime::Runtime::new()?;
    let service = runtime.block_on(Service::bind(
        AppConfig::new(data_dir),
        ServeConfig::loopback(0),
    ))?;
    let url = service.url().parse()?;
    let executor = service.executor();
    runtime.spawn(async move {
        if let Err(error) = service.run(std::future::pending()).await {
            tracing::error!(%error, "the Upkeep service stopped");
        }
    });

    let close_warned = Arc::new(AtomicBool::new(false));
    tauri::Builder::default()
        .setup(move |app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("Upkeep")
                .inner_size(1280.0, 820.0)
                .min_inner_size(760.0, 520.0)
                .build()?;
            Ok(())
        })
        .on_window_event(move |window, event| {
            let WindowEvent::CloseRequested { api, .. } = event else {
                return;
            };
            let active = executor.active_count();
            if active == 0 || close_warned.swap(true, Ordering::SeqCst) {
                return;
            }
            // Closing ends active runs; ask once, and let a second close quit.
            api.prevent_close();
            let warning = format!(
                "window.upkeepCloseWarning && window.upkeepCloseWarning({active}, {})",
                CLOSE_AGAIN.as_secs()
            );
            let _ = tauri::Manager::get_webview_window(window, "main")
                .map(|webview| webview.eval(warning));
            let reset = close_warned.clone();
            std::thread::spawn(move || {
                std::thread::sleep(CLOSE_AGAIN);
                reset.store(false, Ordering::SeqCst);
            });
        })
        .run(tauri::generate_context!())?;
    Ok(())
}
