use std::io::Read;
use std::path::PathBuf;

use clap::Subcommand;

use crate::client::config::ClientDir;
use crate::client::destinations::{Destination, Destinations, SshAuth, scan_host_keys};
use crate::client::hosts::{Host, Hosts};

#[derive(Debug, Subcommand)]
pub enum HostCommand {
    /// Register a runner reached on its TCP API.
    Add {
        name: String,
        /// For example http://100.64.0.22:7878.
        #[arg(long)]
        url: String,
        /// Owner-only file with the runner's token.
        #[arg(long)]
        token_file: PathBuf,
    },
    List,
    Remove {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum DestCommand {
    /// Register an SFTP destination. Its repository key is made in the client folder.
    Add {
        name: String,
        /// user@address the runners connect to.
        #[arg(long)]
        ssh: String,
        #[arg(long, default_value_t = 22)]
        port: u16,
        /// Absolute folder on the destination; each host gets a repository in it.
        #[arg(long)]
        path: String,
        /// Private key file for SSH. It is sent to the runner for each job.
        #[arg(long, required_unless_present = "password_stdin")]
        key: Option<PathBuf>,
        /// Read the SSH password from standard input. It is kept in an owner-only file.
        #[arg(long, conflicts_with = "key")]
        password_stdin: bool,
        /// Accepted host key, `<type> <base64>`. Repeat for several. Without it, ssh-keyscan reads them.
        #[arg(long = "host-key", value_name = "KEY")]
        host_keys: Vec<String>,
    },
    List,
    /// Remove a destination. Its repository key stays in the client folder.
    Remove {
        name: String,
    },
}

pub fn host(command: &HostCommand, dir: &ClientDir) -> anyhow::Result<()> {
    let mut hosts = Hosts::load(dir)?;
    match command {
        HostCommand::Add {
            name,
            url,
            token_file,
        } => {
            let host = Host::new(url, std::path::absolute(token_file)?)?;
            host.endpoint()?;
            hosts.add(name, host)?;
            println!("host {name} added");
        }
        HostCommand::List => {
            for (name, host) in &hosts.entries {
                println!(
                    "{name:<16}  {:<28}  token {}",
                    host.url,
                    host.token_file.display()
                );
            }
        }
        HostCommand::Remove { name } => {
            hosts.remove(name)?;
            println!("host {name} removed");
        }
    }
    Ok(())
}

pub fn dest(command: &DestCommand, dir: &ClientDir) -> anyhow::Result<()> {
    let mut destinations = Destinations::load(dir)?;
    match command {
        DestCommand::Add {
            name,
            ssh,
            port,
            path,
            key,
            password_stdin,
            host_keys,
        } => {
            let mut destination = Destination {
                ssh: ssh.clone(),
                port: *port,
                path: path.clone(),
                auth: if key.is_some() {
                    SshAuth::Key
                } else {
                    SshAuth::Password
                },
                key_file: key.as_deref().map(std::path::absolute).transpose()?,
                host_keys: host_keys.clone(),
            };
            destination.validate_address()?;
            if destination.host_keys.is_empty() {
                destination.host_keys = scan_host_keys(destination.address(), *port)?;
                println!("host keys from ssh-keyscan; check them against the destination:");
                destination
                    .host_keys
                    .iter()
                    .for_each(|key| println!("  {key}"));
            }
            let password = if *password_stdin {
                Some(read_password()?)
            } else {
                None
            };
            destinations.add(name, destination, password.as_deref())?;
            println!(
                "destination {name} added; repository key: {} (keep a copy: without it the snapshots cannot be read)",
                dir.repository_key_file(name).display()
            );
        }
        DestCommand::List => {
            for (name, destination) in &destinations.entries {
                println!(
                    "{name:<16}  {}:{}  {}  auth {:?}",
                    destination.ssh, destination.port, destination.path, destination.auth
                );
            }
        }
        DestCommand::Remove { name } => {
            destinations.remove(name)?;
            println!(
                "destination {name} removed; its repository key stays at {}",
                dir.repository_key_file(name).display()
            );
        }
    }
    Ok(())
}

/// The first line of standard input, without its line ending.
fn read_password() -> anyhow::Result<String> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let password = input.lines().next().unwrap_or_default().to_owned();
    anyhow::ensure!(!password.is_empty(), "no password on standard input");
    Ok(password)
}
