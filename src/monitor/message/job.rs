use serde_json::Value;

use super::titles::Titles;
use super::{Letterhead, escape, header, span};
use crate::engine::step_dir::Progress;
use crate::store::{Job, JobStatus, Phase, Step, StepStatus};

/// Characters of a failure reason a message shows.
const REASON_CHARS: usize = 200;

/// What a job message shows besides the job and its steps.
#[derive(Clone, Copy, Debug)]
pub struct JobView<'a> {
    pub titles: &'a Titles,
    /// The latest progress of the running step.
    pub progress: Option<&'a Progress>,
    /// This server's alerts wait while the job runs.
    pub alerts_held: bool,
}

impl Letterhead {
    /// The one message of a plan's job. It is edited as the job runs: "⏳ OS UPDATE RUNNING",
    /// then "✅ OS UPDATE DONE" or "❌ OS UPDATE FAILED".
    pub fn job_message(&self, job: &Job, steps: &[Step], view: JobView, now: i64) -> String {
        let report = Report {
            job,
            steps,
            titles: view.titles,
        };
        let label = view.titles.plan_label(&job.target);
        let (emoji, outcome) = match job.status {
            JobStatus::Running | JobStatus::Rebooting => ("⏳", "RUNNING"),
            JobStatus::Succeeded | JobStatus::Degraded => ("✅", "DONE"),
            JobStatus::Failed | JobStatus::Cancelled => ("❌", "FAILED"),
        };
        let mut lines = vec![header(emoji, &format!("{label} {outcome}"), &self.host)];
        lines.extend(report.done_lines());
        if report.has_failed() {
            lines.push(report.failure());
            lines.extend(report.recovery_lines());
        }
        if job.status.is_active() {
            lines.extend(report.current(view.progress));
        } else {
            if report.has_failed() {
                lines.extend(report.state());
            }
            lines.push(self.took(job, view.alerts_held, now));
        }
        lines.join("\n")
    }

    /// For example "Took 27 minutes (23:28 to 23:55). No alerts were sent during the update."
    fn took(&self, job: &Job, alerts_held: bool, now: i64) -> String {
        let finished_at = job.finished_at.unwrap_or(now);
        let mut line = format!(
            "Took {} ({} to {}).",
            span(finished_at - job.created_at),
            self.time(job.created_at, now),
            self.time(finished_at, now)
        );
        if alerts_held {
            line.push_str(" No alerts were sent during the update.");
        }
        line
    }
}

/// One job and its steps, in plain words.
struct Report<'a> {
    job: &'a Job,
    steps: &'a [Step],
    titles: &'a Titles,
}

impl Report<'_> {
    fn title(&self, step: &Step) -> String {
        escape(&self.titles.step(step, self.steps))
    }

    fn phase(&self, phase: Phase) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(move |step| step.phase == phase)
    }

    /// The main steps stopped early: a step failed, or the job was cancelled or aborted.
    fn has_failed(&self) -> bool {
        self.job.phase == Phase::Recovery
            || matches!(self.job.status, JobStatus::Failed | JobStatus::Cancelled)
    }

    /// One line per main step that is done, such as "✓ Snapshot: 113 GB read, 43 GB stored".
    fn done_lines(&self) -> Vec<String> {
        self.phase(Phase::Main)
            .filter_map(|step| {
                let mark = match step.status {
                    StepStatus::Ok | StepStatus::Reboot => "✓",
                    StepStatus::Degraded => "⚠️",
                    _ => return None,
                };
                let warned = if step.status == StepStatus::Degraded {
                    " (with warnings)"
                } else {
                    ""
                };
                Some(format!(
                    "{mark} {}{warned}{}",
                    self.title(step),
                    summary(step)
                ))
            })
            .collect()
    }

    /// "Failed at: Install updates: <reason>", or why the job stopped when no step failed.
    fn failure(&self) -> String {
        match self
            .phase(Phase::Main)
            .find(|step| step.status == StepStatus::Failed)
        {
            Some(step) => format!("Failed at: {}: {}", self.title(step), escape(&reason(step))),
            None => format!(
                "Stopped: {}",
                escape(self.job.error.as_deref().unwrap_or("stopped"))
            ),
        }
    }

    /// One "↩ Roll back: <summary>" line per recovery step that ran.
    fn recovery_lines(&self) -> Vec<String> {
        self.phase(Phase::Recovery)
            .filter(|step| step.finished_at.is_some() && step.status != StepStatus::Skipped)
            .map(|step| match step.status {
                StepStatus::Failed => {
                    format!("↩ {} failed: {}", self.title(step), escape(&reason(step)))
                }
                _ => format!("↩ {}{}", self.title(step), summary(step)),
            })
            .collect()
    }

    /// The step that runs now, such as "⏳ 3/7 Snapshot… 29% · 1024 MiB read".
    fn current(&self, progress: Option<&Progress>) -> Option<String> {
        if self.job.status == JobStatus::Rebooting {
            return Some("⏳ Restarting the server…".to_owned());
        }
        let step = self
            .steps
            .iter()
            .find(|step| step.status == StepStatus::Running)
            .or_else(|| {
                self.phase(self.job.phase)
                    .find(|step| step.status == StepStatus::Pending)
            })?;
        let mut line = match step.phase {
            Phase::Main => format!(
                "⏳ {}/{} {}…",
                step.index + 1,
                self.phase(Phase::Main).count(),
                self.title(step)
            ),
            Phase::Recovery => format!("⏳ ↩ {}…", self.title(step)),
        };
        if let Some(progress) = progress.filter(|_| step.status == StepStatus::Running) {
            if let Some(percent) = progress.percent {
                line.push_str(&format!(" {}%", percent.round()));
            }
            if !progress.message.is_empty() {
                line.push_str(&format!(" · {}", escape(&progress.message)));
            }
        }
        Some(line)
    }

    /// The last plain `state` an adapter gave, such as "The site runs the old version".
    fn state(&self) -> Option<String> {
        self.steps
            .iter()
            .filter_map(|step| step.result.as_ref()?.get("state")?.as_str())
            .next_back()
            .map(escape)
    }
}

/// ": a · b" from the adapter's `summary` lines, or nothing when it gave none.
fn summary(step: &Step) -> String {
    let lines: Vec<String> = step
        .result
        .as_ref()
        .and_then(|result| result.get("summary"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(escape)
        .collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!(": {}", lines.join(" · "))
    }
}

/// The adapter's plain `error`, else the runner's error: its last line, cut to `REASON_CHARS`.
fn reason(step: &Step) -> String {
    let text = step
        .result
        .as_ref()
        .and_then(|result| result.get("error"))
        .and_then(Value::as_str)
        .or(step.error.as_deref())
        .unwrap_or("failed");
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("failed");
    match line.char_indices().nth(REASON_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::super::tests::{NOW, letterhead};
    use super::*;

    fn job(status: JobStatus, phase: Phase, error: Option<&str>) -> Job {
        Job {
            id: 7,
            target: "os-update".into(),
            status,
            phase,
            end_status: None,
            cancel_requested: false,
            requested_by: "Sakthi".into(),
            error: error.map(Into::into),
            reboot_boot_id: None,
            created_at: NOW - 27 * 60,
            finished_at: (!status.is_active()).then_some(NOW),
            params: None,
        }
    }

    fn step(phase: Phase, index: i64, name: &str, status: StepStatus, result: Value) -> Step {
        let (adapter, action) = name.split_once(':').unwrap();
        Step {
            job_id: 7,
            phase,
            index,
            adapter: adapter.into(),
            action: action.into(),
            adapter_dir: String::new(),
            run: String::new(),
            timeout_secs: 60,
            status,
            exit_code: None,
            result: (!result.is_null()).then_some(result),
            error: (status == StepStatus::Failed).then(|| "exit code 1".to_owned()),
            started_at: None,
            finished_at: (!matches!(
                status,
                StepStatus::Pending | StepStatus::Running | StepStatus::Skipped
            ))
            .then_some(NOW),
        }
    }

    fn titles() -> Titles {
        Titles {
            plan: Some("OS update".into()),
            actions: BTreeMap::from([(("os".into(), "snapshot".into()), "Taking snapshot".into())]),
            ..Titles::default()
        }
    }

    fn view<'a>(titles: &'a Titles, progress: Option<&'a Progress>) -> JobView<'a> {
        JobView {
            titles,
            progress,
            alerts_held: true,
        }
    }

    fn os_steps(snapshot: StepStatus, update: StepStatus) -> Vec<Step> {
        vec![
            step(
                Phase::Main,
                0,
                "db:maintenance_on",
                StepStatus::Ok,
                json!({"summary": ["databases stopped"]}),
            ),
            step(
                Phase::Main,
                1,
                "os:snapshot",
                snapshot,
                json!({"summary": ["113 GB read, 43 GB stored"]}),
            ),
            step(
                Phase::Main,
                2,
                "os:update",
                update,
                json!({"summary": ["51 packages updated", "kernel 7.0.0-43", "restart needed"]}),
            ),
        ]
    }

    #[test]
    fn running_shows_done_steps_and_the_current_step_with_progress() {
        let titles = titles();
        let mut steps = os_steps(StepStatus::Running, StepStatus::Pending);
        steps[1].result = None;
        let progress = Progress {
            percent: Some(29.4),
            message: "1024 MiB read".into(),
        };
        let running = job(JobStatus::Running, Phase::Main, None);
        assert_eq!(
            letterhead().job_message(&running, &steps, view(&titles, Some(&progress)), NOW),
            "<b>⏳ OS UPDATE RUNNING · YRP server (.22)</b>\n\
             ✓ Maintenance on: databases stopped\n\
             ⏳ 2/3 Taking snapshot… 29% · 1024 MiB read"
        );

        let rebooting = job(JobStatus::Rebooting, Phase::Main, None);
        let steps = os_steps(StepStatus::Ok, StepStatus::Reboot);
        let message = letterhead().job_message(&rebooting, &steps, view(&titles, None), NOW);
        assert!(
            message.ends_with("\n⏳ Restarting the server…"),
            "{message}"
        );
    }

    #[test]
    fn done_lists_each_step_summary_and_the_time() {
        let titles = titles();
        let steps = os_steps(StepStatus::Ok, StepStatus::Reboot);
        let done = job(JobStatus::Succeeded, Phase::Main, None);
        assert_eq!(
            letterhead().job_message(&done, &steps, view(&titles, None), NOW),
            "<b>✅ OS UPDATE DONE · YRP server (.22)</b>\n\
             ✓ Maintenance on: databases stopped\n\
             ✓ Taking snapshot: 113 GB read, 43 GB stored\n\
             ✓ Install updates: 51 packages updated · kernel 7.0.0-43 · restart needed\n\
             Took 27 minutes (16:50 to 17:17). No alerts were sent during the update."
        );
    }

    #[test]
    fn failure_shows_the_plain_reason_recovery_and_state() {
        let titles = Titles::default();
        let mut steps = os_steps(StepStatus::Ok, StepStatus::Failed);
        steps[2].result = Some(json!({"error": "building the website files: no <internet>"}));
        steps.push(step(
            Phase::Recovery,
            0,
            "bench:rollback",
            StepStatus::Ok,
            json!({"summary": ["code back to frappe 9f8ae9c"],
                   "state": "The site runs the old version, as before the update."}),
        ));
        steps.push(step(
            Phase::Recovery,
            1,
            "bench:maintenance_off",
            StepStatus::Failed,
            Value::Null,
        ));
        let failed = job(
            JobStatus::Failed,
            Phase::Recovery,
            Some("step os:update failed"),
        );
        let mut view = view(&titles, None);
        view.alerts_held = false;
        assert_eq!(
            letterhead().job_message(&failed, &steps, view, NOW),
            "<b>❌ OS UPDATE FAILED · YRP server (.22)</b>\n\
             ✓ Maintenance on: databases stopped\n\
             ✓ Snapshot: 113 GB read, 43 GB stored\n\
             Failed at: Install updates: building the website files: no &lt;internet&gt;\n\
             ↩ Roll back: code back to frappe 9f8ae9c\n\
             ↩ Maintenance off failed: exit code 1\n\
             The site runs the old version, as before the update.\n\
             Took 27 minutes (16:50 to 17:17)."
        );
    }

    #[test]
    fn running_recovery_shows_the_failure_and_the_recovery_step() {
        let titles = Titles::default();
        let mut steps = os_steps(StepStatus::Failed, StepStatus::Skipped);
        steps.push(step(
            Phase::Recovery,
            0,
            "bench:rollback",
            StepStatus::Running,
            Value::Null,
        ));
        let failed = job(JobStatus::Running, Phase::Recovery, Some("x"));
        let message = letterhead().job_message(&failed, &steps, view(&titles, None), NOW);
        assert!(
            message.ends_with("\nFailed at: Snapshot: exit code 1\n⏳ ↩ Roll back…"),
            "{message}"
        );
    }

    #[test]
    fn reasons_use_the_last_line_cut_short_and_cancel_says_stopped() {
        let mut failed = step(Phase::Main, 0, "os:update", StepStatus::Failed, Value::Null);
        failed.error = Some(format!("first line\n{}\n", "x".repeat(300)));
        assert_eq!(reason(&failed), format!("{}…", "x".repeat(200)));

        let titles = Titles::default();
        let steps = os_steps(StepStatus::Skipped, StepStatus::Skipped);
        let cancelled = job(
            JobStatus::Cancelled,
            Phase::Recovery,
            Some("cancelled by request"),
        );
        let message = letterhead().job_message(&cancelled, &steps[..1], view(&titles, None), NOW);
        assert!(
            message.contains("\nStopped: cancelled by request\n"),
            "{message}"
        );
    }
}
