use axum::http::{HeaderMap, header};

use crate::app::serve::SessionLimits;
use crate::app::store::SessionRecord;

pub const COOKIE: &str = "upkeep_session";
/// The signed-in email, for the lock screen. The page reads it; the service never trusts it.
pub const EMAIL_COOKIE: &str = "upkeep_email";
const EMAIL_MAX_AGE_S: i64 = 30 * 24 * 3600;
/// The header that carries the CSRF token on every request that changes something.
pub const CSRF_HEADER: &str = "x-csrf-token";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Live,
    /// Unused for too long: the user unlocks with the password.
    Idle,
    /// Past its lifetime: the user signs in again.
    Expired,
}

pub fn state(session: &SessionRecord, now: i64, limits: SessionLimits) -> SessionState {
    if now - session.created_at >= limits.absolute_ms {
        SessionState::Expired
    } else if now - session.last_seen_at >= limits.idle_ms {
        SessionState::Idle
    } else {
        SessionState::Live
    }
}

/// The cookies of a service. `Secure` only on a website: the desktop app is plain HTTP on the
/// loopback address.
#[derive(Clone, Copy, Debug)]
pub struct Cookies {
    pub secure: bool,
    pub limits: SessionLimits,
}

impl Cookies {
    fn cookie(self, name: &str, value: &str, max_age_s: i64, http_only: bool) -> String {
        let mut cookie = format!("{name}={value}; Max-Age={max_age_s}; Path=/; SameSite=Strict");
        if http_only {
            cookie.push_str("; HttpOnly");
        }
        if self.secure {
            cookie.push_str("; Secure");
        }
        cookie
    }

    /// The session cookie: HttpOnly and SameSite=Strict, for the session's lifetime.
    pub fn session(self, token: &str) -> String {
        self.cookie(COOKIE, token, self.limits.absolute_ms / 1000, true)
    }

    pub fn clear_session(self) -> String {
        self.cookie(COOKIE, "", 0, true)
    }

    /// The email cookie, readable by the page, kept for 30 days.
    pub fn email(self, email: &str) -> String {
        self.cookie(EMAIL_COOKIE, &percent_encode(email), EMAIL_MAX_AGE_S, false)
    }

    pub fn clear_email(self) -> String {
        self.cookie(EMAIL_COOKIE, "", 0, false)
    }
}

/// Keeps letters, digits and `@.-_+`; encodes every other byte as `%XX`.
fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'@' | b'.' | b'-' | b'_' | b'+' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
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

    const LIMITS: SessionLimits = SessionLimits {
        idle_ms: 60_000,
        absolute_ms: 3_600_000,
    };

    fn session(created_at: i64, last_seen_at: i64) -> SessionRecord {
        SessionRecord {
            email: "a@x.com".into(),
            csrf: "csrf-token".into(),
            created_at,
            last_seen_at,
        }
    }

    #[test]
    fn sessions_go_idle_and_end_after_the_configured_times() {
        assert_eq!(state(&session(0, 0), 59_999, LIMITS), SessionState::Live);
        assert_eq!(state(&session(0, 0), 60_000, LIMITS), SessionState::Idle);
        let used = session(0, 3_599_000);
        assert_eq!(state(&used, 3_599_999, LIMITS), SessionState::Live);
        assert_eq!(state(&used, 3_600_000, LIMITS), SessionState::Expired);
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
    fn cookies_are_strict_and_secure_only_on_a_website() {
        let local = Cookies {
            secure: false,
            limits: LIMITS,
        };
        assert_eq!(
            local.session("abc"),
            "upkeep_session=abc; Max-Age=3600; Path=/; SameSite=Strict; HttpOnly"
        );
        let website = Cookies {
            secure: true,
            ..local
        };
        assert!(website.session("abc").ends_with("; HttpOnly; Secure"));
        assert_eq!(
            website.email("a.b+c@x.com"),
            "upkeep_email=a.b+c@x.com; Max-Age=2592000; Path=/; SameSite=Strict; Secure"
        );
        assert!(
            local
                .email("a\"b;@x.com")
                .starts_with("upkeep_email=a%22b%3B@x.com;")
        );
        assert!(
            local.clear_email().contains("Max-Age=0") && !local.clear_email().contains("HttpOnly")
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; upkeep_session=abc"),
        );
        assert_eq!(read_cookie(&headers), Some("abc"));
    }
}
