use std::process::Stdio;

use tokio::process::Command;

use super::step_dir::StepDir;
use crate::settings::LauncherKind;

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("cannot start step process: {0}")]
    Io(#[from] std::io::Error),
    #[error("systemd-run failed: {0}")]
    SystemdRun(String),
}

/// Starts the `upkeep step-exec` process for a step, detached from the runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launcher {
    /// A transient systemd unit: survives a restart or stop of upkeep.service.
    Systemd,
    /// A child in its own session: survives the runner process, not a stop of its cgroup.
    Setsid,
}

impl Launcher {
    pub fn from_kind(kind: LauncherKind) -> Self {
        match kind {
            LauncherKind::Systemd => Self::Systemd,
            LauncherKind::Setsid => Self::Setsid,
            LauncherKind::Auto if is_root_under_systemd() => Self::Systemd,
            LauncherKind::Auto => Self::Setsid,
        }
    }

    pub async fn launch(self, step: &StepDir, unit: &str) -> Result<(), LaunchError> {
        let executable = std::env::current_exe()?;
        match self {
            Self::Systemd => {
                let output = Command::new("systemd-run")
                    .arg(format!("--unit={unit}"))
                    .args(["--collect", "--quiet", "--"])
                    .arg(&executable)
                    .arg("step-exec")
                    .arg(step.path())
                    .stdin(Stdio::null())
                    .output()
                    .await?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                    return Err(LaunchError::SystemdRun(stderr));
                }
            }
            Self::Setsid => {
                let wrapper_log = std::fs::File::create(step.wrapper_log())?;
                Command::new(&executable)
                    .arg("step-exec")
                    .arg(step.path())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(wrapper_log)
                    .spawn()?;
            }
        }
        Ok(())
    }
}

fn is_root_under_systemd() -> bool {
    nix::unistd::geteuid().is_root() && std::path::Path::new("/run/systemd/system").exists()
}
