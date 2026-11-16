use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer};

use crate::time::format_unix;

const MINUTES_PER_DAY: i64 = 24 * 60;

/// A fixed offset from UTC, such as "+05:30". Servers keep UTC; alerts use the owner's time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UtcOffset {
    minutes: i64,
}

impl UtcOffset {
    pub fn local_seconds(self, unix: i64) -> i64 {
        unix + self.minutes * 60
    }

    /// Minutes since local midnight.
    pub fn minute_of_day(self, unix: i64) -> i64 {
        self.local_seconds(unix)
            .div_euclid(60)
            .rem_euclid(MINUTES_PER_DAY)
    }

    /// Days since 1970-01-01 in local time.
    pub fn day(self, unix: i64) -> i64 {
        self.local_seconds(unix).div_euclid(86_400)
    }

    /// For example "2026-11-16 10:05 +05:30".
    pub fn format(self, unix: i64) -> String {
        let local = format_unix(self.local_seconds(unix));
        format!("{} {self}", local[..16].replace('T', " "))
    }
}

impl fmt::Display for UtcOffset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.minutes < 0 { '-' } else { '+' };
        let minutes = self.minutes.abs();
        write!(formatter, "{sign}{:02}:{:02}", minutes / 60, minutes % 60)
    }
}

impl FromStr for UtcOffset {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = || format!("invalid UTC offset {text:?}: use +HH:MM or -HH:MM");
        let (sign, rest) = match text.split_at_checked(1) {
            Some(("+", rest)) => (1, rest),
            Some(("-", rest)) => (-1, rest),
            _ => return Err(bad()),
        };
        let time: ClockTime = rest.parse().map_err(|_| bad())?;
        Ok(Self {
            minutes: sign * time.minutes,
        })
    }
}

/// A local time of day, such as "09:00".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockTime {
    pub minutes: i64,
}

impl FromStr for ClockTime {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = || format!("invalid time {text:?}: use HH:MM");
        let (hours, minutes) = text.split_once(':').ok_or_else(bad)?;
        let hours: i64 = hours.parse().map_err(|_| bad())?;
        let minutes: i64 = minutes.parse().map_err(|_| bad())?;
        if !(0..24).contains(&hours) || !(0..60).contains(&minutes) {
            return Err(bad());
        }
        Ok(Self {
            minutes: hours * 60 + minutes,
        })
    }
}

/// A local time range such as "22:00-07:00". It can pass midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuietHours {
    start: ClockTime,
    end: ClockTime,
}

impl QuietHours {
    pub fn contains(self, minute_of_day: i64) -> bool {
        let (start, end) = (self.start.minutes, self.end.minutes);
        if start <= end {
            (start..end).contains(&minute_of_day)
        } else {
            minute_of_day >= start || minute_of_day < end
        }
    }
}

impl FromStr for QuietHours {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (start, end) = text
            .split_once('-')
            .ok_or_else(|| format!("invalid quiet hours {text:?}: use HH:MM-HH:MM"))?;
        Ok(Self {
            start: start.trim().parse()?,
            end: end.trim().parse()?,
        })
    }
}

macro_rules! deserialize_from_str {
    ($($name:ident),+) => {$(
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    )+};
}

deserialize_from_str!(UtcOffset, ClockTime, QuietHours);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_shift_the_local_clock() {
        let india: UtcOffset = "+05:30".parse().unwrap();
        // 2026-11-16 04:35 UTC.
        let moment = 1_794_803_700;
        assert_eq!(india.format(moment), "2026-11-16 10:05 +05:30");
        assert_eq!(india.minute_of_day(moment), 10 * 60 + 5);
        let west: UtcOffset = "-03:00".parse().unwrap();
        assert_eq!(west.format(moment), "2026-11-16 01:35 -03:00");
        assert_eq!(west.day(moment), india.day(moment));
        for bad in ["05:30", "+5", "+25:00", "+05:60"] {
            assert!(bad.parse::<UtcOffset>().is_err(), "{bad}");
        }
    }

    #[test]
    fn quiet_hours_can_pass_midnight() {
        let night: QuietHours = "22:00-07:00".parse().unwrap();
        assert!(night.contains(23 * 60) && night.contains(0) && night.contains(6 * 60 + 59));
        assert!(!night.contains(7 * 60) && !night.contains(12 * 60));

        let lunch: QuietHours = "13:00-14:00".parse().unwrap();
        assert!(lunch.contains(13 * 60 + 30) && !lunch.contains(14 * 60));
        assert!("22:00".parse::<QuietHours>().is_err());
    }
}
