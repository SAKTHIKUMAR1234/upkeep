use std::time::Duration;

use clap::Subcommand;
use futures_util::future::join_all;
use serde_json::{Value, json};

use super::output::text;
use crate::client::Client;
use crate::client::config::ClientDir;
use crate::client::hosts::Hosts;
use crate::time::format_unix;

const HOST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Subcommand)]
pub enum AlertCommand {
    /// Send a test message to the runner's Telegram chat.
    Test,
}

pub async fn alert(client: &Client, command: &AlertCommand) -> anyhow::Result<()> {
    match command {
        AlertCommand::Test => {
            client.post("/monitor/alert-test", &json!({})).await?;
            println!("test message sent");
        }
    }
    Ok(())
}

/// The checks of one runner, worst first.
pub fn print_checks(report: &Value) {
    let mut checks: Vec<&Value> = report["checks"].as_array().into_iter().flatten().collect();
    if checks.is_empty() {
        println!("monitor: no checks yet");
        return;
    }
    checks.sort_by_key(|check| {
        (
            std::cmp::Reverse(rank(&check["observed"])),
            text(&check["key"]),
        )
    });
    println!("{:<9} {:<36} {:<20} VALUE", "STATE", "CHECK", "SINCE");
    for check in checks {
        let since = check["since"].as_i64().map(format_unix).unwrap_or_default();
        println!(
            "{:<9} {:<36} {:<20} {}",
            state(check),
            text(&check["key"]),
            since,
            text(&check["value"])
        );
    }
    if report["telegram"] == false {
        println!("alerts: off (no [monitor.telegram])");
    } else if report["outbox"].as_i64().unwrap_or(0) > 0 {
        println!("alerts: {} waiting to be sent", text(&report["outbox"]));
    }
}

/// The current level, with the alerted level when it is still debounced, for example `ok>crit`.
fn state(check: &Value) -> String {
    let observed = short(&check["observed"]);
    let level = short(&check["level"]);
    if observed == level {
        observed.to_owned()
    } else {
        format!("{level}>{observed}")
    }
}

fn short(level: &Value) -> &'static str {
    match level.as_str() {
        Some("ok") => "ok",
        Some("warn") => "WARN",
        Some("critical") => "CRIT",
        _ => "?",
    }
}

fn rank(level: &Value) -> u8 {
    match level.as_str() {
        Some("critical") => 2,
        Some("warn") => 1,
        _ => 0,
    }
}

/// `upkeep status --all`: one line per registered host.
pub async fn print_all(dir: &ClientDir) -> anyhow::Result<()> {
    let hosts = Hosts::load(dir)?;
    anyhow::ensure!(
        !hosts.entries.is_empty(),
        "no hosts registered; add one with `upkeep host add`"
    );
    let reports = join_all(hosts.entries.iter().map(|(name, host)| async move {
        let report = async {
            let client = Client::new(host.endpoint()?);
            let report = tokio::time::timeout(HOST_TIMEOUT, client.get("/monitor")).await??;
            anyhow::Ok(report)
        };
        (name, report.await)
    }))
    .await;

    println!(
        "{:<16} {:<12} {:>4} {:>4} {:>4}  PROBLEMS",
        "HOST", "STATE", "OK", "WARN", "CRIT"
    );
    for (name, report) in reports {
        match report {
            Ok(report) => println!("{}", host_line(name, &report)),
            Err(error) => println!(
                "{name:<16} {:<12} {:>4} {:>4} {:>4}  {error}",
                "UNREACHABLE", "-", "-", "-"
            ),
        }
    }
    Ok(())
}

pub fn host_line(name: &str, report: &Value) -> String {
    let checks: Vec<&Value> = report["checks"].as_array().into_iter().flatten().collect();
    let count = |wanted: u8| {
        checks
            .iter()
            .filter(|check| rank(&check["observed"]) == wanted)
            .count()
    };
    let worst = checks
        .iter()
        .map(|check| rank(&check["observed"]))
        .max()
        .unwrap_or(0);
    let state = match worst {
        2 => "CRITICAL",
        1 => "WARN",
        _ if checks.is_empty() => "no checks",
        _ => "ok",
    };
    let problems: Vec<String> = checks
        .iter()
        .filter(|check| rank(&check["observed"]) > 0)
        .map(|check| format!("{} {}", text(&check["key"]), text(&check["value"])))
        .collect();
    format!(
        "{name:<16} {state:<12} {:>4} {:>4} {:>4}  {}",
        count(0),
        count(1),
        count(2),
        problems.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_line_counts_levels_and_lists_problems() {
        let report = json!({"checks": [
            {"key": "disk:/", "observed": "critical", "level": "critical", "value": "93% used"},
            {"key": "load", "observed": "ok", "level": "ok", "value": "0.1"},
            {"key": "swap", "observed": "warn", "level": "ok", "value": "60% used"},
        ]});
        let line = host_line("erp", &report);
        assert!(
            line.starts_with("erp              CRITICAL        1    1    1  "),
            "{line}"
        );
        assert!(line.ends_with("disk:/ 93% used; swap 60% used"), "{line}");
        assert_eq!(state(&report["checks"][2]), "ok>WARN");
    }
}
