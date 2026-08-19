//! [`SqliteStore`] — the per-app structured-state tier.
//!
//! One SQLite database file per app under `<data>/apps/<id>/<id>.db`, on the
//! pod's block volume (a real POSIX filesystem — required for WAL). This is the
//! "hot/structured" tier: rows, relationships, indexes, and FTS. Large binary
//! blobs go to [`super::BlobStore`], never here.
//!
//! The pool is opened **lazily** (`connect_lazy_with`) and capped at a **single
//! connection**: a managed pod has one user, so serializing writes behind one
//! connection (plus WAL for readers and a busy-timeout) is the simplest way to
//! avoid `SQLITE_BUSY`/"database is locked" under concurrent axum handlers.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

use super::AppResult;

/// A handle to one app's SQLite database.
#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if absent) the database at `db_path` in WAL mode. Cheap and
    /// synchronous — the connection is established lazily on first query.
    pub fn open(db_path: &Path) -> AppResult<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        // One connection = one writer. WAL still allows concurrent readers.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(options);
        Ok(Self { pool })
    }

    /// The underlying pool, for the app's queries.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Apply idempotent DDL, one complete statement per call. Statements should
    /// use `IF NOT EXISTS` so this is a no-op after first boot and new schema
    /// ships as additional entries (the notes-r2 lazy-migration pattern).
    pub async fn apply_schema(&self, statements: &[&str]) -> AppResult<()> {
        for stmt in statements {
            sqlx::query(stmt).execute(&self.pool).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn opens_applies_schema_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("test.db")).unwrap();
        store
            .apply_schema(&[
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n INTEGER NOT NULL)",
                // Idempotent: re-applying must not error.
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n INTEGER NOT NULL)",
            ])
            .await
            .unwrap();

        sqlx::query("INSERT INTO t (id, n) VALUES ('a', 1)")
            .execute(store.pool())
            .await
            .unwrap();

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM t")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}
