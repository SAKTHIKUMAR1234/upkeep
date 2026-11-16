use std::path::PathBuf;

/// The three directories the runner works in, and every path derived from them.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl Paths {
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("upkeep.toml")
    }

    pub fn token_file(&self) -> PathBuf {
        self.config_dir.join("token")
    }

    pub fn adapters_dir(&self) -> PathBuf {
        self.config_dir.join("adapters")
    }

    pub fn plans_dir(&self) -> PathBuf {
        self.config_dir.join("plans")
    }

    pub fn database(&self) -> PathBuf {
        self.state_dir.join("upkeep.db")
    }

    pub fn socket(&self) -> PathBuf {
        self.state_dir.join("upkeep.sock")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join("upkeep.lock")
    }

    pub fn adapter_state_dir(&self, adapter: &str) -> PathBuf {
        self.state_dir.join("adapters").join(adapter)
    }

    pub fn job_log_dir(&self, job_id: i64) -> PathBuf {
        self.log_dir.join("jobs").join(job_id.to_string())
    }

    pub fn job_log(&self, job_id: i64) -> PathBuf {
        self.job_log_dir(job_id).join("job.log")
    }
}
