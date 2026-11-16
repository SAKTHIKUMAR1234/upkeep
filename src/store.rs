use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::Value;

use crate::plan::{JobPlan, StepSpec};
use crate::time::now_unix;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY,
    target TEXT NOT NULL,
    status TEXT NOT NULL,
    phase TEXT NOT NULL DEFAULT 'main',
    end_status TEXT,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    requested_by TEXT NOT NULL,
    error TEXT,
    reboot_boot_id TEXT,
    created_at INTEGER NOT NULL,
    finished_at INTEGER,
    params TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS one_active_job
    ON jobs (status IN ('running', 'rebooting')) WHERE status IN ('running', 'rebooting');
CREATE TABLE IF NOT EXISTS steps (
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    phase TEXT NOT NULL,
    idx INTEGER NOT NULL,
    adapter TEXT NOT NULL,
    action TEXT NOT NULL,
    adapter_dir TEXT NOT NULL,
    run TEXT NOT NULL,
    timeout_secs INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    exit_code INTEGER,
    result TEXT,
    error TEXT,
    started_at INTEGER,
    finished_at INTEGER,
    PRIMARY KEY (job_id, phase, idx)
);
";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("another job is already active")]
    Busy,
    #[error("job {0} not found")]
    NotFound(i64),
    #[error("corrupt value \"{0}\" in database")]
    Corrupt(String),
}

macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            fn parse(text: &str) -> Result<Self, StoreError> {
                match text {
                    $($text => Ok(Self::$variant),)+
                    other => Err(StoreError::Corrupt(other.to_owned())),
                }
            }
        }
    };
}

text_enum!(JobStatus {
    Running => "running",
    Rebooting => "rebooting",
    Succeeded => "succeeded",
    Degraded => "degraded",
    Failed => "failed",
    Cancelled => "cancelled",
});

text_enum!(Phase {
    Main => "main",
    Recovery => "recovery",
});

text_enum!(StepStatus {
    Pending => "pending",
    Running => "running",
    Ok => "ok",
    Degraded => "degraded",
    Reboot => "reboot",
    Failed => "failed",
    Skipped => "skipped",
});

impl JobStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Rebooting)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Job {
    pub id: i64,
    pub target: String,
    pub status: JobStatus,
    pub phase: Phase,
    pub end_status: Option<JobStatus>,
    pub cancel_requested: bool,
    pub requested_by: String,
    pub error: Option<String>,
    pub reboot_boot_id: Option<String>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    /// Non-secret values the client sent for the adapters, such as the snapshot destination.
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Step {
    pub job_id: i64,
    pub phase: Phase,
    pub index: i64,
    pub adapter: String,
    pub action: String,
    pub adapter_dir: String,
    pub run: String,
    pub timeout_secs: u64,
    pub status: StepStatus,
    pub exit_code: Option<i32>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// How a step ended.
#[derive(Clone, Debug, PartialEq)]
pub struct StepOutcome {
    pub status: StepStatus,
    pub exit_code: Option<i32>,
    pub result: Option<Value>,
    pub error: Option<String>,
}

const JOB_COLUMNS: &str = "id, target, status, phase, end_status, cancel_requested, requested_by, error, \
     reboot_boot_id, created_at, finished_at, params";
const STEP_COLUMNS: &str = "job_id, phase, idx, adapter, action, adapter_dir, run, timeout_secs, status, \
     exit_code, result, error, started_at, finished_at";

/// The job database. One connection, shared behind a mutex; every query is short.
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        add_params_column(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stores a new running job with all its steps. Fails with `Busy` while another job is active.
    pub fn create_job(
        &self,
        plan: &JobPlan,
        requested_by: &str,
        job_params: Option<&Value>,
    ) -> Result<i64, StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let inserted = transaction.execute(
            "INSERT INTO jobs (target, status, requested_by, created_at, params) \
             VALUES (?1, 'running', ?2, ?3, ?4)",
            params![
                plan.target,
                requested_by,
                now_unix(),
                job_params.map(Value::to_string)
            ],
        );
        if let Err(rusqlite::Error::SqliteFailure(error, _)) = &inserted
            && error.code == rusqlite::ErrorCode::ConstraintViolation
        {
            return Err(StoreError::Busy);
        }
        inserted?;

        let job_id = transaction.last_insert_rowid();
        insert_steps(&transaction, job_id, Phase::Main, &plan.steps)?;
        insert_steps(&transaction, job_id, Phase::Recovery, &plan.recovery)?;
        transaction.commit()?;
        Ok(job_id)
    }

    pub fn job(&self, id: i64) -> Result<Job, StoreError> {
        let sql = format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1");
        self.lock()
            .query_row(&sql, [id], job_from_row)
            .optional()?
            .ok_or(StoreError::NotFound(id))
    }

    pub fn jobs(&self, limit: u32) -> Result<Vec<Job>, StoreError> {
        let sql = format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY id DESC LIMIT ?1");
        let connection = self.lock();
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([limit], job_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn active_job(&self) -> Result<Option<Job>, StoreError> {
        let sql =
            format!("SELECT {JOB_COLUMNS} FROM jobs WHERE status IN ('running', 'rebooting')");
        Ok(self.lock().query_row(&sql, [], job_from_row).optional()?)
    }

    pub fn steps(&self, job_id: i64) -> Result<Vec<Step>, StoreError> {
        let sql = format!(
            "SELECT {STEP_COLUMNS} FROM steps WHERE job_id = ?1 \
             ORDER BY CASE phase WHEN 'main' THEN 0 ELSE 1 END, idx"
        );
        let connection = self.lock();
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([job_id], step_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The first step of `phase` that has not finished yet.
    pub fn next_step(&self, job_id: i64, phase: Phase) -> Result<Option<Step>, StoreError> {
        let sql = format!(
            "SELECT {STEP_COLUMNS} FROM steps WHERE job_id = ?1 AND phase = ?2 \
             AND status IN ('pending', 'running') ORDER BY idx LIMIT 1"
        );
        Ok(self
            .lock()
            .query_row(&sql, params![job_id, phase.as_str()], step_from_row)
            .optional()?)
    }

    /// When the newest successful `snapshot` step finished.
    pub fn last_snapshot_at(&self) -> Result<Option<i64>, StoreError> {
        Ok(self.lock().query_row(
            "SELECT MAX(finished_at) FROM steps WHERE action = 'snapshot' AND status IN ('ok', 'degraded')",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn mark_step_running(&self, step: &Step) -> Result<(), StoreError> {
        self.lock().execute(
            "UPDATE steps SET status = 'running', started_at = ?4 WHERE job_id = ?1 AND phase = ?2 AND idx = ?3",
            params![step.job_id, step.phase.as_str(), step.index, now_unix()],
        )?;
        Ok(())
    }

    /// Saves a step result. A reboot result also moves the job to `rebooting` in the same transaction.
    pub fn finish_step(
        &self,
        step: &Step,
        outcome: &StepOutcome,
        boot_id: &str,
    ) -> Result<(), StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let result = outcome.result.as_ref().map(Value::to_string);
        transaction.execute(
            "UPDATE steps SET status = ?4, exit_code = ?5, result = ?6, error = ?7, finished_at = ?8 \
             WHERE job_id = ?1 AND phase = ?2 AND idx = ?3",
            params![
                step.job_id,
                step.phase.as_str(),
                step.index,
                outcome.status.as_str(),
                outcome.exit_code,
                result,
                outcome.error,
                now_unix()
            ],
        )?;
        if outcome.status == StepStatus::Reboot {
            transaction.execute(
                "UPDATE jobs SET status = 'rebooting', reboot_boot_id = ?2 WHERE id = ?1",
                params![step.job_id, boot_id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn set_running(&self, job_id: i64) -> Result<(), StoreError> {
        self.lock()
            .execute("UPDATE jobs SET status = 'running' WHERE id = ?1", [job_id])?;
        Ok(())
    }

    /// Flags an active job for cancel. Returns false when the job is not active.
    pub fn request_cancel(&self, job_id: i64) -> Result<bool, StoreError> {
        self.job(job_id)?;
        let changed = self.lock().execute(
            "UPDATE jobs SET cancel_requested = 1 WHERE id = ?1 AND status IN ('running', 'rebooting')",
            [job_id],
        )?;
        Ok(changed == 1)
    }

    /// Skips the remaining main steps and switches the job to its recovery steps.
    pub fn enter_recovery(
        &self,
        job_id: i64,
        end_status: JobStatus,
        error: &str,
    ) -> Result<(), StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE steps SET status = 'skipped' WHERE job_id = ?1 AND phase = 'main' AND status = 'pending'",
            [job_id],
        )?;
        transaction.execute(
            "UPDATE jobs SET phase = 'recovery', end_status = ?2, error = ?3 WHERE id = ?1",
            params![job_id, end_status.as_str(), error],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn finish_job(
        &self,
        job_id: i64,
        status: JobStatus,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.lock().execute(
            "UPDATE jobs SET status = ?2, finished_at = ?3, error = COALESCE(?4, error) WHERE id = ?1",
            params![job_id, status.as_str(), now_unix(), error],
        )?;
        Ok(())
    }
}

/// Databases from before job params get the column on open.
fn add_params_column(connection: &Connection) -> Result<(), StoreError> {
    let has_params = connection
        .prepare("SELECT 1 FROM pragma_table_info('jobs') WHERE name = 'params'")?
        .exists([])?;
    if !has_params {
        connection.execute_batch("ALTER TABLE jobs ADD COLUMN params TEXT")?;
    }
    Ok(())
}

fn insert_steps(
    transaction: &rusqlite::Transaction,
    job_id: i64,
    phase: Phase,
    steps: &[StepSpec],
) -> Result<(), StoreError> {
    let mut statement = transaction.prepare(
        "INSERT INTO steps (job_id, phase, idx, adapter, action, adapter_dir, run, timeout_secs) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for (index, step) in steps.iter().enumerate() {
        statement.execute(params![
            job_id,
            phase.as_str(),
            index as i64,
            step.adapter,
            step.action,
            step.adapter_dir.to_string_lossy(),
            step.run,
            step.timeout.as_secs() as i64
        ])?;
    }
    Ok(())
}

fn parse_column<T>(
    row: &Row,
    index: usize,
    parse: fn(&str) -> Result<T, StoreError>,
) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    parse(&text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn job_from_row(row: &Row) -> rusqlite::Result<Job> {
    let end_status: Option<String> = row.get(4)?;
    let params_text: Option<String> = row.get(11)?;
    Ok(Job {
        id: row.get(0)?,
        target: row.get(1)?,
        status: parse_column(row, 2, JobStatus::parse)?,
        phase: parse_column(row, 3, Phase::parse)?,
        end_status: end_status
            .map(|_| parse_column(row, 4, JobStatus::parse))
            .transpose()?,
        cancel_requested: row.get(5)?,
        requested_by: row.get(6)?,
        error: row.get(7)?,
        reboot_boot_id: row.get(8)?,
        created_at: row.get(9)?,
        finished_at: row.get(10)?,
        params: params_text.and_then(|text| serde_json::from_str(&text).ok()),
    })
}

fn step_from_row(row: &Row) -> rusqlite::Result<Step> {
    let result: Option<String> = row.get(10)?;
    Ok(Step {
        job_id: row.get(0)?,
        phase: parse_column(row, 1, Phase::parse)?,
        index: row.get(2)?,
        adapter: row.get(3)?,
        action: row.get(4)?,
        adapter_dir: row.get(5)?,
        run: row.get(6)?,
        timeout_secs: row.get(7)?,
        status: parse_column(row, 8, StepStatus::parse)?,
        exit_code: row.get(9)?,
        result: result.and_then(|text| serde_json::from_str(&text).ok()),
        error: row.get(11)?,
        started_at: row.get(12)?,
        finished_at: row.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn plan(target: &str) -> JobPlan {
        let spec = |action: &str| StepSpec {
            adapter: "app".into(),
            action: action.into(),
            adapter_dir: "/adapters/app".into(),
            run: "./run".into(),
            timeout: Duration::from_secs(60),
        };
        JobPlan {
            target: target.into(),
            confirm: false,
            steps: vec![spec("backup"), spec("update")],
            recovery: vec![spec("restore")],
        }
    }

    #[test]
    fn allows_only_one_active_job() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();

        let first = store.create_job(&plan("one"), "test", None).unwrap();
        assert!(matches!(
            store.create_job(&plan("two"), "test", None),
            Err(StoreError::Busy)
        ));

        store.finish_job(first, JobStatus::Succeeded, None).unwrap();
        assert!(store.create_job(&plan("three"), "test", None).is_ok());
    }

    #[test]
    fn recovery_skips_pending_main_steps() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        let id = store.create_job(&plan("one"), "test", None).unwrap();

        let first = store.next_step(id, Phase::Main).unwrap().unwrap();
        let failed = StepOutcome {
            status: StepStatus::Failed,
            exit_code: Some(1),
            result: None,
            error: Some("exit code 1".into()),
        };
        store.finish_step(&first, &failed, "boot").unwrap();
        store
            .enter_recovery(id, JobStatus::Failed, "backup failed")
            .unwrap();

        assert!(store.next_step(id, Phase::Main).unwrap().is_none());
        assert_eq!(
            store
                .next_step(id, Phase::Recovery)
                .unwrap()
                .unwrap()
                .action,
            "restore"
        );
        let statuses: Vec<StepStatus> = store
            .steps(id)
            .unwrap()
            .iter()
            .map(|step| step.status)
            .collect();
        assert_eq!(
            statuses,
            [StepStatus::Failed, StepStatus::Skipped, StepStatus::Pending]
        );
    }

    #[test]
    fn keeps_job_params_and_upgrades_an_old_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(&SCHEMA.replace(",\n    params TEXT", ""))
            .unwrap();

        let store = Store::open(&path).unwrap();
        let params = serde_json::json!({"destination": {"name": "laptop"}});
        let id = store
            .create_job(&plan("one"), "test", Some(&params))
            .unwrap();

        assert_eq!(store.job(id).unwrap().params, Some(params));
        drop(store);
        assert!(Store::open(&path).is_ok());
    }
}
