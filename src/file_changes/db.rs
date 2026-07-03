//! Local aggregation of per-file change frequency at `~/.autter/internal/file-changes-db`.
//!
//! Rows track how often each file is touched during checkpoints. The `synced` flag
//! marks rows pending upload to the org database when the user is logged in.

use crate::error::AutterError;
use rusqlite::{Connection, params};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const SCHEMA_VERSION: usize = 1;

const MIGRATIONS: &[&str] = &[r#"
    CREATE TABLE IF NOT EXISTS file_change_counts (
        repo_url        TEXT NOT NULL,
        file_path       TEXT NOT NULL,
        change_count    INTEGER NOT NULL DEFAULT 0,
        lines_added     INTEGER NOT NULL DEFAULT 0,
        lines_deleted   INTEGER NOT NULL DEFAULT 0,
        last_changed_at INTEGER NOT NULL,
        synced          INTEGER NOT NULL DEFAULT 0,
        attempts        INTEGER NOT NULL DEFAULT 0,
        last_sync_error TEXT,
        last_sync_at    INTEGER,
        next_retry_at   INTEGER NOT NULL DEFAULT 0,
        created_at      INTEGER NOT NULL,
        updated_at      INTEGER NOT NULL,
        PRIMARY KEY (repo_url, file_path)
    );

    CREATE INDEX IF NOT EXISTS idx_file_change_counts_repo
        ON file_change_counts (repo_url, change_count DESC);

    CREATE INDEX IF NOT EXISTS idx_file_change_counts_pending
        ON file_change_counts (synced, next_retry_at) WHERE synced = 0;
    "#];

static FILE_CHANGES_DB: OnceLock<Mutex<FileChangesDatabase>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChangeRow {
    pub repo_url: String,
    pub file_path: String,
    pub change_count: u64,
    pub lines_added: u64,
    pub lines_deleted: u64,
    pub last_changed_at: u64,
}

#[derive(Debug, Clone)]
pub struct PendingFileChangeRow {
    pub repo_url: String,
    pub file_path: String,
    pub change_count: u64,
    pub lines_added: u64,
    pub lines_deleted: u64,
    pub last_changed_at: u64,
    pub attempts: u32,
}

pub struct FileChangesDatabase {
    conn: Connection,
}

impl FileChangesDatabase {
    pub fn global() -> Result<&'static Mutex<FileChangesDatabase>, AutterError> {
        let db_mutex = FILE_CHANGES_DB.get_or_init(|| match Self::new() {
            Ok(db) => Mutex::new(db),
            Err(e) => {
                eprintln!("[Error] Failed to initialize file-changes database: {}", e);
                let temp_path = std::env::temp_dir().join("autter-file-changes-db-failed");
                let conn = Connection::open(&temp_path).expect("Failed to create temp DB");
                Mutex::new(FileChangesDatabase { conn })
            }
        });
        Ok(db_mutex)
    }

    pub fn open_at_path(path: &std::path::Path) -> Result<Self, AutterError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA cache_size=-2000;
            PRAGMA temp_store=MEMORY;
            "#,
        )?;
        let mut db = Self { conn };
        db.initialize_schema()?;
        Ok(db)
    }

    fn new() -> Result<Self, AutterError> {
        let db_path = Self::database_path()?;
        Self::open_at_path(&db_path)
    }

    fn database_path() -> Result<PathBuf, AutterError> {
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(test_path) = std::env::var("AUTTER_TEST_FILE_CHANGES_DB_PATH") {
            return Ok(PathBuf::from(test_path));
        }

        let home = dirs::home_dir().ok_or_else(|| {
            AutterError::Generic("Could not determine home directory".to_string())
        })?;
        Ok(home
            .join(".autter")
            .join("internal")
            .join("file-changes-db"))
    }

    fn initialize_schema(&mut self) -> Result<(), AutterError> {
        let version_check: Result<usize, _> = self.conn.query_row(
            "SELECT value FROM schema_metadata WHERE key = 'version'",
            [],
            |row| {
                let version_str: String = row.get(0)?;
                version_str
                    .parse::<usize>()
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            },
        );

        if let Ok(current_version) = version_check {
            if current_version == SCHEMA_VERSION {
                return Ok(());
            }
            if current_version > SCHEMA_VERSION {
                return Err(AutterError::Generic(format!(
                    "File-changes database schema version {} is newer than supported version {}. \
                     Please upgrade autter to the latest version.",
                    current_version, SCHEMA_VERSION
                )));
            }
        }

        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            "#,
        )?;

        let current_version: usize = self
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| {
                    let version_str: String = row.get(0)?;
                    version_str
                        .parse::<usize>()
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
                },
            )
            .unwrap_or(0);

        for target_version in current_version..SCHEMA_VERSION {
            self.apply_migration(target_version)?;
            self.conn.execute(
                r#"
                INSERT INTO schema_metadata (key, value)
                VALUES ('version', ?1)
                ON CONFLICT(key) DO UPDATE SET
                    value = excluded.value
                WHERE CAST(schema_metadata.value AS INTEGER) < CAST(excluded.value AS INTEGER)
                "#,
                params![(target_version + 1).to_string()],
            )?;
        }

        Ok(())
    }

    fn apply_migration(&mut self, from_version: usize) -> Result<(), AutterError> {
        if from_version >= MIGRATIONS.len() {
            return Err(AutterError::Generic(format!(
                "No migration defined for version {} -> {}",
                from_version,
                from_version + 1
            )));
        }

        let tx = self.conn.transaction()?;
        tx.execute_batch(MIGRATIONS[from_version])?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_change(
        &mut self,
        repo_url: &str,
        file_path: &str,
        lines_added: u32,
        lines_deleted: u32,
        changed_at: u64,
    ) -> Result<(), AutterError> {
        let now = changed_at;
        self.conn.execute(
            r#"
            INSERT INTO file_change_counts (
                repo_url, file_path, change_count, lines_added, lines_deleted,
                last_changed_at, synced, created_at, updated_at
            ) VALUES (?1, ?2, 1, ?3, ?4, ?5, 0, ?6, ?6)
            ON CONFLICT(repo_url, file_path) DO UPDATE SET
                change_count = change_count + 1,
                lines_added = lines_added + excluded.lines_added,
                lines_deleted = lines_deleted + excluded.lines_deleted,
                last_changed_at = excluded.last_changed_at,
                synced = 0,
                updated_at = excluded.updated_at
            "#,
            params![
                repo_url,
                file_path,
                lines_added,
                lines_deleted,
                changed_at,
                now
            ],
        )?;
        Ok(())
    }

    pub fn top_files(
        &self,
        repo_url: &str,
        limit: usize,
    ) -> Result<Vec<FileChangeRow>, AutterError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT repo_url, file_path, change_count, lines_added, lines_deleted, last_changed_at
            FROM file_change_counts
            WHERE repo_url = ?1
            ORDER BY change_count DESC, lines_added DESC, file_path ASC
            LIMIT ?2
            "#,
        )?;

        let rows = stmt.query_map(params![repo_url, limit as i64], |row| {
            Ok(FileChangeRow {
                repo_url: row.get(0)?,
                file_path: row.get(1)?,
                change_count: row.get::<_, i64>(2)? as u64,
                lines_added: row.get::<_, i64>(3)? as u64,
                lines_deleted: row.get::<_, i64>(4)? as u64,
                last_changed_at: row.get::<_, i64>(5)? as u64,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub fn dequeue_pending(
        &mut self,
        limit: usize,
    ) -> Result<Vec<PendingFileChangeRow>, AutterError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let tx = self.conn.transaction()?;
        let mut stmt = tx.prepare(
            r#"
            SELECT repo_url, file_path, change_count, lines_added, lines_deleted,
                   last_changed_at, attempts
            FROM file_change_counts
            WHERE synced = 0 AND next_retry_at <= ?1
            ORDER BY updated_at ASC
            LIMIT ?2
            "#,
        )?;

        let rows: Vec<PendingFileChangeRow> = stmt
            .query_map(params![now, limit as i64], |row| {
                Ok(PendingFileChangeRow {
                    repo_url: row.get(0)?,
                    file_path: row.get(1)?,
                    change_count: row.get::<_, i64>(2)? as u64,
                    lines_added: row.get::<_, i64>(3)? as u64,
                    lines_deleted: row.get::<_, i64>(4)? as u64,
                    last_changed_at: row.get::<_, i64>(5)? as u64,
                    attempts: row.get::<_, i64>(6)? as u32,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        drop(stmt);

        for row in &rows {
            tx.execute(
                "UPDATE file_change_counts SET attempts = attempts + 1 WHERE repo_url = ?1 AND file_path = ?2",
                params![row.repo_url, row.file_path],
            )?;
        }

        tx.commit()?;
        Ok(rows)
    }

    pub fn mark_synced(
        &mut self,
        repo_url: &str,
        file_paths: &[String],
    ) -> Result<(), AutterError> {
        if file_paths.is_empty() {
            return Ok(());
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let tx = self.conn.transaction()?;
        for file_path in file_paths {
            tx.execute(
                r#"
                UPDATE file_change_counts
                SET synced = 1, last_sync_at = ?3, last_sync_error = NULL, next_retry_at = 0
                WHERE repo_url = ?1 AND file_path = ?2
                "#,
                params![repo_url, file_path, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_failed(
        &mut self,
        repo_url: &str,
        file_paths: &[String],
        error: &str,
        retry_delay_secs: u64,
    ) -> Result<(), AutterError> {
        if file_paths.is_empty() {
            return Ok(());
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let next_retry = now + retry_delay_secs as i64;

        let tx = self.conn.transaction()?;
        for file_path in file_paths {
            tx.execute(
                r#"
                UPDATE file_change_counts
                SET last_sync_error = ?3, next_retry_at = ?4
                WHERE repo_url = ?1 AND file_path = ?2
                "#,
                params![repo_url, file_path, error, next_retry],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_db() -> (FileChangesDatabase, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("file-changes.db");
        let db = FileChangesDatabase::open_at_path(&db_path).unwrap();
        (db, temp_dir)
    }

    #[test]
    fn test_record_and_top_files() {
        let (mut db, _temp) = create_test_db();
        db.record_change("https://github.com/user/repo", "src/a.rs", 10, 2, 1000)
            .unwrap();
        db.record_change("https://github.com/user/repo", "src/a.rs", 5, 1, 1001)
            .unwrap();
        db.record_change("https://github.com/user/repo", "src/b.rs", 3, 0, 1002)
            .unwrap();

        let top = db.top_files("https://github.com/user/repo", 10).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].file_path, "src/a.rs");
        assert_eq!(top[0].change_count, 2);
        assert_eq!(top[0].lines_added, 15);
        assert_eq!(top[0].lines_deleted, 3);
        assert_eq!(top[1].file_path, "src/b.rs");
        assert_eq!(top[1].change_count, 1);
    }

    #[test]
    fn test_sync_queue_roundtrip() {
        let (mut db, _temp) = create_test_db();
        db.record_change("https://github.com/user/repo", "lib.rs", 1, 0, 2000)
            .unwrap();

        let pending = db.dequeue_pending(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].file_path, "lib.rs");
        assert_eq!(pending[0].attempts, 1);

        db.mark_synced("https://github.com/user/repo", &["lib.rs".to_string()])
            .unwrap();

        let pending = db.dequeue_pending(10).unwrap();
        assert!(pending.is_empty());
    }
}
