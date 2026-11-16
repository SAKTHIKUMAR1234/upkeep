use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::config::{
    ClientDir, ConfigError, check_name, known_names, load_table, pick, read_secret, save_table,
};
use crate::secrets::Secrets;

const TABLE: &str = "destinations";
const REPOSITORY_KEY_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SshAuth {
    Key,
    Password,
}

/// Where snapshots go: an SFTP folder reached with SSH. Each host gets its own repository in it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    /// `user@address`.
    pub ssh: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Absolute folder on the destination.
    pub path: String,
    pub auth: SshAuth,
    /// Private key sent to the runner for each job when `auth = "key"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
    /// Accepted SSH host keys, `<type> <base64>`. The runner checks the destination against them.
    pub host_keys: Vec<String>,
}

fn default_port() -> u16 {
    22
}

impl Destination {
    pub fn validate_address(&self) -> Result<(), ConfigError> {
        match self.ssh.split_once('@') {
            Some((user, address)) if !user.is_empty() && !address.is_empty() => Ok(()),
            _ => Err(ConfigError::Invalid(format!(
                "invalid --ssh {:?}: use user@address",
                self.ssh
            ))),
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: String| Err(ConfigError::Invalid(message));
        self.validate_address()?;
        if !self.path.starts_with('/') {
            return invalid(format!("--path {:?} must be absolute", self.path));
        }
        if (self.auth == SshAuth::Key) != self.key_file.is_some() {
            return invalid("use either --key <file> or --password-stdin".into());
        }
        if self.host_keys.is_empty() {
            return invalid("no SSH host keys for the destination".into());
        }
        if let Some(bad) = self
            .host_keys
            .iter()
            .find(|key| key.split_whitespace().count() != 2)
        {
            return invalid(format!("invalid host key {bad:?}: use \"<type> <base64>\""));
        }
        Ok(())
    }

    pub fn user(&self) -> &str {
        self.ssh.split_once('@').map_or("", |(user, _)| user)
    }

    pub fn address(&self) -> &str {
        self.ssh.split_once('@').map_or("", |(_, address)| address)
    }

    /// The repository folder of one host.
    pub fn repository(&self, host: &str) -> String {
        format!("{}/{host}", self.path.trim_end_matches('/'))
    }

    /// The non-secret part the runner gets in the job params.
    pub fn job_params(&self, name: &str, host: &str) -> Value {
        json!({
            "name": name,
            "user": self.user(),
            "address": self.address(),
            "port": self.port,
            "repository": self.repository(host),
            "auth": self.auth,
            "host_keys": self.host_keys,
        })
    }

    /// The repository key and the SSH key or password, read from the client folder.
    pub fn job_secrets(&self, dir: &ClientDir, name: &str) -> Result<Secrets, ConfigError> {
        let mut secrets = Secrets::default();
        secrets.insert(
            "repository_key",
            read_secret(&dir.repository_key_file(name))?,
        );
        match &self.key_file {
            Some(key_file) => secrets.insert("ssh_key", read_secret(key_file)?),
            None => secrets.insert("ssh_password", read_secret(&dir.ssh_password_file(name))?),
        }
        Ok(secrets)
    }
}

/// `destinations.toml` in the client folder, with the secrets in `secrets/`.
pub struct Destinations {
    dir: ClientDir,
    pub entries: BTreeMap<String, Destination>,
}

impl Destinations {
    pub fn load(dir: &ClientDir) -> Result<Self, ConfigError> {
        Ok(Self {
            entries: load_table(&dir.destinations_file(), TABLE)?,
            dir: dir.clone(),
        })
    }

    pub fn get(&self, name: &str) -> Result<&Destination, ConfigError> {
        pick(&self.entries, "destination", name)
    }

    /// The named destination, or the only one when no name is given.
    pub fn choose(&self, name: Option<&str>) -> Result<(String, &Destination), ConfigError> {
        match name {
            Some(name) => Ok((name.to_owned(), self.get(name)?)),
            None if self.entries.len() == 1 => {
                let (name, destination) = self.entries.iter().next().expect("one entry");
                Ok((name.clone(), destination))
            }
            None => Err(ConfigError::Invalid(format!(
                "choose a destination with --dest; known: {}",
                known_names(&self.entries)
            ))),
        }
    }

    /// Saves a destination and its password, and makes its repository key when it has none.
    pub fn add(
        &mut self,
        name: &str,
        destination: Destination,
        password: Option<&str>,
    ) -> Result<(), ConfigError> {
        check_name(name)?;
        destination.validate()?;
        if self.entries.contains_key(name) {
            return Err(ConfigError::Exists {
                kind: "destination",
                name: name.to_owned(),
            });
        }
        if let Some(password) = password {
            self.dir
                .write_secret(&self.dir.ssh_password_file(name), password)?;
        }
        let key_file = self.dir.repository_key_file(name);
        if !key_file.exists() {
            self.dir.write_secret(&key_file, &new_repository_key()?)?;
        }
        self.entries.insert(name.to_owned(), destination);
        self.save()
    }

    /// Removes the entry and its SSH password. The repository key stays: without it the snapshots cannot be read.
    pub fn remove(&mut self, name: &str) -> Result<Destination, ConfigError> {
        self.get(name)?;
        let destination = self.entries.remove(name).expect("checked above");
        self.save()?;
        match std::fs::remove_file(self.dir.ssh_password_file(name)) {
            Err(source) if source.kind() != std::io::ErrorKind::NotFound => {
                return Err(ConfigError::Write {
                    path: self.dir.ssh_password_file(name).display().to_string(),
                    source,
                });
            }
            _ => {}
        }
        Ok(destination)
    }

    fn save(&self) -> Result<(), ConfigError> {
        save_table(
            &self.dir,
            &self.dir.destinations_file(),
            TABLE,
            &self.entries,
        )
    }
}

fn new_repository_key() -> Result<String, ConfigError> {
    let mut bytes = [0u8; REPOSITORY_KEY_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|source| ConfigError::Read {
            path: "/dev/urandom".into(),
            source,
        })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Reads the destination's host keys with `ssh-keyscan`.
pub fn scan_host_keys(address: &str, port: u16) -> Result<Vec<String>, ConfigError> {
    let output = Command::new("ssh-keyscan")
        .args(["-T", "10", "-p", &port.to_string(), address])
        .output()
        .map_err(|source| ConfigError::Read {
            path: "ssh-keyscan".into(),
            source,
        })?;
    let keys = parse_keyscan(&String::from_utf8_lossy(&output.stdout));
    if keys.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "ssh-keyscan found no host keys at {address}:{port}; pass --host-key"
        )));
    }
    Ok(keys)
}

/// `<type> <base64>` from `ssh-keyscan` lines `<host> <type> <base64>`.
pub fn parse_keyscan(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(
            |line| match line.split_whitespace().collect::<Vec<_>>()[..] {
                [_, kind, key] => Some(format!("{kind} {key}")),
                _ => None,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn destination(auth: SshAuth, key_file: Option<PathBuf>) -> Destination {
        Destination {
            ssh: "backup@100.64.0.21".into(),
            port: 2222,
            path: "/backups/upkeep/".into(),
            auth,
            key_file,
            host_keys: vec!["ssh-ed25519 AAAAC3Nza".into()],
        }
    }

    #[test]
    fn validates_the_fields() {
        assert!(destination(SshAuth::Password, None).validate().is_ok());
        let cases = [
            Destination {
                ssh: "nouser".into(),
                ..destination(SshAuth::Password, None)
            },
            Destination {
                path: "relative".into(),
                ..destination(SshAuth::Password, None)
            },
            Destination {
                host_keys: vec![],
                ..destination(SshAuth::Password, None)
            },
            Destination {
                host_keys: vec!["onlytype".into()],
                ..destination(SshAuth::Password, None)
            },
            destination(SshAuth::Key, None),
            destination(SshAuth::Password, Some("/key".into())),
        ];
        for case in cases {
            assert!(case.validate().is_err(), "{case:?}");
        }
    }

    #[test]
    fn job_params_have_no_secrets_and_name_the_host_repository() {
        let params = destination(SshAuth::Password, None).job_params("laptop", "vm-a");
        assert_eq!(
            params,
            json!({
                "name": "laptop", "user": "backup", "address": "100.64.0.21", "port": 2222,
                "repository": "/backups/upkeep/vm-a", "auth": "password",
                "host_keys": ["ssh-ed25519 AAAAC3Nza"],
            })
        );
    }

    #[test]
    fn add_keeps_secrets_owner_only_and_remove_keeps_the_repository_key() {
        let temporary = tempfile::tempdir().unwrap();
        let dir = ClientDir::new(temporary.path().join("upkeep"));
        let mut destinations = Destinations::load(&dir).unwrap();

        destinations
            .add(
                "nas",
                destination(SshAuth::Password, None),
                Some("pa55word"),
            )
            .unwrap();

        let secrets = Destinations::load(&dir)
            .unwrap()
            .get("nas")
            .unwrap()
            .job_secrets(&dir, "nas")
            .unwrap();
        let redactor = secrets.redactor();
        assert_eq!(redactor.redact("pa55word"), "[redacted]");
        let key = std::fs::read_to_string(dir.repository_key_file("nas")).unwrap();
        assert_eq!(key.len(), REPOSITORY_KEY_BYTES * 2);
        for file in [dir.repository_key_file("nas"), dir.ssh_password_file("nas")] {
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", file.display());
        }

        destinations.remove("nas").unwrap();
        assert!(!dir.ssh_password_file("nas").exists());
        assert!(dir.repository_key_file("nas").exists());

        destinations
            .add("nas", destination(SshAuth::Password, None), Some("other"))
            .unwrap();
        let kept = std::fs::read_to_string(dir.repository_key_file("nas")).unwrap();
        assert_eq!(kept, key, "a re-added destination keeps its repository key");
    }

    #[test]
    fn job_secrets_refuse_a_key_readable_by_others() {
        let temporary = tempfile::tempdir().unwrap();
        let dir = ClientDir::new(temporary.path().join("upkeep"));
        let key_file = temporary.path().join("id_ed25519");
        std::fs::write(&key_file, "-----BEGIN KEY-----\n").unwrap();
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut destinations = Destinations::load(&dir).unwrap();
        destinations
            .add(
                "laptop",
                destination(SshAuth::Key, Some(key_file.clone())),
                None,
            )
            .unwrap();

        let error = destinations
            .get("laptop")
            .unwrap()
            .job_secrets(&dir, "laptop");
        assert!(error.unwrap_err().to_string().contains("chmod 600"));
    }

    #[test]
    fn choose_needs_a_name_when_there_are_several() {
        let temporary = tempfile::tempdir().unwrap();
        let dir = ClientDir::new(temporary.path().join("upkeep"));
        let mut destinations = Destinations::load(&dir).unwrap();
        destinations
            .add("a", destination(SshAuth::Password, None), Some("x"))
            .unwrap();
        assert_eq!(destinations.choose(None).unwrap().0, "a");

        destinations
            .add("b", destination(SshAuth::Password, None), Some("x"))
            .unwrap();
        assert!(destinations.choose(None).is_err());
        assert_eq!(destinations.choose(Some("b")).unwrap().0, "b");
    }

    #[test]
    fn parses_keyscan_output() {
        let output = "# 10.0.2.2:22 SSH-2.0-OpenSSH_9.6\n10.0.2.2 ssh-ed25519 AAAAC3Nza\n[h]:2222 ssh-rsa AAAAB3Nz\n";
        assert_eq!(
            parse_keyscan(output),
            ["ssh-ed25519 AAAAC3Nza", "ssh-rsa AAAAB3Nz"]
        );
    }
}
