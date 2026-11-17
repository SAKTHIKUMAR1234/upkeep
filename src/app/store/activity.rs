use rusqlite::params;
use rusqlite::types::ToSql;
use serde::{Deserialize, Serialize};

use super::Db;
use super::page::{Cursor, PAGE_SIZE, Page};

text_enum!(Activity {
    SignIn => "sign_in",
    FailedSignIn => "failed_sign_in",
    Run => "run",
    Stop => "stop",
    ServerAdd => "server_add",
    ServerEdit => "server_edit",
    ServerDelete => "server_delete",
    OperationCreate => "operation_create",
    OperationEdit => "operation_edit",
    OperationDelete => "operation_delete",
    ScriptEdit => "script_edit",
    UserAdd => "user_add",
    UserPassword => "user_password",
    UserDisable => "user_disable",
    UserEnable => "user_enable",
});

/// One line of the activity log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ActivityRecord {
    pub id: i64,
    pub at: i64,
    pub user: String,
    pub action: String,
    pub server: Option<String>,
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub struct ActivityFilter {
    pub user: Option<String>,
    pub server: Option<String>,
    pub action: Option<Activity>,
}

impl Db {
    pub fn record(
        &self,
        at: i64,
        user: &str,
        action: Activity,
        server: Option<&str>,
        detail: &str,
    ) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT INTO activity (at, user_email, action, server_name, detail) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![at, user, action.as_str(), server, detail],
        )?;
        Ok(())
    }

    /// One page of the activity log after `cursor`, newest first.
    pub fn activity(
        &self,
        filter: &ActivityFilter,
        cursor: Cursor,
    ) -> rusqlite::Result<Page<ActivityRecord>> {
        let mut sql = String::from(
            "SELECT id, at, user_email, action, server_name, detail FROM activity WHERE ",
        );
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        let filters = [
            ("user_email", filter.user.clone()),
            ("server_name", filter.server.clone()),
            (
                "action",
                filter.action.map(|action| action.as_str().to_owned()),
            ),
        ];
        for (column, value) in filters {
            if let Some(value) = value {
                sql.push_str(&format!("{column} = ? AND "));
                values.push(Box::new(value));
            }
        }
        sql.push_str(&format!(
            "(at, id) < (?, ?) ORDER BY at DESC, id DESC LIMIT {}",
            PAGE_SIZE + 1
        ));
        values.push(Box::new(cursor.at));
        values.push(Box::new(cursor.id));

        let connection = self.lock();
        let mut statement = connection.prepare(&sql)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                Ok(ActivityRecord {
                    id: row.get(0)?,
                    at: row.get(1)?,
                    user: row.get(2)?,
                    action: row.get(3)?,
                    server: row.get(4)?,
                    detail: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page::from_rows(rows, |entry| Cursor {
            at: entry.at,
            id: entry.id,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::database;
    use super::*;

    #[test]
    fn activity_pages_newest_first_and_filters() {
        let (_dir, db) = database();
        for index in 0..60 {
            let user = if index % 2 == 0 { "a@x.com" } else { "b@x.com" };
            db.record(
                100,
                user,
                Activity::Run,
                Some("Replica"),
                &format!("run #{index}"),
            )
            .unwrap();
        }
        db.record(200, "a@x.com", Activity::SignIn, None, "this computer")
            .unwrap();

        let first = db
            .activity(&ActivityFilter::default(), Cursor::start())
            .unwrap();
        assert_eq!(first.items.len(), PAGE_SIZE);
        assert_eq!(first.items[0].action, "sign_in");
        let second = db
            .activity(
                &ActivityFilter::default(),
                Cursor::parse(first.next.as_deref().unwrap()).unwrap(),
            )
            .unwrap();
        assert_eq!(second.items.len(), 11);
        assert!(second.next.is_none());
        assert_eq!(second.items.last().unwrap().detail, "run #0");

        let filter = ActivityFilter {
            user: Some("b@x.com".into()),
            action: Some(Activity::Run),
            server: Some("Replica".into()),
        };
        let page = db.activity(&filter, Cursor::start()).unwrap();
        assert_eq!(page.items.len(), 30);
        assert!(page.items.iter().all(|entry| entry.user == "b@x.com"));
    }
}
