use serde::{Deserialize, Serialize};

use super::checks::Level;

/// When a check is announced.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// Rounds in a row a new level must be seen before it counts.
    pub alert_after: u32,
    /// Seconds after which a still-bad check is sent again.
    pub repeat_every: i64,
    /// Seconds a new bad level must last before it counts, on top of `alert_after`.
    pub bad_after: i64,
}

/// The messages a round holds back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    Nothing,
    /// Quiet hours: warn messages wait.
    Warn,
    /// A job runs or has just ended: nothing is sent, and a check still bad afterwards is sent as new.
    All,
}

/// What the runner remembers about one check between rounds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckState {
    /// The settled level: the last one seen `alert_after` rounds in a row.
    pub level: Level,
    /// The first round of the streak that settled `level`.
    pub since: i64,
    /// Rounds in a row that saw a level other than `level`.
    pub streak: u32,
    /// The first round of the current streak.
    #[serde(default)]
    pub streak_since: i64,
    /// Rounds that saw `level` since it settled, counting the streak that settled it.
    #[serde(default)]
    pub rounds: u32,
    /// When a message for `level` was last sent. None while it waits, for example in quiet hours.
    pub last_sent_at: Option<i64>,
    /// A message went out in this bad spell, so its end gets a recovery message.
    pub notified: bool,
}

/// A message the state machine asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Alert {
        level: Level,
        repeat: bool,
    },
    /// The check was `was` from `since` until the first ok round, `until`.
    Recovery {
        was: Level,
        since: i64,
        until: i64,
    },
}

impl CheckState {
    /// A check seen for the first time starts as ok, so a bad first round is debounced too.
    pub fn new(now: i64) -> Self {
        Self {
            level: Level::Ok,
            since: now,
            streak: 0,
            streak_since: now,
            rounds: 0,
            last_sent_at: None,
            notified: false,
        }
    }

    /// Applies one round's level and returns the message to send, if any. `hold` says which messages wait.
    pub fn advance(
        &mut self,
        observed: Level,
        now: i64,
        hold: Hold,
        policy: Policy,
    ) -> Option<Notice> {
        if observed == self.level {
            self.streak = 0;
            self.rounds += 1;
        } else {
            if self.streak == 0 {
                self.streak_since = now;
            }
            self.streak += 1;
            if self.is_settled(observed, now, policy) {
                let was = self.level;
                let was_since = self.since;
                self.level = observed;
                self.since = self.streak_since;
                self.rounds = self.streak;
                self.streak = 0;
                self.last_sent_at = None;
                if observed == Level::Ok {
                    let notified = std::mem::take(&mut self.notified);
                    return (notified && hold != Hold::All).then_some(Notice::Recovery {
                        was,
                        since: was_since,
                        until: self.since,
                    });
                }
            }
        }
        self.due_alert(now, hold, policy)
    }

    /// The current streak has lasted long enough to replace the settled level.
    fn is_settled(&self, observed: Level, now: i64, policy: Policy) -> bool {
        self.streak >= policy.alert_after.max(1)
            && (!observed.is_bad() || now - self.streak_since >= policy.bad_after)
    }

    fn due_alert(&mut self, now: i64, hold: Hold, policy: Policy) -> Option<Notice> {
        let held = match hold {
            Hold::Nothing => false,
            Hold::Warn => self.level == Level::Warn,
            Hold::All => true,
        };
        if !self.level.is_bad() || held {
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
        bad_after: 0,
    };

    /// Feeds one level per minute and returns the notices.
    fn run(
        state: &mut CheckState,
        start: i64,
        levels: &[Level],
        hold: Hold,
    ) -> Vec<Option<Notice>> {
        levels
            .iter()
            .enumerate()
            .map(|(round, level)| state.advance(*level, start + round as i64 * 60, hold, POLICY))
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
            Hold::Nothing,
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
        let notices = run(&mut state, 0, &levels, Hold::Nothing);
        assert_eq!(
            notices,
            [
                None,
                alert(Level::Critical),
                None,
                None,
                Some(Notice::Recovery {
                    was: Level::Critical,
                    since: 0,
                    until: 180,
                })
            ]
        );
        assert_eq!((state.since, state.rounds), (180, 2));
    }

    #[test]
    fn rounds_count_every_round_of_the_settled_level() {
        let mut state = CheckState::new(0);
        run(&mut state, 0, &[Level::Warn; 5], Hold::Nothing);
        assert_eq!(
            (state.level, state.since, state.rounds),
            (Level::Warn, 0, 5)
        );
    }

    #[test]
    fn flapping_between_warn_and_critical_still_alerts() {
        let mut state = CheckState::new(0);
        let notices = run(
            &mut state,
            0,
            &[Level::Warn, Level::Critical],
            Hold::Nothing,
        );
        assert_eq!(notices, [None, alert(Level::Critical)]);
    }

    #[test]
    fn a_still_bad_check_repeats_after_the_repeat_time() {
        let mut state = CheckState::new(0);
        run(
            &mut state,
            0,
            &[Level::Critical, Level::Critical],
            Hold::Nothing,
        );
        assert_eq!(
            state.advance(Level::Critical, 60 + 6 * 3600 - 1, Hold::Nothing, POLICY),
            None
        );
        assert_eq!(
            state.advance(Level::Critical, 60 + 6 * 3600, Hold::Nothing, POLICY),
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
        let notices = run(&mut state, 0, &levels, Hold::Nothing);
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
                Hold::Warn
            ),
            [None, None, None]
        );
        assert_eq!(
            state.advance(Level::Warn, 600, Hold::Nothing, POLICY),
            alert(Level::Warn)
        );
    }

    #[test]
    fn quiet_hours_never_hold_critical() {
        let mut state = CheckState::new(0);
        let notices = run(
            &mut state,
            0,
            &[Level::Critical, Level::Critical],
            Hold::Warn,
        );
        assert_eq!(notices, [None, alert(Level::Critical)]);
    }

    #[test]
    fn a_warn_that_was_never_sent_clears_silently() {
        let mut state = CheckState::new(0);
        let levels = [Level::Warn, Level::Warn, Level::Ok, Level::Ok];
        assert!(
            run(&mut state, 0, &levels, Hold::Warn)
                .iter()
                .all(Option::is_none)
        );
        assert!(!state.notified);
    }

    #[test]
    fn a_job_holds_every_message_and_a_still_bad_check_alerts_as_new_after_it() {
        let mut state = CheckState::new(0);
        let held = run(&mut state, 0, &[Level::Critical; 3], Hold::All);
        assert_eq!(held, [None, None, None]);
        assert_eq!((state.level, state.notified), (Level::Critical, false));
        assert_eq!(
            state.advance(Level::Critical, 180, Hold::Nothing, POLICY),
            alert(Level::Critical)
        );
    }

    #[test]
    fn a_job_holds_the_recovery_of_an_announced_check() {
        let mut state = CheckState::new(0);
        run(&mut state, 0, &[Level::Critical; 2], Hold::Nothing);
        assert!(state.notified);
        let held = run(&mut state, 120, &[Level::Ok; 2], Hold::All);
        assert_eq!(held, [None, None]);
        assert_eq!((state.level, state.notified), (Level::Ok, false));
    }

    #[test]
    fn a_bad_level_waits_for_bad_after_but_recovery_does_not() {
        let policy = Policy {
            bad_after: 600,
            ..POLICY
        };
        let mut state = CheckState::new(0);
        for minute in 0..10 {
            assert_eq!(
                state.advance(Level::Critical, minute * 60, Hold::Nothing, policy),
                None
            );
        }
        assert_eq!(
            state.advance(Level::Critical, 600, Hold::Nothing, policy),
            alert(Level::Critical)
        );
        assert_eq!(state.since, 0);
        state.advance(Level::Ok, 660, Hold::Nothing, policy);
        assert!(matches!(
            state.advance(Level::Ok, 720, Hold::Nothing, policy),
            Some(Notice::Recovery { .. })
        ));
    }
}
