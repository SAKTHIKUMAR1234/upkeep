use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use super::job_log::JobLog;
use super::step_dir::{ExitRecord, StepDir, StepFile};
use crate::secrets::Secrets;

const KILL_GRACE: Duration = Duration::from_secs(10);
const OUTPUT_DRAIN: Duration = Duration::from_secs(5);

/// Entry point of `upkeep step-exec <dir>`: runs one adapter action and writes `exit.json`.
pub async fn execute(path: &Path) -> anyhow::Result<()> {
    // Fails with EPERM under systemd-run, where the process already leads its own session.
    let _ = nix::unistd::setsid();

    let step = StepDir::new(path.to_path_buf());
    if !step.claim()? {
        return Ok(());
    }
    let file = step.read_step()?;
    let record = run_action(&step, &file).await;
    step.write_exit(&record)?;
    Ok(())
}

async fn run_action(step: &StepDir, file: &StepFile) -> ExitRecord {
    let secrets = match &file.secrets_file {
        Some(path) => Secrets::load(path),
        None => Ok(Secrets::default()),
    };
    let log = secrets.and_then(|secrets| {
        let label = format!("{}:{}", file.adapter, file.action);
        Ok(JobLog::open(&file.job_log, label)?.with_redactor(secrets.redactor()))
    });
    let log = match log {
        Ok(log) => Arc::new(log),
        Err(error) => return failed_to_start(error),
    };
    let child = File::open(step.context_file()).and_then(|context| spawn(file, context));
    match child {
        Ok(child) => supervise(child, step, file, log).await,
        Err(error) => {
            log.line("", &format!("cannot start adapter: {error}"));
            failed_to_start(error)
        }
    }
}

fn failed_to_start(error: std::io::Error) -> ExitRecord {
    ExitRecord {
        error: Some(format!("cannot start adapter: {error}")),
        ..ExitRecord::default()
    }
}

fn spawn(file: &StepFile, context: File) -> std::io::Result<Child> {
    Command::new("sh")
        .arg("-c")
        .arg(format!("{} \"$1\"", file.run))
        .arg("upkeep-adapter")
        .arg(&file.action)
        .current_dir(&file.adapter_dir)
        .envs(&file.env)
        .stdin(context)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
}

async fn supervise(
    mut child: Child,
    step: &StepDir,
    file: &StepFile,
    log: Arc<JobLog>,
) -> ExitRecord {
    let stdout = child
        .stdout
        .take()
        .map(|out| read_stdout(out, step, log.clone()));
    let stderr = child.stderr.take().map(|err| read_stderr(err, log.clone()));

    let timeout = Duration::from_secs(file.timeout_secs);
    let (status, timed_out) = wait_or_kill(&mut child, timeout).await;
    if timed_out {
        log.line(
            "",
            &format!("timed out after {}s, stopped", file.timeout_secs),
        );
    }

    let result = match stdout {
        Some(task) => tokio::time::timeout(OUTPUT_DRAIN, task)
            .await
            .ok()
            .and_then(Result::ok)
            .flatten(),
        None => None,
    };
    if let Some(task) = stderr {
        let _ = tokio::time::timeout(OUTPUT_DRAIN, task).await;
    }

    let (code, signal, mut error) = match status {
        Ok(status) => (
            status.code(),
            std::os::unix::process::ExitStatusExt::signal(&status),
            None,
        ),
        Err(error) => (None, None, Some(error.to_string())),
    };
    if timed_out {
        error = Some(format!("timed out after {}s", file.timeout_secs));
    }
    ExitRecord {
        code,
        signal,
        timed_out,
        result,
        error,
    }
}

/// Waits for the action; past the timeout it stops the whole process group.
async fn wait_or_kill(child: &mut Child, timeout: Duration) -> (std::io::Result<ExitStatus>, bool) {
    if let Ok(status) = tokio::time::timeout(timeout, child.wait()).await {
        return (status, false);
    }

    let group = child.id().map(|id| Pid::from_raw(id as i32));
    if let Some(group) = group {
        let _ = killpg(group, Signal::SIGTERM);
    }
    if let Ok(status) = tokio::time::timeout(KILL_GRACE, child.wait()).await {
        return (status, true);
    }
    if let Some(group) = group {
        let _ = killpg(group, Signal::SIGKILL);
    }
    (child.wait().await, true)
}

/// Copies adapter stdout, with secrets redacted, to `stdout.jsonl` and the job log; returns the last result message.
fn read_stdout(
    stream: impl AsyncRead + Unpin + Send + 'static,
    step: &StepDir,
    log: Arc<JobLog>,
) -> JoinHandle<Option<Value>> {
    let raw = OpenOptions::new()
        .create(true)
        .append(true)
        .open(step.stdout_file());
    tokio::spawn(async move {
        let mut raw = raw.ok();
        let mut lines = BufReader::new(stream).lines();
        let mut result = None;
        while let Ok(Some(line)) = lines.next_line().await {
            let line = log.redact(&line);
            if let Some(file) = raw.as_mut() {
                let _ = writeln!(file, "{line}");
            }
            let (text, line_result) = render(&line);
            log.line("", &text);
            result = line_result.or(result);
        }
        result
    })
}

fn read_stderr(
    stream: impl AsyncRead + Unpin + Send + 'static,
    log: Arc<JobLog>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log.line(" stderr", &line);
        }
    })
}

/// Turns one protocol line into log text, plus the result object for a result line.
fn render(line: &str) -> (String, Option<Value>) {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        return (line.to_owned(), None);
    };
    let text = |key: &str| {
        message
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match message.get("type").and_then(Value::as_str) {
        Some("log") => {
            let level = message
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("info");
            (format!("{level}: {}", text("message")), None)
        }
        Some("progress") => {
            let percent = message
                .get("percent")
                .map(Value::to_string)
                .unwrap_or_else(|| "?".into());
            (format!("progress {percent}% {}", text("message")), None)
        }
        Some("result") => (format!("result {message}"), Some(message)),
        _ => (line.to_owned(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_protocol_lines() {
        assert_eq!(
            render(r#"{"type":"log","level":"warn","message":"disk low"}"#).0,
            "warn: disk low"
        );
        assert_eq!(
            render(r#"{"type":"progress","percent":40,"message":"copy"}"#).0,
            "progress 40% copy"
        );
        assert_eq!(render("plain text").0, "plain text");

        let (text, result) = render(r#"{"type":"result","snapshot":"snap-1"}"#);
        assert!(text.starts_with("result "));
        assert_eq!(result.unwrap()["snapshot"], "snap-1");
    }
}
