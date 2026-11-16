use super::wording::describe;
use super::{Letterhead, count, escape, span};
use crate::monitor::checks::{Level, Observation};
use crate::monitor::state::CheckState;
use crate::store::UpdateCheck;

/// One server in the daily summary.
#[derive(Clone, Debug)]
pub enum Section {
    Host(HostReport),
    /// A peer whose `/monitor` did not answer.
    Unreachable {
        name: String,
        error: String,
    },
}

/// The checks and the newest package check of one server.
#[derive(Clone, Debug)]
pub struct HostReport {
    pub name: String,
    pub checks: Vec<(Observation, CheckState)>,
    pub updates: Option<UpdateCheck>,
}

impl Letterhead {
    /// The daily summary: one block per server, then the updates that wait.
    pub fn digest(&self, sections: &[Section], now: i64) -> String {
        let mut blocks = vec![format!("<b>📊 Daily summary · {}</b>", self.date(now))];
        let mut updates = Vec::new();
        for section in sections {
            match section {
                Section::Host(host) => {
                    blocks.push(host_block(host, now));
                    if let Some(check) = host.updates {
                        updates.push(self.updates(&host.name, check, now));
                    }
                }
                Section::Unreachable { name, error } => blocks.push(format!(
                    "<b>{}</b> · 🚨 not answering\nError: {}",
                    escape(name),
                    escape(error)
                )),
            }
        }
        if !updates.is_empty() {
            blocks.push(format!("Updates waiting: {}.", updates.join(", ")));
        }
        blocks.join("\n\n")
    }

    /// For example "YRP server (.22) has 70 packages (4 risky)".
    fn updates(&self, host: &str, check: UpdateCheck, now: i64) -> String {
        let mut text = match (check.packages, check.risky) {
            (0, _) => format!("{} is up to date", escape(host)),
            (packages, 0) => format!("{} has {packages} packages", escape(host)),
            (packages, risky) => {
                format!("{} has {packages} packages ({risky} risky)", escape(host))
            }
        };
        if now - check.checked_at > 86_400 {
            text.push_str(&format!(" (checked {} ago)", span(now - check.checked_at)));
        }
        text
    }
}

fn host_block(host: &HostReport, now: i64) -> String {
    let mut problems: Vec<&(Observation, CheckState)> = host
        .checks
        .iter()
        .filter(|(_, state)| state.level.is_bad())
        .collect();
    problems.sort_by_key(|(observation, state)| (std::cmp::Reverse(state.level), &observation.key));
    let badge = match problems.first() {
        None => "✅ all OK".to_owned(),
        Some((_, state)) => format!("{} {}", emoji(state.level), problems.len()),
    };

    let mut lines = vec![
        format!("<b>{}</b> · {badge}", escape(&host.name)),
        facts(host),
    ];
    for (observation, state) in problems {
        lines.push(format!(
            "{} {} (for {})",
            emoji(state.level),
            describe(observation, &host.name).title,
            span(now - state.since)
        ));
    }
    lines.join("\n")
}

/// For example "32 checks OK · disk 69% · memory 79% · last snapshot 1 day ago".
fn facts(host: &HostReport) -> String {
    let amount = |kind: &str| {
        host.checks
            .iter()
            .filter(|(observation, _)| observation.kind() == kind)
            .find_map(|(observation, _)| observation.amount)
    };
    let ok = host
        .checks
        .iter()
        .filter(|(_, state)| state.level == Level::Ok)
        .count();
    let mut facts = vec![format!("{} OK", count(ok, "check"))];
    let fullest_disk = host
        .checks
        .iter()
        .filter(|(observation, _)| observation.kind() == "disk")
        .filter_map(|(observation, _)| observation.amount)
        .reduce(f64::max);
    if let Some(disk) = fullest_disk {
        facts.push(format!("disk {disk:.0}%"));
    }
    if let Some(memory) = amount("memory") {
        facts.push(format!("memory {memory:.0}%"));
    }
    let has_snapshot_check = host
        .checks
        .iter()
        .any(|(observation, _)| observation.kind() == "snapshot-age");
    match amount("snapshot-age") {
        Some(age) => facts.push(format!("last snapshot {} ago", span(age as i64))),
        None if has_snapshot_check => facts.push("no snapshot yet".into()),
        None => {}
    }
    facts.join(" · ")
}

fn emoji(level: Level) -> &'static str {
    match level {
        Level::Ok => "✅",
        Level::Warn => "⚠️",
        Level::Critical => "🔴",
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{NOW, letterhead};
    use super::*;

    fn check(observation: Observation, level: Level, since: i64) -> (Observation, CheckState) {
        let state = CheckState {
            level,
            since,
            ..CheckState::new(since)
        };
        (observation, state)
    }

    #[test]
    fn summary_lists_each_server_its_problems_and_updates() {
        let day = 86_400;
        let yrp = HostReport {
            name: "YRP server (.22)".into(),
            checks: vec![
                check(
                    Observation::new("disk:/", Level::Ok, "69% used").with_amount(69.0),
                    Level::Ok,
                    0,
                ),
                check(
                    Observation::new("disk:/data", Level::Ok, "12% used").with_amount(12.0),
                    Level::Ok,
                    0,
                ),
                check(
                    Observation::new("memory", Level::Ok, "79% used").with_amount(79.4),
                    Level::Ok,
                    0,
                ),
                check(
                    Observation::new("snapshot-age", Level::Ok, "x").with_amount(day as f64 + 60.0),
                    Level::Ok,
                    0,
                ),
                check(
                    Observation::new("reboot-required", Level::Warn, "x"),
                    Level::Warn,
                    NOW - 2 * day,
                ),
            ],
            updates: Some(UpdateCheck {
                packages: 70,
                risky: 4,
                checked_at: NOW - 3600,
            }),
        };
        let replica = HostReport {
            name: "Replica <.11>".into(),
            checks: vec![check(
                Observation::new("disk:/", Level::Ok, "15% used").with_amount(15.0),
                Level::Ok,
                0,
            )],
            updates: Some(UpdateCheck {
                packages: 53,
                risky: 0,
                checked_at: NOW - 3 * day,
            }),
        };
        let gone = Section::Unreachable {
            name: "Backup".into(),
            error: "no answer in 10s".into(),
        };
        let summary = letterhead().digest(&[Section::Host(yrp), Section::Host(replica), gone], NOW);
        assert_eq!(
            summary,
            "<b>📊 Daily summary · 16-11-2026</b>\n\n\
             <b>YRP server (.22)</b> · ⚠️ 1\n\
             4 checks OK · disk 69% · memory 79% · last snapshot 1 day ago\n\
             ⚠️ Restart needed (for 2 days)\n\n\
             <b>Replica &lt;.11&gt;</b> · ✅ all OK\n\
             1 check OK · disk 15%\n\n\
             <b>Backup</b> · 🚨 not answering\nError: no answer in 10s\n\n\
             Updates waiting: YRP server (.22) has 70 packages (4 risky), \
             Replica &lt;.11&gt; has 53 packages (checked 3 days ago)."
        );
    }

    #[test]
    fn critical_problems_come_first() {
        let host = HostReport {
            name: "erp".into(),
            checks: vec![
                check(
                    Observation::new("load", Level::Warn, "x"),
                    Level::Warn,
                    NOW - 60,
                ),
                check(
                    Observation::new("snapshot-age", Level::Critical, "x"),
                    Level::Critical,
                    NOW - 120,
                ),
            ],
            updates: None,
        };
        let summary = letterhead().digest(&[Section::Host(host)], NOW);
        assert!(
            summary.ends_with(
                "<b>erp</b> · 🔴 2\n0 checks OK · no snapshot yet\n\
                 🔴 No snapshot yet (for 2 minutes)\n⚠️ Server is very busy (for 1 minute)"
            ),
            "{summary}"
        );
    }
}
