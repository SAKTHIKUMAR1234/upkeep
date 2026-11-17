use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use super::AppConfig;
use super::operations::{self, OperationError};
use super::runs::ssh::{Ssh, SshError, check_key};
use super::store::ServerFields;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A server as the add and edit dialogs send it.
#[derive(Clone, Debug, Deserialize)]
pub struct ServerInput {
    pub name: String,
    pub address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub ssh_user: String,
    pub key_path: String,
}

fn default_port() -> u16 {
    22
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidServer {
    #[error("Enter a name of 1 to 80 characters.")]
    Name,
    #[error("Enter a host name or an IP address.")]
    Address,
    #[error("Enter a port from 1 to 65535.")]
    Port,
    #[error("Enter an SSH user such as ubuntu.")]
    User,
    #[error("Enter the full path of the private key file.")]
    KeyPath,
}

impl ServerInput {
    /// Trims and checks every field. `~/` in the key path becomes the home folder.
    pub fn validate(&self) -> Result<ServerFields, InvalidServer> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
            return Err(InvalidServer::Name);
        }
        let address = self.address.trim();
        if !is_valid_address(address) {
            return Err(InvalidServer::Address);
        }
        if self.port == 0 {
            return Err(InvalidServer::Port);
        }
        let ssh_user = self.ssh_user.trim();
        if !is_valid_user(ssh_user) {
            return Err(InvalidServer::User);
        }
        let key_path = expand_home(self.key_path.trim());
        if !Path::new(&key_path).is_absolute() || key_path.chars().any(char::is_control) {
            return Err(InvalidServer::KeyPath);
        }
        Ok(ServerFields {
            name: name.to_owned(),
            address: address.to_owned(),
            port: self.port,
            ssh_user: ssh_user.to_owned(),
            key_path,
        })
    }
}

/// A host name, IPv4 or IPv6 address. Never starts with `-`, so ssh cannot read it as an option.
pub fn is_valid_address(address: &str) -> bool {
    !address.is_empty()
        && address.len() <= 253
        && !address.starts_with('-')
        && address
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-:".contains(character))
}

/// A POSIX user name.
pub fn is_valid_user(user: &str) -> bool {
    let mut characters = user.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
        && user.len() <= 32
        && characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || "_-.".contains(character)
        })
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_owned(),
    }
}

/// The result of "Test connection".
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub message: String,
}

/// Connects, checks `sudo -n` and counts the operation folders.
pub async fn test_connection(config: &AppConfig, fields: &ServerFields) -> TestResult {
    if let Err(error) = check_key(Path::new(&fields.key_path)) {
        return TestResult {
            ok: false,
            message: error.to_string(),
        };
    }
    let ssh = Ssh::new(config, fields);
    let (ok, message) = match operations::list(&ssh, TEST_TIMEOUT).await {
        Ok(found) => (
            true,
            format!(
                "connected, sudo OK, /.upkeep found ({} operation{})",
                found.len(),
                if found.len() == 1 { "" } else { "s" }
            ),
        ),
        Err(OperationError::NoFolder) => (
            true,
            "connected, sudo OK, /.upkeep not found (the first new operation creates it)".into(),
        ),
        Err(OperationError::Ssh(SshError::Connect(reason))) => {
            (false, format!("cannot connect: {reason}"))
        }
        Err(error) => (false, error.to_string()),
    };
    TestResult { ok, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ServerInput {
        ServerInput {
            name: " Replica server (.11) ".into(),
            address: "100.64.0.11".into(),
            port: 22,
            ssh_user: "ubuntu".into(),
            key_path: "/home/a/.ssh/id_ed25519".into(),
        }
    }

    #[test]
    fn valid_input_is_trimmed() {
        let fields = input().validate().unwrap();
        assert_eq!(fields.name, "Replica server (.11)");
        let home = ServerInput {
            key_path: "~/.ssh/id".into(),
            ..input()
        };
        assert!(Path::new(&home.validate().unwrap().key_path).is_absolute());
    }

    #[test]
    fn unsafe_fields_are_refused() {
        let cases = [
            (
                ServerInput {
                    name: " ".into(),
                    ..input()
                },
                InvalidServer::Name,
            ),
            (
                ServerInput {
                    address: "-oProxyCommand=x".into(),
                    ..input()
                },
                InvalidServer::Address,
            ),
            (
                ServerInput {
                    address: "a b".into(),
                    ..input()
                },
                InvalidServer::Address,
            ),
            (
                ServerInput {
                    address: "host;reboot".into(),
                    ..input()
                },
                InvalidServer::Address,
            ),
            (ServerInput { port: 0, ..input() }, InvalidServer::Port),
            (
                ServerInput {
                    ssh_user: "-l".into(),
                    ..input()
                },
                InvalidServer::User,
            ),
            (
                ServerInput {
                    ssh_user: "Root".into(),
                    ..input()
                },
                InvalidServer::User,
            ),
            (
                ServerInput {
                    ssh_user: "a b".into(),
                    ..input()
                },
                InvalidServer::User,
            ),
            (
                ServerInput {
                    key_path: "id_ed25519".into(),
                    ..input()
                },
                InvalidServer::KeyPath,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(input.validate(), Err(expected.clone()), "{input:?}");
        }
        assert!(is_valid_address("fe80::1") && is_valid_address("db-1.example.com"));
    }
}
