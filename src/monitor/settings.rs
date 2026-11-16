use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Deserializer};

use super::clock::{ClockTime, QuietHours, UtcOffset};

/// The `[monitor]` table of `upkeep.toml`. The runner reads it again before every round.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct MonitorSettings {
    pub enabled: bool,
    #[serde(deserialize_with = "duration")]
    pub interval: Duration,
    /// Rounds in a row a check must show a new state before it is announced.
    pub alert_after: u32,
    /// A check that stays bad is announced again after this time.
    #[serde(deserialize_with = "duration")]
    pub repeat_every: Duration,
    /// Offset of the local time used for quiet hours, the digest and message times, such as "+05:30".
    pub utc_offset: UtcOffset,
    /// Local time range, such as "22:00-07:00", when warn messages wait. Critical always sends.
    pub quiet_hours: Option<QuietHours>,
    /// Local time of the daily digest, such as "09:00". No digest when absent.
    pub digest_at: Option<ClockTime>,
    /// Check kinds to skip, for example ["security-updates", "swap"].
    pub disable: Vec<String>,
    /// Mount points to check. Empty means every mounted disk file system.
    pub disk_mounts: Vec<PathBuf>,
    pub disk: Limits,
    pub inodes: Limits,
    pub memory: Limits,
    pub swap: Limits,
    /// Load average over 5 minutes, per CPU.
    pub load: Limits,
    /// Age of the newest snapshot. Not checked when absent.
    #[serde(deserialize_with = "optional_duration")]
    pub snapshot_max_age: Option<Duration>,
    pub telegram: Option<TelegramSettings>,
    #[serde(rename = "peer")]
    pub peers: Vec<PeerSettings>,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(60),
            alert_after: 2,
            repeat_every: Duration::from_secs(6 * 3600),
            utc_offset: UtcOffset::default(),
            quiet_hours: None,
            digest_at: None,
            disable: Vec::new(),
            disk_mounts: Vec::new(),
            disk: Limits::new(80.0, 90.0),
            inodes: Limits::new(80.0, 90.0),
            memory: Limits::new(90.0, 95.0),
            swap: Limits::new(50.0, 80.0),
            load: Limits::new(2.0, 4.0),
            snapshot_max_age: None,
            telegram: None,
            peers: Vec::new(),
        }
    }
}

impl MonitorSettings {
    pub fn is_enabled(&self, kind: &str) -> bool {
        !self.disable.iter().any(|disabled| disabled == kind)
    }
}

/// Warn and critical thresholds. A value at or above a threshold reaches that state.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub warn: f64,
    pub critical: f64,
}

impl Limits {
    pub const fn new(warn: f64, critical: f64) -> Self {
        Self { warn, critical }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TelegramSettings {
    /// Owner-only file with the bot token from @BotFather.
    pub token_file: PathBuf,
    /// The chat or group id, such as "-1001234567890".
    pub chat_id: String,
    /// Topic id in a group with topics.
    pub message_thread_id: Option<i64>,
    /// Bot API base URL. Change it only for tests.
    #[serde(default = "default_api_url")]
    pub api_url: String,
}

fn default_api_url() -> String {
    "https://api.telegram.org".into()
}

/// Another runner this one watches, on its token-protected TCP API.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PeerSettings {
    pub name: String,
    /// `http://<address>:<port>`, the peer's `listen` address.
    pub url: String,
    /// Owner-only file with the peer's API token.
    pub token_file: PathBuf,
}

fn duration<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    humantime::parse_duration(&text).map_err(serde::de::Error::custom)
}

fn optional_duration<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    duration(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_full_table_and_fills_defaults() {
        let settings: MonitorSettings = toml::from_str(
            r#"
            interval = "30s"
            quiet_hours = "22:00-07:00"
            utc_offset = "+05:30"
            digest_at = "09:00"
            snapshot_max_age = "26h"
            disk = { warn = 70, critical = 85 }
            [telegram]
            token_file = "/etc/upkeep/telegram-token"
            chat_id = "-100123"
            message_thread_id = 42
            [[peer]]
            name = "erp"
            url = "http://100.64.0.22:7878"
            token_file = "/etc/upkeep/peers/erp.token"
            "#,
        )
        .unwrap();

        assert_eq!(settings.interval, Duration::from_secs(30));
        assert_eq!(settings.alert_after, 2);
        assert_eq!(settings.disk, Limits::new(70.0, 85.0));
        assert_eq!(settings.inodes, Limits::new(80.0, 90.0));
        assert_eq!(
            settings.snapshot_max_age,
            Some(Duration::from_secs(26 * 3600))
        );
        let telegram = settings.telegram.unwrap();
        assert_eq!(telegram.api_url, "https://api.telegram.org");
        assert_eq!(telegram.message_thread_id, Some(42));
        assert_eq!(settings.peers[0].name, "erp");
    }

    #[test]
    fn rejects_unknown_keys_and_bad_durations() {
        assert!(toml::from_str::<MonitorSettings>("disk_warn = 80").is_err());
        assert!(toml::from_str::<MonitorSettings>("interval = \"soon\"").is_err());
    }

    #[test]
    fn the_commented_example_is_valid() {
        let text = include_str!("../../examples/config/upkeep.toml");
        let block: Vec<&str> = text[text.find("# [monitor]").unwrap()..]
            .lines()
            .map(|line| line.trim_start_matches('#').trim_start())
            .collect();
        let table: toml::Table = toml::from_str(&block.join("\n")).unwrap();
        let settings: MonitorSettings = table["monitor"].clone().try_into().unwrap();
        assert_eq!(settings.peers.len(), 1);
        assert_eq!(settings.telegram.unwrap().message_thread_id, Some(12));
    }
}
