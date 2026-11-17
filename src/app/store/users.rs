use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::{Db, normalize_email};

/// A person who may sign in. The password hash never leaves the store in this type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct User {
    pub email: String,
    pub disabled: bool,
    pub created_at: i64,
}

impl Db {
    /// Adds a user; `false` when the email exists.
    pub fn add_user(&self, email: &str, password_hash: &str, now: i64) -> rusqlite::Result<bool> {
        let added = self.lock().execute(
            "INSERT OR IGNORE INTO users (email, password_hash, created_at) VALUES (?1, ?2, ?3)",
            params![normalize_email(email), password_hash, now],
        )?;
        Ok(added == 1)
    }

    /// The stored hash and whether the user is disabled.
    pub fn password_hash(&self, email: &str) -> rusqlite::Result<Option<(String, bool)>> {
        self.lock()
            .query_row(
                "SELECT password_hash, disabled FROM users WHERE email = ?1",
                [normalize_email(email)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }

    /// Replaces the password and ends all the user's sessions; `false` for an unknown email.
    pub fn set_password(&self, email: &str, password_hash: &str) -> rusqlite::Result<bool> {
        let email = normalize_email(email);
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE users SET password_hash = ?2 WHERE email = ?1",
            params![email, password_hash],
        )?;
        transaction.execute("DELETE FROM sessions WHERE email = ?1", [&email])?;
        transaction.commit()?;
        Ok(changed == 1)
    }

    /// Disables or enables a user. Disabling ends the user's sessions.
    pub fn set_disabled(&self, email: &str, disabled: bool) -> rusqlite::Result<bool> {
        let email = normalize_email(email);
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE users SET disabled = ?2 WHERE email = ?1",
            params![email, disabled],
        )?;
        if disabled {
            transaction.execute("DELETE FROM sessions WHERE email = ?1", [&email])?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    pub fn users(&self) -> rusqlite::Result<Vec<User>> {
        let connection = self.lock();
        let mut statement =
            connection.prepare("SELECT email, disabled, created_at FROM users ORDER BY email")?;
        statement
            .query_map([], |row| {
                Ok(User {
                    email: row.get(0)?,
                    disabled: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })?
            .collect()
    }

    /// Consecutive failed sign-ins for an email, and until when it must wait.
    #[cfg(test)]
    pub fn login_failures(&self, email: &str) -> rusqlite::Result<(u32, i64)> {
        Ok(self
            .lock()
            .query_row(
                "SELECT failures, locked_until FROM login_failures WHERE email = ?1",
                [normalize_email(email)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0)))
    }

    /// Reads the failures in a row and the lock end of an email, and stores the pair that
    /// `change` returns, in one step. An `Err` from `change` stores nothing.
    pub fn change_login_failures<E>(
        &self,
        email: &str,
        change: impl FnOnce(u32, i64) -> Result<(u32, i64), E>,
    ) -> rusqlite::Result<Result<(u32, i64), E>> {
        let email = normalize_email(email);
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let (failures, locked_until) = transaction
            .query_row(
                "SELECT failures, locked_until FROM login_failures WHERE email = ?1",
                [&email],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0));
        let changed = change(failures, locked_until);
        if let Ok((failures, locked_until)) = changed {
            transaction.execute(
                "INSERT INTO login_failures (email, failures, locked_until) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (email) DO UPDATE SET failures = ?2, locked_until = ?3",
                params![email, failures, locked_until],
            )?;
        }
        transaction.commit()?;
        Ok(changed)
    }

    pub fn clear_login_failures(&self, email: &str) -> rusqlite::Result<()> {
        self.lock().execute(
            "DELETE FROM login_failures WHERE email = ?1",
            [normalize_email(email)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::database;

    #[test]
    fn emails_compare_without_case() {
        let (_dir, db) = database();
        assert!(db.add_user("Owner@Example.com", "hash", 1).unwrap());
        assert!(!db.add_user("owner@example.COM", "other", 2).unwrap());
        assert_eq!(
            db.password_hash("OWNER@example.com").unwrap(),
            Some(("hash".into(), false))
        );
        assert_eq!(db.users().unwrap()[0].email, "owner@example.com");
    }

    #[test]
    fn password_change_and_disable_end_sessions() {
        let (_dir, db) = database();
        db.add_user("a@example.com", "hash", 1).unwrap();
        let (token, _) = db.create_session("a@example.com", 10).unwrap();
        assert!(db.set_password("A@example.com", "new").unwrap());
        assert!(db.session(&token).unwrap().is_none());

        let (token, _) = db.create_session("a@example.com", 10).unwrap();
        assert!(db.set_disabled("a@example.com", true).unwrap());
        assert!(db.session(&token).unwrap().is_none());
        assert_eq!(
            db.password_hash("a@example.com").unwrap(),
            Some(("new".into(), true))
        );
        assert!(!db.set_password("b@example.com", "x").unwrap());
    }

    #[test]
    fn login_failures_are_stored_per_email() {
        let (_dir, db) = database();
        assert_eq!(db.login_failures("x@example.com").unwrap(), (0, 0));
        let stored = db
            .change_login_failures("X@example.com", |_, _| Ok::<_, ()>((5, 99)))
            .unwrap();
        assert_eq!(stored, Ok((5, 99)));
        let refused = db
            .change_login_failures("x@example.com", |failures, _| Err(failures))
            .unwrap();
        assert_eq!(refused, Err(5));
        assert_eq!(db.login_failures("x@example.com").unwrap(), (5, 99));
        db.clear_login_failures("x@example.com").unwrap();
        assert_eq!(db.login_failures("x@example.com").unwrap(), (0, 0));
    }
}
