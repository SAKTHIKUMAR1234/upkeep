pub mod job_log;
pub mod launcher;
pub mod step_dir;
pub mod step_exec;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::process::Command;
use tokio::sync::broadcast;

use crate::adapter::Catalog;
use crate::paths::Paths;
use crate::plan::{JobPlan, PlanError};
use crate::secrets::Secrets;
use crate::settings::Settings;
use crate::store::{Job, JobStatus, Phase, Step, StepOutcome, StepStatus, Store, StoreError};
use job_log::JobLog;
use launcher::Launcher;
use step_dir::{ExitRecord, StepDir, StepFile, current_boot_id};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const START_GRACE: Duration = Duration::from_secs(15);
const EVENT_BUFFER: usize = 64;

/// A job started or finished. The monitor announces these on Telegram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobEvent {
    Started(i64),
    Finished(i64),
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("plan \"{0}\" needs confirmation: send confirm = true")]
    ConfirmRequired(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("job {0} is not active")]
    NotActive(i64),
    #[error("cannot keep the job secrets: {0}")]
    Secrets(std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("step files: {0}")]
    Io(#[from] std::io::Error),
}

/// A request to start a plan or a single `adapter:action`.
#[derive(Debug, Deserialize)]
pub struct RunRequest {
    pub target: String,
    #[serde(default)]
    pub confirm: bool,
    pub requested_by: Option<String>,
    /// Non-secret values for the adapters, kept with the job.
    #[serde(default)]
    pub params: Option<Map<String, Value>>,
    /// Secret values for the adapters, kept only in a root-only file until the job ends.
    #[serde(default)]
    pub secrets: Secrets,
}

/// Owns job execution on this server: starts jobs, drives their steps and resumes them after a restart.
pub struct Runner {
    paths: Paths,
    settings: Settings,
    store: Store,
    launcher: Launcher,
    host: String,
    events: broadcast::Sender<JobEvent>,
}

impl Runner {
    pub fn new(paths: Paths, settings: Settings, store: Store) -> Self {
        Self {
            launcher: Launcher::from_kind(settings.launcher),
            host: settings.host_name(),
            paths,
            settings,
            store,
            events: broadcast::channel(EVENT_BUFFER).0,
        }
    }

    /// Job starts and ends from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.events.subscribe()
    }

    fn announce(&self, event: JobEvent) {
        // An error only means that nobody listens.
        let _ = self.events.send(event);
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn catalog(&self) -> Catalog {
        Catalog::load(&self.paths.adapters_dir())
    }

    pub fn start(
        self: &Arc<Self>,
        request: RunRequest,
        default_requester: &str,
    ) -> Result<i64, RunError> {
        let plan = JobPlan::resolve(&request.target, &self.catalog(), &self.paths.plans_dir())?;
        if plan.confirm && !request.confirm {
            return Err(RunError::ConfirmRequired(plan.target));
        }

        let requester = request.requested_by.as_deref().unwrap_or(default_requester);
        let params = request.params.map(Value::Object);
        let job_id = self.store.create_job(&plan, requester, params.as_ref())?;
        if !request.secrets.is_empty()
            && let Err(error) = request.secrets.save(&self.paths.job_secrets(job_id))
        {
            self.store.finish_job(
                job_id,
                JobStatus::Failed,
                Some("cannot keep the job secrets"),
            )?;
            return Err(RunError::Secrets(error));
        }
        let steps = plan.steps.len() + plan.recovery.len();
        self.log(
            job_id,
            &format!(
                "job started: {} by {requester} ({steps} steps)",
                plan.target
            ),
        );
        tracing::info!(job_id, target = %plan.target, "job started");
        self.announce(JobEvent::Started(job_id));
        self.spawn_drive(job_id);
        Ok(job_id)
    }

    pub fn cancel(&self, job_id: i64) -> Result<(), RunError> {
        if !self.store.request_cancel(job_id)? {
            return Err(RunError::NotActive(job_id));
        }
        self.log(job_id, "cancel requested: stopping after the current step");
        Ok(())
    }

    /// Picks up the active job after a runner start: re-attaches to a running step or
    /// continues after a reboot.
    pub fn resume(self: &Arc<Self>) -> Result<(), StoreError> {
        let Some(job) = self.store.active_job()? else {
            return Ok(());
        };
        if job.status == JobStatus::Rebooting {
            let rebooted = job.reboot_boot_id.as_deref() != current_boot_id().ok().as_deref();
            let note = if rebooted {
                "host rebooted"
            } else {
                "host did not reboot"
            };
            self.log(
                job.id,
                &format!("runner started, {note}: continuing with the next step"),
            );
            self.store.set_running(job.id)?;
        } else {
            self.log(job.id, "runner restarted: re-attaching to the job");
        }
        tracing::info!(job_id = job.id, "resuming job");
        self.spawn_drive(job.id);
        Ok(())
    }

    fn spawn_drive(self: &Arc<Self>, job_id: i64) {
        let runner = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = runner.drive(job_id).await {
                tracing::error!(job_id, %error, "job aborted");
                runner.log(job_id, &format!("job aborted: {error}"));
                runner.forget_secrets(job_id);
                match runner
                    .store
                    .finish_job(job_id, JobStatus::Failed, Some(&error.to_string()))
                {
                    Ok(()) => runner.announce(JobEvent::Finished(job_id)),
                    Err(error) => tracing::error!(job_id, %error, "cannot mark job failed"),
                }
            }
        });
    }

    async fn drive(&self, job_id: i64) -> Result<(), DriveError> {
        loop {
            let job = self.store.job(job_id)?;
            let Some(step) = self.store.next_step(job_id, job.phase)? else {
                return Ok(self.finish(&job)?);
            };
            if job.phase == Phase::Main
                && job.cancel_requested
                && step.status == StepStatus::Pending
            {
                self.stop_main(&job, JobStatus::Cancelled, "cancelled by request")?;
                continue;
            }

            let outcome = self.run_step(&job, &step).await?;
            self.store
                .finish_step(&step, &outcome, &current_boot_id()?)?;
            self.log(job_id, &describe(&step, &outcome));
            match outcome.status {
                StepStatus::Reboot => match self.reboot(job_id).await {
                    Ok(()) => return Ok(()),
                    Err(reason) => {
                        self.store.set_running(job_id)?;
                        self.stop_main(&job, JobStatus::Failed, &reason)?;
                    }
                },
                StepStatus::Failed => {
                    let reason = format!("step {}:{} failed", step.adapter, step.action);
                    self.stop_main(&job, JobStatus::Failed, &reason)?;
                }
                _ => {}
            }
        }
    }

    /// Ends the main steps early and switches to `on_failure`. Recovery steps always all run.
    fn stop_main(&self, job: &Job, end_status: JobStatus, reason: &str) -> Result<(), StoreError> {
        if job.phase != Phase::Main {
            return Ok(());
        }
        self.log(job.id, &format!("{reason}: running on_failure steps"));
        self.store.enter_recovery(job.id, end_status, reason)
    }

    fn finish(&self, job: &Job) -> Result<(), StoreError> {
        let status = match job.phase {
            Phase::Recovery => job.end_status.unwrap_or(JobStatus::Failed),
            Phase::Main if self.has_degraded_step(job.id)? => JobStatus::Degraded,
            Phase::Main => JobStatus::Succeeded,
        };
        self.store.finish_job(job.id, status, None)?;
        self.announce(JobEvent::Finished(job.id));
        self.forget_secrets(job.id);
        self.log(job.id, &format!("job finished: {}", status.as_str()));
        tracing::info!(job_id = job.id, status = status.as_str(), "job finished");
        Ok(())
    }

    fn forget_secrets(&self, job_id: i64) {
        match std::fs::remove_file(self.paths.job_secrets(job_id)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                tracing::error!(job_id, %error, "cannot delete the job secrets");
            }
            _ => {}
        }
    }

    fn has_degraded_step(&self, job_id: i64) -> Result<bool, StoreError> {
        let steps = self.store.steps(job_id)?;
        Ok(steps.iter().any(|step| step.status == StepStatus::Degraded))
    }

    /// Runs a step once: launches it when nobody owns it yet, then waits for its exit record.
    async fn run_step(&self, job: &Job, step: &Step) -> Result<StepOutcome, DriveError> {
        let dir = StepDir::for_step(&self.paths, step);
        if step.status == StepStatus::Pending {
            dir.prepare(&self.step_file(step), &self.context(job, step)?)?;
            self.store.mark_step_running(step)?;
            self.log(job.id, &format!("step {} started", label(step)));
        } else {
            self.log(job.id, &format!("step {} re-attached", label(step)));
        }

        if dir.read_exit()?.is_none() && dir.owner()?.is_none() {
            let unit = format!(
                "upkeep-job{}-{}{}-{}",
                job.id,
                step.phase.as_str(),
                step.index,
                std::process::id()
            );
            if let Err(error) = self.launcher.launch(&dir, &unit).await {
                return Ok(failed(error.to_string()));
            }
        }
        Ok(wait_for_exit(&dir).await?)
    }

    fn step_file(&self, step: &Step) -> StepFile {
        let state_dir = self.paths.adapter_state_dir(&step.adapter);
        let env = BTreeMap::from([
            ("UPKEEP_JOB_ID".to_owned(), step.job_id.to_string()),
            ("UPKEEP_ACTION".to_owned(), step.action.clone()),
            ("UPKEEP_HOST".to_owned(), self.host.clone()),
            (
                "UPKEEP_STATE_DIR".to_owned(),
                state_dir.to_string_lossy().into_owned(),
            ),
        ]);
        StepFile {
            adapter: step.adapter.clone(),
            action: step.action.clone(),
            adapter_dir: PathBuf::from(&step.adapter_dir),
            run: step.run.clone(),
            timeout_secs: step.timeout_secs,
            job_log: self.paths.job_log(step.job_id),
            secrets_file: Some(self.paths.job_secrets(step.job_id)),
            env,
        }
    }

    /// The JSON the adapter reads on stdin.
    fn context(&self, job: &Job, step: &Step) -> Result<Value, DriveError> {
        let state_dir = self.paths.adapter_state_dir(&step.adapter);
        std::fs::create_dir_all(&state_dir)?;
        let previous: Vec<Value> = self
            .store
            .steps(job.id)?
            .iter()
            .filter(|earlier| earlier.finished_at.is_some())
            .map(|earlier| {
                json!({
                    "phase": earlier.phase, "index": earlier.index, "adapter": earlier.adapter,
                    "action": earlier.action, "status": earlier.status,
                    "exit_code": earlier.exit_code, "result": earlier.result,
                })
            })
            .collect();
        let secrets_file = self.paths.job_secrets(job.id);
        Ok(json!({
            "job_id": job.id,
            "target": job.target,
            "host": self.host,
            "state_dir": state_dir,
            "params": job.params.clone().unwrap_or_else(|| json!({})),
            "secrets_file": secrets_file.exists().then_some(secrets_file),
            "step": {"phase": step.phase, "index": step.index, "adapter": step.adapter, "action": step.action},
            "previous": previous,
        }))
    }

    async fn reboot(&self, job_id: i64) -> Result<(), String> {
        let command = &self.settings.reboot_command;
        self.log(
            job_id,
            &format!("reboot requested: running {}", command.join(" ")),
        );
        let status = Command::new(&command[0])
            .args(&command[1..])
            .status()
            .await
            .map_err(|error| format!("reboot command failed: {error}"))?;
        if !status.success() {
            return Err(format!("reboot command failed: {status}"));
        }
        Ok(())
    }

    fn log(&self, job_id: i64, text: &str) {
        match JobLog::open(&self.paths.job_log(job_id), "upkeep".into()) {
            Ok(log) => log.line("", text),
            Err(error) => tracing::warn!(job_id, %error, "cannot open job log"),
        }
    }
}

/// Polls the step folder until the step process records its exit or is found dead.
async fn wait_for_exit(dir: &StepDir) -> std::io::Result<StepOutcome> {
    let started = Instant::now();
    loop {
        if let Some(record) = dir.read_exit()? {
            return Ok(outcome(record));
        }
        match dir.owner()? {
            Some(owner) if owner.is_alive() => {}
            Some(_) => {
                return Ok(dir.read_exit()?.map(outcome).unwrap_or_else(|| {
                    failed("step was interrupted before it finished; it is not run again".into())
                }));
            }
            None if started.elapsed() > START_GRACE => {
                return Ok(failed("step process did not start".into()));
            }
            None => {}
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn outcome(record: ExitRecord) -> StepOutcome {
    let (status, error) = match (record.timed_out, record.code) {
        (true, _) => (
            StepStatus::Failed,
            record.error.or(Some("timed out".to_owned())),
        ),
        (false, Some(0)) => (StepStatus::Ok, None),
        (false, Some(10)) => (StepStatus::Reboot, None),
        (false, Some(20)) => (StepStatus::Degraded, None),
        (false, Some(code)) => (StepStatus::Failed, Some(format!("exit code {code}"))),
        (false, None) => {
            let reason = record
                .signal
                .map(|signal| format!("killed by signal {signal}"));
            (StepStatus::Failed, record.error.or(reason))
        }
    };
    StepOutcome {
        status,
        exit_code: record.code,
        result: record.result,
        error,
    }
}

fn failed(error: String) -> StepOutcome {
    StepOutcome {
        status: StepStatus::Failed,
        exit_code: None,
        result: None,
        error: Some(error),
    }
}

fn label(step: &Step) -> String {
    format!("{} {}:{}", step.phase.as_str(), step.adapter, step.action)
}

fn describe(step: &Step, outcome: &StepOutcome) -> String {
    let mut text = format!("step {} finished: {}", label(step), outcome.status.as_str());
    if let Some(error) = &outcome.error {
        text.push_str(&format!(" ({error})"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_exit_codes_to_step_status() {
        let status = |code, timed_out| {
            outcome(ExitRecord {
                code,
                timed_out,
                ..ExitRecord::default()
            })
            .status
        };
        assert_eq!(status(Some(0), false), StepStatus::Ok);
        assert_eq!(status(Some(10), false), StepStatus::Reboot);
        assert_eq!(status(Some(20), false), StepStatus::Degraded);
        assert_eq!(status(Some(1), false), StepStatus::Failed);
        assert_eq!(status(None, false), StepStatus::Failed);
        assert_eq!(status(Some(0), true), StepStatus::Failed);
    }
}
