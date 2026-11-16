use std::net::SocketAddr;
use std::path::Path;

use serde::Deserialize;

use crate::monitor::settings::MonitorSettings;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
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
    #[error("reboot_command must not be empty")]
    EmptyRebootCommand,
}

/// How a step process is detached from the runner.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LauncherKind {
    /// systemd-run when running as root under systemd, setsid otherwise.
    #[default]
    Auto,
    Systemd,
    Setsid,
}

/// Runner settings from `<config>/upkeep.toml`. The file is optional.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// TCP address of the token-protected API. No TCP listener when absent.
    pub listen: Option<SocketAddr>,
    #[serde(default = "default_reboot_command")]
    pub reboot_command: Vec<String>,
    #[serde(default)]
    pub launcher: LauncherKind,
    /// Host name sent to adapters. Defaults to the system host name.
    pub host: Option<String>,
    #[serde(default)]
    pub monitor: MonitorSettings,
}

fn default_reboot_command() -> Vec<String> {
    vec!["systemctl".into(), "reboot".into()]
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen: None,
            reboot_command: default_reboot_command(),
            launcher: LauncherKind::Auto,
            host: None,
            monitor: MonitorSettings::default(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        let display = path.display().to_string();
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(SettingsError::Read {
                    path: display,
                    source,
                });
            }
        };

        let settings: Self = toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: display,
            source,
        })?;
        if settings.reboot_command.is_empty() {
            return Err(SettingsError::EmptyRebootCommand);
        }
        Ok(settings)
    }

    pub fn host_name(&self) -> String {
        self.host.clone().unwrap_or_else(|| {
            nix::unistd::gethostname()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".into())
        })
    }
}
