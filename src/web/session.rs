use std::io::Read;

use axum::http::{HeaderMap, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::users::{Role, WebDb, normalize_email};
use crate::api::auth::constant_time_eq;

/// A session lasts this long, then the user signs in again.
pub const SESSION_TTL_SECS: i64 = 12 * 3600;
const TOKEN_BYTES: usize = 32;

/// Why a request to `/api` is refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AccessError {
    #[error("sign in first")]
    SignedOut,
    #[error("your role cannot do this")]
    Forbidden,
    #[error("missing or wrong X-CSRF-Token header")]
    Csrf,
}

/// 32 random bytes from the kernel, as URL-safe base64.
pub fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .expect("the kernel random source is readable");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-256 as URL-safe base64. Also the PKCE S256 challenge of a verifier.
pub fn sha256(text: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(text.as_bytes()))
}

/// A signed-in user, read on each request so a removed user or changed role applies at once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Session {
    pub email: String,
    pub role: Role,
    /// Sent back by the page in `X-CSRF-Token` on every change.
    pub csrf: String,
    pub expires_at: i64,
}

impl Session {
    pub fn require(&self, role: Role) -> Result<(), AccessError> {
        if self.role >= role {
            Ok(())
        } else {
            Err(AccessError::Forbidden)
        }
    }

    pub fn check_csrf(&self, presented: Option<&str>) -> Result<(), AccessError> {
        match presented {
            Some(presented) if constant_time_eq(&self.csrf, presented) => Ok(()),
            _ => Err(AccessError::Csrf),
        }
    }
}

impl WebDb {
    /// Starts a session and returns its id for the cookie. Only a hash of the id is stored.
    pub fn create_session(&self, email: &str, now: i64) -> rusqlite::Result<String> {
        let id = random_token();
        let connection = self.lock();
        connection.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now])?;
        connection.execute(
            "INSERT INTO sessions (id_hash, email, csrf, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                sha256(&id),
                normalize_email(email),
                random_token(),
                now,
                now + SESSION_TTL_SECS
            ],
        )?;
        Ok(id)
    }

    /// The live session with this id whose user is still on the list.
    pub fn session(&self, id: &str, now: i64) -> rusqlite::Result<Option<Session>> {
        let row: Option<(String, String, String, i64)> = self
            .lock()
            .query_row(
                "SELECT s.email, u.role, s.csrf, s.expires_at FROM sessions s \
                 JOIN users u ON u.email = s.email WHERE s.id_hash = ?1 AND s.expires_at > ?2",
                params![sha256(id), now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        Ok(row.and_then(|(email, role, csrf, expires_at)| {
            Some(Session {
                email,
                role: Role::parse(&role)?,
                csrf,
                expires_at,
            })
        }))
    }

    pub fn delete_session(&self, id: &str) -> rusqlite::Result<()> {
        self.lock()
            .execute("DELETE FROM sessions WHERE id_hash = ?1", [sha256(id)])?;
        Ok(())
    }
}

/// A host-only, HttpOnly, SameSite=Lax cookie. `Secure` and the `__Host-` prefix go together.
#[derive(Clone, Debug)]
pub struct Cookie {
    name: String,
    secure: bool,
}

impl Cookie {
    pub fn new(name: &str, secure: bool) -> Self {
        let prefix = if secure { "__Host-" } else { "" };
        Self {
            name: format!("{prefix}{name}"),
            secure,
        }
    }

    pub fn read<'a>(&self, headers: &'a HeaderMap) -> Option<&'a str> {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(name, _)| *name == self.name)
            .map(|(_, value)| value)
    }

    /// The `Set-Cookie` value that keeps `value` for `max_age` seconds; 0 removes the cookie.
    pub fn set(&self, value: &str, max_age: i64) -> String {
        let secure = if self.secure { "; Secure" } else { "" };
        format!(
            "{}={value}; Max-Age={max_age}; Path=/; HttpOnly; SameSite=Lax{secure}",
            self.name
        )
    }

    pub fn clear(&self) -> String {
        self.set("", 0)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::super::users::tests::database;
    use super::*;

    fn session(role: Role) -> Session {
        Session {
            email: "a@example.com".into(),
            role,
            csrf: "csrf-token".into(),
            expires_at: 0,
        }
    }

    #[test]
    fn roles_allow_their_own_level_and_below() {
        assert!(session(Role::Viewer).require(Role::Viewer).is_ok());
        assert_eq!(
            session(Role::Viewer).require(Role::Operator),
            Err(AccessError::Forbidden)
        );
        assert!(session(Role::Operator).require(Role::Operator).is_ok());
        assert_eq!(
            session(Role::Operator).require(Role::Admin),
            Err(AccessError::Forbidden)
        );
        assert!(session(Role::Admin).require(Role::Operator).is_ok());
    }

    #[test]
    fn csrf_token_must_match_exactly() {
        let session = session(Role::Admin);
        assert!(session.check_csrf(Some("csrf-token")).is_ok());
        for wrong in [None, Some(""), Some("csrf-toke"), Some("csrf-token2")] {
            assert_eq!(
                session.check_csrf(wrong),
                Err(AccessError::Csrf),
                "{wrong:?}"
            );
        }
    }

    #[test]
    fn sessions_expire_and_end_with_the_user() {
        let (_dir, db) = database();
        db.set_user("a@example.com", Role::Operator, "cli").unwrap();
        let id = db.create_session("A@example.com", 1000).unwrap();

        let live = db.session(&id, 1001).unwrap().unwrap();
        assert_eq!(
            (live.email.as_str(), live.role),
            ("a@example.com", Role::Operator)
        );
        assert_eq!(live.expires_at, 1000 + SESSION_TTL_SECS);
        assert!(db.session(&id, 1000 + SESSION_TTL_SECS).unwrap().is_none());
        assert!(db.session("other-id", 1001).unwrap().is_none());

        db.remove_user("a@example.com").unwrap();
        db.set_user("a@example.com", Role::Viewer, "cli").unwrap();
        assert!(db.session(&id, 1001).unwrap().is_none());
    }

    #[test]
    fn logout_deletes_the_session_and_the_id_is_stored_hashed() {
        let (_dir, db) = database();
        db.set_user("a@example.com", Role::Viewer, "cli").unwrap();
        let id = db.create_session("a@example.com", 1000).unwrap();
        let stored: String = db
            .lock()
            .query_row("SELECT id_hash FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_ne!(stored, id);

        db.delete_session(&id).unwrap();
        assert!(db.session(&id, 1001).unwrap().is_none());
    }

    #[test]
    fn tokens_are_random_and_long() {
        let (first, second) = (random_token(), random_token());
        assert_ne!(first, second);
        assert_eq!(first.len(), 43);
    }

    #[test]
    fn cookies_use_the_host_prefix_when_secure() {
        let secure = Cookie::new("upkeep", true);
        assert_eq!(
            secure.set("abc", 60),
            "__Host-upkeep=abc; Max-Age=60; Path=/; HttpOnly; SameSite=Lax; Secure"
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; __Host-upkeep=abc; upkeep=no"),
        );
        assert_eq!(secure.read(&headers), Some("abc"));
        let plain = Cookie::new("upkeep", false);
        assert_eq!(plain.read(&headers), Some("no"));
        assert!(!plain.clear().contains("Secure"));
    }
}
