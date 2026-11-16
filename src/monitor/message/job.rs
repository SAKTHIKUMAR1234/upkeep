use super::{Letterhead, count, escape, header, span};
use crate::store::{Job, JobStatus, Phase, Step, StepStatus};

/// Steps listed by name in a start message; the rest are counted.
const LISTED_STEPS: usize = 8;

impl Letterhead {
    /// For example "🛠️ UPDATE STARTED", the plan, who started it and its steps.
    pub fn job_started(&self, job: &Job, steps: &[Step]) -> String {
        let main: Vec<&Step> = main_steps(steps).collect();
        let mut names: Vec<String> = main
            .iter()
            .take(LISTED_STEPS)
            .map(|step| step_name(step))
            .collect();
        if main.len() > LISTED_STEPS {
            names.push(format!("and {} more", main.len() - LISTED_STEPS));
        }
        [
            header("🛠️", "UPDATE STARTED", &self.host),
            format!(
                "Plan: <b>{}</b> · started by {}",
                escape(&job.target),
                escape(&job.requested_by)
            ),
            format!("{}: {}", count(main.len(), "step"), names.join(", ")),
        ]
        .join("\n")
    }

    /// "✅ UPDATE DONE" or "❌ UPDATE FAILED", with the steps done, the failure and the time taken.
    pub fn job_finished(&self, job: &Job, steps: &[Step], now: i64) -> String {
        let main: Vec<&Step> = main_steps(steps).collect();
        let done = main.iter().filter(|step| is_done(step)).count();
        let plan = format!("Plan: <b>{}</b>", escape(&job.target));
        let succeeded = matches!(job.status, JobStatus::Succeeded | JobStatus::Degraded);
        let mut lines = if succeeded {
            vec![
                header("✅", "UPDATE DONE", &self.host),
                format!("{plan} · all {done} steps done"),
            ]
        } else {
            vec![
                header("❌", "UPDATE FAILED", &self.host),
                format!("{plan} · {done} of {} steps done", main.len()),
                failure(job, &main),
            ]
        };
        let warned: Vec<String> = main
            .iter()
            .filter(|step| step.status == StepStatus::Degraded)
            .map(|step| step_name(step))
            .collect();
        if !warned.is_empty() {
            lines.push(format!("With warnings: {}", warned.join(", ")));
        }
        lines.extend(recovery(steps));
        let finished_at = job.finished_at.unwrap_or(now);
        lines.push(format!(
            "Took {} ({} to {}).",
            span(finished_at - job.created_at),
            self.time(job.created_at, now),
            self.time(finished_at, now)
        ));
        lines.join("\n")
    }
}

fn main_steps(steps: &[Step]) -> impl Iterator<Item = &Step> {
    steps.iter().filter(|step| step.phase == Phase::Main)
}

fn is_done(step: &Step) -> bool {
    matches!(
        step.status,
        StepStatus::Ok | StepStatus::Degraded | StepStatus::Reboot
    )
}

fn step_name(step: &Step) -> String {
    format!(
        "<code>{}:{}</code>",
        escape(&step.adapter),
        escape(&step.action)
    )
}

/// The failed step and its reason, or the job's reason when no step failed.
fn failure(job: &Job, main: &[&Step]) -> String {
    let reason = job.error.as_deref().unwrap_or("stopped");
    match main
        .iter()
        .position(|step| step.status == StepStatus::Failed)
    {
        Some(index) => {
            let step = main[index];
            format!(
                "Failed at step {}/{} {}: {}",
                index + 1,
                main.len(),
                step_name(step),
                escape(step.error.as_deref().unwrap_or(reason))
            )
        }
        None => format!("Stopped: {}", escape(reason)),
    }
}

/// For example "Recovery: 2 steps ran, all OK".
fn recovery(steps: &[Step]) -> Option<String> {
    let ran: Vec<&Step> = steps
        .iter()
        .filter(|step| step.phase == Phase::Recovery && step.finished_at.is_some())
        .collect();
    if ran.is_empty() {
        return None;
    }
    let failed: Vec<String> = ran
        .iter()
        .filter(|step| !is_done(step))
        .map(|step| step_name(step))
        .collect();
    let outcome = if failed.is_empty() {
        "all OK".to_owned()
    } else {
        format!("failed: {}", failed.join(", "))
    };
    Some(format!(
        "Recovery: {} ran, {outcome}",
        count(ran.len(), "step")
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{NOW, letterhead};
    use super::*;

    fn job(status: JobStatus, error: Option<&str>) -> Job {
        Job {
            id: 7,
            target: "os-update".into(),
            status,
            phase: Phase::Main,
            end_status: None,
            cancel_requested: false,
            requested_by: "Sakthi <ops>".into(),
            error: error.map(Into::into),
            reboot_boot_id: None,
            created_at: NOW - 18 * 60,
            finished_at: Some(NOW),
            params: None,
        }
    }

    fn step(phase: Phase, index: i64, name: &str, status: StepStatus, error: Option<&str>) -> Step {
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
            result: None,
            error: error.map(Into::into),
            started_at: None,
            finished_at: (status != StepStatus::Pending && status != StepStatus::Skipped)
                .then_some(NOW),
        }
    }

    #[test]
    fn start_names_the_plan_requester_and_steps() {
        let steps = [
            step(
                Phase::Main,
                0,
                "app:maintenance_on",
                StepStatus::Pending,
                None,
            ),
            step(Phase::Main, 1, "os:update", StepStatus::Pending, None),
            step(
                Phase::Recovery,
                0,
                "app:maintenance_off",
                StepStatus::Pending,
                None,
            ),
        ];
        assert_eq!(
            letterhead().job_started(&job(JobStatus::Running, None), &steps),
            "<b>🛠️ UPDATE STARTED · YRP server (.22)</b>\n\
             Plan: <b>os-update</b> · started by Sakthi &lt;ops&gt;\n\
             2 steps: <code>app:maintenance_on</code>, <code>os:update</code>"
        );
    }

    #[test]
    fn done_shows_steps_warnings_and_time() {
        let steps = [
            step(Phase::Main, 0, "os:update", StepStatus::Reboot, None),
            step(Phase::Main, 1, "os:verify", StepStatus::Degraded, None),
        ];
        assert_eq!(
            letterhead().job_finished(&job(JobStatus::Degraded, None), &steps, NOW),
            "<b>✅ UPDATE DONE · YRP server (.22)</b>\n\
             Plan: <b>os-update</b> · all 2 steps done\n\
             With warnings: <code>os:verify</code>\n\
             Took 18 minutes (16:59 to 17:17)."
        );
    }

    #[test]
    fn failure_names_the_step_reason_and_recovery() {
        let steps = [
            step(Phase::Main, 0, "app:backup", StepStatus::Ok, None),
            step(
                Phase::Main,
                1,
                "os:update",
                StepStatus::Failed,
                Some("exit code 100 <dpkg>"),
            ),
            step(Phase::Main, 2, "os:verify", StepStatus::Skipped, None),
            step(
                Phase::Recovery,
                0,
                "app:maintenance_off",
                StepStatus::Ok,
                None,
            ),
        ];
        let failed = job(JobStatus::Failed, Some("step os:update failed"));
        assert_eq!(
            letterhead().job_finished(&failed, &steps, NOW),
            "<b>❌ UPDATE FAILED · YRP server (.22)</b>\n\
             Plan: <b>os-update</b> · 1 of 3 steps done\n\
             Failed at step 2/3 <code>os:update</code>: exit code 100 &lt;dpkg&gt;\n\
             Recovery: 1 step ran, all OK\n\
             Took 18 minutes (16:59 to 17:17)."
        );
        let cancelled = job(JobStatus::Cancelled, Some("cancelled by request"));
        let message = letterhead().job_finished(&cancelled, &steps[..1], NOW);
        assert!(
            message.contains("\nStopped: cancelled by request\n"),
            "{message}"
        );
    }
}
