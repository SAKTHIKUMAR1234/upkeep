use serde::Serialize;

use super::checks::Level;

/// When a check is announced.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// Rounds in a row a new level must be seen before it counts.
    pub alert_after: u32,
    /// Seconds after which a still-bad check is sent again.
    pub repeat_every: i64,
}

/// What the runner remembers about one check between rounds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CheckState {
    /// The settled level: the last one seen `alert_after` rounds in a row.
    pub level: Level,
    /// When `level` settled.
    pub since: i64,
    /// Rounds in a row that saw a level other than `level`.
    pub streak: u32,
    /// When a message for `level` was last sent. None while it waits, for example in quiet hours.
    pub last_sent_at: Option<i64>,
    /// A message went out in this bad spell, so its end gets a recovery message.
    pub notified: bool,
}

/// A message the state machine asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Alert { level: Level, repeat: bool },
    Recovery { was: Level, since: i64 },
}

impl CheckState {
    /// A check seen for the first time starts as ok, so a bad first round is debounced too.
    pub fn new(now: i64) -> Self {
        Self {
            level: Level::Ok,
            since: now,
            streak: 0,
            last_sent_at: None,
            notified: false,
        }
    }

    /// Applies one round's level and returns the message to send, if any.
    /// In quiet hours warn messages wait; critical and recovery messages always go out.
    pub fn advance(
        &mut self,
        observed: Level,
        now: i64,
        quiet: bool,
        policy: Policy,
    ) -> Option<Notice> {
        if observed == self.level {
            self.streak = 0;
        } else {
            self.streak += 1;
            if self.streak >= policy.alert_after.max(1) {
                let was = self.level;
                let was_since = self.since;
                self.level = observed;
                self.since = now;
                self.streak = 0;
                self.last_sent_at = None;
                if observed == Level::Ok {
                    let notified = std::mem::take(&mut self.notified);
                    return notified.then_some(Notice::Recovery {
                        was,
                        since: was_since,
                    });
                }
            }
        }
        self.due_alert(now, quiet, policy)
    }

    fn due_alert(&mut self, now: i64, quiet: bool, policy: Policy) -> Option<Notice> {
        if !self.level.is_bad() || (quiet && self.level == Level::Warn) {
            return None;
        }
        let repeat = match self.last_sent_at {
            None => false,
            Some(sent_at) if now - sent_at >= policy.repeat_every => true,
            Some(_) => return None,
        };
        self.last_sent_at = Some(now);
        self.notified = true;
        Some(Notice::Alert {
            level: self.level,
            repeat,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: Policy = Policy {
        alert_after: 2,
        repeat_every: 6 * 3600,
    };

    /// Feeds one level per minute and returns the notices.
    fn run(
        state: &mut CheckState,
        start: i64,
        levels: &[Level],
        quiet: bool,
    ) -> Vec<Option<Notice>> {
        levels
            .iter()
            .enumerate()
            .map(|(round, level)| state.advance(*level, start + round as i64 * 60, quiet, POLICY))
            .collect()
    }

    fn alert(level: Level) -> Option<Notice> {
        Some(Notice::Alert {
            level,
            repeat: false,
        })
    }

    #[test]
    fn a_single_bad_round_does_not_alert() {
        let mut state = CheckState::new(0);
        let notices = run(
            &mut state,
            0,
            &[Level::Critical, Level::Ok, Level::Critical, Level::Ok],
            false,
        );
        assert!(notices.iter().all(Option::is_none));
        assert_eq!(state.level, Level::Ok);
    }

    #[test]
    fn alerts_after_two_rounds_then_recovers_after_two() {
        let mut state = CheckState::new(0);
        let levels = [
            Level::Critical,
            Level::Critical,
            Level::Critical,
            Level::Ok,
            Level::Ok,
        ];
        let notices = run(&mut state, 0, &levels, false);
        assert_eq!(
            notices,
            [
                None,
                alert(Level::Critical),
                None,
                None,
                Some(Notice::Recovery {
                    was: Level::Critical,
                    since: 60
                })
            ]
        );
    }

    #[test]
    fn flapping_between_warn_and_critical_still_alerts() {
        let mut state = CheckState::new(0);
        let notices = run(&mut state, 0, &[Level::Warn, Level::Critical], false);
        assert_eq!(notices, [None, alert(Level::Critical)]);
    }

    #[test]
    fn a_still_bad_check_repeats_after_the_repeat_time() {
        let mut state = CheckState::new(0);
        run(&mut state, 0, &[Level::Critical, Level::Critical], false);
        assert_eq!(
            state.advance(Level::Critical, 60 + 6 * 3600 - 1, false, POLICY),
            None
        );
        assert_eq!(
            state.advance(Level::Critical, 60 + 6 * 3600, false, POLICY),
            Some(Notice::Alert {
                level: Level::Critical,
                repeat: true
            })
        );
    }

    #[test]
    fn escalation_is_a_new_alert() {
        let mut state = CheckState::new(0);
        let levels = [Level::Warn, Level::Warn, Level::Critical, Level::Critical];
        let notices = run(&mut state, 0, &levels, false);
        assert_eq!(
            notices,
            [None, alert(Level::Warn), None, alert(Level::Critical)]
        );
    }

    #[test]
    fn quiet_hours_hold_warn_until_they_end() {
        let mut state = CheckState::new(0);
        assert_eq!(
            run(
                &mut state,
                0,
                &[Level::Warn, Level::Warn, Level::Warn],
                true
            ),
            [None, None, None]
        );
        assert_eq!(
            state.advance(Level::Warn, 600, false, POLICY),
            alert(Level::Warn)
        );
    }

    #[test]
    fn quiet_hours_never_hold_critical() {
        let mut state = CheckState::new(0);
        let notices = run(&mut state, 0, &[Level::Critical, Level::Critical], true);
        assert_eq!(notices, [None, alert(Level::Critical)]);
    }

    #[test]
    fn a_warn_that_was_never_sent_clears_silently() {
        let mut state = CheckState::new(0);
        let levels = [Level::Warn, Level::Warn, Level::Ok, Level::Ok];
        assert!(
            run(&mut state, 0, &levels, true)
                .iter()
                .all(Option::is_none)
        );
        assert!(!state.notified);
    }
}
