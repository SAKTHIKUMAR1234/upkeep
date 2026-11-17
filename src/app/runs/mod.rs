pub mod output;
pub mod ssh;
pub mod steps;
pub mod terminal;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

use super::AppConfig;
use super::operations::{self, OperationError};
use super::store::{
    CachedOperation, Db, NewRun, RunRecord, RunStatus, Server, StepStatus, StoreError,
};
use crate::time::now_millis;
use output::{Batch, RunLog};
use ssh::{Ssh, SshError};
use steps::{CHECK_SCRIPT, CONNECT, Outcome, RESTART, RUN_SCRIPT, Steps};
use terminal::Terminal;

const EVENT_BUFFER: usize = 1024;
const READ_BUFFER: usize = 16 * 1024;
const REBOOT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const BOOT_ID: &str = "cat /proc/sys/kernel/random/boot_id";

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(
        "{server} already has an active run: {operation} (run #{run_id}). Wait for it to end or stop it first."
    )]
    Busy {
        server: String,
        operation: String,
        run_id: i64,
    },
    #[error("run #{0} is not active")]
    NotActive(i64),
    #[error(transparent)]
    Operation(#[from] OperationError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot write the run output: {0}")]
    Output(#[from] std::io::Error),
}

/// What an open run view receives.
#[derive(Clone, Debug)]
pub enum RunEvent {
    /// The run's status or steps changed.
    Run(RunRecord),
    Output(OutputEvent),
}

/// Lines appended to the output file, with their byte range in it, and the rows after them that
/// the terminal may still redraw.
#[derive(Clone, Debug, Serialize)]
pub struct OutputEvent {
    pub start: u64,
    pub end: u64,
    pub text: String,
    pub live: Vec<String>,
}

/// What a run view follows: the events, and the live rows that are not in the output file yet.
pub struct Subscription {
    pub events: broadcast::Receiver<RunEvent>,
    pub live: watch::Receiver<Vec<String>>,
}

struct ActiveRun {
    events: broadcast::Sender<RunEvent>,
    live: watch::Receiver<Vec<String>>,
    /// Holds the email of the user who stopped the run.
    stop: watch::Sender<Option<String>>,
}

/// Runs operations in the background, one at a time per server. Runs go on when no screen
/// watches them.
pub struct Executor {
    db: Arc<Db>,
    config: Arc<AppConfig>,
    active: Mutex<HashMap<i64, ActiveRun>>,
    changes: broadcast::Sender<RunRecord>,
}

impl Executor {
    pub fn new(db: Arc<Db>, config: Arc<AppConfig>) -> Self {
        Self {
            db,
            config,
            active: Mutex::default(),
            changes: broadcast::channel(EVENT_BUFFER).0,
        }
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Starts `run.sh` of an operation on `server`. `has_check` is the last known state; the run
    /// checks the server for it.
    pub fn start(
        self: &Arc<Self>,
        server: &Server,
        operation: &CachedOperation,
        user: &str,
    ) -> Result<RunRecord, RunError> {
        let folder = operation.folder.as_str();
        operations::script_command(folder, "run.sh")?;
        let title = operation
            .title
            .clone()
            .unwrap_or_else(|| operations::title_of(folder));
        let now = now_millis();
        let mut steps = Steps::default();
        steps.start(CONNECT, now);
        if operation.has_check {
            steps.add_waiting(CHECK_SCRIPT);
        }
        let new_run = NewRun {
            server_id: server.id,
            server_name: &server.name,
            operation: folder,
            title: &title,
            user,
            steps: &steps.0,
            now,
        };
        let Some(run_id) = self.db.start_run(&new_run)? else {
            return Err(self.busy(server));
        };
        let log = RunLog::create(&self.config.run_log(run_id))?;
        let run = self.db.run(run_id)?.expect("the run was just stored");

        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let (stop, stop_receiver) = watch::channel(None);
        let (live, live_receiver) = watch::channel(Vec::new());
        let active = ActiveRun {
            events: events.clone(),
            live: live_receiver,
            stop,
        };
        self.lock().insert(run_id, active);
        let _ = self.changes.send(run.clone());

        let task = RunTask {
            db: self.db.clone(),
            config: self.config.clone(),
            ssh: Ssh::shared(&self.config, server),
            run: run.clone(),
            steps,
            log,
            batch: Batch::default(),
            events,
            live,
            stop: stop_receiver,
            output_since_step: false,
        };
        let executor = self.clone();
        tokio::spawn(async move {
            let ended = task.execute().await;
            executor.lock().remove(&ended.id);
            let _ = executor.changes.send(ended);
        });
        Ok(run)
    }

    fn busy(&self, server: &Server) -> RunError {
        let active = self.db.active_run(server.id).ok().flatten();
        RunError::Busy {
            server: server.name.clone(),
            operation: active
                .as_ref()
                .map_or_else(|| "an operation".into(), |run| run.operation.clone()),
            run_id: active.map_or(0, |run| run.id),
        }
    }

    /// Asks an active run to stop. The run ends its remote process and marks itself stopped.
    pub fn stop(&self, run_id: i64, user: &str) -> Result<(), RunError> {
        let active = self.lock();
        let run = active.get(&run_id).ok_or(RunError::NotActive(run_id))?;
        run.stop.send_replace(Some(user.to_owned()));
        Ok(())
    }

    /// Events of an active run; `None` when the run is not active.
    pub fn subscribe(&self, run_id: i64) -> Option<Subscription> {
        self.lock().get(&run_id).map(|run| Subscription {
            events: run.events.subscribe(),
            live: run.live.clone(),
        })
    }

    /// Every run that starts or ends.
    pub fn changes(&self) -> broadcast::Receiver<RunRecord> {
        self.changes.subscribe()
    }

    pub fn active_count(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, ActiveRun>> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// How a run ended.
struct End {
    status: RunStatus,
    exit_code: Option<i32>,
    detail: Option<String>,
}

impl End {
    fn failed(status: RunStatus, exit_code: Option<i32>, detail: String) -> Self {
        Self {
            status,
            exit_code,
            detail: Some(detail),
        }
    }
}

/// One run from start to end.
struct RunTask {
    db: Arc<Db>,
    config: Arc<AppConfig>,
    ssh: Ssh,
    run: RunRecord,
    steps: Steps,
    log: RunLog,
    batch: Batch,
    events: broadcast::Sender<RunEvent>,
    live: watch::Sender<Vec<String>>,
    stop: watch::Receiver<Option<String>>,
    /// Whether the running step printed lines other than `::step` ones.
    output_since_step: bool,
}

impl RunTask {
    /// Runs every step, stores the end and returns the final record.
    async fn execute(mut self) -> RunRecord {
        let end = match self.flow().await {
            Ok(end) | Err(end) => end,
        };
        let step_status = match end.status {
            RunStatus::Succeeded => StepStatus::Ok,
            RunStatus::Stopped => StepStatus::Stopped,
            _ => StepStatus::Failed,
        };
        self.steps.finish(step_status, now_millis());
        if let Some(detail) = &end.detail {
            self.print(&format!("upkeep: {detail}")).await;
        }
        self.run.status = end.status;
        self.run.exit_code = end.exit_code;
        self.run.detail = end.detail;
        self.run.finished_at = Some(now_millis());
        self.save_steps();
        let stored = self.db.finish_run(
            self.run.id,
            end.status,
            end.exit_code,
            self.run.detail.as_deref(),
            now_millis(),
        );
        if let Err(error) = stored {
            tracing::error!(run = self.run.id, %error, "cannot store the end of a run");
        }
        self.publish();
        self.run
    }

    async fn flow(&mut self) -> Result<End, End> {
        let has_check = self.connect().await?;
        self.begin(RUN_SCRIPT);
        let code = self.stream("run.sh").await?;
        match steps::outcome(code) {
            Outcome::Ok => {}
            Outcome::Restart => self.restart().await?,
            Outcome::Failed(code) => {
                return Err(End::failed(
                    RunStatus::Failed,
                    code,
                    exit_text("run.sh", code),
                ));
            }
        }
        if has_check {
            self.begin(CHECK_SCRIPT);
            let code = self.stream("check.sh").await?;
            if code != Some(0) {
                return Err(End::failed(
                    RunStatus::CheckFailed,
                    code,
                    exit_text("check.sh", code),
                ));
            }
        }
        Ok(End {
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            detail: None,
        })
    }

    /// The first step: SSH, sudo and the folder work. Returns whether `check.sh` exists.
    async fn connect(&mut self) -> Result<bool, End> {
        let probe = operations::probe(&self.ssh, &self.run.operation);
        let probed = self.unless_stopped(probe).await?;
        if let Some(reachable) = operations::reachability(&probed)
            && let Err(error) = self.db.set_reachable(self.run.server_id, reachable)
        {
            tracing::warn!(%error, "cannot store the server state");
        }
        let has_check =
            probed.map_err(|error| End::failed(RunStatus::Failed, None, error.to_string()))?;
        if !has_check {
            self.steps.remove_waiting(CHECK_SCRIPT);
        } else if !self.steps.0.iter().any(|step| step.title == CHECK_SCRIPT) {
            self.steps.add_waiting(CHECK_SCRIPT);
        }
        Ok(has_check)
    }

    /// Exit 10: restarts the server and waits until SSH drops and comes back, or the boot id
    /// changes. The shared connection ends after the request, and the waiting uses a new
    /// connection for each try, so a connection that hangs on the old boot cannot answer.
    async fn restart(&mut self) -> Result<(), End> {
        self.begin(RESTART);
        let before = boot_id(&self.ssh).await;
        let reboot = self.config.reboot_command.clone();
        self.print(&format!("$ {reboot}")).await;
        let request = self.ssh.run(&reboot, None, REBOOT_REQUEST_TIMEOUT);
        // The connection may drop before the command answers.
        let _ = self.unless_stopped(request).await?;
        self.ssh.close().await;
        self.print("waiting for the server to come back").await;

        let deadline = tokio::time::Instant::now() + self.config.reboot_timeout;
        let mut dropped = false;
        while tokio::time::Instant::now() < deadline {
            self.unless_stopped(tokio::time::sleep(self.config.reboot_poll))
                .await?;
            match boot_id(&self.ssh.unshared()).await {
                None => dropped = true,
                Some(current) if dropped || before.as_ref().is_some_and(|old| *old != current) => {
                    self.print("the server is back").await;
                    return Ok(());
                }
                Some(_) => {}
            }
        }
        let minutes = self.config.reboot_timeout.as_secs() / 60;
        Err(End::failed(
            RunStatus::Failed,
            Some(10),
            format!("the server did not come back within {minutes} min"),
        ))
    }

    /// Runs a script of the operation on a terminal under the server lock and streams its
    /// lines. Returns its exit code, or ends the run when another run holds the lock.
    async fn stream(&mut self, file: &str) -> Result<Option<i32>, End> {
        let folder = &self.run.operation;
        let shown = operations::script_command(folder, file).expect("the folder was checked");
        let owner = format!("{folder} by {}, run #{}", self.run.user, self.run.id);
        let command = operations::locked_script_command(folder, file, &owner)
            .expect("the folder was checked");
        self.print(&format!("$ {shown}")).await;
        let spawned = self
            .ssh
            .command(&command, true)
            .and_then(|mut command| command.spawn().map_err(SshError::Spawn));
        let mut child =
            spawned.map_err(|error| End::failed(RunStatus::Failed, None, error.to_string()))?;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let stderr = tokio::spawn(read_to_string(
            child.stderr.take().expect("stderr is piped"),
        ));

        let mut terminal = Terminal::default();
        let mut buffer = vec![0u8; READ_BUFFER];
        let mut stop = self.stop.clone();
        loop {
            let due_at = self.batch.due_at().unwrap_or_else(Instant::now);
            tokio::select! {
                read = stdout.read(&mut buffer) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let lines = terminal.push(&buffer[..count]);
                        self.lines(lines, terminal.open_lines()).await;
                    }
                },
                () = tokio::time::sleep_until(due_at), if self.batch.is_pending() => self.flush().await,
                () = stop_requested(&mut stop) => {
                    end_process_group(&mut child).await;
                    self.lines(terminal.finish(), Vec::new()).await;
                    self.flush().await;
                    return Err(self.stopped());
                }
            }
        }
        self.lines(terminal.finish(), Vec::new()).await;
        self.flush().await;
        let status = child.wait().await;
        let stderr = stderr.await.unwrap_or_default();
        if !stderr.trim().is_empty() {
            self.print(stderr.trim_end()).await;
        }
        match status {
            Ok(status) if status.code() == Some(operations::EXIT_LOCKED) => {
                self.refuse_if_locked().await?;
                Ok(status.code())
            }
            Ok(status) if status.code() == Some(255) => Err(End::failed(
                RunStatus::Failed,
                Some(255),
                "the SSH connection failed or was lost".into(),
            )),
            Ok(status) => Ok(status.code()),
            Err(error) => Err(End::failed(RunStatus::Failed, None, error.to_string())),
        }
    }

    /// Ends the run when another run holds the server lock. A script may also exit with the
    /// same code on its own; then the lock is free and the code stands.
    async fn refuse_if_locked(&self) -> Result<(), End> {
        let Ok(Some(owner)) = operations::lock_owner(&self.ssh).await else {
            return Ok(());
        };
        let mut detail = "another operation is already running on this server".to_owned();
        if !owner.is_empty() {
            detail = format!("{detail}: {owner}");
        }
        Err(End::failed(
            RunStatus::Failed,
            Some(operations::EXIT_LOCKED),
            detail,
        ))
    }

    /// Adds closed lines and the live rows to the output, and turns `::step` lines into steps.
    /// Output before a step goes out before the step changes.
    async fn lines(&mut self, lines: Vec<String>, live: Vec<String>) {
        for line in lines {
            if let Some(title) = steps::step_title(&line) {
                self.flush().await;
                self.steps
                    .script_step(title, self.output_since_step, now_millis());
                self.output_since_step = false;
                self.save_steps();
                self.publish();
            } else {
                self.output_since_step = true;
            }
            self.batch.push_line(&line);
        }
        self.batch.set_live(live);
        if self.batch.is_due(Instant::now()) {
            self.flush().await;
        }
    }

    /// A line of the app itself, such as the command it runs.
    async fn print(&mut self, line: &str) {
        self.batch.push_line(line);
        self.flush().await;
    }

    /// Writes the waiting output to the file and sends it as one event.
    async fn flush(&mut self) {
        let Some((text, live)) = self.batch.take(Instant::now()) else {
            return;
        };
        match self.log.append(&text).await {
            Ok((start, end)) => {
                self.live.send_replace(live.clone());
                let event = OutputEvent {
                    start,
                    end,
                    text,
                    live,
                };
                let _ = self.events.send(RunEvent::Output(event));
            }
            Err(error) => tracing::error!(run = self.run.id, %error, "cannot write run output"),
        }
    }

    fn begin(&mut self, title: &str) {
        self.steps.start_waiting(title, now_millis());
        self.output_since_step = false;
        self.save_steps();
        self.publish();
    }

    fn save_steps(&mut self) {
        self.run.steps = self.steps.0.clone();
        if let Err(error) = self.db.save_steps(self.run.id, &self.steps.0) {
            tracing::warn!(run = self.run.id, %error, "cannot store run steps");
        }
    }

    fn publish(&self) {
        let _ = self.events.send(RunEvent::Run(self.run.clone()));
    }

    /// Waits for `future` unless a user stops the run first.
    async fn unless_stopped<T>(&self, future: impl Future<Output = T>) -> Result<T, End> {
        let mut stop = self.stop.clone();
        tokio::select! {
            value = future => Ok(value),
            () = stop_requested(&mut stop) => Err(self.stopped()),
        }
    }

    fn stopped(&self) -> End {
        let user = self.stop.borrow().clone().unwrap_or_default();
        End::failed(RunStatus::Stopped, None, format!("stopped by {user}"))
    }
}

/// Completes when a user stops the run.
async fn stop_requested(stop: &mut watch::Receiver<Option<String>>) {
    if stop.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn boot_id(ssh: &Ssh) -> Option<String> {
    let output = ssh.run(BOOT_ID, None, PROBE_TIMEOUT).await.ok()?;
    (output.code == Some(0)).then(|| output.stdout.trim().to_owned())
}

fn exit_text(script: &str, code: Option<i32>) -> String {
    match code {
        Some(code) => format!("{script} exited with {code}"),
        None => format!("{script} ended without an exit code"),
    }
}

/// Ends the local `ssh` and everything in its process group. Closing the connection hangs up the
/// remote terminal, which ends the remote script.
async fn end_process_group(child: &mut Child) {
    if let Some(pid) = child.id() {
        let group = nix::unistd::Pid::from_raw(pid as i32);
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
    }
    let _ = child.wait().await;
}

async fn read_to_string(mut reader: impl AsyncRead + Unpin) -> String {
    let mut bytes = Vec::new();
    let _ = reader.read_to_end(&mut bytes).await;
    String::from_utf8_lossy(&bytes).into_owned()
}
