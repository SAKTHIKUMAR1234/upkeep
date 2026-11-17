/// An enum stored as text, with `as_str` and `parse`.
macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

mod activity;
mod page;
mod runs;
mod servers;
mod sessions;
mod users;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

pub use activity::{Activity, ActivityFilter, ActivityRecord};
pub use page::{Cursor, Page};
pub use runs::{NewRun, RunFilter, RunRecord, RunStatus, ServerStats, Step, StepStatus};
pub use servers::{CachedOperation, Server, ServerFields, is_unique_violation};
pub use sessions::SessionRecord;
pub use users::User;

/// All times are unix milliseconds.
const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    disabled INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS login_failures (
    email TEXT PRIMARY KEY,
    failures INTEGER NOT NULL,
    locked_until INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    email TEXT NOT NULL,
    csrf TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_email ON sessions (email);
CREATE TABLE IF NOT EXISTS servers (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    address TEXT NOT NULL,
    port INTEGER NOT NULL,
    ssh_user TEXT NOT NULL,
    key_path TEXT NOT NULL,
    reachable INTEGER,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS operations (
    server_id INTEGER NOT NULL,
    folder TEXT NOT NULL,
    title TEXT,
    has_check INTEGER NOT NULL,
    PRIMARY KEY (server_id, folder)
);
CREATE TABLE IF NOT EXISTS runs (
    id INTEGER PRIMARY KEY,
    server_id INTEGER NOT NULL,
    server_name TEXT NOT NULL,
    operation TEXT NOT NULL,
    title TEXT NOT NULL,
    user_email TEXT NOT NULL,
    status TEXT NOT NULL,
    exit_code INTEGER,
    detail TEXT,
    steps TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS runs_one_per_server ON runs (server_id) WHERE status = 'running';
CREATE INDEX IF NOT EXISTS runs_started ON runs (started_at, id);
CREATE INDEX IF NOT EXISTS runs_server ON runs (server_id, started_at, id);
CREATE INDEX IF NOT EXISTS runs_server_operation ON runs (server_id, operation, started_at, id);
CREATE INDEX IF NOT EXISTS runs_operation ON runs (operation, started_at, id);
CREATE INDEX IF NOT EXISTS runs_user ON runs (user_email, started_at, id);
CREATE INDEX IF NOT EXISTS runs_status ON runs (status, started_at, id);
CREATE VIRTUAL TABLE IF NOT EXISTS runs_search USING fts5 (
    server_name, operation, title, user_email, content = 'runs', content_rowid = 'id'
);
CREATE TRIGGER IF NOT EXISTS runs_search_insert AFTER INSERT ON runs BEGIN
    INSERT INTO runs_search (rowid, server_name, operation, title, user_email)
    VALUES (new.id, new.server_name, new.operation, new.title, new.user_email);
END;
CREATE TABLE IF NOT EXISTS activity (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    user_email TEXT NOT NULL,
    action TEXT NOT NULL,
    server_name TEXT,
    detail TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS activity_at ON activity (at, id);
CREATE INDEX IF NOT EXISTS activity_user ON activity (user_email, at, id);
CREATE INDEX IF NOT EXISTS activity_server ON activity (server_name, at, id);
CREATE INDEX IF NOT EXISTS activity_action ON activity (action, at, id);
";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("corrupt value {0:?} in the database")]
    Corrupt(String),
}

/// The app's SQLite database.
pub struct Db {
    connection: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Lower-case, trimmed email, so emails compare without case.
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// The longest email the app accepts, in bytes.
pub const MAX_EMAIL_BYTES: usize = 254;

/// An email of at most [`MAX_EMAIL_BYTES`] with one `@`, a dot in the domain and no spaces.
pub fn is_valid_email(email: &str) -> bool {
    email.len() <= MAX_EMAIL_BYTES
        && !email
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.contains('@')
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn database() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("app.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn opening_twice_keeps_the_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        Db::open(&path).unwrap();
        let db = Db::open(&path).unwrap();
        let mode: String = db
            .lock()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
}
