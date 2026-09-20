use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::{AppError, AppResult};

pub struct ResourceRecord {
    pub resource_url: String,
    pub resource_type: String,
    pub local_path: String,
    pub mime_type: Option<String>,
    pub byte_size: u64,
    pub sha256: String,
}

pub struct PackageReplacement<'a> {
    pub node_origin: &'a str,
    pub module_id: &'a str,
    pub module_title: &'a str,
    pub module_root_url: &'a str,
    pub package_id: &'a str,
    pub package_title: &'a str,
    pub root_url: &'a str,
    pub saved_at: &'a str,
    pub byte_size: u64,
    pub resources: &'a [ResourceRecord],
}

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

    pub fn replace_package(&self, package: &PackageReplacement<'_>) -> AppResult<Vec<String>> {
        let byte_size = i64::try_from(package.byte_size)
            .map_err(|_| AppError::Storage("package exceeds SQLite integer size".into()))?;
        let resource_sizes = package
            .resources
            .iter()
            .map(|resource| {
                i64::try_from(resource.byte_size)
                    .map_err(|_| AppError::Storage("resource exceeds SQLite integer size".into()))
            })
            .collect::<AppResult<Vec<_>>>()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?;
        let transaction = connection
            .transaction()
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let previous_paths = {
            let mut statement = transaction
                .prepare(
                    "SELECT DISTINCT local_path FROM resources
                     WHERE node_origin = ?1 AND package_id = ?2",
                )
                .map_err(|error| AppError::Storage(error.to_string()))?;
            statement
                .query_map(params![package.node_origin, package.package_id], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|error| AppError::Storage(error.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| AppError::Storage(error.to_string()))?
        };
        transaction
            .execute(
                "INSERT INTO modules (node_origin, module_id, module_title, module_root_url)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(node_origin, module_id) DO UPDATE SET
                   module_title = excluded.module_title,
                   module_root_url = excluded.module_root_url",
                params![
                    package.node_origin,
                    package.module_id,
                    package.module_title,
                    package.module_root_url
                ],
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        transaction
            .execute(
                "INSERT INTO packages
                   (node_origin, package_id, module_id, package_title, root_url, status, byte_size, saved_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'ready', ?6, ?7)
                 ON CONFLICT(node_origin, package_id) DO UPDATE SET
                   module_id = excluded.module_id,
                   package_title = excluded.package_title,
                   root_url = excluded.root_url,
                   status = 'ready',
                   byte_size = excluded.byte_size,
                   saved_at = excluded.saved_at",
                params![
                    package.node_origin,
                    package.package_id,
                    package.module_id,
                    package.package_title,
                    package.root_url,
                    byte_size,
                    package.saved_at
                ],
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        transaction
            .execute(
                "DELETE FROM resources WHERE node_origin = ?1 AND package_id = ?2",
                params![package.node_origin, package.package_id],
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        for (resource, byte_size) in package.resources.iter().zip(resource_sizes) {
            transaction
                .execute(
                    "INSERT INTO resources
                       (node_origin, package_id, resource_url, resource_type, local_path, mime_type,
                        byte_size, sha256)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        package.node_origin,
                        package.package_id,
                        resource.resource_url,
                        resource.resource_type,
                        resource.local_path,
                        resource.mime_type,
                        byte_size,
                        resource.sha256
                    ],
                )
                .map_err(|error| AppError::Storage(error.to_string()))?;
        }
        transaction
            .execute(
                "DELETE FROM modules
                 WHERE node_origin = ?1
                   AND NOT EXISTS (
                     SELECT 1 FROM packages WHERE packages.node_origin = modules.node_origin
                       AND packages.module_id = modules.module_id
                   )",
                params![package.node_origin],
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut orphaned_paths = Vec::new();
        for path in previous_paths {
            let references: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM resources WHERE local_path = ?1",
                    params![path],
                    |row| row.get(0),
                )
                .map_err(|error| AppError::Storage(error.to_string()))?;
            if references == 0 {
                orphaned_paths.push(path);
            }
        }
        transaction
            .commit()
            .map_err(|error| AppError::Storage(error.to_string()))?;
        Ok(orphaned_paths)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mime_type_for_resource(
        &self,
        node_origin: &str,
        package_id: &str,
        resource_url: &str,
        resource_type: &str,
        sha256: &str,
        byte_size: u64,
    ) -> AppResult<Option<String>> {
        let byte_size = i64::try_from(byte_size)
            .map_err(|_| AppError::Storage("resource exceeds SQLite integer size".into()))?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?;
        let mut statement = connection
            .prepare(
                "SELECT mime_type FROM resources
                 WHERE sha256 = ?5 AND byte_size = ?6 AND mime_type IS NOT NULL
                   AND node_origin = ?1 AND resource_url = ?3 AND resource_type = ?4
                 ORDER BY
                   CASE WHEN package_id = ?2 THEN 0 ELSE 1 END,
                   package_id
                 LIMIT 1",
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut rows = statement
            .query(params![
                node_origin,
                package_id,
                resource_url,
                resource_type,
                sha256,
                byte_size
            ])
            .map_err(|error| AppError::Storage(error.to_string()))?;
        rows.next()
            .map_err(|error| AppError::Storage(error.to_string()))?
            .map(|row| row.get(0))
            .transpose()
            .map_err(|error| AppError::Storage(error.to_string()))
    }

    pub fn unreferenced_paths(&self, paths: &[String]) -> AppResult<Vec<String>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?;
        let mut statement = connection
            .prepare("SELECT EXISTS(SELECT 1 FROM resources WHERE local_path = ?1)")
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut unreferenced = Vec::new();
        for path in paths {
            let referenced: bool = statement
                .query_row(params![path], |row| row.get(0))
                .map_err(|error| AppError::Storage(error.to_string()))?;
            if !referenced {
                unreferenced.push(path.clone());
            }
        }
        Ok(unreferenced)
    }

    pub fn delete_package(&self, node_origin: &str, package_id: &str) -> AppResult<Vec<String>> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?;
        let transaction = connection
            .transaction()
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let local_paths = {
            let mut statement = transaction
                .prepare(
                    "SELECT local_path FROM resources
                     WHERE node_origin = ?1 AND package_id = ?2",
                )
                .map_err(|error| AppError::Storage(error.to_string()))?;
            statement
                .query_map(params![node_origin, package_id], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|error| AppError::Storage(error.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| AppError::Storage(error.to_string()))?
        };
        transaction
            .execute(
                "DELETE FROM packages WHERE node_origin = ?1 AND package_id = ?2",
                params![node_origin, package_id],
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        transaction
            .execute(
                "DELETE FROM modules
                 WHERE node_origin = ?1
                   AND NOT EXISTS (
                     SELECT 1 FROM packages WHERE packages.node_origin = modules.node_origin
                       AND packages.module_id = modules.module_id
                   )",
                params![node_origin],
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut orphaned_paths = Vec::new();
        for path in local_paths {
            let references: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM resources WHERE local_path = ?1",
                    params![path],
                    |row| row.get(0),
                )
                .map_err(|error| AppError::Storage(error.to_string()))?;
            if references == 0 {
                orphaned_paths.push(path);
            }
        }
        transaction
            .commit()
            .map_err(|error| AppError::Storage(error.to_string()))?;
        Ok(orphaned_paths)
    }

    #[cfg(not(mobile))]
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

    #[cfg(any(target_os = "android", test))]
    pub fn package_module_info(
        &self,
        node_origin: &str,
        package_id: &str,
    ) -> AppResult<(String, String, String, String)> {
        self.connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?
            .query_row(
                "SELECT packages.root_url, modules.module_id, modules.module_title,
                        modules.module_root_url
                 FROM packages
                 JOIN modules ON modules.node_origin = packages.node_origin
                   AND modules.module_id = packages.module_id
                 WHERE packages.node_origin = ?1 AND packages.package_id = ?2
                   AND packages.status = 'ready'",
                params![node_origin, package_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|error| AppError::Storage(error.to_string()))
    }

    #[cfg(any(target_os = "android", test))]
    pub fn module_identity(
        &self,
        node_origin: &str,
        module_id: &str,
    ) -> AppResult<(String, String)> {
        self.connection
            .lock()
            .map_err(|_| AppError::Internal("блокировка каталога повреждена".into()))?
            .query_row(
                "SELECT module_title, module_root_url FROM modules
                 WHERE node_origin = ?1 AND module_id = ?2",
                params![node_origin, module_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
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
                  CREATE INDEX IF NOT EXISTS resources_local_path_idx ON resources(local_path);
                  CREATE INDEX IF NOT EXISTS resources_sha256_idx ON resources(sha256);
                  PRAGMA user_version = 1;
                  COMMIT;",
            )
            .map_err(|error| AppError::Storage(error.to_string()))?;
    }
    connection
        .execute_batch(
            "CREATE INDEX IF NOT EXISTS resources_local_path_idx ON resources(local_path);
             CREATE INDEX IF NOT EXISTS resources_sha256_idx ON resources(sha256);",
        )
        .map_err(|error| AppError::Storage(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::{Library, PackageReplacement, ResourceRecord, migrate};

    fn resource(url: &str, path: &str, sha256: &str) -> ResourceRecord {
        ResourceRecord {
            resource_url: url.into(),
            resource_type: "binary".into(),
            local_path: path.into(),
            mime_type: Some("video/mp4".into()),
            byte_size: 42,
            sha256: sha256.into(),
        }
    }

    fn publish(
        library: &Library,
        package_id: &str,
        package_title: &str,
        root_url: &str,
        resources: &[ResourceRecord],
    ) -> crate::error::AppResult<Vec<String>> {
        library.replace_package(&PackageReplacement {
            node_origin: "https://node.example",
            module_id: "video",
            module_title: "Video",
            module_root_url: "/video",
            package_id,
            package_title,
            root_url,
            saved_at: "1",
            byte_size: resources.iter().map(|resource| resource.byte_size).sum(),
            resources,
        })
    }

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
        let connection = Connection::open(directory.path().join("library.sqlite3")).unwrap();
        migrate(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO modules VALUES (?1, ?2, ?3, ?4)",
                params!["https://node.example", "video", "Video", "/video"],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO packages VALUES (?1, ?2, ?3, ?4, ?5, 'downloading', 0, ?6)",
                params![
                    "https://node.example",
                    "video_1",
                    "video",
                    "Example",
                    "/video?package_id=video_1",
                    "1"
                ],
            )
            .unwrap();
        drop(connection);

        let library = Library::open(directory.path()).unwrap();
        let modules = library.modules().unwrap();
        assert_eq!("failed", modules[0].packages[0].status);
    }

    #[test]
    fn deletes_package_and_empty_module() {
        let directory = tempfile::tempdir().unwrap();
        let library = Library::open(directory.path()).unwrap();
        publish(&library, "video_1", "Example", "/video", &[]).unwrap();

        let orphaned = library
            .delete_package("https://node.example", "video_1")
            .unwrap();

        assert!(orphaned.is_empty());
        assert!(library.modules().unwrap().is_empty());
    }

    #[test]
    fn returns_module_identity_for_ready_package() {
        let directory = tempfile::tempdir().unwrap();
        let library = Library::open(directory.path()).unwrap();
        publish(
            &library,
            "video_1",
            "Example",
            "/video?package_id=video_1",
            &[],
        )
        .unwrap();

        assert_eq!(
            library
                .package_module_info("https://node.example", "video_1")
                .unwrap(),
            (
                "/video?package_id=video_1".into(),
                "video".into(),
                "Video".into(),
                "/video".into(),
            )
        );
        assert_eq!(
            library
                .module_identity("https://node.example", "video")
                .unwrap(),
            ("Video".into(), "/video".into())
        );
    }

    #[test]
    fn atomically_replaces_resources_and_reports_orphans() {
        let directory = tempfile::tempdir().unwrap();
        let library = Library::open(directory.path()).unwrap();
        let old = resource("/old", "/objects/old", "old");
        publish(&library, "video_1", "Old", "/old-root", &[old]).unwrap();

        let new = resource("/new", "/objects/new", "new");
        let orphaned = publish(&library, "video_1", "New", "/new-root", &[new]).unwrap();

        assert_eq!(orphaned, vec!["/objects/old"]);
        assert!(
            library
                .resource("https://node.example", "video_1", "/old")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            library
                .resource("https://node.example", "video_1", "/new")
                .unwrap()
                .unwrap()
                .local_path,
            "/objects/new"
        );
        let modules = library.modules().unwrap();
        assert_eq!(modules[0].packages[0].package_title, "New");
        assert_eq!(modules[0].packages[0].root_url, "/new-root");
        assert_eq!(modules[0].packages[0].status, "ready");
    }

    #[test]
    fn failed_replacement_preserves_ready_package() {
        let directory = tempfile::tempdir().unwrap();
        let library = Library::open(directory.path()).unwrap();
        let old = resource("/old", "/objects/old", "old");
        publish(&library, "video_1", "Old", "/old-root", &[old]).unwrap();
        let invalid = [
            resource("/duplicate", "/objects/new-a", "new-a"),
            resource("/duplicate", "/objects/new-b", "new-b"),
        ];

        assert!(publish(&library, "video_1", "New", "/new-root", &invalid).is_err());

        let modules = library.modules().unwrap();
        assert_eq!(modules[0].packages[0].package_title, "Old");
        assert_eq!(modules[0].packages[0].root_url, "/old-root");
        assert_eq!(modules[0].packages[0].status, "ready");
        assert_eq!(
            library
                .resource("https://node.example", "video_1", "/old")
                .unwrap()
                .unwrap()
                .local_path,
            "/objects/old"
        );
    }

    #[test]
    fn shared_object_is_not_orphaned() {
        let directory = tempfile::tempdir().unwrap();
        let library = Library::open(directory.path()).unwrap();
        publish(
            &library,
            "video_1",
            "One",
            "/one",
            &[resource("/one", "/objects/shared", "shared")],
        )
        .unwrap();
        publish(
            &library,
            "video_2",
            "Two",
            "/two",
            &[resource("/two", "/objects/shared", "shared")],
        )
        .unwrap();

        let orphaned = publish(
            &library,
            "video_1",
            "One updated",
            "/one",
            &[resource("/new", "/objects/new", "new")],
        )
        .unwrap();

        assert!(orphaned.is_empty());
        assert_eq!(
            library
                .resource("https://node.example", "video_2", "/two")
                .unwrap()
                .unwrap()
                .local_path,
            "/objects/shared"
        );
    }

    #[test]
    fn deterministically_prefers_matching_resource_mime_type() {
        let directory = tempfile::tempdir().unwrap();
        let library = Library::open(directory.path()).unwrap();
        let mut fallback = resource("/target", "/objects/shared", "shared");
        fallback.mime_type = Some("video/fallback".into());
        publish(&library, "a_fallback", "Fallback", "/fallback", &[fallback]).unwrap();
        let mut exact = resource("/target", "/objects/shared", "shared");
        exact.mime_type = Some("video/exact".into());
        publish(&library, "z_exact", "Exact", "/target", &[exact]).unwrap();

        assert_eq!(
            library
                .mime_type_for_resource(
                    "https://node.example",
                    "z_exact",
                    "/target",
                    "binary",
                    "shared",
                    42,
                )
                .unwrap()
                .as_deref(),
            Some("video/exact")
        );
        assert_eq!(
            library
                .mime_type_for_resource(
                    "https://node.example",
                    "new_package",
                    "/target",
                    "binary",
                    "shared",
                    42,
                )
                .unwrap()
                .as_deref(),
            Some("video/fallback")
        );
        assert!(
            library
                .mime_type_for_resource(
                    "https://node.example",
                    "new_package",
                    "/target",
                    "css",
                    "shared",
                    42,
                )
                .unwrap()
                .is_none()
        );
    }
}
