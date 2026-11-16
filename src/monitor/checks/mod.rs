pub mod host;
pub mod system;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::settings::Limits;

/// The state of one check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Critical,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }

    pub fn is_bad(self) -> bool {
        self != Self::Ok
    }
}

impl fmt::Display for Level {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Level {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "ok" => Ok(Self::Ok),
            "warn" => Ok(Self::Warn),
            "critical" => Ok(Self::Critical),
            other => Err(format!("unknown level {other:?}")),
        }
    }
}

/// What one check saw in one round.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Observation {
    /// Stable id, such as `disk:/` or `service:frappe-bench/web`.
    pub key: String,
    pub level: Level,
    /// Short readable value, such as `91% used`.
    pub value: String,
}

impl Observation {
    pub fn new(key: impl Into<String>, level: Level, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            level,
            value: value.into(),
        }
    }
}

/// The level of `value` against `limits`.
pub fn evaluate(value: f64, limits: Limits) -> Level {
    if value >= limits.critical {
        Level::Critical
    } else if value >= limits.warn {
        Level::Warn
    } else {
        Level::Ok
    }
}

/// A percent check, such as `disk:/` at `91% used`.
pub fn percent(key: String, used: f64, limits: Limits) -> Observation {
    Observation::new(key, evaluate(used, limits), format!("{used:.0}% used"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_are_inclusive() {
        let limits = Limits::new(80.0, 90.0);
        assert_eq!(evaluate(79.9, limits), Level::Ok);
        assert_eq!(evaluate(80.0, limits), Level::Warn);
        assert_eq!(evaluate(89.99, limits), Level::Warn);
        assert_eq!(evaluate(90.0, limits), Level::Critical);
        assert_eq!(evaluate(100.0, limits), Level::Critical);
    }

    #[test]
    fn levels_order_from_ok_to_critical() {
        assert!(Level::Ok < Level::Warn && Level::Warn < Level::Critical);
        for level in [Level::Ok, Level::Warn, Level::Critical] {
            assert_eq!(level.as_str().parse::<Level>().unwrap(), level);
        }
        assert_eq!(
            percent("disk:/".into(), 91.4, Limits::new(80.0, 90.0)),
            Observation::new("disk:/", Level::Critical, "91% used")
        );
    }
}
