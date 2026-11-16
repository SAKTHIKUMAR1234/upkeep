mod output;
mod registry;
mod snapshots;

use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde_json::{Map, Value, json};

use crate::client::config::ClientDir;
use crate::client::destinations::Destinations;
use crate::client::hosts::Hosts;
use crate::client::{Client, Endpoint};
use crate::paths::Paths;
use crate::secrets::Secrets;
pub use registry::{DestCommand, HostCommand};

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
    /// Send job commands to this registered host instead of the local runner.
    #[arg(long = "host", global = true, value_name = "NAME")]
    pub remote: Option<String>,
    /// The client's folder with hosts, destinations and their secrets [default: ~/.config/upkeep].
    #[arg(long, global = true, env = "UPKEEP_CLIENT_DIR")]
    pub client_dir: Option<PathBuf>,
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
        /// Give the job this snapshot destination, for snapshot, retention and restore steps.
        #[arg(long, value_name = "NAME")]
        dest: Option<String>,
        /// A value for the adapters, for example `--param snapshot=latest`.
        #[arg(long = "param", value_name = "KEY=VALUE")]
        params: Vec<String>,
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
    /// Register the servers this client drives.
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Register snapshot destinations.
    Dest {
        #[command(subcommand)]
        command: DestCommand,
    },
    /// Take a snapshot of a host and send it to a destination.
    Snapshot {
        host: String,
        /// Destination name.
        #[arg(long, value_name = "DEST")]
        to: String,
        /// Plan on the host that takes the snapshot and applies retention.
        #[arg(long, default_value = "snapshot")]
        plan: String,
    },
    /// List the snapshots of a host on a destination.
    Snapshots {
        host: String,
        #[arg(long, value_name = "NAME")]
        dest: Option<String>,
    },
    /// Restore a snapshot, or one path of it, into a folder on this machine.
    Restore {
        host: String,
        /// Snapshot id, or `latest`.
        snapshot: String,
        /// Restore only this path of the snapshot, for example /etc/nginx.
        #[arg(long)]
        path: Option<String>,
        /// Folder on this machine. Files keep their full path under it.
        #[arg(long, value_name = "DIR")]
        to: PathBuf,
        #[arg(long, value_name = "NAME")]
        dest: Option<String>,
    },
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

    pub fn client_dir(&self) -> anyhow::Result<ClientDir> {
        Ok(ClientDir::resolve(self.client_dir.clone())?)
    }

    /// The runner of `--host`, or the local one on the unix socket.
    fn client(&self) -> anyhow::Result<Client> {
        match &self.remote {
            Some(name) => Ok(remote_client(&self.client_dir()?, name)?),
            None => Ok(Client::new(Endpoint::Unix(self.paths().socket()))),
        }
    }

    /// The host name used for the snapshot repository: the `--host` name, or this machine's name.
    fn host_name(&self) -> String {
        self.remote.clone().unwrap_or_else(|| {
            nix::unistd::gethostname()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "localhost".into())
        })
    }
}

fn remote_client(dir: &ClientDir, name: &str) -> anyhow::Result<Client> {
    let hosts = Hosts::load(dir)?;
    Ok(Client::new(hosts.get(name)?.endpoint()?))
}

/// Runs a client subcommand.
pub async fn run_client(cli: &Cli) -> anyhow::Result<()> {
    match &cli.command {
        Command::Host { command } => registry::host(command, &cli.client_dir()?),
        Command::Dest { command } => registry::dest(command, &cli.client_dir()?),
        Command::Snapshot { host, to, plan } => {
            let dir = cli.client_dir()?;
            snapshots::take(&remote_client(&dir, host)?, &dir, host, to, plan).await
        }
        Command::Snapshots { host, dest } => {
            snapshots::list(&cli.client_dir()?, host, dest.as_deref())
        }
        Command::Restore {
            host,
            snapshot,
            path,
            to,
            dest,
        } => snapshots::restore(
            &cli.client_dir()?,
            host,
            dest.as_deref(),
            snapshot,
            path.as_deref(),
            to,
        ),
        command => run_job_command(cli, command).await,
    }
}

async fn run_job_command(cli: &Cli, command: &Command) -> anyhow::Result<()> {
    let client = cli.client()?;
    match command {
        Command::Status => {
            output::print_health(&client.get("/health").await?);
            let jobs = client.get("/jobs?limit=1").await?;
            match jobs["jobs"].as_array().and_then(|jobs| jobs.first()) {
                Some(job) => println!("\nlatest job:\n{}", output::job_line(job)),
                None => println!("\nno jobs yet"),
            }
        }
        Command::Adapters => output::print_adapters(&client.get("/adapters").await?),
        Command::Run {
            target,
            confirm,
            dest,
            params,
        } => {
            let mut job_params = parse_params(params)?;
            let mut secrets = Secrets::default();
            if let Some(dest) = dest {
                let dir = cli.client_dir()?;
                let destinations = Destinations::load(&dir)?;
                let destination = destinations.get(dest)?;
                job_params.insert(
                    "destination".into(),
                    destination.job_params(dest, &cli.host_name()),
                );
                secrets = destination.job_secrets(&dir, dest)?;
            }
            let id = start_job(&client, target, *confirm, job_params, secrets).await?;
            println!("job {id} started; follow it with: upkeep logs {id} -f");
        }
        Command::Jobs { limit } => {
            let jobs = client.get(&format!("/jobs?limit={limit}")).await?;
            jobs["jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .for_each(|job| println!("{}", output::job_line(job)));
        }
        Command::Job { id } => output::print_job(&client.get(&format!("/jobs/{id}")).await?),
        Command::Logs { id, follow } => print_log(&client, *id, *follow).await?,
        Command::Cancel { id } => {
            client
                .post(&format!("/jobs/{id}/cancel"), &json!({}))
                .await?;
            println!("job {id}: cancel requested, it stops after the current step");
        }
        _ => unreachable!("handled in main or run_client"),
    }
    Ok(())
}

/// Starts a job and returns its id. Secrets travel only in this request.
async fn start_job(
    client: &Client,
    target: &str,
    confirm: bool,
    params: Map<String, Value>,
    secrets: Secrets,
) -> anyhow::Result<i64> {
    let requested_by = std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .ok();
    let body = json!({
        "target": target, "confirm": confirm, "requested_by": requested_by,
        "params": params, "secrets": secrets,
    });
    let started = client.post("/run", &body).await?;
    started["id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("the runner returned no job id: {started}"))
}

async fn print_log(client: &Client, id: i64, follow: bool) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout();
    client
        .stream(&format!("/jobs/{id}/log?follow={follow}"), |chunk| {
            let _ = stdout.write_all(chunk).and_then(|()| stdout.flush());
        })
        .await?;
    Ok(())
}

/// `KEY=VALUE` pairs into job params.
fn parse_params(pairs: &[String]) -> anyhow::Result<Map<String, Value>> {
    pairs
        .iter()
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) if !key.is_empty() && key != "destination" => {
                Ok((key.to_owned(), Value::String(value.to_owned())))
            }
            _ => anyhow::bail!("invalid --param {pair:?}: use KEY=VALUE"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_are_key_value_pairs() {
        let params = parse_params(&["snapshot=latest".into(), "path=/a=b".into()]).unwrap();
        assert_eq!(
            Value::Object(params),
            json!({"snapshot": "latest", "path": "/a=b"})
        );
        for bad in ["novalue", "=x", "destination=x"] {
            assert!(parse_params(&[bad.into()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn global_host_flag_and_positional_host_do_not_clash() {
        let cli = Cli::try_parse_from(["upkeep", "--host", "vm-a", "jobs"]).unwrap();
        assert_eq!(cli.remote.as_deref(), Some("vm-a"));
        let cli = Cli::try_parse_from(["upkeep", "snapshot", "vm-a", "--to", "laptop"]).unwrap();
        assert!(
            matches!(cli.command, Command::Snapshot { ref host, ref to, .. } if host == "vm-a" && to == "laptop")
        );
    }
}
