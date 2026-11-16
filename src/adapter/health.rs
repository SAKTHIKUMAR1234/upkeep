use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use futures_util::future::{BoxFuture, join_all};
use serde::Serialize;
use tokio::process::Command;

use super::manifest::{DEFAULT_CHECK_TIMEOUT, Ping, PingTarget, Service};
use super::{Adapter, Catalog, LoadError};
use crate::paths::Paths;

#[derive(Debug, Serialize)]
pub struct HealthReport {
    pub ok: bool,
    pub items: Vec<HealthItem>,
    pub errors: Vec<LoadError>,
}

#[derive(Debug, Serialize)]
pub struct HealthItem {
    pub adapter: String,
    pub kind: &'static str,
    pub name: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Runs every service check and ping of every adapter at once, reporting them in manifest order.
pub async fn check_all(catalog: &Catalog, paths: &Paths) -> HealthReport {
    let mut checks: Vec<BoxFuture<'_, HealthItem>> = Vec::new();
    for adapter in catalog.adapters.values() {
        let name = &adapter.manifest.name;
        for service in &adapter.manifest.services {
            checks.push(Box::pin(async move {
                item(
                    name,
                    "service",
                    service.name(),
                    check_service(service).await,
                )
            }));
        }
        let state_dir = paths.adapter_state_dir(name);
        for ping in &adapter.manifest.pings {
            let state_dir = state_dir.clone();
            checks.push(Box::pin(async move {
                item(
                    name,
                    "ping",
                    &ping.name,
                    check_ping(adapter, ping, &state_dir).await,
                )
            }));
        }
    }

    let items = join_all(checks).await;
    HealthReport {
        ok: catalog.errors.is_empty() && items.iter().all(|item| item.ok),
        items,
        errors: catalog.errors.clone(),
    }
}

fn item(adapter: &str, kind: &'static str, name: &str, result: Result<(), String>) -> HealthItem {
    HealthItem {
        adapter: adapter.to_owned(),
        kind,
        name: name.to_owned(),
        ok: result.is_ok(),
        detail: result.err(),
    }
}

async fn check_service(service: &Service) -> Result<(), String> {
    let mut command = match service {
        Service::Systemd { unit, .. } => command("systemctl", &["is-active", unit]),
        Service::SystemdUser { unit, user, .. } => {
            let machine = format!("--machine={user}@");
            command("systemctl", &["--user", &machine, "is-active", unit])
        }
        Service::Docker { container, .. } => command(
            "docker",
            &["inspect", "--format", "{{.State.Running}}", container],
        ),
        Service::Process { pattern, .. } => command("pgrep", &["-f", pattern]),
    };
    let stdout = run(&mut command, DEFAULT_CHECK_TIMEOUT).await?;

    if matches!(service, Service::Docker { .. }) && stdout.trim() != "true" {
        return Err("container is not running".into());
    }
    Ok(())
}

async fn check_ping(adapter: &Adapter, ping: &Ping, state_dir: &Path) -> Result<(), String> {
    match &ping.target {
        PingTarget::Http { url, expect } => {
            let max_time = ping.timeout.as_secs().max(1).to_string();
            let mut curl = command(
                "curl",
                &[
                    "-sS",
                    "-o",
                    "/dev/null",
                    "-w",
                    "%{http_code}",
                    "--max-time",
                    &max_time,
                    url,
                ],
            );
            let code = run(&mut curl, ping.timeout + Duration::from_secs(2)).await?;
            if code.trim() != expect.to_string() {
                return Err(format!("expected HTTP {expect}, got {}", code.trim()));
            }
            Ok(())
        }
        PingTarget::Command { command: script } => {
            let mut shell = command("sh", &["-c", script]);
            shell
                .current_dir(&adapter.dir)
                .env("UPKEEP_STATE_DIR", state_dir);
            run(&mut shell, ping.timeout).await.map(drop)
        }
    }
}

pub fn command(program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Runs a check command and returns its stdout, or why it failed.
pub async fn run(command: &mut Command, timeout: Duration) -> Result<String, String> {
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs()))?
        .map_err(|error| error.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty());
    Err(match detail {
        Some(text) => format!("{}: {text}", output.status),
        None => output.status.to_string(),
    })
}
