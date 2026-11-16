use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic_file;

/// Shorter values are too likely to appear in normal log text to be redacted.
const MIN_REDACTED_LENGTH: usize = 4;
const REDACTED: &str = "[redacted]";

/// Secret values for the adapters of one job, such as a repository key or an SSH password.
/// The runner keeps them in a root-only file while the job runs, never in the database or a log.
#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secrets(BTreeMap<String, String>);

impl fmt::Debug for Secrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&String> = self.0.keys().collect();
        write!(formatter, "Secrets({names:?}, values redacted)")
    }
}

impl Secrets {
    pub fn insert(&mut self, name: &str, value: String) {
        self.0.insert(name.to_owned(), value);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Writes the secrets to an owner-only file in an owner-only folder.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        atomic_file::write_private(path, &serde_json::to_vec(self)?)
    }

    /// Reads a secrets file; a missing file means no secrets.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn redactor(&self) -> Redactor {
        let mut needles: Vec<String> = self
            .0
            .values()
            .flat_map(|value| std::iter::once(value.as_str()).chain(value.lines()))
            .map(str::trim)
            .filter(|needle| needle.len() >= MIN_REDACTED_LENGTH)
            .map(str::to_owned)
            .collect();
        needles.sort_by_key(|needle| std::cmp::Reverse(needle.len()));
        needles.dedup();
        Redactor { needles }
    }
}

/// Replaces secret values in text before it reaches a log or a result.
/// Each line of a multi-line secret, such as a private key, is replaced on its own too.
#[derive(Clone, Debug, Default)]
pub struct Redactor {
    needles: Vec<String>,
}

impl Redactor {
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut text = Cow::Borrowed(text);
        for needle in &self.needles {
            if text.contains(needle.as_str()) {
                text = Cow::Owned(text.replace(needle.as_str(), REDACTED));
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets(pairs: &[(&str, &str)]) -> Secrets {
        let mut secrets = Secrets::default();
        for (name, value) in pairs {
            secrets.insert(name, (*value).to_owned());
        }
        secrets
    }

    #[test]
    fn redacts_values_and_each_line_of_a_multi_line_value() {
        let redactor = secrets(&[
            ("ssh_password", "hunter2-pass"),
            (
                "ssh_key",
                "-----BEGIN KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END KEY-----\n",
            ),
        ])
        .redactor();

        assert_eq!(
            redactor.redact("login with hunter2-pass failed"),
            "login with [redacted] failed"
        );
        assert_eq!(
            redactor.redact("key line b3BlbnNzaC1rZXktdjEAAAAA leaked"),
            "key line [redacted] leaked"
        );
        assert!(matches!(
            redactor.redact("nothing secret"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn short_values_are_not_redacted() {
        let redactor = secrets(&[("pin", "ab")]).redactor();
        assert_eq!(redactor.redact("abc"), "abc");
    }

    #[test]
    fn debug_output_hides_values() {
        let text = format!("{:?}", secrets(&[("repository_key", "very-secret-value")]));
        assert!(text.contains("repository_key"));
        assert!(!text.contains("very-secret-value"));
    }

    #[test]
    fn saved_file_is_owner_only_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets").join("7.json");
        let saved = secrets(&[("repository_key", "k3y-value")]);

        saved.save(&path).unwrap();

        assert_eq!(Secrets::load(&path).unwrap(), saved);
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!((file_mode & 0o777, dir_mode & 0o777), (0o600, 0o700));
        assert!(
            Secrets::load(&dir.path().join("missing.json"))
                .unwrap()
                .is_empty()
        );
    }
}
