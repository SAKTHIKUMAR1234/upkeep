use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use crate::time::now_rfc3339;

/// Appends timestamped lines to `<log-dir>/jobs/<id>/job.log`.
/// The runner writes between steps and the step process during its step, never both at once.
pub struct JobLog {
    file: Mutex<File>,
    label: String,
}

impl JobLog {
    pub fn open(path: &Path, label: String) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
            label,
        })
    }

    pub fn line(&self, stream: &str, text: &str) {
        let line = format!("{} [{}{stream}] {text}\n", now_rfc3339(), self.label);
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = file.write_all(line.as_bytes()) {
            tracing::warn!(%error, "cannot write job log");
        }
    }
}
