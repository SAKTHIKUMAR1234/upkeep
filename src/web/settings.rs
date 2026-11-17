use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::client::config::check_name;
use crate::client::hosts::Host;

#[derive(Debug, thiserror::Error)]
pub enum WebSettingsError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("invalid {path}: {reason}")]
    Invalid { path: String, reason: String },
}

/// The web page settings from `<config>/web.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSettings {
    /// Address the web page listens on, such as a Tailscale address.
    pub listen: SocketAddr,
    /// The address users open, such as `https://upkeep.example.com`. Google redirects to `<public_url>/auth/callback`.
    pub public_url: String,
    pub google_client_id: String,
    /// Owner-only file with the Google OAuth client secret.
    pub google_client_secret_file: PathBuf,
    /// Users, sessions and the audit log. Defaults to `web.db` in the runner state folder.
    pub database: Option<PathBuf>,
    /// Send the session cookie over HTTPS only. Turn off only for local tests over plain HTTP.
    #[serde(default = "yes")]
    pub cookie_secure: bool,
    #[serde(default)]
    pub google: GoogleEndpoints,
    /// The runners this page shows, by name.
    pub hosts: BTreeMap<String, WebHost>,
}

fn yes() -> bool {
    true
}

/// Google's OAuth endpoints. Change them only for tests.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoogleEndpoints {
    pub auth_url: String,
    pub token_url: String,
    pub userinfo_url: String,
}

impl Default for GoogleEndpoints {
    fn default() -> Self {
        Self {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            userinfo_url: "https://openidconnect.googleapis.com/v1/userinfo".into(),
        }
    }
}

/// A runner the page shows, reached with its API token. The browser never sees the token.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebHost {
    /// The name on the server card, such as "Replica server (.11)".
    pub label: String,
    pub url: String,
    pub token_file: PathBuf,
}

impl WebHost {
    pub fn host(&self) -> anyhow::Result<Host> {
        Ok(Host::new(&self.url, self.token_file.clone())?)
    }
}

impl WebSettings {
    /// Reads and checks the settings. A missing file is an error: the page has nothing to show without hosts.
    pub fn load(path: &Path) -> Result<Self, WebSettingsError> {
        let display = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|source| WebSettingsError::Read {
            path: display.clone(),
            source,
        })?;
        let settings: Self = toml::from_str(&text).map_err(|source| WebSettingsError::Parse {
            path: display.clone(),
            source,
        })?;
        settings
            .check()
            .map_err(|reason| WebSettingsError::Invalid {
                path: display,
                reason,
            })?;
        Ok(settings)
    }

    fn check(&self) -> Result<(), String> {
        let scheme = if self.cookie_secure {
            "https://"
        } else {
            "http"
        };
        if !self.public_url.starts_with(scheme) || self.public_url.ends_with('/') {
            return Err(format!(
                "public_url {:?} must start with {scheme} and not end with /",
                self.public_url
            ));
        }
        if self.google_client_id.trim().is_empty() {
            return Err("google_client_id is empty".into());
        }
        if self.hosts.is_empty() {
            return Err("add at least one [hosts.<name>]".into());
        }
        for (name, host) in &self.hosts {
            check_name(name).map_err(|error| error.to_string())?;
            host.host()
                .map_err(|error| format!("host {name}: {error}"))?;
            if host.label.trim().is_empty() {
                return Err(format!("host {name}: label is empty"));
            }
        }
        Ok(())
    }

    /// `database`, else `web.db` in `state_dir`.
    pub fn database_path(&self, state_dir: &Path) -> PathBuf {
        self.database
            .clone()
            .unwrap_or_else(|| state_dir.join("web.db"))
    }

    pub fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.public_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
listen = "127.0.0.1:7880"
public_url = "https://upkeep.example.com"
google_client_id = "client-id"
google_client_secret_file = "/etc/upkeep/google-secret"

[hosts.replica]
label = "Replica server (.11)"
url = "http://100.64.0.11:7878"
token_file = "/etc/upkeep/hosts/replica.token"
"#;

    fn load(text: &str) -> Result<WebSettings, WebSettingsError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web.toml");
        std::fs::write(&path, text).unwrap();
        WebSettings::load(&path)
    }

    #[test]
    fn valid_settings_load_with_defaults() {
        let settings = load(VALID).unwrap();
        assert!(settings.cookie_secure);
        assert_eq!(settings.google, GoogleEndpoints::default());
        assert_eq!(
            settings.redirect_uri(),
            "https://upkeep.example.com/auth/callback"
        );
        assert_eq!(
            settings.database_path(Path::new("/var/lib/upkeep")),
            Path::new("/var/lib/upkeep/web.db")
        );
        assert_eq!(settings.hosts["replica"].label, "Replica server (.11)");
    }

    #[test]
    fn bad_settings_fail_loudly() {
        let cases = [
            (
                VALID.replace("https://upkeep", "http://upkeep"),
                "public_url",
            ),
            (VALID.replace(".com\"", ".com/\""), "public_url"),
            (VALID.replace("\"client-id\"", "\" \""), "google_client_id"),
            (
                VALID.replace("[hosts.replica]", "[hosts.Replica]"),
                "invalid name",
            ),
            (
                VALID.replace("http://100", "https://100"),
                "invalid runner URL",
            ),
            (VALID.replace("Replica server (.11)", ""), "label is empty"),
            (VALID.replace("listen", "listen_on"), "unknown field"),
            (VALID.split("[hosts").next().unwrap().to_owned(), "hosts"),
        ];
        for (text, expected) in cases {
            let error = load(&text).unwrap_err().to_string();
            assert!(error.contains(expected), "{expected}: {error}");
        }
    }

    #[test]
    fn plain_http_is_allowed_only_without_secure_cookies() {
        let local = VALID.replace("https://upkeep.example.com", "http://127.0.0.1:7880");
        assert!(load(&local).is_err());
        assert!(load(&format!("cookie_secure = false\n{local}")).is_ok());
    }
}
