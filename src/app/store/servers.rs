use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;

use super::Db;

/// A server the app reaches over SSH. Only the path of its private key is stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Server {
    pub id: i64,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub ssh_user: String,
    pub key_path: String,
    /// The result of the last SSH contact; `None` before the first one.
    pub reachable: Option<bool>,
    pub created_at: i64,
}

/// The fields a user sets on a server, already validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerFields {
    pub name: String,
    pub address: String,
    pub port: u16,
    pub ssh_user: String,
    pub key_path: String,
}

/// An operation folder as last seen on the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CachedOperation {
    pub folder: String,
    /// The name given when the app created it.
    pub title: Option<String>,
    pub has_check: bool,
}

impl Server {
    pub fn fields(&self) -> ServerFields {
        ServerFields {
            name: self.name.clone(),
            address: self.address.clone(),
            port: self.port,
            ssh_user: self.ssh_user.clone(),
            key_path: self.key_path.clone(),
        }
    }
}

const SERVER_COLUMNS: &str = "id, name, address, port, ssh_user, key_path, reachable, created_at";

fn server_row(row: &Row) -> rusqlite::Result<Server> {
    Ok(Server {
        id: row.get(0)?,
        name: row.get(1)?,
        address: row.get(2)?,
        port: row.get(3)?,
        ssh_user: row.get(4)?,
        key_path: row.get(5)?,
        reachable: row.get(6)?,
        created_at: row.get(7)?,
    })
}

impl Db {
    pub fn add_server(&self, fields: &ServerFields, now: i64) -> rusqlite::Result<i64> {
        let connection = self.lock();
        connection.execute(
            "INSERT INTO servers (name, address, port, ssh_user, key_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                fields.name,
                fields.address,
                fields.port,
                fields.ssh_user,
                fields.key_path,
                now
            ],
        )?;
        Ok(connection.last_insert_rowid())
    }

    /// Changes a server; a new address or user clears its reachable state.
    pub fn update_server(&self, id: i64, fields: &ServerFields) -> rusqlite::Result<bool> {
        let changed = self.lock().execute(
            "UPDATE servers SET name = ?2, address = ?3, port = ?4, ssh_user = ?5, key_path = ?6, \
             reachable = NULL WHERE id = ?1",
            params![
                id,
                fields.name,
                fields.address,
                fields.port,
                fields.ssh_user,
                fields.key_path
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn delete_server(&self, id: i64) -> rusqlite::Result<bool> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM operations WHERE server_id = ?1", [id])?;
        let deleted = transaction.execute("DELETE FROM servers WHERE id = ?1", [id])?;
        transaction.commit()?;
        Ok(deleted == 1)
    }

    pub fn server(&self, id: i64) -> rusqlite::Result<Option<Server>> {
        self.lock()
            .query_row(
                &format!("SELECT {SERVER_COLUMNS} FROM servers WHERE id = ?1"),
                [id],
                server_row,
            )
            .optional()
    }

    pub fn servers(&self) -> rusqlite::Result<Vec<Server>> {
        let connection = self.lock();
        let mut statement = connection.prepare(&format!(
            "SELECT {SERVER_COLUMNS} FROM servers ORDER BY name COLLATE NOCASE"
        ))?;
        statement.query_map([], server_row)?.collect()
    }

    pub fn set_reachable(&self, id: i64, reachable: bool) -> rusqlite::Result<()> {
        self.lock().execute(
            "UPDATE servers SET reachable = ?2 WHERE id = ?1",
            params![id, reachable],
        )?;
        Ok(())
    }

    /// Replaces the cached folder list of a server and keeps the titles of folders that remain.
    pub fn replace_operations(
        &self,
        server_id: i64,
        folders: &[(String, bool)],
    ) -> rusqlite::Result<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let cached: Vec<String> = transaction
            .prepare("SELECT folder FROM operations WHERE server_id = ?1")?
            .query_map([server_id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for gone in cached
            .iter()
            .filter(|folder| !folders.iter().any(|(name, _)| name == *folder))
        {
            transaction.execute(
                "DELETE FROM operations WHERE server_id = ?1 AND folder = ?2",
                params![server_id, gone],
            )?;
        }
        for (folder, has_check) in folders {
            transaction.execute(
                "INSERT INTO operations (server_id, folder, has_check) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (server_id, folder) DO UPDATE SET has_check = ?3",
                params![server_id, folder, has_check],
            )?;
        }
        transaction.commit()
    }

    /// Records one operation, for example right after the app created it.
    pub fn save_operation(
        &self,
        server_id: i64,
        folder: &str,
        title: Option<&str>,
        has_check: bool,
    ) -> rusqlite::Result<()> {
        self.lock().execute(
            "INSERT INTO operations (server_id, folder, title, has_check) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (server_id, folder) DO UPDATE SET title = coalesce(?3, title), \
             has_check = ?4",
            params![server_id, folder, title, has_check],
        )?;
        Ok(())
    }

    pub fn forget_operation(&self, server_id: i64, folder: &str) -> rusqlite::Result<()> {
        self.lock().execute(
            "DELETE FROM operations WHERE server_id = ?1 AND folder = ?2",
            params![server_id, folder],
        )?;
        Ok(())
    }

    pub fn cached_operations(&self, server_id: i64) -> rusqlite::Result<Vec<CachedOperation>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT folder, title, has_check FROM operations WHERE server_id = ?1 ORDER BY folder",
        )?;
        statement
            .query_map([server_id], |row| {
                Ok(CachedOperation {
                    folder: row.get(0)?,
                    title: row.get(1)?,
                    has_check: row.get(2)?,
                })
            })?
            .collect()
    }

    /// Every cached operation folder name, for the history filter.
    pub fn operation_folders(&self) -> rusqlite::Result<Vec<String>> {
        let connection = self.lock();
        let mut statement =
            connection.prepare("SELECT DISTINCT folder FROM operations ORDER BY folder")?;
        statement.query_map([], |row| row.get(0))?.collect()
    }
}

/// Whether an insert or update failed on a unique column.
pub fn is_unique_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}

#[cfg(test)]
mod tests {
    use super::super::tests::database;
    use super::*;

    fn fields(name: &str) -> ServerFields {
        ServerFields {
            name: name.into(),
            address: "10.0.0.1".into(),
            port: 22,
            ssh_user: "ubuntu".into(),
            key_path: "/home/a/.ssh/id_ed25519".into(),
        }
    }

    #[test]
    fn servers_have_unique_names() {
        let (_dir, db) = database();
        let id = db.add_server(&fields("Replica"), 1).unwrap();
        let error = db.add_server(&fields("Replica"), 2).unwrap_err();
        assert!(is_unique_violation(&error));

        db.set_reachable(id, true).unwrap();
        assert_eq!(db.server(id).unwrap().unwrap().reachable, Some(true));
        assert!(db.update_server(id, &fields("Replica 2")).unwrap());
        let server = db.server(id).unwrap().unwrap();
        assert_eq!(
            (server.name.as_str(), server.reachable),
            ("Replica 2", None)
        );
        assert!(db.delete_server(id).unwrap());
        assert!(db.servers().unwrap().is_empty());
    }

    #[test]
    fn operation_cache_follows_the_server_and_keeps_titles() {
        let (_dir, db) = database();
        db.save_operation(1, "disk-cleanup", Some("Disk cleanup"), false)
            .unwrap();
        db.replace_operations(
            1,
            &[("disk-cleanup".into(), true), ("os-update".into(), false)],
        )
        .unwrap();
        let cached = db.cached_operations(1).unwrap();
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].title.as_deref(), Some("Disk cleanup"));
        assert!(cached[0].has_check);

        db.replace_operations(1, &[("os-update".into(), false)])
            .unwrap();
        assert_eq!(db.operation_folders().unwrap(), ["os-update"]);
        db.forget_operation(1, "os-update").unwrap();
        assert!(db.cached_operations(1).unwrap().is_empty());
    }
}
