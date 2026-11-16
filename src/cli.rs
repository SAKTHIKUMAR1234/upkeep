use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::client::{Client, Endpoint};
use crate::paths::Paths;
use crate::time::format_unix;

#[derive(Debug, Parser)]
#[command(
    name = "upkeep",
    version,
    about = "Runs adapter actions and plans as durable jobs"
)]
pub struct Cli {
    #[arg(long, global = true, default_value = "/etc/upkeep")]
    pub config_dir: PathBuf,
    #[arg(long, global = true, default_value = "/var/lib/upkeep")]
    pub state_dir: PathBuf,
    #[arg(long, global = true, default_value = "/var/log/upkeep")]
    pub log_dir: PathBuf,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the runner daemon.
    Serve,
    /// Show health and the latest job.
    Status,
    /// List adapters, their actions and manifest errors.
    Adapters,
    /// Start a plan or a single adapter:action.
    Run {
        target: String,
        /// Confirm a plan marked confirm = true.
        #[arg(long)]
        confirm: bool,
    },
    /// List recent jobs.
    Jobs {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show one job step by step.
    Job { id: i64 },
    /// Print a job's log.
    Logs {
        id: i64,
        /// Keep printing until the job ends.
        #[arg(short, long)]
        follow: bool,
    },
    /// Stop a job after its current step.
    Cancel { id: i64 },
    /// Run one step detached from the runner (internal).
    #[command(hide = true)]
    StepExec { dir: PathBuf },
}

impl Cli {
    pub fn paths(&self) -> Paths {
        Paths {
            config_dir: self.config_dir.clone(),
            state_dir: self.state_dir.clone(),
            log_dir: self.log_dir.clone(),
        }
    }
}

/// Runs a client subcommand against the local socket.
pub async fn run_client(command: &Command, paths: &Paths) -> anyhow::Result<()> {
    let client = Client::new(Endpoint::Unix(paths.socket()));
    match command {
        Command::Status => {
            print_health(&client.get("/health").await?);
            let jobs = client.get("/jobs?limit=1").await?;
            match jobs["jobs"].as_array().and_then(|jobs| jobs.first()) {
                Some(job) => println!("\nlatest job:\n{}", job_line(job)),
                None => println!("\nno jobs yet"),
            }
        }
        Command::Adapters => print_adapters(&client.get("/adapters").await?),
        Command::Run { target, confirm } => {
            let requested_by = std::env::var("SUDO_USER")
                .or_else(|_| std::env::var("USER"))
                .ok();
            let body = json!({"target": target, "confirm": confirm, "requested_by": requested_by});
            let started = client.post("/run", &body).await?;
            println!(
                "job {} started; follow it with: upkeep logs {} -f",
                started["id"], started["id"]
            );
        }
        Command::Jobs { limit } => {
            let jobs = client.get(&format!("/jobs?limit={limit}")).await?;
            jobs["jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .for_each(|job| println!("{}", job_line(job)));
        }
        Command::Job { id } => print_job(&client.get(&format!("/jobs/{id}")).await?),
        Command::Logs { id, follow } => {
            let mut stdout = std::io::stdout();
            client
                .stream(&format!("/jobs/{id}/log?follow={follow}"), |chunk| {
                    let _ = stdout.write_all(chunk).and_then(|()| stdout.flush());
                })
                .await?;
        }
        Command::Cancel { id } => {
            client
                .post(&format!("/jobs/{id}/cancel"), &json!({}))
                .await?;
            println!("job {id}: cancel requested, it stops after the current step");
        }
        Command::Serve | Command::StepExec { .. } => unreachable!("handled in main"),
    }
    Ok(())
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "-".into(),
        other => other.to_string(),
    }
}

fn job_line(job: &Value) -> String {
    let created = job["created_at"]
        .as_i64()
        .map(format_unix)
        .unwrap_or_default();
    format!(
        "{:>5}  {:<10}  {:<24}  {}  by {}",
        text(&job["id"]),
        text(&job["status"]),
        text(&job["target"]),
        created,
        text(&job["requested_by"])
    )
}

fn print_health(report: &Value) {
    let overall = if report["ok"].as_bool() == Some(true) {
        "ok"
    } else {
        "FAILED"
    };
    println!("health: {overall}");
    for item in report["items"].as_array().into_iter().flatten() {
        let state = if item["ok"].as_bool() == Some(true) {
            "ok"
        } else {
            "FAILED"
        };
        let detail = item["detail"]
            .as_str()
            .map(|detail| format!("  ({detail})"))
            .unwrap_or_default();
        println!(
            "  {:<6}  {:<12}  {:<8}  {}{detail}",
            state,
            text(&item["adapter"]),
            text(&item["kind"]),
            text(&item["name"])
        );
    }
    print_errors(report);
}

fn print_adapters(listing: &Value) {
    for adapter in listing["adapters"].as_array().into_iter().flatten() {
        let actions: Vec<&String> = adapter["actions"]
            .as_object()
            .map(|map| map.keys().collect())
            .unwrap_or_default();
        let writes = if adapter["writes_data"].as_bool() == Some(true) {
            "  [writes data]"
        } else {
            ""
        };
        println!("{}{writes}", text(&adapter["name"]));
        println!(
            "  actions: {}",
            actions
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    print_errors(listing);
}

fn print_errors(listing: &Value) {
    for error in listing["errors"].as_array().into_iter().flatten() {
        println!(
            "INVALID {}: {}",
            text(&error["path"]),
            text(&error["error"])
        );
    }
}

fn print_job(detail: &Value) {
    let job = &detail["job"];
    println!("{}", job_line(job));
    if let Some(error) = job["error"].as_str() {
        println!("  reason: {error}");
    }
    for step in detail["steps"].as_array().into_iter().flatten() {
        let error = step["error"]
            .as_str()
            .map(|error| format!("  ({error})"))
            .unwrap_or_default();
        println!(
            "  {:<8} {:>2}  {:<9}  {}:{}{error}",
            text(&step["phase"]),
            text(&step["index"]),
            text(&step["status"]),
            text(&step["adapter"]),
            text(&step["action"])
        );
    }
}
