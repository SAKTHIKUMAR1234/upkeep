use crate::app::store::{Step, StepStatus};

const STEP_PREFIX: &str = "::step ";
/// Output lines longer than this are cut into several lines.
const MAX_LINE: usize = 16 * 1024;

pub const CONNECT: &str = "Connect over SSH";
pub const RUN_SCRIPT: &str = "run.sh";
pub const RESTART: &str = "Restart and wait";
pub const CHECK_SCRIPT: &str = "check.sh";

/// The title of a `::step <title>` line.
pub fn step_title(line: &str) -> Option<&str> {
    let title = line.strip_prefix(STEP_PREFIX)?.trim();
    (!title.is_empty()).then_some(title)
}

/// What the exit code of `run.sh` means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// Exit 10: restart the server, then go on.
    Restart,
    /// Any other exit, or none when a signal ended the script.
    Failed(Option<i32>),
}

pub fn outcome(code: Option<i32>) -> Outcome {
    match code {
        Some(0) => Outcome::Ok,
        Some(10) => Outcome::Restart,
        other => Outcome::Failed(other),
    }
}

/// The steps of a run, in order: done, running, then waiting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Steps(pub Vec<Step>);

impl Steps {
    /// Ends the running step as OK and starts `title` before the waiting steps.
    pub fn start(&mut self, title: &str, now: i64) {
        self.finish(StepStatus::Ok, now);
        let position = self
            .0
            .iter()
            .position(|step| step.status == StepStatus::Waiting)
            .unwrap_or(self.0.len());
        let step = Step {
            title: title.to_owned(),
            status: StepStatus::Running,
            started_at: Some(now),
            finished_at: None,
        };
        self.0.insert(position, step);
    }

    /// Starts the waiting step `title`, or adds it.
    pub fn start_waiting(&mut self, title: &str, now: i64) {
        self.remove_waiting(title);
        self.start(title, now);
    }

    /// A `::step` line: the script's first step takes over the `run.sh` step while it has no
    /// output yet, so a script with steps shows only its own.
    pub fn script_step(&mut self, title: &str, run_step_has_output: bool, now: i64) {
        match self.running_mut() {
            Some(step) if step.title == RUN_SCRIPT && !run_step_has_output => {
                step.title = title.to_owned();
            }
            _ => self.start(title, now),
        }
    }

    pub fn finish(&mut self, status: StepStatus, now: i64) {
        if let Some(step) = self.running_mut() {
            step.status = status;
            step.finished_at = Some(now);
        }
    }

    pub fn add_waiting(&mut self, title: &str) {
        self.0.push(Step {
            title: title.to_owned(),
            status: StepStatus::Waiting,
            started_at: None,
            finished_at: None,
        });
    }

    pub fn remove_waiting(&mut self, title: &str) {
        self.0
            .retain(|step| !(step.title == title && step.status == StepStatus::Waiting));
    }

    fn running_mut(&mut self) -> Option<&mut Step> {
        self.0
            .iter_mut()
            .find(|step| step.status == StepStatus::Running)
    }
}

/// Cuts a byte stream into lines. Drops `\r` before `\n`, as a remote terminal sends it.
#[derive(Debug, Default)]
pub struct LineSplitter {
    partial: Vec<u8>,
}

impl LineSplitter {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                lines.push(self.take());
            } else {
                self.partial.push(byte);
                if self.partial.len() >= MAX_LINE {
                    lines.push(self.take());
                }
            }
        }
        lines
    }

    /// The last line when the stream ended without a newline.
    pub fn finish(&mut self) -> Option<String> {
        (!self.partial.is_empty()).then(|| self.take())
    }

    fn take(&mut self) -> String {
        if self.partial.last() == Some(&b'\r') {
            self.partial.pop();
        }
        let line = String::from_utf8_lossy(&self.partial).into_owned();
        self.partial.clear();
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(steps: &Steps) -> Vec<(&str, StepStatus)> {
        steps
            .0
            .iter()
            .map(|step| (step.title.as_str(), step.status))
            .collect()
    }

    #[test]
    fn step_lines_need_the_prefix_and_a_title() {
        assert_eq!(
            step_title("::step Taking snapshot"),
            Some("Taking snapshot")
        );
        assert_eq!(step_title("::step   Install  "), Some("Install"));
        for line in [
            "::step",
            "::step  ",
            " ::step x",
            "::stepx",
            "step x",
            "::STEP x",
        ] {
            assert_eq!(step_title(line), None, "{line:?}");
        }
    }

    #[test]
    fn exit_codes_map_to_outcomes() {
        assert_eq!(outcome(Some(0)), Outcome::Ok);
        assert_eq!(outcome(Some(10)), Outcome::Restart);
        assert_eq!(outcome(Some(1)), Outcome::Failed(Some(1)));
        assert_eq!(outcome(Some(255)), Outcome::Failed(Some(255)));
        assert_eq!(outcome(None), Outcome::Failed(None));
    }

    #[test]
    fn script_steps_replace_the_silent_run_step_and_stay_before_check() {
        let mut steps = Steps::default();
        steps.start(CONNECT, 1);
        steps.add_waiting(CHECK_SCRIPT);
        steps.start(RUN_SCRIPT, 2);
        steps.script_step("Stop apps", false, 3);
        steps.script_step("Snapshot", true, 4);
        assert_eq!(
            titles(&steps),
            [
                (CONNECT, StepStatus::Ok),
                ("Stop apps", StepStatus::Ok),
                ("Snapshot", StepStatus::Running),
                (CHECK_SCRIPT, StepStatus::Waiting),
            ]
        );
        steps.start_waiting(CHECK_SCRIPT, 5);
        steps.finish(StepStatus::Failed, 6);
        assert_eq!(
            titles(&steps)[2..],
            [
                ("Snapshot", StepStatus::Ok),
                (CHECK_SCRIPT, StepStatus::Failed)
            ]
        );
    }

    #[test]
    fn a_step_after_output_keeps_the_run_step() {
        let mut steps = Steps::default();
        steps.start(RUN_SCRIPT, 1);
        steps.script_step("Later step", true, 2);
        assert_eq!(
            titles(&steps),
            [
                (RUN_SCRIPT, StepStatus::Ok),
                ("Later step", StepStatus::Running)
            ]
        );
    }

    #[test]
    fn lines_split_across_chunks_and_lose_carriage_returns() {
        let mut splitter = LineSplitter::default();
        assert_eq!(splitter.push(b"one\r\ntw"), ["one"]);
        assert_eq!(splitter.push(b"o\n\nthree"), ["two", ""]);
        assert_eq!(splitter.finish().as_deref(), Some("three"));
        assert_eq!(splitter.finish(), None);
        let long = vec![b'x'; MAX_LINE + 10];
        let lines = splitter.push(&long);
        assert_eq!(lines.len(), 1);
        assert_eq!(splitter.finish().unwrap().len(), 10);
    }
}
