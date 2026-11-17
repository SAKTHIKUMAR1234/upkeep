use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Unix time in milliseconds.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

pub fn format_unix(seconds: i64) -> String {
    let moment = UNIX_EPOCH + Duration::from_secs(seconds.max(0) as u64);
    humantime::format_rfc3339_seconds(moment).to_string()
}

pub fn now_rfc3339() -> String {
    format_unix(now_unix())
}
