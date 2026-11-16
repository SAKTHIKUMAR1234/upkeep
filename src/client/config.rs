use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::atomic_file;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot find the client config folder: set HOME or --client-dir")]
    NoHome,
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("invalid name {0:?}: use lowercase letters, digits, - and _")]
    InvalidName(String),
    #[error("no {kind} named {name:?}; known: {known}")]
    Unknown {
        kind: &'static str,
        name: String,
        known: String,
    },
    #[error("{kind} {name:?} already exists; remove it first")]
    Exists { kind: &'static str, name: String },
    #[error("{0}")]
    Invalid(String),
}

/// The client's own folder, `~/.config/upkeep` by default: hosts, destinations and their secrets.
#[derive(Clone, Debug)]
pub struct ClientDir {
    root: PathBuf,
}

impl ClientDir {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// `explicit`, else `$XDG_CONFIG_HOME/upkeep`, else `~/.config/upkeep`.
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self, ConfigError> {
        let from_environment = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let root = explicit
            .or_else(|| from_environment("XDG_CONFIG_HOME").map(|config| config.join("upkeep")))
            .or_else(|| from_environment("HOME").map(|home| home.join(".config/upkeep")))
            .ok_or(ConfigError::NoHome)?;
        Ok(Self::new(root))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn hosts_file(&self) -> PathBuf {
        self.root.join("hosts.toml")
    }

    pub fn destinations_file(&self) -> PathBuf {
        self.root.join("destinations.toml")
    }

    pub fn secrets_dir(&self) -> PathBuf {
        self.root.join("secrets")
    }

    /// The key that encrypts the snapshot repositories of a destination. Losing it loses the snapshots.
    pub fn repository_key_file(&self, destination: &str) -> PathBuf {
        self.secrets_dir()
            .join(format!("{destination}.repository-key"))
    }

    pub fn ssh_password_file(&self, destination: &str) -> PathBuf {
        self.secrets_dir()
            .join(format!("{destination}.ssh-password"))
    }

    /// Creates the folder and its secrets folder, readable by the owner only.
    pub fn create(&self) -> Result<(), ConfigError> {
        for dir in [self.root.clone(), self.secrets_dir()] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|source| ConfigError::Write {
                    path: dir.display().to_string(),
                    source,
                })?;
        }
        Ok(())
    }

    /// Writes a secret into an owner-only file.
    pub fn write_secret(&self, path: &Path, value: &str) -> Result<(), ConfigError> {
        self.create()?;
        atomic_file::write_private(path, value.as_bytes()).map_err(|source| ConfigError::Write {
            path: path.display().to_string(),
            source,
        })
    }
}

/// Reads a secret file that only its owner may read.
pub fn read_secret(path: &Path) -> Result<String, ConfigError> {
    let read_error = |source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    };
    let mode = std::fs::metadata(path)
        .map_err(read_error)?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(ConfigError::Invalid(format!(
            "{} must not be readable by group or others (chmod 600)",
            path.display()
        )));
    }
    std::fs::read_to_string(path).map_err(read_error)
}

/// Reads the `[<key>.<name>]` tables of a client config file; a missing file is empty.
pub fn load_table<T: DeserializeOwned>(
    path: &Path,
    key: &str,
) -> Result<BTreeMap<String, T>, ConfigError> {
    let display = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => {
            return Err(ConfigError::Read {
                path: display,
                source,
            });
        }
    };
    let mut file: BTreeMap<String, BTreeMap<String, T>> =
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: display,
            source,
        })?;
    Ok(file.remove(key).unwrap_or_default())
}

pub fn save_table<T: Serialize>(
    dir: &ClientDir,
    path: &Path,
    key: &str,
    table: &BTreeMap<String, T>,
) -> Result<(), ConfigError> {
    dir.create()?;
    let file = BTreeMap::from([(key, table)]);
    let text =
        toml::to_string_pretty(&file).map_err(|error| ConfigError::Invalid(error.to_string()))?;
    atomic_file::write(path, text.as_bytes()).map_err(|source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    })
}

pub fn check_name(name: &str) -> Result<(), ConfigError> {
    let valid = name.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || "-_".contains(character)
    }) && name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidName(name.to_owned()))
    }
}

/// Picks `name` from a table, or fails with the known names.
pub fn pick<'a, T>(
    table: &'a BTreeMap<String, T>,
    kind: &'static str,
    name: &str,
) -> Result<&'a T, ConfigError> {
    table.get(name).ok_or_else(|| ConfigError::Unknown {
        kind,
        name: name.to_owned(),
        known: known_names(table),
    })
}

pub fn known_names<T>(table: &BTreeMap<String, T>) -> String {
    if table.is_empty() {
        return "none".into();
    }
    table.keys().cloned().collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct Item {
        url: String,
    }

    #[test]
    fn tables_round_trip_and_a_missing_file_is_empty() {
        let temporary = tempfile::tempdir().unwrap();
        let dir = ClientDir::new(temporary.path().join("upkeep"));
        let path = dir.hosts_file();
        assert!(load_table::<Item>(&path, "hosts").unwrap().is_empty());

        let table = BTreeMap::from([(
            "vm-a".to_owned(),
            Item {
                url: "http://x:1".into(),
            },
        )]);
        save_table(&dir, &path, "hosts", &table).unwrap();

        assert_eq!(load_table::<Item>(&path, "hosts").unwrap(), table);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("[hosts.vm-a]")
        );
    }

    #[test]
    fn names_are_lowercase_words() {
        assert!(check_name("erp-aws_2").is_ok());
        for bad in ["", "-x", "Erp", "a b", "a/b", "a.b"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn pick_lists_known_names() {
        let table = BTreeMap::from([("laptop".to_owned(), 1)]);
        let error = pick(&table, "destination", "nas").unwrap_err().to_string();
        assert_eq!(error, "no destination named \"nas\"; known: laptop");
    }
}
