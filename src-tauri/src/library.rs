use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::{AppError, AppResult};

#[derive(Debug, Serialize)]
pub struct SavedPackage {
    node_origin: String,
    package_id: String,
    package_title: String,
    root_url: String,
    status: String,
    byte_size: i64,
    saved_at: String,
}

pub struct StoredResource {
    pub local_path: String,
    pub mime_type: Option<String>,
    pub byte_size: u64,
}

#[derive(Debug, Serialize)]
pub struct SavedModule {
    node_origin: String,
    module_id: String,
    module_title: String,
    module_root_url: String,
    packages: Vec<SavedPackage>,
}

pub struct Library {
    connection: Mutex<Connection>,
}

impl Library {
    pub fn open(data_dir: &Path) -> AppResult<Self> {
        std::fs::create_dir_all(data_dir).map_err(|error| AppError::Storage(error.to_string()))?;
        let connection = Connection::open(data_dir.join("library.sqlite3"))
            .map_err(|error| AppError::Storage(error.to_string()))?;
        migrate(&connection)?;
        connection
            .execute(
                "UPDATE packages SET status = 'failed' WHERE status IN ('pending', 'downloading')",
                [],
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn modules(&self) -> AppResult<Vec<SavedModule>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?;
        let mut modules_statement = connection
            .prepare(
                "SELECT node_origin, module_id, module_title, module_root_url
                 FROM modules
                 ORDER BY module_title COLLATE NOCASE",
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let modules = modules_statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|error| AppError::Storage(error.to_string()))?;

        let mut result = Vec::new();
        for module in modules {
            let (node_origin, module_id, module_title, module_root_url) =
                module.map_err(|error| AppError::Storage(error.to_string()))?;
            let mut package_statement = connection
                .prepare(
                    "SELECT node_origin, package_id, package_title, root_url, status, byte_size, saved_at
                     FROM packages
                     WHERE node_origin = ?1 AND module_id = ?2
                     ORDER BY saved_at DESC",
                )
                .map_err(|error| AppError::Storage(error.to_string()))?;
            let packages = package_statement
                .query_map(params![node_origin, module_id], |row| {
                    Ok(SavedPackage {
                        node_origin: row.get(0)?,
                        package_id: row.get(1)?,
                        package_title: row.get(2)?,
                        root_url: row.get(3)?,
                        status: row.get(4)?,
                        byte_size: row.get(5)?,
                        saved_at: row.get(6)?,
                    })
                })
                .map_err(|error| AppError::Storage(error.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| AppError::Storage(error.to_string()))?;
            result.push(SavedModule {
                node_origin,
                module_id,
                module_title,
                module_root_url,
                packages,
            });
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_package(
        &self,
        node_origin: &str,
        module_id: &str,
        module_title: &str,
        module_root_url: &str,
        package_id: &str,
        package_title: &str,
        root_url: &str,
        saved_at: &str,
    ) -> AppResult<()> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?;
        let transaction = connection
            .transaction()
            .map_err(|error| AppError::Storage(error.to_string()))?;
        transaction
            .execute(
                "INSERT INTO modules (node_origin, module_id, module_title, module_root_url)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(node_origin, module_id) DO UPDATE SET
                   module_title = excluded.module_title,
                   module_root_url = excluded.module_root_url",
                params![node_origin, module_id, module_title, module_root_url],
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        transaction
            .execute(
                "INSERT INTO packages
                   (node_origin, package_id, module_id, package_title, root_url, status, byte_size, saved_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'downloading', 0, ?6)
                 ON CONFLICT(node_origin, package_id) DO UPDATE SET
                   module_id = excluded.module_id,
                   package_title = excluded.package_title,
                   root_url = excluded.root_url,
                   status = 'downloading',
                   byte_size = 0,
                   saved_at = excluded.saved_at",
                params![node_origin, package_id, module_id, package_title, root_url, saved_at],
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        transaction
            .execute(
                "DELETE FROM resources WHERE node_origin = ?1 AND package_id = ?2",
                params![node_origin, package_id],
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        transaction
            .commit()
            .map_err(|error| AppError::Storage(error.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_resource(
        &self,
        node_origin: &str,
        package_id: &str,
        resource_url: &str,
        resource_type: &str,
        local_path: &str,
        mime_type: Option<&str>,
        byte_size: u64,
        sha256: &str,
    ) -> AppResult<()> {
        let byte_size = i64::try_from(byte_size)
            .map_err(|_| AppError::Storage("resource exceeds SQLite integer size".into()))?;
        self.connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?
            .execute(
                "INSERT OR REPLACE INTO resources
                   (node_origin, package_id, resource_url, resource_type, local_path, mime_type,
                    byte_size, sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    node_origin,
                    package_id,
                    resource_url,
                    resource_type,
                    local_path,
                    mime_type,
                    byte_size,
                    sha256
                ],
            )
            .map(|_| ())
            .map_err(|error| AppError::Storage(error.to_string()))
    }

    pub fn finish_package(
        &self,
        node_origin: &str,
        package_id: &str,
        status: &str,
        byte_size: u64,
    ) -> AppResult<()> {
        let byte_size = i64::try_from(byte_size)
            .map_err(|_| AppError::Storage("package exceeds SQLite integer size".into()))?;
        self.connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?
            .execute(
                "UPDATE packages SET status = ?3, byte_size = ?4
                 WHERE node_origin = ?1 AND package_id = ?2",
                params![node_origin, package_id, status, byte_size],
            )
            .map(|_| ())
            .map_err(|error| AppError::Storage(error.to_string()))
    }

    pub fn package_root_url(&self, node_origin: &str, package_id: &str) -> AppResult<String> {
        self.connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?
            .query_row(
                "SELECT root_url FROM packages
                 WHERE node_origin = ?1 AND package_id = ?2 AND status = 'ready'",
                params![node_origin, package_id],
                |row| row.get(0),
            )
            .map_err(|error| AppError::Storage(error.to_string()))
    }

    pub fn module_offline_info(
        &self,
        node_origin: &str,
        module_id: &str,
    ) -> AppResult<(String, Vec<String>)> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?;
        let root_url = connection
            .query_row(
                "SELECT module_root_url FROM modules WHERE node_origin = ?1 AND module_id = ?2",
                params![node_origin, module_id],
                |row| row.get(0),
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut statement = connection
            .prepare(
                "SELECT package_id FROM packages
                 WHERE node_origin = ?1 AND module_id = ?2 AND status = 'ready'
                 ORDER BY saved_at DESC",
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let package_ids = statement
            .query_map(params![node_origin, module_id], |row| row.get(0))
            .map_err(|error| AppError::Storage(error.to_string()))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|error| AppError::Storage(error.to_string()))?;
        if package_ids.is_empty() {
            return Err(AppError::Storage(
                "module has no ready offline packages".into(),
            ));
        }
        Ok((root_url, package_ids))
    }

    pub fn resource(
        &self,
        node_origin: &str,
        package_id: &str,
        resource_url: &str,
    ) -> AppResult<Option<StoredResource>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?;
        let mut statement = connection
            .prepare(
                "SELECT local_path, mime_type, byte_size FROM resources
                 WHERE node_origin = ?1 AND package_id = ?2 AND resource_url = ?3",
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut rows = statement
            .query(params![node_origin, package_id, resource_url])
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let Some(row) = rows
            .next()
            .map_err(|error| AppError::Storage(error.to_string()))?
        else {
            return Ok(None);
        };
        let byte_size: i64 = row
            .get(2)
            .map_err(|error| AppError::Storage(error.to_string()))?;
        Ok(Some(StoredResource {
            local_path: row
                .get(0)
                .map_err(|error| AppError::Storage(error.to_string()))?,
            mime_type: row
                .get(1)
                .map_err(|error| AppError::Storage(error.to_string()))?,
            byte_size: u64::try_from(byte_size)
                .map_err(|_| AppError::Storage("negative resource size".into()))?,
        }))
    }

    pub fn container_resource(
        &self,
        node_origin: &str,
        package_id: &str,
    ) -> AppResult<Option<StoredResource>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?;
        let mut statement = connection
            .prepare(
                "SELECT local_path, mime_type, byte_size FROM resources
                 WHERE node_origin = ?1 AND package_id = ?2 AND resource_type = 'container'
                 LIMIT 1",
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut rows = statement
            .query(params![node_origin, package_id])
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let Some(row) = rows
            .next()
            .map_err(|error| AppError::Storage(error.to_string()))?
        else {
            return Ok(None);
        };
        let byte_size: i64 = row
            .get(2)
            .map_err(|error| AppError::Storage(error.to_string()))?;
        Ok(Some(StoredResource {
            local_path: row
                .get(0)
                .map_err(|error| AppError::Storage(error.to_string()))?,
            mime_type: row
                .get(1)
                .map_err(|error| AppError::Storage(error.to_string()))?,
            byte_size: u64::try_from(byte_size)
                .map_err(|_| AppError::Storage("negative resource size".into()))?,
        }))
    }
}

fn migrate(connection: &Connection) -> AppResult<()> {
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA trusted_schema = OFF;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(|error| AppError::Storage(error.to_string()))?;
    let version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| AppError::Storage(error.to_string()))?;
    if version > 1 {
        return Err(AppError::Storage(format!(
            "library schema {version} is newer than this application supports"
        )));
    }
    if version == 0 {
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS modules (
                   node_origin TEXT NOT NULL,
                   module_id TEXT NOT NULL,
                   module_title TEXT NOT NULL,
                   module_root_url TEXT NOT NULL,
                   PRIMARY KEY (node_origin, module_id)
                 ) STRICT;
                 CREATE TABLE IF NOT EXISTS packages (
                   node_origin TEXT NOT NULL,
                   package_id TEXT NOT NULL,
                   module_id TEXT NOT NULL,
                   package_title TEXT NOT NULL,
                   root_url TEXT NOT NULL,
                   status TEXT NOT NULL CHECK (status IN ('pending', 'downloading', 'ready', 'failed')),
                   byte_size INTEGER NOT NULL DEFAULT 0 CHECK (byte_size >= 0),
                   saved_at TEXT NOT NULL,
                   PRIMARY KEY (node_origin, package_id),
                   FOREIGN KEY (node_origin, module_id) REFERENCES modules(node_origin, module_id)
                 ) STRICT;
                 CREATE INDEX IF NOT EXISTS packages_module_idx
                   ON packages(node_origin, module_id, saved_at DESC);
                 CREATE TABLE IF NOT EXISTS resources (
                   node_origin TEXT NOT NULL,
                   package_id TEXT NOT NULL,
                   resource_url TEXT NOT NULL,
                   resource_type TEXT NOT NULL,
                   local_path TEXT NOT NULL,
                   mime_type TEXT,
                   byte_size INTEGER NOT NULL CHECK (byte_size >= 0),
                   sha256 TEXT NOT NULL,
                   PRIMARY KEY (node_origin, package_id, resource_url),
                   FOREIGN KEY (node_origin, package_id) REFERENCES packages(node_origin, package_id)
                     ON DELETE CASCADE
                  ) STRICT;
                 PRAGMA user_version = 1;
                 COMMIT;",
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{Library, migrate};

    #[test]
    fn creates_versioned_schema() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(1, version);
    }

    #[test]
    fn recovers_interrupted_downloads_on_open() {
        let directory = tempfile::tempdir().unwrap();
        let library = Library::open(directory.path()).unwrap();
        library
            .begin_package(
                "https://node.example",
                "video",
                "Video",
                "/video",
                "video_1",
                "Example",
                "/video?package_id=video_1",
                "1",
            )
            .unwrap();
        drop(library);

        let library = Library::open(directory.path()).unwrap();
        let modules = library.modules().unwrap();
        assert_eq!("failed", modules[0].packages[0].status);
    }
}
