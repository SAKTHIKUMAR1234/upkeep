use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Endpoint;
use super::config::{ClientDir, ConfigError, check_name, load_table, pick, save_table};
use crate::api::auth::Token;

const TABLE: &str = "hosts";

/// A server with an Upkeep runner, reached on its token-protected TCP API.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Host {
    /// `http://<address>:<port>`, the runner's `listen` address.
    pub url: String,
    /// Owner-only file with the runner's bearer token.
    pub token_file: PathBuf,
}

impl Host {
    pub fn new(url: &str, token_file: PathBuf) -> Result<Self, ConfigError> {
        let host = Self {
            url: url.trim_end_matches('/').to_owned(),
            token_file,
        };
        host.address()?;
        Ok(host)
    }

    /// `<address>:<port>` from the URL. The API has no TLS; reach it over a private network.
    pub fn address(&self) -> Result<String, ConfigError> {
        let invalid = || {
            ConfigError::Invalid(format!(
                "invalid runner URL {:?}: use http://<address>:<port>",
                self.url
            ))
        };
        let address = self.url.strip_prefix("http://").ok_or_else(invalid)?;
        let (name, port) = address.rsplit_once(':').ok_or_else(invalid)?;
        if name.is_empty() || name.contains('/') || port.parse::<u16>().is_err() {
            return Err(invalid());
        }
        Ok(address.to_owned())
    }

    pub fn endpoint(&self) -> anyhow::Result<Endpoint> {
        let token = Token::load(&self.token_file)?;
        Ok(Endpoint::Tcp {
            address: self.address()?,
            token: Some(token.as_str().to_owned()),
        })
    }
}

/// `hosts.toml` in the client folder.
pub struct Hosts {
    dir: ClientDir,
    pub entries: BTreeMap<String, Host>,
}

impl Hosts {
    pub fn load(dir: &ClientDir) -> Result<Self, ConfigError> {
        Ok(Self {
            entries: load_table(&dir.hosts_file(), TABLE)?,
            dir: dir.clone(),
        })
    }

    pub fn get(&self, name: &str) -> Result<&Host, ConfigError> {
        pick(&self.entries, "host", name)
    }

    pub fn add(&mut self, name: &str, host: Host) -> Result<(), ConfigError> {
        check_name(name)?;
        if self.entries.contains_key(name) {
            return Err(ConfigError::Exists {
                kind: "host",
                name: name.to_owned(),
            });
        }
        self.entries.insert(name.to_owned(), host);
        self.save()
    }

    pub fn remove(&mut self, name: &str) -> Result<Host, ConfigError> {
        self.get(name)?;
        let host = self.entries.remove(name).expect("checked above");
        self.save()?;
        Ok(host)
    }

    fn save(&self) -> Result<(), ConfigError> {
        save_table(&self.dir, &self.dir.hosts_file(), TABLE, &self.entries)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn url_must_be_http_with_a_port() {
        let host = |url: &str| Host::new(url, "token".into());
        assert_eq!(
            host("http://100.64.0.22:7878/").unwrap().address().unwrap(),
            "100.64.0.22:7878"
        );
        for bad in [
            "https://h:1",
            "http://h",
            "http://:1",
            "h:1",
            "http://h:x",
            "http://h:1/api",
        ] {
            assert!(host(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn add_list_and_remove_hosts() {
        let temporary = tempfile::tempdir().unwrap();
        let dir = ClientDir::new(temporary.path().join("upkeep"));
        let host = Host::new("http://127.0.0.1:7878", "/tokens/vm-a".into()).unwrap();

        let mut hosts = Hosts::load(&dir).unwrap();
        hosts.add("vm-a", host.clone()).unwrap();
        assert!(matches!(
            hosts.add("vm-a", host.clone()),
            Err(ConfigError::Exists { .. })
        ));

        let mut reloaded = Hosts::load(&dir).unwrap();
        assert_eq!(reloaded.get("vm-a").unwrap(), &host);
        reloaded.remove("vm-a").unwrap();
        assert!(Hosts::load(&dir).unwrap().entries.is_empty());
        assert!(reloaded.remove("vm-a").is_err());
    }

    #[test]
    fn endpoint_needs_an_owner_only_token_file() {
        let temporary = tempfile::tempdir().unwrap();
        let token_file = temporary.path().join("token");
        std::fs::write(&token_file, "s3cret\n").unwrap();
        let host = Host::new("http://127.0.0.1:7878", token_file.clone()).unwrap();

        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(host.endpoint().is_err());

        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        match host.endpoint().unwrap() {
            Endpoint::Tcp { address, token } => {
                assert_eq!(
                    (address.as_str(), token.as_deref()),
                    ("127.0.0.1:7878", Some("s3cret"))
                );
            }
            other => panic!("unexpected endpoint {other:?}"),
        }
    }
}
