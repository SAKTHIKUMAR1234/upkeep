use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::checks::{Level, Observation};
use super::state::CheckState;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS checks (
    key TEXT PRIMARY KEY,
    level TEXT NOT NULL,
    since INTEGER NOT NULL,
    streak INTEGER NOT NULL,
    last_sent_at INTEGER,
    notified INTEGER NOT NULL,
    observed TEXT NOT NULL,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    streak_since INTEGER NOT NULL DEFAULT 0,
    rounds INTEGER NOT NULL DEFAULT 0,
    subject TEXT,
    amount REAL
);
CREATE TABLE IF NOT EXISTS check_history (
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL,
    level TEXT NOT NULL,
    value TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS outbox (
    id INTEGER PRIMARY KEY,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    last_error TEXT
);
CREATE TABLE IF NOT EXISTS meta (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

const ENQUEUE: &str = "INSERT INTO outbox (text, created_at, next_attempt_at) VALUES (?1, ?2, ?2)";

/// Columns added after the first release, with their definitions.
const ADDED_CHECK_COLUMNS: &[(&str, &str)] = &[
    ("streak_since", "INTEGER NOT NULL DEFAULT 0"),
    ("rounds", "INTEGER NOT NULL DEFAULT 0"),
    ("subject", "TEXT"),
    ("amount", "REAL"),
];
/// The outbox holds Telegram HTML since this format version.
const OUTBOX_FORMAT: (&str, &str) = ("outbox_format", "html");

/// One check as `GET /monitor` shows it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckRow {
    pub key: String,
    #[serde(flatten)]
    pub state: CheckState,
    /// The level seen in the latest round. It can differ from `level` while it is debounced.
    pub observed: Level,
    pub value: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub amount: Option<f64>,
    pub updated_at: i64,
}

impl CheckRow {
    /// The latest round's observation.
    pub fn observation(&self) -> Observation {
        Observation {
            key: self.key.clone(),
            level: self.observed,
            value: self.value.clone(),
            subject: self.subject.clone(),
            amount: self.amount,
        }
    }
}

/// A message waiting in the outbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub id: i64,
    pub text: String,
    pub attempts: u32,
}

/// Monitor state, check history and the alert outbox, in `monitor.db` next to the job database.
pub struct MonitorDb {
    connection: Mutex<Connection>,
}

impl MonitorDb {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        upgrade(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn checks(&self) -> rusqlite::Result<Vec<CheckRow>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT key, level, since, streak, last_sent_at, notified, observed, value, updated_at, \
             streak_since, rounds, subject, amount FROM checks ORDER BY key",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(CheckRow {
                key: row.get(0)?,
                state: CheckState {
                    level: level_column(row, 1)?,
                    since: row.get(2)?,
                    streak: row.get(3)?,
                    streak_since: row.get(9)?,
                    rounds: row.get(10)?,
                    last_sent_at: row.get(4)?,
                    notified: row.get(5)?,
                },
                observed: level_column(row, 6)?,
                value: row.get(7)?,
                subject: row.get(11)?,
                amount: row.get(12)?,
                updated_at: row.get(8)?,
            })
        })?;
        rows.collect()
    }

    pub fn states(&self) -> rusqlite::Result<BTreeMap<String, CheckState>> {
        Ok(self
            .checks()?
            .into_iter()
            .map(|row| (row.key, row.state))
            .collect())
    }

    /// Saves one round and queues its messages in one transaction, so a crash neither loses nor repeats them.
    /// Checks missing from the round are dropped; a changed level is added to the history.
    pub fn save_round(
        &self,
        round: &[(Observation, CheckState)],
        messages: &[String],
        now: i64,
    ) -> rusqlite::Result<()> {
        let previous = self.states()?;
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM checks", [])?;
        for (observation, state) in round {
            transaction.execute(
                "INSERT INTO checks (key, level, since, streak, last_sent_at, notified, observed, value, \
                 updated_at, streak_since, rounds, subject, amount) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    observation.key,
                    state.level.as_str(),
                    state.since,
                    state.streak,
                    state.last_sent_at,
                    state.notified,
                    observation.level.as_str(),
                    observation.value,
                    now,
                    state.streak_since,
                    state.rounds,
                    observation.subject,
                    observation.amount
                ],
            )?;
            let old_level = previous.get(&observation.key).map(|old| old.level);
            if old_level.unwrap_or(Level::Ok) != state.level {
                transaction.execute(
                    "INSERT INTO check_history (key, level, value, at) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        observation.key,
                        state.level.as_str(),
                        observation.value,
                        now
                    ],
                )?;
            }
        }
        for text in messages {
            transaction.execute(ENQUEUE, params![text, now])?;
        }
        transaction.commit()
    }

    /// The level changes of a check, oldest first.
    pub fn history(&self, key: &str) -> rusqlite::Result<Vec<(Level, i64)>> {
        let connection = self.lock();
        let mut statement =
            connection.prepare("SELECT level, at FROM check_history WHERE key = ?1 ORDER BY id")?;
        let rows = statement.query_map([key], |row| Ok((level_column(row, 0)?, row.get(1)?)))?;
        rows.collect()
    }

    pub fn enqueue(&self, text: &str, now: i64) -> rusqlite::Result<i64> {
        let connection = self.lock();
        connection.execute(ENQUEUE, params![text, now])?;
        Ok(connection.last_insert_rowid())
    }

    /// Messages to send now, oldest first. A message that waits for a retry holds back the newer ones, to keep order.
    pub fn due(&self, now: i64) -> rusqlite::Result<Vec<Pending>> {
        let connection = self.lock();
        let mut statement = connection
            .prepare("SELECT id, text, attempts, next_attempt_at FROM outbox ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            let pending = Pending {
                id: row.get(0)?,
                text: row.get(1)?,
                attempts: row.get(2)?,
            };
            Ok((pending, row.get::<_, i64>(3)?))
        })?;
        let mut due = Vec::new();
        for row in rows {
            let (pending, next_attempt_at) = row?;
            if next_attempt_at > now {
                break;
            }
            due.push(pending);
        }
        Ok(due)
    }

    pub fn delivered(&self, id: i64) -> rusqlite::Result<()> {
        self.lock()
            .execute("DELETE FROM outbox WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn retry_later(&self, id: i64, next_attempt_at: i64, error: &str) -> rusqlite::Result<()> {
        self.lock().execute(
            "UPDATE outbox SET attempts = attempts + 1, next_attempt_at = ?2, last_error = ?3 WHERE id = ?1",
            params![id, next_attempt_at, error],
        )?;
        Ok(())
    }

    /// Drops messages created before `cutoff` and returns how many.
    pub fn expire(&self, cutoff: i64) -> rusqlite::Result<usize> {
        self.lock()
            .execute("DELETE FROM outbox WHERE created_at < ?1", [cutoff])
    }

    pub fn outbox_len(&self) -> rusqlite::Result<i64> {
        self.lock()
            .query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))
    }

    pub fn meta(&self, name: &str) -> rusqlite::Result<Option<String>> {
        self.lock()
            .query_row("SELECT value FROM meta WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .optional()
    }

    pub fn set_meta(&self, name: &str, value: &str) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT INTO meta (name, value) VALUES (?1, ?2) ON CONFLICT (name) DO UPDATE SET value = ?2",
            params![name, value],
        )?;
        Ok(())
    }
}

/// Brings a database from an older release to the current shape.
fn upgrade(connection: &Connection) -> rusqlite::Result<()> {
    let mut added = Vec::new();
    for (column, definition) in ADDED_CHECK_COLUMNS {
        let exists = connection
            .prepare("SELECT 1 FROM pragma_table_info('checks') WHERE name = ?1")?
            .exists([column])?;
        if !exists {
            connection.execute_batch(&format!(
                "ALTER TABLE checks ADD COLUMN {column} {definition}"
            ))?;
            added.push(*column);
        }
    }
    if added.contains(&"streak_since") {
        connection.execute_batch("UPDATE checks SET streak_since = updated_at")?;
    }
    escape_old_outbox(connection)
}

/// Messages queued by an older release are plain text: escapes them for parse_mode=HTML.
fn escape_old_outbox(connection: &Connection) -> rusqlite::Result<()> {
    let (name, format) = OUTBOX_FORMAT;
    let current: Option<String> = connection
        .query_row("SELECT value FROM meta WHERE name = ?1", [name], |row| {
            row.get(0)
        })
        .optional()?;
    if current.as_deref() != Some(format) {
        connection.execute_batch(
            "UPDATE outbox SET text = replace(replace(replace(text, '&', '&amp;'), '<', '&lt;'), '>', '&gt;')",
        )?;
        connection.execute(
            "INSERT OR REPLACE INTO meta (name, value) VALUES (?1, ?2)",
            [name, format],
        )?;
    }
    Ok(())
}

fn level_column(row: &rusqlite::Row, index: usize) -> rusqlite::Result<Level> {
    let text: String = row.get(index)?;
    text.parse().map_err(|error: String| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, error.into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_keep_state_history_and_drop_missing_checks() {
        let dir = tempfile::tempdir().unwrap();
        let db = MonitorDb::open(&dir.path().join("monitor.db")).unwrap();
        let disk = |level| Observation::new("disk:/", level, "91% used");
        let mut state = CheckState::new(0);

        db.save_round(&[(disk(Level::Critical), state.clone())], &[], 10)
            .unwrap();
        state.level = Level::Critical;
        let load = Observation::new("load", Level::Ok, "0.1");
        db.save_round(
            &[
                (disk(Level::Critical), state.clone()),
                (load.clone(), CheckState::new(20)),
            ],
            &["disk alert".to_owned()],
            20,
        )
        .unwrap();
        db.save_round(&[(load, CheckState::new(20))], &[], 30)
            .unwrap();

        assert_eq!(db.history("disk:/").unwrap(), [(Level::Critical, 20)]);
        let keys: Vec<String> = db
            .checks()
            .unwrap()
            .into_iter()
            .map(|row| row.key)
            .collect();
        assert_eq!(keys, ["load"]);
        assert_eq!(db.due(20).unwrap()[0].text, "disk alert");
    }

    #[test]
    fn an_old_database_gets_the_new_columns_and_escaped_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("monitor.db");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            "CREATE TABLE checks (key TEXT PRIMARY KEY, level TEXT NOT NULL, since INTEGER NOT NULL, \
             streak INTEGER NOT NULL, last_sent_at INTEGER, notified INTEGER NOT NULL, \
             observed TEXT NOT NULL, value TEXT NOT NULL, updated_at INTEGER NOT NULL);
             INSERT INTO checks VALUES ('load', 'ok', 5, 0, NULL, 0, 'ok', '0.1', 9);
             CREATE TABLE outbox (id INTEGER PRIMARY KEY, text TEXT NOT NULL, created_at INTEGER NOT NULL,
             attempts INTEGER NOT NULL DEFAULT 0, next_attempt_at INTEGER NOT NULL, last_error TEXT);
             INSERT INTO outbox (text, created_at, next_attempt_at) VALUES ('a <b> & c', 1, 1);",
        )
        .unwrap();
        drop(old);

        let db = MonitorDb::open(&path).unwrap();
        let row = &db.checks().unwrap()[0];
        assert_eq!(
            (row.state.rounds, row.state.streak_since, row.amount),
            (0, 9, None)
        );
        assert_eq!(db.due(1).unwrap()[0].text, "a &lt;b&gt; &amp; c");
        drop(db);
        let again = MonitorDb::open(&path).unwrap();
        assert_eq!(again.due(1).unwrap()[0].text, "a &lt;b&gt; &amp; c");
    }

    #[test]
    fn rows_keep_subject_amount_and_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let db = MonitorDb::open(&dir.path().join("monitor.db")).unwrap();
        let disk = Observation::new("disk:/", Level::Warn, "84% used")
            .with_subject("786 GB of 936 GB")
            .with_amount(84.2);
        let mut state = CheckState::new(0);
        state.rounds = 3;
        db.save_round(&[(disk.clone(), state.clone())], &[], 10)
            .unwrap();
        let row = db.checks().unwrap().remove(0);
        assert_eq!(row.observation(), disk);
        assert_eq!(row.state, state);
        let json = serde_json::to_value(&row).unwrap();
        let back: CheckRow = serde_json::from_value(json).unwrap();
        assert_eq!(back.observation(), disk);
    }

    #[test]
    fn outbox_keeps_messages_until_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("monitor.db");
        let db = MonitorDb::open(&path).unwrap();
        let first = db.enqueue("one", 100).unwrap();
        db.enqueue("two", 100).unwrap();

        db.retry_later(first, 160, "HTTP 502").unwrap();
        let texts = |now| -> Vec<String> {
            db.due(now)
                .unwrap()
                .into_iter()
                .map(|pending| pending.text)
                .collect()
        };
        assert!(texts(159).is_empty());
        assert_eq!(texts(160), ["one", "two"]);
        assert_eq!(db.due(160).unwrap()[0].attempts, 1);

        drop(db);
        let reopened = MonitorDb::open(&path).unwrap();
        reopened.delivered(first).unwrap();
        assert_eq!(reopened.outbox_len().unwrap(), 1);
        assert_eq!(reopened.expire(101).unwrap(), 1);
        assert_eq!(reopened.outbox_len().unwrap(), 0);
    }
}
