use rusqlite::types::ToSql;
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::page::{Cursor, PAGE_SIZE, Page};
use super::{Db, StoreError};

text_enum!(RunStatus {
    Running => "running",
    Succeeded => "succeeded",
    Failed => "failed",
    CheckFailed => "check_failed",
    Stopped => "stopped",
    Interrupted => "interrupted",
});

text_enum!(StepStatus {
    Waiting => "waiting",
    Running => "running",
    Ok => "ok",
    Failed => "failed",
    Stopped => "stopped",
});

/// One step of a run: a fixed step of the app, or a `::step` line of the script.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub title: String,
    pub status: StepStatus,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunRecord {
    pub id: i64,
    pub server_id: i64,
    /// The server's name when the run started.
    pub server_name: String,
    pub operation: String,
    /// The operation's name when the run started.
    pub title: String,
    pub user: String,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    /// Why the run failed or stopped, in words.
    pub detail: Option<String>,
    pub steps: Vec<Step>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// History filters. Each one is backed by an index.
#[derive(Clone, Debug, Default)]
pub struct RunFilter {
    pub server_id: Option<i64>,
    pub operation: Option<String>,
    pub user: Option<String>,
    pub status: Option<RunStatus>,
    pub since: Option<i64>,
    pub search: Option<String>,
}

/// The figures on a server's page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServerStats {
    pub runs_since: i64,
    pub last_run: Option<RunRecord>,
}

const RUN_COLUMNS: &str = "id, server_id, server_name, operation, title, user_email, status, \
                           exit_code, detail, steps, started_at, finished_at";

fn run_row(row: &Row) -> rusqlite::Result<Result<RunRecord, StoreError>> {
    let status: String = row.get(6)?;
    let steps: String = row.get(9)?;
    let Some(status) = RunStatus::parse(&status) else {
        return Ok(Err(StoreError::Corrupt(status)));
    };
    let Ok(steps) = serde_json::from_str(&steps) else {
        return Ok(Err(StoreError::Corrupt(steps)));
    };
    Ok(Ok(RunRecord {
        id: row.get(0)?,
        server_id: row.get(1)?,
        server_name: row.get(2)?,
        operation: row.get(3)?,
        title: row.get(4)?,
        user: row.get(5)?,
        status,
        exit_code: row.get(7)?,
        detail: row.get(8)?,
        steps,
        started_at: row.get(10)?,
        finished_at: row.get(11)?,
    }))
}

/// A run being started.
pub struct NewRun<'a> {
    pub server_id: i64,
    pub server_name: &'a str,
    pub operation: &'a str,
    pub title: &'a str,
    pub user: &'a str,
    pub steps: &'a [Step],
    pub now: i64,
}

impl Db {
    /// Stores a new running run; `None` when the server already has one.
    pub fn start_run(&self, run: &NewRun) -> Result<Option<i64>, StoreError> {
        let connection = self.lock();
        let inserted = connection.execute(
            "INSERT INTO runs (server_id, server_name, operation, title, user_email, status, steps, \
             started_at) VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7)",
            params![
                run.server_id,
                run.server_name,
                run.operation,
                run.title,
                run.user,
                steps_json(run.steps),
                run.now
            ],
        );
        match inserted {
            Ok(_) => Ok(Some(connection.last_insert_rowid())),
            Err(error) if super::servers::is_unique_violation(&error) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save_steps(&self, id: i64, steps: &[Step]) -> rusqlite::Result<()> {
        self.lock().execute(
            "UPDATE runs SET steps = ?2 WHERE id = ?1",
            params![id, steps_json(steps)],
        )?;
        Ok(())
    }

    pub fn finish_run(
        &self,
        id: i64,
        status: RunStatus,
        exit_code: Option<i32>,
        detail: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.lock().execute(
            "UPDATE runs SET status = ?2, exit_code = ?3, detail = ?4, finished_at = ?5 \
             WHERE id = ?1",
            params![id, status.as_str(), exit_code, detail, now],
        )?;
        Ok(())
    }

    /// Marks runs that a previous process left running as interrupted; returns how many.
    pub fn interrupt_running_runs(&self) -> rusqlite::Result<usize> {
        self.lock().execute(
            "UPDATE runs SET status = 'interrupted', \
             detail = 'Upkeep stopped while the run was active', \
             finished_at = coalesce(finished_at, started_at) WHERE status = 'running'",
            [],
        )
    }

    pub fn run(&self, id: i64) -> Result<Option<RunRecord>, StoreError> {
        self.one_run(
            &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
            &[&id],
        )
    }

    pub fn active_run(&self, server_id: i64) -> Result<Option<RunRecord>, StoreError> {
        self.one_run(
            &format!("SELECT {RUN_COLUMNS} FROM runs WHERE server_id = ?1 AND status = 'running'"),
            &[&server_id],
        )
    }

    pub fn active_runs(&self) -> Result<Vec<RunRecord>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE status = 'running' ORDER BY id"
        ))?;
        statement.query_map([], run_row)?.map(|row| row?).collect()
    }

    /// The newest run of one operation on one server.
    pub fn last_run(
        &self,
        server_id: i64,
        operation: &str,
    ) -> Result<Option<RunRecord>, StoreError> {
        self.one_run(
            &format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE server_id = ?1 AND operation = ?2 \
                 ORDER BY started_at DESC, id DESC LIMIT 1"
            ),
            &[&server_id, &operation],
        )
    }

    pub fn server_stats(&self, server_id: i64, since: i64) -> Result<ServerStats, StoreError> {
        let runs_since = self.lock().query_row(
            "SELECT count(*) FROM runs WHERE server_id = ?1 AND started_at >= ?2",
            params![server_id, since],
            |row| row.get(0),
        )?;
        let last_run = self.one_run(
            &format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE server_id = ?1 \
                 ORDER BY started_at DESC, id DESC LIMIT 1"
            ),
            &[&server_id],
        )?;
        Ok(ServerStats {
            runs_since,
            last_run,
        })
    }

    /// Runs started since `since`, and how many of them ended in each status.
    pub fn status_counts(&self, since: i64) -> rusqlite::Result<Vec<(String, i64)>> {
        let connection = self.lock();
        let mut statement = connection
            .prepare("SELECT status, count(*) FROM runs WHERE started_at >= ?1 GROUP BY status")?;
        statement
            .query_map([since], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }

    /// One page of history after `cursor`, newest first.
    pub fn runs(&self, filter: &RunFilter, cursor: Cursor) -> Result<Page<RunRecord>, StoreError> {
        let (conditions, mut values) = filter.conditions();
        values.push(Box::new(cursor.at));
        values.push(Box::new(cursor.id));
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE {conditions} (started_at, id) < (?, ?) \
             ORDER BY started_at DESC, id DESC LIMIT {}",
            PAGE_SIZE + 1
        );
        let connection = self.lock();
        let mut statement = connection.prepare(&sql)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(values.iter()), run_row)?
            .map(|row| row?)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Page::from_rows(rows, |run| Cursor {
            at: run.started_at,
            id: run.id,
        }))
    }

    fn one_run(&self, sql: &str, values: &[&dyn ToSql]) -> Result<Option<RunRecord>, StoreError> {
        self.lock()
            .query_row(sql, values, run_row)
            .optional()?
            .transpose()
    }
}

impl RunFilter {
    /// SQL conditions, each followed by `AND`, and their values in order.
    fn conditions(&self) -> (String, Vec<Box<dyn ToSql>>) {
        let mut sql = String::new();
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        let mut add = |condition: &str, value: Box<dyn ToSql>| {
            sql.push_str(condition);
            sql.push_str(" AND ");
            values.push(value);
        };
        if let Some(server_id) = self.server_id {
            add("server_id = ?", Box::new(server_id));
        }
        if let Some(operation) = &self.operation {
            add("operation = ?", Box::new(operation.clone()));
        }
        if let Some(user) = &self.user {
            add("user_email = ?", Box::new(user.clone()));
        }
        if let Some(status) = self.status {
            add("status = ?", Box::new(status.as_str()));
        }
        if let Some(since) = self.since {
            add("started_at >= ?", Box::new(since));
        }
        match self.search.as_deref().map(search_term) {
            Some(Some(SearchTerm::RunId(id))) => add("id = ?", Box::new(id)),
            Some(Some(SearchTerm::Words(query))) => add(
                "id IN (SELECT rowid FROM runs_search WHERE runs_search MATCH ?)",
                Box::new(query),
            ),
            Some(None) => sql.push_str("0 AND "),
            None => {}
        }
        (sql, values)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SearchTerm {
    RunId(i64),
    /// An FTS5 query: every word as a quoted prefix.
    Words(String),
}

/// `#142` or `142` finds a run by id; other text finds runs whose server, operation or user has
/// words that start with each searched word. Text without letters or digits matches nothing.
fn search_term(text: &str) -> Option<SearchTerm> {
    let text = text.trim();
    if let Ok(id) = text.trim_start_matches('#').parse() {
        return Some(SearchTerm::RunId(id));
    }
    let words: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| format!("\"{word}\"*"))
        .collect();
    (!words.is_empty()).then(|| SearchTerm::Words(words.join(" ")))
}

fn steps_json(steps: &[Step]) -> String {
    serde_json::to_string(steps).expect("steps serialize")
}

#[cfg(test)]
mod tests {
    use super::super::tests::database;
    use super::*;

    fn add(db: &Db, server_id: i64, operation: &str, user: &str, at: i64) -> i64 {
        let id = db
            .start_run(&NewRun {
                server_id,
                server_name: &format!("Server {server_id}"),
                operation,
                title: operation,
                user,
                steps: &[],
                now: at,
            })
            .unwrap()
            .unwrap();
        db.finish_run(id, RunStatus::Succeeded, Some(0), None, at + 1)
            .unwrap();
        id
    }

    fn all_pages(db: &Db, filter: &RunFilter) -> Vec<i64> {
        let mut ids = Vec::new();
        let mut cursor = Cursor::start();
        loop {
            let page = db.runs(filter, cursor).unwrap();
            ids.extend(page.items.iter().map(|run| run.id));
            match page.next {
                Some(next) => cursor = Cursor::parse(&next).unwrap(),
                None => return ids,
            }
        }
    }

    #[test]
    fn keyset_pages_visit_every_row_once_even_with_equal_times() {
        let (_dir, db) = database();
        // 130 runs over 3 distinct times, so many share a time across page borders.
        let mut expected: Vec<(i64, i64)> = (0..130)
            .map(|index| {
                let at = 1000 + index % 3;
                (at, add(&db, 1, "os-update", "a@example.com", at))
            })
            .collect();
        expected.sort_by(|left, right| right.cmp(left));
        let expected: Vec<i64> = expected.into_iter().map(|(_, id)| id).collect();

        assert_eq!(all_pages(&db, &RunFilter::default()), expected);
        let first = db.runs(&RunFilter::default(), Cursor::start()).unwrap();
        assert_eq!(first.items.len(), PAGE_SIZE);
    }

    #[test]
    fn filters_and_search_narrow_the_history() {
        let (_dir, db) = database();
        let os = add(&db, 1, "os-update", "a@example.com", 10);
        let bench = add(&db, 2, "hr-bench-update", "dev@example.com", 20);
        let by_server = RunFilter {
            server_id: Some(2),
            ..RunFilter::default()
        };
        assert_eq!(all_pages(&db, &by_server), [bench]);
        let by_user = RunFilter {
            user: Some("a@example.com".into()),
            since: Some(5),
            ..RunFilter::default()
        };
        assert_eq!(all_pages(&db, &by_user), [os]);
        for (search, expected) in [
            ("bench", vec![bench]),
            ("os upd", vec![os]),
            ("dev", vec![bench]),
            (&format!("#{os}"), vec![os]),
            ("server", vec![bench, os]),
            ("!!", vec![]),
        ] {
            let filter = RunFilter {
                search: Some(search.into()),
                ..RunFilter::default()
            };
            assert_eq!(all_pages(&db, &filter), expected, "{search}");
        }
    }

    #[test]
    fn one_running_run_per_server() {
        let (_dir, db) = database();
        let run = |server_id| NewRun {
            server_id,
            server_name: "S",
            operation: "os-update",
            title: "OS update",
            user: "a@example.com",
            steps: &[],
            now: 1,
        };
        let first = db.start_run(&run(1)).unwrap().unwrap();
        assert_eq!(db.start_run(&run(1)).unwrap(), None);
        assert!(db.start_run(&run(2)).unwrap().is_some());
        assert_eq!(db.active_run(1).unwrap().unwrap().id, first);

        assert_eq!(db.interrupt_running_runs().unwrap(), 2);
        let interrupted = db.run(first).unwrap().unwrap();
        assert_eq!(interrupted.status, RunStatus::Interrupted);
        assert!(db.start_run(&run(1)).unwrap().is_some());
    }

    #[test]
    fn search_terms_quote_each_word() {
        assert_eq!(search_term("#12"), Some(SearchTerm::RunId(12)));
        assert_eq!(
            search_term("os \"upd"),
            Some(SearchTerm::Words("\"os\"* \"upd\"*".into()))
        );
        assert_eq!(search_term(" - "), None);
    }
}
