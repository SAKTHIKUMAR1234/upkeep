use rusqlite::{OptionalExtension, params};

use super::{Db, normalize_email};
use crate::app::auth::token::{hash_token, random_token};

/// A stored session. The store keeps only the hash of its token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecord {
    pub email: String,
    pub csrf: String,
    pub created_at: i64,
    pub last_seen_at: i64,
}

impl Db {
    /// Starts a session and returns its token and CSRF token.
    pub fn create_session(&self, email: &str, now: i64) -> rusqlite::Result<(String, String)> {
        let (token, csrf) = (random_token(), random_token());
        self.lock().execute(
            "INSERT INTO sessions (token_hash, email, csrf, created_at, last_seen_at) \
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![hash_token(&token), normalize_email(email), csrf, now],
        )?;
        Ok((token, csrf))
    }

    /// The session of this token while its user exists and is enabled.
    pub fn session(&self, token: &str) -> rusqlite::Result<Option<SessionRecord>> {
        self.lock()
            .query_row(
                "SELECT s.email, s.csrf, s.created_at, s.last_seen_at FROM sessions s \
                 JOIN users u ON u.email = s.email AND u.disabled = 0 WHERE s.token_hash = ?1",
                [hash_token(token)],
                |row| {
                    Ok(SessionRecord {
                        email: row.get(0)?,
                        csrf: row.get(1)?,
                        created_at: row.get(2)?,
                        last_seen_at: row.get(3)?,
                    })
                },
            )
            .optional()
    }

    pub fn touch_session(&self, token: &str, now: i64) -> rusqlite::Result<()> {
        self.lock().execute(
            "UPDATE sessions SET last_seen_at = ?2 WHERE token_hash = ?1",
            params![hash_token(token), now],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, token: &str) -> rusqlite::Result<()> {
        self.lock().execute(
            "DELETE FROM sessions WHERE token_hash = ?1",
            [hash_token(token)],
        )?;
        Ok(())
    }

    /// Removes sessions that started before `created_before` or were last used before
    /// `seen_before`.
    pub fn delete_old_sessions(
        &self,
        created_before: i64,
        seen_before: i64,
    ) -> rusqlite::Result<()> {
        self.lock().execute(
            "DELETE FROM sessions WHERE created_at < ?1 OR last_seen_at < ?2",
            params![created_before, seen_before],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::database;

    #[test]
    fn tokens_are_stored_hashed_and_sessions_end_on_delete() {
        let (_dir, db) = database();
        db.add_user("a@example.com", "hash", 1).unwrap();
        let (token, csrf) = db.create_session("A@example.com", 1000).unwrap();
        let stored: String = db
            .lock()
            .query_row("SELECT token_hash FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_ne!(stored, token);

        let session = db.session(&token).unwrap().unwrap();
        assert_eq!(
            (session.email.as_str(), session.csrf),
            ("a@example.com", csrf)
        );
        db.touch_session(&token, 2000).unwrap();
        assert_eq!(db.session(&token).unwrap().unwrap().last_seen_at, 2000);
        assert!(db.session("other").unwrap().is_none());

        db.delete_session(&token).unwrap();
        assert!(db.session(&token).unwrap().is_none());
    }

    #[test]
    fn old_sessions_are_removed() {
        let (_dir, db) = database();
        db.add_user("a@example.com", "hash", 1).unwrap();
        let (old, _) = db.create_session("a@example.com", 100).unwrap();
        let (new, _) = db.create_session("a@example.com", 5000).unwrap();
        db.delete_old_sessions(1000, 1000).unwrap();
        assert!(db.session(&old).unwrap().is_none());
        assert!(db.session(&new).unwrap().is_some());
    }
}
