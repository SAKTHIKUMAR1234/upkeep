use std::path::Path;
use std::process::Stdio;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::output::{human_bytes, text};
use super::{print_log, start_job};
use crate::client::Client;
use crate::client::config::ClientDir;
use crate::client::destinations::Destinations;
use crate::client::repository::Repository;

/// `upkeep snapshot <host> --to <dest>`: runs the snapshot plan with the destination and waits for it.
pub async fn take(
    client: &Client,
    dir: &ClientDir,
    host: &str,
    dest: &str,
    plan: &str,
) -> anyhow::Result<()> {
    let destinations = Destinations::load(dir)?;
    let destination = destinations.get(dest)?;
    let mut params = Map::new();
    params.insert("destination".into(), destination.job_params(dest, host));
    let secrets = destination.job_secrets(dir, dest)?;

    let id = start_job(client, plan, false, params, secrets).await?;
    println!("job {id} started on {host}: {plan} to {dest}");
    print_log(client, id, true).await?;

    let detail = client.get(&format!("/jobs/{id}")).await?;
    for step in detail["steps"].as_array().into_iter().flatten() {
        if let Some(line) = describe_result(&step["action"], &step["result"]) {
            println!("{line}");
        }
    }
    let status = text(&detail["job"]["status"]);
    anyhow::ensure!(
        matches!(status.as_str(), "succeeded" | "degraded"),
        "job {id} {status}: see upkeep --host {host} job {id}"
    );
    Ok(())
}

/// One line for a snapshot or retention step result.
fn describe_result(action: &Value, result: &Value) -> Option<String> {
    let bytes = |key: &str| {
        result[key]
            .as_u64()
            .map(human_bytes)
            .unwrap_or_else(|| "?".into())
    };
    match action.as_str()? {
        "snapshot" if result.is_object() => Some(format!(
            "snapshot {}: {} new data, {} sent after compression, {} scanned",
            text(&result["snapshot_id"]),
            bytes("data_added"),
            bytes("data_added_packed"),
            bytes("total_bytes_processed"),
        )),
        "retention" if result.is_object() => Some(format!(
            "retention: kept {}, removed {}",
            text(&result["kept"]),
            text(&result["removed"]),
        )),
        _ => None,
    }
}

/// `upkeep snapshots <host>`: reads the host's repository on the destination directly.
pub fn list(dir: &ClientDir, host: &str, dest: Option<&str>) -> anyhow::Result<()> {
    let destinations = Destinations::load(dir)?;
    let (name, destination) = destinations.choose(dest)?;
    let repository = Repository::open(dir, &name, destination, host)?;
    let output = repository
        .command(&["snapshots", "--json"])
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| repository.engine_error())?;
    anyhow::ensure!(
        output.status.success(),
        "restic snapshots failed: {}",
        output.status
    );

    let snapshots = parse_snapshots(&output.stdout)?;
    println!("{} snapshots of {host} on {name}", snapshots.len());
    for snapshot in &snapshots {
        println!("{}", snapshot.line());
    }
    Ok(())
}

/// `upkeep restore <host> <id> --to <dir>`: restores into a folder on this machine.
pub fn restore(
    dir: &ClientDir,
    host: &str,
    dest: Option<&str>,
    snapshot: &str,
    path: Option<&str>,
    target: &Path,
) -> anyhow::Result<()> {
    let destinations = Destinations::load(dir)?;
    let (name, destination) = destinations.choose(dest)?;
    let repository = Repository::open(dir, &name, destination, host)?;
    let target = target.to_string_lossy();
    let mut args = vec!["restore", snapshot, "--target", target.as_ref()];
    if let Some(path) = path {
        args.extend(["--include", path]);
    }
    let status = repository
        .command(&args)
        .status()
        .with_context(|| repository.engine_error())?;
    anyhow::ensure!(status.success(), "restic restore failed: {status}");
    println!(
        "restored {snapshot}{} of {host} into {target}",
        path.map(|path| format!(" {path}")).unwrap_or_default()
    );
    Ok(())
}

/// One snapshot from `restic snapshots --json`.
#[derive(Debug, Deserialize, PartialEq)]
pub struct Snapshot {
    pub short_id: String,
    pub time: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub summary: Option<Summary>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Summary {
    pub data_added: Option<u64>,
    pub data_added_packed: Option<u64>,
    pub total_bytes_processed: Option<u64>,
}

impl Snapshot {
    pub fn line(&self) -> String {
        let summary = self.summary.as_ref();
        let size = |value: Option<u64>| value.map(human_bytes).unwrap_or_else(|| "-".into());
        format!(
            "{}  {}  added {:>10}  size {:>10}  {}",
            self.short_id,
            self.time.get(..19).unwrap_or(&self.time).replace('T', " "),
            size(summary.and_then(|summary| summary.data_added_packed)),
            size(summary.and_then(|summary| summary.total_bytes_processed)),
            self.tags.join(","),
        )
    }
}

pub fn parse_snapshots(json: &[u8]) -> serde_json::Result<Vec<Snapshot>> {
    serde_json::from_slice(json)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SNAPSHOTS: &str = r#"[
      {"time":"2026-11-16T14:45:54.556715482+05:30","short_id":"3612fbdc","id":"3612fbdcb0e6",
       "hostname":"vm-a","tags":["upkeep","job=4","plan=snapshot"],"paths":["/"],
       "summary":{"data_added":589667,"data_added_packed":48254,"total_bytes_processed":2147483648}},
      {"time":"2026-11-16T15:00:00+05:30","short_id":"be7009a1","id":"be7009a1d912","paths":["/"]}
    ]"#;

    #[test]
    fn parses_and_prints_restic_snapshots() {
        let snapshots = parse_snapshots(SNAPSHOTS.as_bytes()).unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(
            snapshots[0].line(),
            "3612fbdc  2026-11-16 14:45:54  added   47.1 KiB  size    2.0 GiB  upkeep,job=4,plan=snapshot"
        );
        assert_eq!(
            snapshots[1].line(),
            "be7009a1  2026-11-16 15:00:00  added          -  size          -  "
        );
    }

    #[test]
    fn describes_snapshot_and_retention_results() {
        let snapshot = json!({"snapshot_id": "3612fbdc", "data_added": 2048, "data_added_packed": 1024,
                              "total_bytes_processed": 1048576});
        assert_eq!(
            describe_result(&json!("snapshot"), &snapshot).unwrap(),
            "snapshot 3612fbdc: 2.0 KiB new data, 1.0 KiB sent after compression, 1.0 MiB scanned"
        );
        assert_eq!(
            describe_result(&json!("retention"), &json!({"kept": 3, "removed": 2})).unwrap(),
            "retention: kept 3, removed 2"
        );
        assert!(describe_result(&json!("update"), &json!({})).is_none());
        assert!(describe_result(&json!("snapshot"), &Value::Null).is_none());
    }
}
