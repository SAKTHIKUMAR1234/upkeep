use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::time::now_unix;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS users (
    email TEXT PRIMARY KEY,
    role TEXT NOT NULL,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    id_hash TEXT PRIMARY KEY,
    email TEXT NOT NULL,
    csrf TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    email TEXT NOT NULL,
    action TEXT NOT NULL,
    host TEXT,
    detail TEXT NOT NULL
);
";

/// What a user may do. Each role can do everything the roles before it can.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Sees servers, jobs and logs.
    Viewer,
    /// Also starts and cancels plans.
    Operator,
    /// Also manages users and reads the audit log.
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::Admin => "admin",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::Viewer, Self::Operator, Self::Admin]
            .into_iter()
            .find(|role| role.as_str() == text)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct User {
    pub email: String,
    pub role: Role,
    pub added_by: String,
    pub added_at: i64,
}

/// One recorded sign-in, refusal, run, cancel or user change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AuditEntry {
    pub at: i64,
    pub email: String,
    pub action: String,
    pub host: Option<String>,
    pub detail: String,
}

/// Emails compare without case: Google may report `Owner@Example.com` for `owner@example.com`.
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// The email in lowercase, or `None` when it does not look like `name@domain.tld`.
pub fn checked_email(email: &str) -> Option<String> {
    let email = normalize_email(email);
    let (name, domain) = email.split_once('@')?;
    let valid = !name.is_empty() && domain.contains('.') && !email.contains(char::is_whitespace);
    valid.then_some(email)
}

/// The web page database: allowed users, sessions and the audit log.
pub struct WebDb {
    connection: Mutex<Connection>,
}

impl WebDb {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The role of an allowed email, or `None` when the email is not on the list.
    pub fn role(&self, email: &str) -> rusqlite::Result<Option<Role>> {
        let role: Option<String> = self
            .lock()
            .query_row(
                "SELECT role FROM users WHERE email = ?1",
                [normalize_email(email)],
                |row| row.get(0),
            )
            .optional()?;
        Ok(role.as_deref().and_then(Role::parse))
    }

    pub fn users(&self) -> rusqlite::Result<Vec<User>> {
        let connection = self.lock();
        let mut statement = connection
            .prepare("SELECT email, role, added_by, added_at FROM users ORDER BY email")?;
        let rows = statement.query_map([], |row| {
            let role: String = row.get(1)?;
            Ok(User {
                email: row.get(0)?,
                role: Role::parse(&role).unwrap_or(Role::Viewer),
                added_by: row.get(2)?,
                added_at: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Adds the email, or changes its role when it is already on the list.
    pub fn set_user(&self, email: &str, role: Role, added_by: &str) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT INTO users (email, role, added_by, added_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (email) DO UPDATE SET role = excluded.role",
            params![normalize_email(email), role.as_str(), added_by, now_unix()],
        )?;
        Ok(())
    }

    /// Removes the email and signs it out everywhere. Returns false when it was not on the list.
    pub fn remove_user(&self, email: &str) -> rusqlite::Result<bool> {
        let email = normalize_email(email);
        let connection = self.lock();
        connection.execute("DELETE FROM sessions WHERE email = ?1", [&email])?;
        Ok(connection.execute("DELETE FROM users WHERE email = ?1", [&email])? > 0)
    }

    pub fn record(
        &self,
        email: &str,
        action: &str,
        host: Option<&str>,
        detail: &str,
    ) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT INTO audit (at, email, action, host, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now_unix(), email, action, host, detail],
        )?;
        Ok(())
    }

    /// The newest audit entries first.
    pub fn audit(&self, limit: u32) -> rusqlite::Result<Vec<AuditEntry>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT at, email, action, host, detail FROM audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(AuditEntry {
                at: row.get(0)?,
                email: row.get(1)?,
                action: row.get(2)?,
                host: row.get(3)?,
                detail: row.get(4)?,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub fn database() -> (tempfile::TempDir, WebDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = WebDb::open(&dir.path().join("web.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn roles_are_ordered_and_parse_from_text() {
        assert!(Role::Viewer < Role::Operator && Role::Operator < Role::Admin);
        for role in [Role::Viewer, Role::Operator, Role::Admin] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("root"), None);
    }

    #[test]
    fn emails_are_checked_and_lowercased() {
        assert_eq!(
            checked_email(" Dev@Example.com ").as_deref(),
            Some("dev@example.com")
        );
        for bad in ["", "dev", "@example.com", "dev@example", "d ev@example.com"] {
            assert_eq!(checked_email(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_allow_list_ignores_case() {
        let (_dir, db) = database();
        db.set_user("Owner@Example.com", Role::Admin, "cli")
            .unwrap();

        assert_eq!(db.role("owner@example.com").unwrap(), Some(Role::Admin));
        assert_eq!(db.role(" OWNER@EXAMPLE.COM ").unwrap(), Some(Role::Admin));
        assert_eq!(db.role("other@example.com").unwrap(), None);
        assert_eq!(db.users().unwrap()[0].email, "owner@example.com");
    }

    #[test]
    fn set_changes_the_role_and_remove_takes_the_user_off() {
        let (_dir, db) = database();
        db.set_user("dev@example.com", Role::Viewer, "cli").unwrap();
        db.set_user("DEV@example.com", Role::Operator, "owner@example.com")
            .unwrap();
        assert_eq!(db.users().unwrap().len(), 1);
        assert_eq!(db.role("dev@example.com").unwrap(), Some(Role::Operator));

        assert!(db.remove_user("Dev@Example.com").unwrap());
        assert!(!db.remove_user("dev@example.com").unwrap());
        assert_eq!(db.role("dev@example.com").unwrap(), None);
    }

    #[test]
    fn audit_lists_the_newest_first() {
        let (_dir, db) = database();
        db.record("a@example.com", "sign-in", None, "").unwrap();
        db.record("a@example.com", "run", Some("replica"), "os-update: job 4")
            .unwrap();
        let entries = db.audit(10).unwrap();
        assert_eq!(entries[0].action, "run");
        assert_eq!(entries[0].host.as_deref(), Some("replica"));
        assert_eq!(entries[1].action, "sign-in");
        assert_eq!(db.audit(1).unwrap().len(), 1);
    }
}
