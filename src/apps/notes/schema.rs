//! The per-app SQLite schema — harvested from notes-r2's `do_notes/schema.rs`
//! (itself the SQLite translation of metalcraft-notes' Postgres migrations).
//!
//! `owner_user_id` is dropped: a pod has one user, so ownership is implicit.
//! FTS5 + triggers replace Postgres `tsvector`/GIN. Statements are idempotent
//! (`IF NOT EXISTS`) and applied one at a time on boot; new schema ships as
//! extra entries and applies lazily.

use sqlx::SqlitePool;

use super::palette;
use super::util::{now_iso, uuid};
use super::{NotesError, NotesResult};

/// Idempotent DDL, one complete statement per entry (trigger bodies contain
/// their own `;`, so they can't be split from a single blob).
pub const STATEMENTS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS notes (
       id               TEXT PRIMARY KEY,
       title            TEXT NOT NULL,
       slug             TEXT NOT NULL UNIQUE,
       body             TEXT NOT NULL DEFAULT '',
       is_favorite      INTEGER NOT NULL DEFAULT 0,
       public_token     TEXT,
       version          INTEGER NOT NULL DEFAULT 1,
       created_at       TEXT NOT NULL,
       updated_at       TEXT NOT NULL,
       last_accessed_at TEXT NOT NULL
     )",
    "CREATE INDEX IF NOT EXISTS idx_notes_updated  ON notes (updated_at DESC)",
    "CREATE INDEX IF NOT EXISTS idx_notes_accessed ON notes (last_accessed_at DESC)",
    "CREATE TABLE IF NOT EXISTS categories (
       id         TEXT PRIMARY KEY,
       name       TEXT NOT NULL,
       color      TEXT NOT NULL UNIQUE,
       created_at TEXT NOT NULL
     )",
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_categories_name ON categories (name COLLATE NOCASE)",
    "CREATE TABLE IF NOT EXISTS note_categories (
       note_id     TEXT NOT NULL REFERENCES notes(id)      ON DELETE CASCADE,
       category_id TEXT NOT NULL REFERENCES categories(id) ON DELETE CASCADE,
       PRIMARY KEY (note_id, category_id)
     )",
    "CREATE INDEX IF NOT EXISTS idx_note_categories_category ON note_categories (category_id)",
    "CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(title, body, content='notes', content_rowid='rowid')",
    "CREATE TRIGGER IF NOT EXISTS notes_ai AFTER INSERT ON notes BEGIN
       INSERT INTO notes_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body);
     END",
    "CREATE TRIGGER IF NOT EXISTS notes_ad AFTER DELETE ON notes BEGIN
       INSERT INTO notes_fts(notes_fts, rowid, title, body) VALUES('delete', old.rowid, old.title, old.body);
     END",
    "CREATE TRIGGER IF NOT EXISTS notes_au AFTER UPDATE ON notes BEGIN
       INSERT INTO notes_fts(notes_fts, rowid, title, body) VALUES('delete', old.rowid, old.title, old.body);
       INSERT INTO notes_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body);
     END",
    "CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
];

/// Apply the schema (idempotent).
pub async fn apply(pool: &SqlitePool) -> NotesResult<()> {
    for stmt in STATEMENTS {
        sqlx::query(stmt)
            .execute(pool)
            .await
            .map_err(NotesError::from)?;
    }
    Ok(())
}

/// Seed the three default categories (home / work / personal) exactly once per
/// pod, guarded by a `meta` marker so it never re-runs (and never re-adds after
/// the user deletes them). Port of metalcraft-notes' `seed_default_categories`.
pub async fn seed_defaults_once(pool: &SqlitePool) -> NotesResult<()> {
    let seeded: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM meta WHERE k = 'seeded'")
        .fetch_one(pool)
        .await?;
    if seeded > 0 {
        return Ok(());
    }
    let mut used: Vec<String> = Vec::new();
    for name in ["home", "work", "personal"] {
        let Some(color) = palette::pick_unused(&used) else { break };
        sqlx::query("INSERT OR IGNORE INTO categories (id, name, color, created_at) VALUES (?, ?, ?, ?)")
            .bind(uuid())
            .bind(name)
            .bind(color)
            .bind(now_iso())
            .execute(pool)
            .await?;
        used.push(color.to_string());
    }
    sqlx::query("INSERT OR REPLACE INTO meta (k, v) VALUES ('seeded', '1')")
        .execute(pool)
        .await?;
    Ok(())
}
