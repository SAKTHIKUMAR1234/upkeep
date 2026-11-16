use std::time::Duration;

use super::checks::{Level, Observation};
use super::clock::UtcOffset;
use super::state::Notice;

/// Host name and clock shared by every message of one runner.
#[derive(Clone, Debug)]
pub struct Letterhead {
    pub host: String,
    pub offset: UtcOffset,
}

impl Letterhead {
    /// For example "CRITICAL · erp\ndisk /: 93% used\n2026-11-16 10:05 +05:30".
    pub fn notice(&self, notice: &Notice, observation: &Observation, now: i64) -> String {
        let label = label(&observation.key);
        let (title, detail) = match notice {
            Notice::Alert { level, repeat } => {
                let still = if *repeat { "STILL " } else { "" };
                (
                    format!("{still}{}", title(*level)),
                    observation.value.clone(),
                )
            }
            Notice::Recovery { was, since } => (
                "RECOVERED".to_owned(),
                format!(
                    "{} (was {was} for {})",
                    observation.value,
                    span(now - since)
                ),
            ),
        };
        self.message(&title, &format!("{label}: {detail}"), now)
    }

    /// The daily digest: counts, then each check that is not ok.
    pub fn digest(&self, checks: &[(Level, &Observation)], now: i64) -> String {
        let count = |wanted| checks.iter().filter(|(level, _)| *level == wanted).count();
        let mut body = format!(
            "{} ok, {} warn, {} critical",
            count(Level::Ok),
            count(Level::Warn),
            count(Level::Critical)
        );
        let mut problems: Vec<_> = checks.iter().filter(|(level, _)| level.is_bad()).collect();
        problems.sort_by_key(|(level, observation)| (std::cmp::Reverse(*level), &observation.key));
        for (level, observation) in problems {
            body.push_str(&format!(
                "\n{} {}: {}",
                title(*level),
                label(&observation.key),
                observation.value
            ));
        }
        self.message("Daily digest", &body, now)
    }

    pub fn test(&self, now: i64) -> String {
        self.message("Test alert", "Upkeep can reach this chat.", now)
    }

    fn message(&self, title: &str, body: &str, now: i64) -> String {
        format!(
            "{title} · {}\n{body}\n{}",
            self.host,
            self.offset.format(now)
        )
    }
}

fn title(level: Level) -> &'static str {
    match level {
        Level::Ok => "OK",
        Level::Warn => "WARN",
        Level::Critical => "CRITICAL",
    }
}

/// `disk:/` reads as `disk /`, and `peer:erp` as `runner erp`.
fn label(key: &str) -> String {
    match key.split_once(':') {
        Some(("peer", name)) => format!("runner {name}"),
        Some((kind, name)) => format!("{kind} {name}"),
        None => key.to_owned(),
    }
}

fn span(seconds: i64) -> String {
    let minutes = (seconds.max(0) / 60) as u64;
    if minutes == 0 {
        return "under 1m".into();
    }
    humantime::format_duration(Duration::from_secs(minutes * 60)).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_803_700;

    fn letterhead() -> Letterhead {
        Letterhead {
            host: "erp".into(),
            offset: "+05:30".parse().unwrap(),
        }
    }

    #[test]
    fn alerts_show_host_check_state_value_and_time() {
        let disk = Observation::new("disk:/", Level::Critical, "93% used");
        let alert = Notice::Alert {
            level: Level::Critical,
            repeat: false,
        };
        assert_eq!(
            letterhead().notice(&alert, &disk, NOW),
            "CRITICAL · erp\ndisk /: 93% used\n2026-11-16 10:05 +05:30"
        );
        let repeat = Notice::Alert {
            level: Level::Warn,
            repeat: true,
        };
        assert!(
            letterhead()
                .notice(&repeat, &disk, NOW)
                .starts_with("STILL WARN · erp\n")
        );
    }

    #[test]
    fn recovery_says_how_long_it_was_bad() {
        let peer = Observation::new("peer:backup", Level::Ok, "up");
        let recovery = Notice::Recovery {
            was: Level::Critical,
            since: NOW - 7500,
        };
        assert_eq!(
            letterhead().notice(&recovery, &peer, NOW),
            "RECOVERED · erp\nrunner backup: up (was critical for 2h 5m)\n2026-11-16 10:05 +05:30"
        );
    }

    #[test]
    fn digest_lists_problems_worst_first() {
        let disk = Observation::new("disk:/", Level::Warn, "85% used");
        let units = Observation::new("failed-units", Level::Critical, "1 failed: x.service");
        let load = Observation::new("load", Level::Ok, "0.10");
        let checks = [
            (Level::Warn, &disk),
            (Level::Ok, &load),
            (Level::Critical, &units),
        ];
        assert_eq!(
            letterhead().digest(&checks, NOW),
            "Daily digest · erp\n1 ok, 1 warn, 1 critical\n\
             CRITICAL failed-units: 1 failed: x.service\nWARN disk /: 85% used\n\
             2026-11-16 10:05 +05:30"
        );
    }
}
