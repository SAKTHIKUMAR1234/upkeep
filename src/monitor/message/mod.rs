pub mod digest;
pub mod job;
pub mod titles;
pub mod wording;

use super::checks::{Level, Observation};
use super::clock::UtcOffset;
use super::state::{CheckState, Notice};
use crate::time::format_unix;
use wording::describe;

/// Server name, clock and reminder time shared by every message of one runner.
#[derive(Clone, Debug)]
pub struct Letterhead {
    /// The friendly server name, such as "YRP server (.22)".
    pub host: String,
    pub offset: UtcOffset,
    /// Seconds between reminders of a check that stays bad.
    pub repeat_every: i64,
}

/// A check that asks for a message in this round.
#[derive(Clone, Copy, Debug)]
pub struct Event<'a> {
    pub notice: &'a Notice,
    pub observation: &'a Observation,
    pub state: &'a CheckState,
}

impl Letterhead {
    /// The messages of one round: one per peer, then one per alert severity, then one for the recoveries.
    pub fn round(&self, events: &[Event], now: i64) -> Vec<String> {
        let (peers, checks): (Vec<Event>, Vec<Event>) = events
            .iter()
            .partition(|event| event.observation.kind() == "peer");
        let mut messages: Vec<String> = peers.iter().map(|event| self.peer(event, now)).collect();
        for level in [Level::Critical, Level::Warn] {
            for repeat in [false, true] {
                let wanted = Notice::Alert { level, repeat };
                let group: Vec<Event> = checks
                    .iter()
                    .filter(|event| *event.notice == wanted)
                    .copied()
                    .collect();
                if !group.is_empty() {
                    messages.push(self.alerts(level, repeat, &group, now));
                }
            }
        }
        let fixed: Vec<Event> = checks
            .into_iter()
            .filter(|event| matches!(event.notice, Notice::Recovery { .. }))
            .collect();
        if !fixed.is_empty() {
            messages.push(self.recoveries(&fixed, now));
        }
        messages
    }

    fn alerts(&self, level: Level, repeat: bool, group: &[Event], now: i64) -> String {
        let (emoji, one, many) = match level {
            Level::Critical => ("🔴", "CRITICAL", "CRITICAL"),
            _ => ("⚠️", "WARNING", "WARNINGS"),
        };
        let still = if repeat { "STILL " } else { "" };
        let label = match group {
            [_] => format!("{still}{one}"),
            _ => format!("{still}{} {many}", group.len()),
        };
        let mut lines = vec![header(emoji, &label, &self.host)];
        if let [event] = group {
            let wording = describe(event.observation, &self.host);
            lines.push(format!("<b>{}</b>", wording.title));
            lines.push(wording.detail);
            lines.push(format!("Impact: {}", wording.impact));
            lines.push(self.since_line(event.state, now));
        } else {
            let mut impacts: Vec<String> = Vec::new();
            for event in group {
                let wording = describe(event.observation, &self.host);
                lines.push(format!(
                    "• <b>{}</b>: {} Since {}.",
                    wording.title,
                    wording.detail,
                    self.time(event.state.since, now)
                ));
                if !impacts.contains(&wording.impact) {
                    impacts.push(wording.impact);
                }
            }
            lines.extend(impacts.iter().map(|impact| format!("Impact: {impact}")));
        }
        lines.push(self.reminder());
        lines.join("\n")
    }

    fn recoveries(&self, group: &[Event], now: i64) -> String {
        let label = match group {
            [_] => "FIXED".to_owned(),
            _ => format!("{} FIXED", group.len()),
        };
        let mut lines = vec![header("✅", &label, &self.host)];
        for event in group {
            let wording = describe(event.observation, &self.host);
            let lasted = self.lasted(event, wording.outage, now);
            if group.len() == 1 {
                lines.push(format!("<b>{}</b>", wording.fixed));
                lines.push(format!("{}.", capitalize(&lasted)));
            } else {
                lines.push(format!("• <b>{}</b>: {lasted}.", wording.fixed));
            }
        }
        lines.join("\n")
    }

    /// A peer runner that stops or starts answering, seen from this runner.
    fn peer(&self, event: &Event, now: i64) -> String {
        let peer = event.observation.subject.as_deref().unwrap_or_default();
        let wording = describe(event.observation, &self.host);
        if let Notice::Recovery { .. } = event.notice {
            let lasted = self.lasted(event, true, now);
            return [
                header("✅", "FIXED", peer),
                format!("<b>{}</b>", wording.fixed),
                format!("{}. Seen from {}.", capitalize(&lasted), escape(&self.host)),
            ]
            .join("\n");
        }
        let error = event.observation.value.trim_start_matches("down: ");
        [
            header("🚨", "SERVER NOT ANSWERING", peer),
            wording.detail,
            wording.impact,
            format!("Error: {}", escape(error)),
            format!(
                "Since {} · no answer for {}",
                self.time(event.state.since, now),
                span(now - event.state.since)
            ),
            self.reminder(),
        ]
        .join("\n")
    }

    /// For example "was down for 12 minutes (17:05 to 17:17)".
    fn lasted(&self, event: &Event, outage: bool, now: i64) -> String {
        let Notice::Recovery { since, until, .. } = *event.notice else {
            return String::new();
        };
        let verb = if outage { "was down for" } else { "lasted" };
        let mut text = format!(
            "{verb} {} ({} to {})",
            span(until - since),
            self.time(since, now),
            self.time(until, now)
        );
        if !outage {
            text.push_str(&format!(". Now {}", escape(&event.observation.value)));
        }
        text
    }

    fn since_line(&self, state: &CheckState, now: i64) -> String {
        let since = format!("Since {}", self.time(state.since, now));
        match state.rounds {
            0 => since,
            1 => format!("{since} · checked once"),
            rounds => format!("{since} · checked {rounds} times"),
        }
    }

    fn reminder(&self) -> String {
        format!("Reminder every {} until fixed.", span(self.repeat_every))
    }

    pub fn test(&self) -> String {
        [
            header("🧪", "TEST", &self.host),
            "Upkeep can reach this chat.".to_owned(),
        ]
        .join("\n")
    }

    /// "17:05" today, or "15-11-2026 17:05" on another day.
    pub fn time(&self, unix: i64, now: i64) -> String {
        let local = format_unix(self.offset.local_seconds(unix));
        if self.offset.day(unix) == self.offset.day(now) {
            local[11..16].to_owned()
        } else {
            format!("{} {}", self.date(unix), &local[11..16])
        }
    }

    /// For example "17-11-2026".
    pub fn date(&self, unix: i64) -> String {
        let local = format_unix(self.offset.local_seconds(unix));
        format!("{}-{}-{}", &local[8..10], &local[5..7], &local[..4])
    }
}

/// The bold first line, such as "🔴 CRITICAL · YRP server (.22)".
pub fn header(emoji: &str, label: &str, host: &str) -> String {
    format!("<b>{emoji} {label} · {}</b>", escape(host))
}

/// Escapes text for Telegram's HTML parse mode.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A length of time in words, such as "12 minutes" or "2 days 3 hours".
pub fn span(seconds: i64) -> String {
    let minutes = seconds.max(0) / 60;
    let parts = [
        (minutes / 1440, "day"),
        (minutes / 60 % 24, "hour"),
        (minutes % 60, "minute"),
    ];
    let Some(first) = parts.iter().position(|(number, _)| *number > 0) else {
        return "under a minute".into();
    };
    parts[first..(first + 2).min(parts.len())]
        .iter()
        .filter(|(number, _)| *number > 0)
        .map(|(number, unit)| count(*number as usize, unit))
        .collect::<Vec<_>>()
        .join(" ")
}

/// For example "1 step" or "3 steps".
pub fn count(number: usize, noun: &str) -> String {
    let plural = if number == 1 { "" } else { "s" };
    format!("{number} {noun}{plural}")
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 2026-11-16 17:17 at +05:30.
    pub const NOW: i64 = 1_794_803_700 + 7 * 3600 + 12 * 60;

    pub fn letterhead() -> Letterhead {
        Letterhead {
            host: "YRP server (.22)".into(),
            offset: "+05:30".parse().unwrap(),
            repeat_every: 6 * 3600,
        }
    }

    fn state(since: i64, rounds: u32) -> CheckState {
        CheckState {
            rounds,
            ..CheckState::new(since)
        }
    }

    const NEW_CRITICAL: Notice = Notice::Alert {
        level: Level::Critical,
        repeat: false,
    };
    const NEW_WARN: Notice = Notice::Alert {
        level: Level::Warn,
        repeat: false,
    };

    #[test]
    fn one_critical_alert_follows_the_mockup() {
        let web = Observation::new(
            "service:production/Production website",
            Level::Critical,
            "down",
        )
        .with_subject("essdee_v16_production-web");
        let since = state(NOW - 12 * 60, 2);
        let event = Event {
            notice: &NEW_CRITICAL,
            observation: &web,
            state: &since,
        };
        assert_eq!(
            letterhead().round(&[event], NOW),
            ["<b>🔴 CRITICAL · YRP server (.22)</b>\n\
              <b>Production website is down</b>\n\
              Service <code>essdee_v16_production-web</code> of production is not running.\n\
              Impact: Users of production may be affected until it runs again.\n\
              Since 17:05 · checked 2 times\n\
              Reminder every 6 hours until fixed."]
        );
    }

    #[test]
    fn alerts_of_one_severity_are_grouped_and_severities_are_not() {
        let disk = Observation::new("disk:/", Level::Warn, "84% used")
            .with_subject("786 GB of 936 GB")
            .with_amount(84.0);
        let reboot = Observation::new("reboot-required", Level::Warn, "a reboot is required");
        let units = Observation::new("failed-units", Level::Critical, "1 failed: x.service")
            .with_subject("x.service")
            .with_amount(1.0);
        let since = state(NOW - 2 * 60, 2);
        let events = [
            Event {
                notice: &NEW_WARN,
                observation: &disk,
                state: &since,
            },
            Event {
                notice: &NEW_CRITICAL,
                observation: &units,
                state: &since,
            },
            Event {
                notice: &NEW_WARN,
                observation: &reboot,
                state: &since,
            },
        ];
        let messages = letterhead().round(&events, NOW);
        assert_eq!(messages.len(), 2);
        assert!(
            messages[0].starts_with(
                "<b>🔴 CRITICAL · YRP server (.22)</b>\n<b>1 system service failed</b>"
            )
        );
        assert_eq!(
            messages[1],
            "<b>⚠️ 2 WARNINGS · YRP server (.22)</b>\n\
             • <b>Disk 84% full</b>: <code>/</code> has 786 GB of 936 GB used. Since 17:15.\n\
             • <b>Restart needed</b>: Kernel updates are installed but not active yet. Since 17:15.\n\
             Impact: No user impact now. Plan a cleanup.\n\
             Impact: No user impact now. Plan a restart.\n\
             Reminder every 6 hours until fixed."
        );
    }

    #[test]
    fn recoveries_show_the_downtime_and_are_grouped() {
        let web =
            Observation::new("service:production/web", Level::Ok, "up").with_subject("web.service");
        let recovery = Notice::Recovery {
            was: Level::Critical,
            since: NOW - 12 * 60,
            until: NOW,
        };
        let now_ok = state(NOW, 2);
        let one = Event {
            notice: &recovery,
            observation: &web,
            state: &now_ok,
        };
        assert_eq!(
            letterhead().round(&[one], NOW),
            ["<b>✅ FIXED · YRP server (.22)</b>\n<b>web is back</b>\n\
              Was down for 12 minutes (17:05 to 17:17)."]
        );

        let disk = Observation::new("disk:/", Level::Ok, "62% used").with_amount(62.0);
        let two = Event {
            notice: &recovery,
            observation: &disk,
            state: &now_ok,
        };
        assert_eq!(
            letterhead().round(&[one, two], NOW),
            ["<b>✅ 2 FIXED · YRP server (.22)</b>\n\
              • <b>web is back</b>: was down for 12 minutes (17:05 to 17:17).\n\
              • <b>Disk / has space again</b>: lasted 12 minutes (17:05 to 17:17). Now 62% used."]
        );
    }

    #[test]
    fn a_peer_gets_its_own_message() {
        let mut replica = letterhead();
        replica.host = "Replica server (.11)".into();
        let peer = Observation::new("peer:erp", Level::Critical, "down: Connection refused")
            .with_subject("YRP server (.22)");
        let since = state(NOW - 2 * 60, 2);
        let event = Event {
            notice: &NEW_CRITICAL,
            observation: &peer,
            state: &since,
        };
        assert_eq!(
            replica.round(&[event], NOW),
            ["<b>🚨 SERVER NOT ANSWERING · YRP server (.22)</b>\n\
              Replica server (.11) cannot reach Upkeep on YRP server (.22).\n\
              Possible reasons: server off, network down, or Upkeep stopped.\n\
              Error: Connection refused\n\
              Since 17:15 · no answer for 2 minutes\n\
              Reminder every 6 hours until fixed."]
        );
    }

    #[test]
    fn host_names_and_values_are_escaped() {
        let mut odd = letterhead();
        odd.host = "a<b>&c".into();
        let disk = Observation::new("disk:/x&y", Level::Ok, "<1% used");
        let recovery = Notice::Recovery {
            was: Level::Warn,
            since: NOW - 60,
            until: NOW,
        };
        let ok = state(NOW, 2);
        let event = Event {
            notice: &recovery,
            observation: &disk,
            state: &ok,
        };
        let message = odd.round(&[event], NOW).remove(0);
        assert!(
            message.starts_with(
                "<b>✅ FIXED · a&lt;b&gt;&amp;c</b>\n<b>Disk /x&amp;y has space again</b>"
            ),
            "{message}"
        );
        assert!(message.ends_with("Now &lt;1% used."), "{message}");
        assert_eq!(
            odd.test(),
            "<b>🧪 TEST · a&lt;b&gt;&amp;c</b>\nUpkeep can reach this chat."
        );
    }

    #[test]
    fn times_show_the_date_only_on_another_day_and_spans_read_as_words() {
        let head = letterhead();
        assert_eq!(head.time(NOW, NOW), "17:17");
        assert_eq!(head.time(NOW - 86_400, NOW), "15-11-2026 17:17");
        assert_eq!(head.date(NOW), "16-11-2026");
        assert_eq!(span(30), "under a minute");
        assert_eq!(span(60), "1 minute");
        assert_eq!(span(2 * 3600 + 5 * 60), "2 hours 5 minutes");
        assert_eq!(span(86_400 + 5 * 60), "1 day");
        assert_eq!(span(2 * 86_400 + 3 * 3600), "2 days 3 hours");
    }
}
