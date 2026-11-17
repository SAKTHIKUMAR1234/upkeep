use axum::http::{HeaderMap, header};

use crate::app::store::SessionRecord;

/// A session ends after this long without a request; the screen then asks for the password.
pub const IDLE_MS: i64 = 15 * 60_000;
/// A session ends this long after sign-in, used or not.
pub const ABSOLUTE_MS: i64 = 12 * 3_600_000;
pub const COOKIE: &str = "upkeep_session";
/// The header that carries the CSRF token on every request that changes something.
pub const CSRF_HEADER: &str = "x-csrf-token";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Live,
    /// Unused for too long: the user unlocks with the password.
    Idle,
    /// Past its 12 hours: the user signs in again.
    Expired,
}

pub fn state(session: &SessionRecord, now: i64) -> SessionState {
    if now - session.created_at >= ABSOLUTE_MS {
        SessionState::Expired
    } else if now - session.last_seen_at >= IDLE_MS {
        SessionState::Idle
    } else {
        SessionState::Live
    }
}

/// The session cookie: HttpOnly and SameSite=Strict. Not `Secure`: the service is plain HTTP on
/// the loopback address.
pub fn set_cookie(token: &str) -> String {
    format!(
        "{COOKIE}={token}; Max-Age={}; Path=/; HttpOnly; SameSite=Strict",
        ABSOLUTE_MS / 1000
    )
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict")
}

pub fn read_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value)
}

/// Whether the request carries the session's CSRF token.
pub fn has_csrf(headers: &HeaderMap, session: &SessionRecord) -> bool {
    headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|presented| crate::api::auth::constant_time_eq(&session.csrf, presented))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn session(created_at: i64, last_seen_at: i64) -> SessionRecord {
        SessionRecord {
            email: "a@x.com".into(),
            csrf: "csrf-token".into(),
            created_at,
            last_seen_at,
        }
    }

    #[test]
    fn sessions_go_idle_after_fifteen_minutes_and_end_after_twelve_hours() {
        assert_eq!(state(&session(0, 0), IDLE_MS - 1), SessionState::Live);
        assert_eq!(state(&session(0, 0), IDLE_MS), SessionState::Idle);
        let used = session(0, ABSOLUTE_MS - 1000);
        assert_eq!(state(&used, ABSOLUTE_MS - 1), SessionState::Live);
        assert_eq!(state(&used, ABSOLUTE_MS), SessionState::Expired);
    }

    #[test]
    fn csrf_header_must_match_the_session() {
        let session = session(0, 0);
        let mut headers = HeaderMap::new();
        assert!(!has_csrf(&headers, &session));
        for wrong in ["", "csrf-toke", "csrf-token2"] {
            headers.insert(CSRF_HEADER, HeaderValue::from_static(wrong));
            assert!(!has_csrf(&headers, &session), "{wrong}");
        }
        headers.insert(CSRF_HEADER, HeaderValue::from_static("csrf-token"));
        assert!(has_csrf(&headers, &session));
    }

    #[test]
    fn cookie_is_strict_and_http_only() {
        let cookie = set_cookie("abc");
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        assert!(!cookie.contains("Secure"));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; upkeep_session=abc"),
        );
        assert_eq!(read_cookie(&headers), Some("abc"));
    }
}
