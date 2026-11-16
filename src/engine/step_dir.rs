use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::atomic_file;
use crate::paths::Paths;
use crate::store::Step;

/// What the detached step process needs to run one adapter action.
#[derive(Debug, Deserialize, Serialize)]
pub struct StepFile {
    pub adapter: String,
    pub action: String,
    pub adapter_dir: PathBuf,
    pub run: String,
    pub timeout_secs: u64,
    pub job_log: PathBuf,
    /// The job's secrets file. Its values are redacted from the log and the result.
    #[serde(default)]
    pub secrets_file: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
}

/// Written once by the step process when the adapter action ends.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ExitRecord {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}

/// Identifies the step process, safe against pid reuse and reboots.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Owner {
    pub pid: i32,
    pub boot_id: String,
    pub start_time: u64,
}

/// The folder of one step under `<log-dir>/jobs/<id>/steps/`. Its files are the
/// hand-off between the runner and the detached step process.
#[derive(Clone, Debug)]
pub struct StepDir {
    path: PathBuf,
}

impl StepDir {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn for_step(paths: &Paths, step: &Step) -> Self {
        let name = format!(
            "{}-{}-{}-{}",
            step.phase.as_str(),
            step.index,
            step.adapter,
            step.action
        );
        Self::new(paths.job_log_dir(step.job_id).join("steps").join(name))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn context_file(&self) -> PathBuf {
        self.path.join("context.json")
    }

    pub fn stdout_file(&self) -> PathBuf {
        self.path.join("stdout.jsonl")
    }

    pub fn wrapper_log(&self) -> PathBuf {
        self.path.join("wrapper.log")
    }

    pub fn prepare(&self, step: &StepFile, context: &Value) -> io::Result<()> {
        std::fs::create_dir_all(&self.path)?;
        atomic_file::write(
            &self.path.join("step.json"),
            &serde_json::to_vec_pretty(step)?,
        )?;
        atomic_file::write(&self.context_file(), &serde_json::to_vec_pretty(context)?)
    }

    pub fn read_step(&self) -> io::Result<StepFile> {
        Ok(serde_json::from_slice(&std::fs::read(
            self.path.join("step.json"),
        )?)?)
    }

    /// Takes ownership of the step for this process. Only one process ever wins.
    pub fn claim(&self) -> io::Result<bool> {
        let temporary = self.path.join(format!("owner.{}", std::process::id()));
        std::fs::write(&temporary, serde_json::to_vec(&Owner::current()?)?)?;
        let claimed = std::fs::hard_link(&temporary, self.path.join("owner.json"));
        std::fs::remove_file(&temporary)?;
        match claimed {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn owner(&self) -> io::Result<Option<Owner>> {
        read_json(&self.path.join("owner.json"))
    }

    pub fn write_exit(&self, record: &ExitRecord) -> io::Result<()> {
        atomic_file::write(
            &self.path.join("exit.json"),
            &serde_json::to_vec_pretty(record)?,
        )
    }

    pub fn read_exit(&self) -> io::Result<Option<ExitRecord>> {
        read_json(&self.path.join("exit.json"))
    }
}

impl Owner {
    pub fn current() -> io::Result<Self> {
        Ok(Self {
            pid: std::process::id() as i32,
            boot_id: current_boot_id()?,
            start_time: process_start_time(std::process::id() as i32)?,
        })
    }

    /// True while the same process still runs and is not a zombie.
    pub fn is_alive(&self) -> bool {
        let same_boot = current_boot_id().is_ok_and(|boot_id| boot_id == self.boot_id);
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)) else {
            return false;
        };
        let fields = stat_fields(&stat);
        same_boot
            && fields.first() != Some(&"Z")
            && fields.get(19).and_then(|value| value.parse().ok()) == Some(self.start_time)
    }
}

pub fn current_boot_id() -> io::Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

fn process_start_time(pid: i32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat_fields(&stat)
        .get(19)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::other("cannot parse /proc stat"))
}

/// Fields of `/proc/<pid>/stat` after the command name, starting at the state field.
fn stat_fields(stat: &str) -> Vec<&str> {
    stat.rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().collect())
        .unwrap_or_default()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_claim_wins() {
        let dir = tempfile::tempdir().unwrap();
        let step = StepDir::new(dir.path().to_path_buf());

        assert!(step.claim().unwrap());
        assert!(!step.claim().unwrap());
        assert_eq!(step.owner().unwrap().unwrap(), Owner::current().unwrap());
    }

    #[test]
    fn owner_liveness_tracks_pid_and_start_time() {
        let mut owner = Owner::current().unwrap();
        assert!(owner.is_alive());

        owner.start_time += 1;
        assert!(!owner.is_alive());

        let rebooted = Owner {
            boot_id: "other-boot".into(),
            ..Owner::current().unwrap()
        };
        assert!(!rebooted.is_alive());
    }

    #[test]
    fn exit_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let step = StepDir::new(dir.path().to_path_buf());
        assert_eq!(step.read_exit().unwrap(), None);

        let record = ExitRecord {
            code: Some(10),
            ..ExitRecord::default()
        };
        step.write_exit(&record).unwrap();
        assert_eq!(step.read_exit().unwrap(), Some(record));
    }
}
