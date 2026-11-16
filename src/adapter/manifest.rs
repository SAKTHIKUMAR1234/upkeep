use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const RESERVED_NAMES: [&str; 2] = ["all", "writers"];

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("{0}")]
    Parse(#[from] toml::de::Error),
    #[error("name \"{name}\" must match the folder name \"{folder}\"")]
    NameMismatch { name: String, folder: String },
    #[error("invalid {what} name \"{name}\": use lowercase letters, digits, '-' and '_'")]
    InvalidName { what: &'static str, name: String },
    #[error("adapter name \"{0}\" is reserved for plan selectors")]
    ReservedName(String),
    #[error("run must not be empty")]
    EmptyRun,
    #[error("at least one action is required")]
    NoActions,
    #[error("action \"{action}\": invalid timeout \"{value}\": {reason}")]
    BadTimeout {
        action: String,
        value: String,
        reason: String,
    },
    #[error("action \"{action}\" needs unknown action \"{need}\"")]
    UnknownNeed { action: String, need: String },
    #[error("duplicate {what} name \"{name}\"")]
    DuplicateCheck { what: &'static str, name: String },
    #[error("ping \"{0}\" must set exactly one of http or command")]
    PingTarget(String),
    #[error("ping \"{0}\": expect is only valid with http")]
    ExpectWithoutHttp(String),
    #[error("{what} \"{name}\": invalid timeout \"{value}\"")]
    BadCheckTimeout {
        what: &'static str,
        name: String,
        value: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    run: String,
    display_name: Option<String>,
    #[serde(default)]
    writes_data: bool,
    #[serde(default)]
    actions: BTreeMap<String, RawAction>,
    #[serde(default, rename = "service")]
    services: Vec<Service>,
    #[serde(default, rename = "ping")]
    pings: Vec<RawPing>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAction {
    title: Option<String>,
    timeout: Option<String>,
    #[serde(default)]
    needs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPing {
    name: String,
    http: Option<String>,
    expect: Option<u16>,
    command: Option<String>,
    timeout: Option<String>,
}

/// A validated `adapter.toml`.
#[derive(Clone, Debug, Serialize)]
pub struct Manifest {
    pub name: String,
    pub run: String,
    /// The plain name in messages, such as "HR staging".
    pub display_name: Option<String>,
    pub writes_data: bool,
    pub actions: BTreeMap<String, Action>,
    pub services: Vec<Service>,
    pub pings: Vec<Ping>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Action {
    /// The plain step name in messages, such as "Taking snapshot".
    pub title: Option<String>,
    #[serde(rename = "timeout_secs", serialize_with = "serialize_secs")]
    pub timeout: Duration,
    pub needs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Service {
    Systemd {
        name: String,
        unit: String,
    },
    SystemdUser {
        name: String,
        unit: String,
        user: String,
    },
    Docker {
        name: String,
        container: String,
    },
    Process {
        name: String,
        pattern: String,
    },
}

impl Service {
    pub fn name(&self) -> &str {
        match self {
            Self::Systemd { name, .. }
            | Self::SystemdUser { name, .. }
            | Self::Docker { name, .. }
            | Self::Process { name, .. } => name,
        }
    }

    /// The unit, container or process pattern the service check looks at.
    pub fn target(&self) -> &str {
        match self {
            Self::Systemd { unit, .. } | Self::SystemdUser { unit, .. } => unit,
            Self::Docker { container, .. } => container,
            Self::Process { pattern, .. } => pattern,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Ping {
    pub name: String,
    #[serde(flatten)]
    pub target: PingTarget,
    #[serde(rename = "timeout_secs", serialize_with = "serialize_secs")]
    pub timeout: Duration,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PingTarget {
    Http { url: String, expect: u16 },
    Command { command: String },
}

impl PingTarget {
    /// The URL or command the ping uses.
    pub fn target(&self) -> &str {
        match self {
            Self::Http { url, .. } => url,
            Self::Command { command } => command,
        }
    }
}

fn serialize_secs<S: serde::Serializer>(
    value: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(value.as_secs())
}

impl Manifest {
    /// Parses and validates a manifest found in the folder `folder`.
    pub fn parse(text: &str, folder: &str) -> Result<Self, ManifestError> {
        let raw: RawManifest = toml::from_str(text)?;
        validate_adapter_name(&raw.name, folder)?;
        if raw.run.trim().is_empty() {
            return Err(ManifestError::EmptyRun);
        }

        let actions = parse_actions(raw.actions)?;
        let services = validate_services(raw.services)?;
        let pings = parse_pings(raw.pings)?;
        Ok(Self {
            name: raw.name,
            run: raw.run,
            display_name: raw.display_name,
            writes_data: raw.writes_data,
            actions,
            services,
            pings,
        })
    }
}

pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    first_ok && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn check_name(what: &'static str, name: &str) -> Result<(), ManifestError> {
    if is_valid_name(name) {
        return Ok(());
    }
    Err(ManifestError::InvalidName {
        what,
        name: name.to_owned(),
    })
}

fn validate_adapter_name(name: &str, folder: &str) -> Result<(), ManifestError> {
    check_name("adapter", name)?;
    if RESERVED_NAMES.contains(&name) {
        return Err(ManifestError::ReservedName(name.to_owned()));
    }
    if name != folder {
        return Err(ManifestError::NameMismatch {
            name: name.to_owned(),
            folder: folder.to_owned(),
        });
    }
    Ok(())
}

fn parse_actions(
    raw: BTreeMap<String, RawAction>,
) -> Result<BTreeMap<String, Action>, ManifestError> {
    if raw.is_empty() {
        return Err(ManifestError::NoActions);
    }

    let names: BTreeSet<&String> = raw.keys().collect();
    for (name, action) in &raw {
        check_name("action", name)?;
        if let Some(need) = action
            .needs
            .iter()
            .find(|need| !names.contains(need) || *need == name)
        {
            return Err(ManifestError::UnknownNeed {
                action: name.clone(),
                need: need.clone(),
            });
        }
    }

    raw.into_iter()
        .map(|(name, action)| {
            let timeout = parse_action_timeout(&name, action.timeout.as_deref())?;
            let parsed = Action {
                title: action.title,
                timeout,
                needs: action.needs,
            };
            Ok((name, parsed))
        })
        .collect()
}

fn parse_action_timeout(action: &str, value: Option<&str>) -> Result<Duration, ManifestError> {
    let Some(value) = value else {
        return Ok(DEFAULT_ACTION_TIMEOUT);
    };
    let bad = |reason: String| ManifestError::BadTimeout {
        action: action.to_owned(),
        value: value.to_owned(),
        reason,
    };
    let timeout = humantime::parse_duration(value).map_err(|error| bad(error.to_string()))?;
    if timeout.is_zero() {
        return Err(bad("must be greater than zero".into()));
    }
    Ok(timeout)
}

fn ensure_unique<'a>(
    what: &'static str,
    names: impl Iterator<Item = &'a str>,
) -> Result<(), ManifestError> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name) {
            return Err(ManifestError::DuplicateCheck {
                what,
                name: name.to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_services(services: Vec<Service>) -> Result<Vec<Service>, ManifestError> {
    ensure_unique("service", services.iter().map(Service::name))?;
    Ok(services)
}

fn parse_pings(raw: Vec<RawPing>) -> Result<Vec<Ping>, ManifestError> {
    ensure_unique("ping", raw.iter().map(|ping| ping.name.as_str()))?;
    raw.into_iter().map(parse_ping).collect()
}

fn parse_ping(raw: RawPing) -> Result<Ping, ManifestError> {
    let target = match (raw.http, raw.command) {
        (Some(url), None) => PingTarget::Http {
            url,
            expect: raw.expect.unwrap_or(200),
        },
        (None, Some(command)) if raw.expect.is_none() => PingTarget::Command { command },
        (None, Some(_)) => return Err(ManifestError::ExpectWithoutHttp(raw.name)),
        _ => return Err(ManifestError::PingTarget(raw.name)),
    };

    let timeout = match raw.timeout {
        None => DEFAULT_CHECK_TIMEOUT,
        Some(value) => {
            humantime::parse_duration(&value).map_err(|_| ManifestError::BadCheckTimeout {
                what: "ping",
                name: raw.name.clone(),
                value,
            })?
        }
    };
    Ok(Ping {
        name: raw.name,
        target,
        timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
name = "bench"
run = "python3 adapter.py"
display_name = "HR staging"
writes_data = true

[actions.maintenance_on]
[actions.backup]
title = "Saving a backup"
timeout = "60m"
[actions.update]
timeout = "90m"
needs = ["maintenance_on", "backup"]

[[service]]
name = "web"
kind = "systemd-user"
unit = "bench-web.service"
user = "frappe"

[[service]]
name = "db"
kind = "docker"
container = "mariadb"

[[ping]]
name = "site"
http = "https://example.test/api/method/ping"

[[ping]]
name = "queue"
command = "true"
timeout = "5s"
"#;

    fn error_of(text: &str) -> String {
        Manifest::parse(text, "bench").unwrap_err().to_string()
    }

    #[test]
    fn parses_a_full_manifest() {
        let manifest = Manifest::parse(VALID, "bench").unwrap();

        assert!(manifest.writes_data);
        assert_eq!(manifest.display_name.as_deref(), Some("HR staging"));
        assert_eq!(
            manifest.actions["backup"].title.as_deref(),
            Some("Saving a backup")
        );
        assert_eq!(manifest.actions["update"].title, None);
        assert_eq!(
            manifest.actions["backup"].timeout,
            Duration::from_secs(3600)
        );
        assert_eq!(
            manifest.actions["maintenance_on"].timeout,
            DEFAULT_ACTION_TIMEOUT
        );
        assert_eq!(
            manifest.actions["update"].needs,
            ["maintenance_on", "backup"]
        );
        assert_eq!(manifest.services.len(), 2);
        assert_eq!(
            manifest.pings[0].target,
            PingTarget::Http {
                url: "https://example.test/api/method/ping".into(),
                expect: 200
            }
        );
        assert_eq!(manifest.pings[1].timeout, Duration::from_secs(5));
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = VALID.replace(
            "writes_data = true",
            "writes_data = true\nwrite_data = true",
        );
        assert!(error_of(&text).contains("unknown field `write_data`"));
    }

    #[test]
    fn rejects_name_that_differs_from_folder() {
        assert!(
            Manifest::parse(VALID, "other")
                .unwrap_err()
                .to_string()
                .contains("folder name")
        );
    }

    #[test]
    fn rejects_reserved_and_invalid_names() {
        let reserved = VALID.replace("name = \"bench\"", "name = \"all\"");
        assert!(
            Manifest::parse(&reserved, "all")
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );

        let invalid = VALID.replace("[actions.backup]", "[actions.Backup]");
        assert!(error_of(&invalid).contains("invalid action name"));
    }

    #[test]
    fn rejects_empty_run_and_missing_actions() {
        assert!(
            error_of("name = \"bench\"\nrun = \" \"\n[actions.a]\n")
                .contains("run must not be empty")
        );
        assert!(error_of("name = \"bench\"\nrun = \"x\"\n").contains("at least one action"));
    }

    #[test]
    fn rejects_bad_timeouts() {
        assert!(error_of(&VALID.replace("\"60m\"", "\"soon\"")).contains("invalid timeout"));
        assert!(error_of(&VALID.replace("\"60m\"", "\"0s\"")).contains("greater than zero"));
    }

    #[test]
    fn rejects_unknown_or_self_needs() {
        let unknown = VALID.replace("[\"maintenance_on\", \"backup\"]", "[\"migrate\"]");
        assert!(error_of(&unknown).contains("needs unknown action \"migrate\""));

        let itself = VALID.replace("[\"maintenance_on\", \"backup\"]", "[\"update\"]");
        assert!(error_of(&itself).contains("needs unknown action \"update\""));
    }

    #[test]
    fn rejects_bad_services() {
        let unknown_kind = VALID.replace("kind = \"docker\"", "kind = \"windows-service\"");
        assert!(error_of(&unknown_kind).contains("unknown variant"));

        let missing_user = VALID.replace("user = \"frappe\"\n", "");
        assert!(error_of(&missing_user).contains("missing field `user`"));

        let duplicate = VALID.replace("name = \"db\"", "name = \"web\"");
        assert!(error_of(&duplicate).contains("duplicate service name \"web\""));
    }

    #[test]
    fn rejects_bad_pings() {
        let both = VALID.replace(
            "command = \"true\"",
            "command = \"true\"\nhttp = \"http://x\"",
        );
        assert!(error_of(&both).contains("exactly one of http or command"));

        let expect = VALID.replace("command = \"true\"", "command = \"true\"\nexpect = 200");
        assert!(error_of(&expect).contains("expect is only valid with http"));
    }
}
