use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{Level, Observation};
use crate::adapter::health::{self, HealthReport};

const SECURITY_CHECK_EVERY: Duration = Duration::from_secs(3600);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const APT_TIMEOUT: Duration = Duration::from_secs(120);
pub const REBOOT_REQUIRED: &str = "/var/run/reboot-required";

/// One check per adapter service and ping, plus one per invalid adapter manifest.
pub fn adapter_health(report: &HealthReport) -> Vec<Observation> {
    let items = report.items.iter().map(|item| {
        let key = format!("{}:{}/{}", item.kind, item.adapter, item.name);
        let observation = match &item.detail {
            _ if item.ok => Observation::new(key, Level::Ok, "up"),
            Some(detail) => Observation::new(key, Level::Critical, detail.clone()),
            None => Observation::new(key, Level::Critical, "down"),
        };
        observation.with_subject(&item.target)
    });
    let errors = report.errors.iter().map(|error| {
        let key = format!("adapter:{}", error.path.display());
        Observation::new(key, Level::Critical, error.error.clone())
    });
    items.chain(errors).collect()
}

/// Critical while any systemd unit is in the failed state.
pub async fn failed_units() -> Observation {
    let mut command = health::command(
        "systemctl",
        &[
            "list-units",
            "--state=failed",
            "--no-legend",
            "--plain",
            "--no-pager",
        ],
    );
    match health::run(&mut command, COMMAND_TIMEOUT).await {
        Ok(output) => parse_failed_units(&output),
        Err(error) => {
            Observation::new("failed-units", Level::Warn, format!("cannot list: {error}"))
        }
    }
}

pub fn parse_failed_units(output: &str) -> Observation {
    let units: Vec<&str> = output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    if units.is_empty() {
        return Observation::new("failed-units", Level::Ok, "none");
    }
    Observation::new(
        "failed-units",
        Level::Critical,
        format!("{} failed: {}", units.len(), units.join(", ")),
    )
    .with_subject(units.join(", "))
    .with_amount(units.len() as f64)
}

pub fn reboot_required(flag: &Path) -> Observation {
    if flag.exists() {
        Observation::new("reboot-required", Level::Warn, "a reboot is required")
    } else {
        Observation::new("reboot-required", Level::Ok, "no")
    }
}

/// Warn when the newest snapshot is older than `max_age`, critical at twice that age.
pub fn snapshot_age(last_snapshot_at: Option<i64>, max_age: Duration, now: i64) -> Observation {
    let Some(finished_at) = last_snapshot_at else {
        return Observation::new("snapshot-age", Level::Warn, "no snapshot yet");
    };
    let age = Duration::from_secs((now - finished_at).max(0) as u64);
    let level = if age >= max_age * 2 {
        Level::Critical
    } else if age > max_age {
        Level::Warn
    } else {
        Level::Ok
    };
    let rounded = Duration::from_secs(age.as_secs() / 60 * 60);
    let age_text = if rounded.is_zero() {
        "under 1m".to_owned()
    } else {
        humantime::format_duration(rounded).to_string()
    };
    Observation::new(
        "snapshot-age",
        level,
        format!(
            "newest is {age_text} old (max {})",
            humantime::format_duration(max_age)
        ),
    )
    .with_amount(age.as_secs() as f64)
}

/// Pending security updates from apt, read at most once an hour.
#[derive(Default)]
pub struct SecurityUpdates {
    last: Mutex<Option<(Instant, Observation)>>,
}

impl SecurityUpdates {
    pub async fn observe(&self) -> Observation {
        if let Some((checked_at, observation)) = self.cached()
            && checked_at.elapsed() < SECURITY_CHECK_EVERY
        {
            return observation;
        }
        let observation = Self::read_apt().await;
        *self.lock() = Some((Instant::now(), observation.clone()));
        observation
    }

    fn cached(&self) -> Option<(Instant, Observation)> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(Instant, Observation)>> {
        self.last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn read_apt() -> Observation {
        let mut command = health::command(
            "apt-get",
            &["-s", "-o", "Debug::NoLocking=true", "dist-upgrade"],
        );
        command.env("LC_ALL", "C");
        match health::run(&mut command, APT_TIMEOUT).await {
            Ok(output) => parse_security_updates(&output),
            Err(error) => Observation::new(
                "security-updates",
                Level::Warn,
                format!("cannot read apt: {error}"),
            ),
        }
    }
}

/// Counts the `Inst` lines of an `apt-get -s` run that come from a security pocket.
pub fn parse_security_updates(output: &str) -> Observation {
    let count = output
        .lines()
        .filter(|line| line.starts_with("Inst ") && line.contains("-security"))
        .count();
    if count == 0 {
        return Observation::new("security-updates", Level::Ok, "none pending");
    }
    Observation::new("security-updates", Level::Warn, format!("{count} pending"))
        .with_amount(count as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_units_are_listed() {
        let output = "motd-news.service    loaded failed failed Message of the Day\n\
                      fwupd-refresh.service loaded failed failed Refresh fwupd metadata\n";
        let observation = parse_failed_units(output);
        assert_eq!(observation.level, Level::Critical);
        assert_eq!(
            observation.value,
            "2 failed: motd-news.service, fwupd-refresh.service"
        );
        assert_eq!(observation.amount, Some(2.0));
        assert_eq!(parse_failed_units("").level, Level::Ok);
    }

    #[test]
    fn counts_only_security_packages() {
        let output = include_str!("../../../adapters/tests/fixtures/apt-simulate.txt");
        let expected = output
            .lines()
            .filter(|line| line.starts_with("Inst ") && line.contains("-security"))
            .count();
        assert!(expected > 0);
        let observation = parse_security_updates(output);
        assert_eq!(observation.level, Level::Warn);
        assert_eq!(observation.value, format!("{expected} pending"));
        assert_eq!(
            parse_security_updates("Reading package lists...\n").level,
            Level::Ok
        );
    }

    #[test]
    fn snapshot_age_warns_then_goes_critical() {
        let day = Duration::from_secs(86_400);
        let now = 10 * 86_400;
        assert_eq!(snapshot_age(None, day, now).level, Level::Warn);
        let fresh = snapshot_age(Some(now - 3600), day, now);
        assert_eq!(fresh.level, Level::Ok);
        assert_eq!(fresh.value, "newest is 1h old (max 1day)");
        assert_eq!(fresh.amount, Some(3600.0));
        assert_eq!(
            snapshot_age(Some(now - 86_400 - 60), day, now).level,
            Level::Warn
        );
        assert_eq!(
            snapshot_age(Some(now - 2 * 86_400), day, now).level,
            Level::Critical
        );
    }
}
