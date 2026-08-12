//! [`NotesStore`] — the data-access layer, ported from metalcraft-notes'
//! `controllers/api.rs` (via notes-r2's `store.rs`) to `sqlx` + SQLite. Every
//! method runs against this pod's own database; the owner is implicit.
//!
//! Concurrency safety carries over unchanged: notes use a `version` integer for
//! optimistic concurrency (a stale `base_version` yields a 409 with the current
//! note so the client merges instead of clobbering); there are no row locks.

use std::collections::{HashMap, HashSet};

use sqlx::SqlitePool;

use serde_json::json;

use super::models::*;
use super::palette::{self, MAX_CATEGORIES};
use super::util::{now_iso, placeholders, slugify, uuid};
use super::{NotesError, NotesResult};
use crate::apps::{AppEventHub, OwnerIdentity};

/// A cloneable handle (the pool is `Arc` inside) bound to the pod owner and the
/// app's shared event hub.
#[derive(Clone)]
pub struct NotesStore {
    pool: SqlitePool,
    owner: OwnerIdentity,
    events: AppEventHub,
}

impl NotesStore {
    pub fn new(pool: SqlitePool, owner: OwnerIdentity, events: AppEventHub) -> Self {
        Self { pool, owner, events }
    }

    /// The app's event hub (for WebSocket subscribers).
    pub fn events(&self) -> &AppEventHub {
        &self.events
    }

    fn owner_id(&self) -> String {
        self.owner.user_id.clone().unwrap_or_else(|| "owner".to_string())
    }

    fn publish_note(&self, view: &NoteView) {
        if let Ok(v) = serde_json::to_value(view) {
            self.events.publish(json!({ "type": "note.upserted", "note": v }));
        }
    }
    fn publish_note_deleted(&self, id: &str) {
        self.events.publish(json!({ "type": "note.deleted", "id": id }));
    }
    fn publish_category(&self, cat: &Category) {
        if let Ok(v) = serde_json::to_value(cat) {
            self.events.publish(json!({ "type": "category.upserted", "category": v }));
        }
    }
    fn publish_category_deleted(&self, id: &str) {
        self.events.publish(json!({ "type": "category.deleted", "id": id }));
    }

    /// Idempotent: apply schema + seed defaults (guarded by a `meta` marker).
    /// Called at the start of each tool invocation; cheap thanks to
    /// `IF NOT EXISTS` and the seed marker.
    pub async fn ensure_ready(&self) -> NotesResult<()> {
        super::schema::apply(&self.pool).await?;
        super::schema::seed_defaults_once(&self.pool).await?;
        Ok(())
    }

    /// `{ sub, email, scopes }` — the pod owner. A pod owner is full-access, so
    /// `scopes` is null (matching the cloud's owner-session shape).
    pub fn whoami(&self) -> serde_json::Value {
        serde_json::json!({
            "sub": self.owner_id(),
            "email": self.owner.email,
            "scopes": serde_json::Value::Null,
        })
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    async fn note_by_slug(&self, slug: &str) -> NotesResult<NoteRow> {
        sqlx::query_as::<_, NoteRow>("SELECT * FROM notes WHERE slug = ?")
            .bind(slug)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| NotesError::not_found("note not found"))
    }

    /// First free slug: `base`, then `base-2`, `base-3`, … (bounded).
    async fn free_slug(&self, base: &str) -> NotesResult<String> {
        for n in 1..=200 {
            let cand = if n == 1 { base.to_string() } else { format!("{base}-{n}") };
            let taken: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM notes WHERE slug = ?)")
                .bind(&cand)
                .fetch_one(&self.pool)
                .await?;
            if taken == 0 {
                return Ok(cand);
            }
        }
        Err(NotesError::conflict("could not allocate a unique slug"))
    }

    async fn categories_for_note(&self, note_id: &str) -> NotesResult<Vec<Category>> {
        let rows = sqlx::query_as::<_, CategoryRow>(
            "SELECT c.id, c.name, c.color, c.created_at
             FROM note_categories nc JOIN categories c ON c.id = nc.category_id
             WHERE nc.note_id = ? ORDER BY c.name",
        )
        .bind(note_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Category::from).collect())
    }

    async fn categories_for_notes(
        &self,
        ids: &[String],
    ) -> NotesResult<HashMap<String, Vec<Category>>> {
        let mut map: HashMap<String, Vec<Category>> = HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let sql = format!(
            "SELECT nc.note_id, c.id, c.name, c.color, c.created_at
             FROM note_categories nc JOIN categories c ON c.id = nc.category_id
             WHERE nc.note_id IN ({}) ORDER BY c.name",
            placeholders(ids.len())
        );
        let mut q = sqlx::query_as::<_, NoteCategoryRow>(&sql);
        for id in ids {
            q = q.bind(id);
        }
        for r in q.fetch_all(&self.pool).await? {
            map.entry(r.note_id).or_default().push(Category {
                id: r.id,
                name: r.name,
                color: r.color,
                created_at: r.created_at,
            });
        }
        Ok(map)
    }

    /// Replace a note's category set with `ids`, rejecting any that don't exist.
    async fn set_note_categories(&self, note_id: &str, ids: &[String]) -> NotesResult<()> {
        let distinct: Vec<String> =
            ids.iter().cloned().collect::<HashSet<_>>().into_iter().collect();
        if !distinct.is_empty() {
            let sql = format!(
                "SELECT COUNT(*) FROM categories WHERE id IN ({})",
                placeholders(distinct.len())
            );
            let mut q = sqlx::query_scalar::<_, i64>(&sql);
            for id in &distinct {
                q = q.bind(id);
            }
            let owned = q.fetch_one(&self.pool).await?;
            if owned as usize != distinct.len() {
                return Err(NotesError::bad_request("one or more categories do not exist"));
            }
        }
        sqlx::query("DELETE FROM note_categories WHERE note_id = ?")
            .bind(note_id)
            .execute(&self.pool)
            .await?;
        for cid in &distinct {
            sqlx::query("INSERT OR IGNORE INTO note_categories (note_id, category_id) VALUES (?, ?)")
                .bind(note_id)
                .bind(cid)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    async fn note_view(&self, note: NoteRow) -> NotesResult<NoteView> {
        let cats = self.categories_for_note(&note.id).await?;
        Ok(NoteView::build(note, &self.owner_id(), cats))
    }

    // ── categories ───────────────────────────────────────────────────────────

    pub async fn list_categories(&self) -> NotesResult<Vec<Category>> {
        let rows = sqlx::query_as::<_, CategoryRow>(
            "SELECT id, name, color, created_at FROM categories ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Category::from).collect())
    }

    pub async fn create_category(&self, name: &str) -> NotesResult<Category> {
        let name = name.trim();
        if name.is_empty() {
            return Err(NotesError::bad_request("name is required"));
        }
        let used: Vec<String> = sqlx::query_scalar("SELECT color FROM categories")
            .fetch_all(&self.pool)
            .await?;
        if used.len() >= MAX_CATEGORIES {
            return Err(NotesError::conflict(format!(
                "category limit reached (max {MAX_CATEGORIES})"
            )));
        }
        let taken: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM categories WHERE name = ? COLLATE NOCASE)")
                .bind(name)
                .fetch_one(&self.pool)
                .await?;
        if taken != 0 {
            return Err(NotesError::conflict(format!("category '{name}' already exists")));
        }
        let color = palette::pick_unused(&used)
            .ok_or_else(|| NotesError::conflict("no palette color available"))?;
        let cat = sqlx::query_as::<_, CategoryRow>(
            "INSERT INTO categories (id, name, color, created_at) VALUES (?, ?, ?, ?)
             RETURNING id, name, color, created_at",
        )
        .bind(uuid())
        .bind(name)
        .bind(color)
        .bind(now_iso())
        .fetch_one(&self.pool)
        .await?;
        let cat = Category::from(cat);
        self.publish_category(&cat);
        Ok(cat)
    }

    // ── notes ────────────────────────────────────────────────────────────────

    pub async fn list_notes(
        &self,
        sort: Option<&str>,
        category: Option<&str>,
    ) -> NotesResult<Vec<NoteSummaryView>> {
        let order = match sort {
            Some("accessed") => "last_accessed_at DESC",
            _ => "updated_at DESC",
        };
        let cols = "id, title, slug, is_favorite, created_at, updated_at, last_accessed_at";
        let rows: Vec<NoteSummaryRow> = if let Some(cat) = category.filter(|c| !c.is_empty()) {
            sqlx::query_as::<_, NoteSummaryRow>(&format!(
                "SELECT {cols} FROM notes
                 WHERE id IN (SELECT note_id FROM note_categories WHERE category_id = ?)
                 ORDER BY {order}"
            ))
            .bind(cat)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, NoteSummaryRow>(&format!("SELECT {cols} FROM notes ORDER BY {order}"))
                .fetch_all(&self.pool)
                .await?
        };
        let ids: Vec<String> = rows.iter().map(|n| n.id.clone()).collect();
        let mut cats = self.categories_for_notes(&ids).await?;
        Ok(rows
            .into_iter()
            .map(|s| {
                let c = cats.remove(&s.id).unwrap_or_default();
                NoteSummaryView::build(s, c)
            })
            .collect())
    }

    pub async fn create_note(
        &self,
        title: Option<&str>,
        body: Option<&str>,
        categories: Option<Vec<String>>,
    ) -> NotesResult<NoteView> {
        let title = title.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Untitled");
        let slug = self.free_slug(&slugify(title)).await?;
        let body_md = body.unwrap_or("");
        let now = now_iso();
        let note = sqlx::query_as::<_, NoteRow>(
            "INSERT INTO notes (id, title, slug, body, created_at, updated_at, last_accessed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING *",
        )
        .bind(uuid())
        .bind(title)
        .bind(&slug)
        .bind(body_md)
        .bind(&now)
        .bind(&now)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        if let Some(ids) = categories {
            self.set_note_categories(&note.id, &ids).await?;
        }
        let view = self.note_view(note).await?;
        self.publish_note(&view);
        Ok(view)
    }

    /// Opening a note bumps `last_accessed_at` only (never version/updated_at —
    /// an open is not an edit).
    pub async fn get_note(&self, slug: &str) -> NotesResult<NoteView> {
        let note = sqlx::query_as::<_, NoteRow>(
            "UPDATE notes SET last_accessed_at = ? WHERE slug = ? RETURNING *",
        )
        .bind(now_iso())
        .bind(slug)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| NotesError::not_found("note not found"))?;
        self.note_view(note).await
    }

    /// Returns `(status, view)`: `(200, updated)` normally, or `(409, current)`
    /// when `base_version` is stale. `title_present`/`body_present` mark which
    /// content fields the caller actually sent (a missing field is untouched;
    /// a categories-only change does not bump the version).
    pub async fn update_note(
        &self,
        slug: &str,
        title: Option<&str>,
        body: Option<&str>,
        base_version: Option<i64>,
        categories: Option<Vec<String>>,
        title_present: bool,
        body_present: bool,
    ) -> NotesResult<(u16, NoteView)> {
        let note = self.note_by_slug(slug).await?;

        if let Some(base) = base_version {
            if base != note.version {
                return Ok((409, self.note_view(note).await?));
            }
        }

        let content_edit = title_present || body_present;
        let note = if content_edit {
            let t = title.map(str::trim).filter(|s| !s.is_empty());
            sqlx::query_as::<_, NoteRow>(
                "UPDATE notes SET
                   title = COALESCE(?, title),
                   body  = COALESCE(?, body),
                   version = version + 1,
                   updated_at = ?
                 WHERE id = ? RETURNING *",
            )
            .bind(t)
            .bind(body)
            .bind(now_iso())
            .bind(&note.id)
            .fetch_one(&self.pool)
            .await?
        } else {
            note
        };

        if let Some(ids) = categories {
            self.set_note_categories(&note.id, &ids).await?;
        }
        let view = self.note_view(note).await?;
        self.publish_note(&view);
        Ok((200, view))
    }

    pub async fn delete_note(&self, slug: &str) -> NotesResult<()> {
        let note = self.note_by_slug(slug).await?;
        sqlx::query("DELETE FROM notes WHERE id = ?")
            .bind(&note.id)
            .execute(&self.pool)
            .await?;
        self.publish_note_deleted(&note.id);
        Ok(())
    }

    // ── public sharing (pod-local; the owner's own shares) ───────────────────

    /// Make a note public (idempotent): set a `public_token` if absent, return it.
    pub async fn share(&self, slug: &str) -> NotesResult<String> {
        let note = self.note_by_slug(slug).await?;
        if let Some(t) = note.public_token {
            return Ok(t);
        }
        let t = super::util::token();
        sqlx::query("UPDATE notes SET public_token = ? WHERE id = ?")
            .bind(&t)
            .bind(&note.id)
            .execute(&self.pool)
            .await?;
        Ok(t)
    }

    /// Revoke a note's public share.
    pub async fn unshare(&self, slug: &str) -> NotesResult<()> {
        let note = self.note_by_slug(slug).await?;
        sqlx::query("UPDATE notes SET public_token = NULL WHERE id = ?")
            .bind(&note.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Resolve a public token → `(title, body)` for the render page.
    pub async fn note_by_token(&self, token: &str) -> NotesResult<(String, String)> {
        let row = sqlx::query_as::<_, NoteRow>(
            "SELECT * FROM notes WHERE public_token = ? AND public_token IS NOT NULL",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| NotesError::not_found("not found"))?;
        Ok((row.title, row.body))
    }

    // ── category update / delete (used by the web UI) ────────────────────────

    pub async fn update_category(
        &self,
        id: &str,
        name: Option<&str>,
        color: Option<&str>,
    ) -> NotesResult<Category> {
        let exists: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM categories WHERE id = ?)")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        if exists == 0 {
            return Err(NotesError::not_found("category not found"));
        }
        let name = name.map(str::trim).filter(|s| !s.is_empty());
        let color = match color.map(str::trim).filter(|s| !s.is_empty()) {
            Some(c) if !palette::is_valid(c) => {
                return Err(NotesError::bad_request("color is not a valid palette color"));
            }
            other => other,
        };
        let res = sqlx::query_as::<_, CategoryRow>(
            "UPDATE categories SET name = COALESCE(?, name), color = COALESCE(?, color)
             WHERE id = ? RETURNING id, name, color, created_at",
        )
        .bind(name)
        .bind(color)
        .bind(id)
        .fetch_optional(&self.pool)
        .await;
        match res {
            Ok(Some(cat)) => {
                let cat = Category::from(cat);
                self.publish_category(&cat);
                Ok(cat)
            }
            Ok(None) => Err(NotesError::new(500, "update failed")),
            Err(e) if e.to_string().to_uppercase().contains("UNIQUE") => {
                Err(NotesError::conflict("that name or color is already in use"))
            }
            Err(e) => Err(NotesError::from(e)),
        }
    }

    pub async fn delete_category(&self, id: &str) -> NotesResult<()> {
        let exists: i64 =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM categories WHERE id = ?)")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        if exists == 0 {
            return Err(NotesError::not_found("category not found"));
        }
        // note_categories rows cascade away via the FK.
        sqlx::query("DELETE FROM categories WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.publish_category_deleted(id);
        Ok(())
    }

    // ── favorites & search (used by the web UI) ──────────────────────────────

    pub async fn favorite_note(&self, slug: &str, on: bool) -> NotesResult<NoteView> {
        let note = self.note_by_slug(slug).await?;
        let note = sqlx::query_as::<_, NoteRow>(
            "UPDATE notes SET is_favorite = ? WHERE id = ? RETURNING *",
        )
        .bind(on as i64)
        .bind(&note.id)
        .fetch_one(&self.pool)
        .await?;
        let view = self.note_view(note).await?;
        self.publish_note(&view);
        Ok(view)
    }

    pub async fn list_favorites(&self) -> NotesResult<Vec<NoteHit>> {
        Ok(sqlx::query_as::<_, NoteHit>(
            "SELECT slug, title, updated_at FROM notes WHERE is_favorite = 1 ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn search(&self, q: &str, category: Option<&str>) -> NotesResult<Vec<NoteHit>> {
        let query = super::util::fts_query(q);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let rows = if let Some(cat) = category.filter(|c| !c.is_empty()) {
            sqlx::query_as::<_, NoteHit>(
                "SELECT n.slug, n.title, n.updated_at
                 FROM notes_fts f JOIN notes n ON n.rowid = f.rowid
                 WHERE notes_fts MATCH ?
                   AND n.id IN (SELECT note_id FROM note_categories WHERE category_id = ?)
                 ORDER BY bm25(notes_fts) LIMIT 50",
            )
            .bind(&query)
            .bind(cat)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, NoteHit>(
                "SELECT n.slug, n.title, n.updated_at
                 FROM notes_fts f JOIN notes n ON n.rowid = f.rowid
                 WHERE notes_fts MATCH ? ORDER BY bm25(notes_fts) LIMIT 50",
            )
            .bind(&query)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows)
    }

    // ── export / import (Obsidian-compatible markdown) ───────────────────────

    async fn category_names(&self, note_id: &str) -> NotesResult<Vec<String>> {
        Ok(self.categories_for_note(note_id).await?.into_iter().map(|c| c.name).collect())
    }

    /// All notes as `(filename, contents)` markdown files (one flat `.md` each).
    pub async fn export_all(&self) -> NotesResult<Vec<(String, String)>> {
        let notes = sqlx::query_as::<_, NoteRow>("SELECT * FROM notes ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        let ids: Vec<String> = notes.iter().map(|n| n.id.clone()).collect();
        let mut cats = self.categories_for_notes(&ids).await?;
        let items: Vec<_> = notes
            .into_iter()
            .map(|n| {
                let names: Vec<String> =
                    cats.remove(&n.id).unwrap_or_default().into_iter().map(|c| c.name).collect();
                (n.title, n.slug, n.body, n.is_favorite != 0, n.created_at, n.updated_at, names)
            })
            .collect();
        Ok(super::portable::note_files(&items))
    }

    /// One note as `(filename, contents)`.
    pub async fn export_one(&self, slug: &str) -> NotesResult<(String, String)> {
        let n = self.note_by_slug(slug).await?;
        let names = self.category_names(&n.id).await?;
        let md = super::portable::serialize_note(
            &n.title,
            &n.slug,
            &n.body,
            n.is_favorite != 0,
            &n.created_at,
            &n.updated_at,
            &names,
        );
        Ok((format!("{}.md", n.slug), md))
    }

    /// Import markdown entries as flat notes. **Additive**: slugs are deduped so
    /// importing never clobbers existing notes. Returns the count created.
    pub async fn import_markdown(&self, entries: Vec<(String, String)>) -> NotesResult<usize> {
        let nodes = super::portable::parse_import(entries);
        let mut cache: HashMap<String, String> = HashMap::new();
        let mut imported = 0usize;
        for node in nodes {
            let base = node
                .preferred_slug
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(slugify)
                .unwrap_or_else(|| slugify(&node.title));
            let slug = self.free_slug(&base).await?;
            let title = if node.title.trim().is_empty() { "Untitled" } else { node.title.trim() };
            let now = now_iso();
            let note = sqlx::query_as::<_, NoteRow>(
                "INSERT INTO notes (id, title, slug, body, is_favorite, created_at, updated_at, last_accessed_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
            )
            .bind(uuid())
            .bind(title)
            .bind(&slug)
            .bind(&node.body)
            .bind(node.favorite as i64)
            .bind(&now)
            .bind(&now)
            .bind(&now)
            .fetch_one(&self.pool)
            .await?;
            let cat_ids = self.resolve_import_categories(&node.categories, &mut cache).await?;
            if !cat_ids.is_empty() {
                self.set_note_categories(&note.id, &cat_ids).await?;
            }
            let view = self.note_view(note).await?;
            self.publish_note(&view);
            imported += 1;
        }
        Ok(imported)
    }

    /// Resolve category names → ids on import: reuse existing (case-insensitive),
    /// else create while the palette allows; names past the cap are skipped.
    async fn resolve_import_categories(
        &self,
        names: &[String],
        cache: &mut HashMap<String, String>,
    ) -> NotesResult<Vec<String>> {
        let mut ids = Vec::new();
        for raw in names {
            let name = raw.trim();
            if name.is_empty() {
                continue;
            }
            let key = name.to_lowercase();
            if let Some(id) = cache.get(&key) {
                ids.push(id.clone());
                continue;
            }
            if let Some(id) = sqlx::query_scalar::<_, String>(
                "SELECT id FROM categories WHERE name = ? COLLATE NOCASE",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            {
                cache.insert(key, id.clone());
                ids.push(id);
                continue;
            }
            let used: Vec<String> = sqlx::query_scalar("SELECT color FROM categories")
                .fetch_all(&self.pool)
                .await?;
            let Some(color) = palette::pick_unused(&used) else { continue };
            let cat = sqlx::query_as::<_, CategoryRow>(
                "INSERT INTO categories (id, name, color, created_at) VALUES (?, ?, ?, ?)
                 RETURNING id, name, color, created_at",
            )
            .bind(uuid())
            .bind(name)
            .bind(color)
            .bind(now_iso())
            .fetch_one(&self.pool)
            .await?;
            let cat = Category::from(cat);
            self.publish_category(&cat);
            cache.insert(key, cat.id.clone());
            ids.push(cat.id);
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> NotesStore {
        // Single connection so the `:memory:` DB is shared across all queries
        // (a multi-connection memory pool gives each connection its own DB).
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let s = NotesStore::new(pool, OwnerIdentity::default(), AppEventHub::new());
        s.ensure_ready().await.unwrap();
        s
    }

    #[tokio::test]
    async fn seeds_default_categories_once() {
        let s = store().await;
        let cats = s.list_categories().await.unwrap();
        let names: Vec<&str> = cats.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"home") && names.contains(&"work") && names.contains(&"personal"));
        // ensure_ready again must not duplicate.
        s.ensure_ready().await.unwrap();
        assert_eq!(s.list_categories().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn note_crud_and_slug_dedup() {
        let s = store().await;
        let a = s.create_note(Some("My Note"), Some("# hi"), None).await.unwrap();
        assert_eq!(a.slug, "my-note");
        assert_eq!(a.version, 1);
        // Same title → deduped slug.
        let b = s.create_note(Some("My Note"), None, None).await.unwrap();
        assert_eq!(b.slug, "my-note-2");

        // get bumps last_accessed but not version.
        let got = s.get_note("my-note").await.unwrap();
        assert_eq!(got.version, 1);

        // content edit bumps version.
        let (status, up) = s
            .update_note("my-note", Some("Renamed"), None, None, None, true, false)
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(up.title, "Renamed");
        assert_eq!(up.version, 2);

        assert_eq!(s.list_notes(None, None).await.unwrap().len(), 2);
        s.delete_note("my-note").await.unwrap();
        assert_eq!(s.list_notes(None, None).await.unwrap().len(), 1);
        assert!(s.get_note("my-note").await.is_err());
    }

    #[tokio::test]
    async fn stale_base_version_returns_409_with_current() {
        let s = store().await;
        s.create_note(Some("Doc"), Some("v1"), None).await.unwrap();
        let (status, view) = s
            .update_note("doc", None, Some("v2"), Some(99), None, false, true)
            .await
            .unwrap();
        assert_eq!(status, 409);
        assert_eq!(view.body, "v1"); // unchanged; caller must merge
    }

    #[tokio::test]
    async fn share_and_public_lookup() {
        let s = store().await;
        s.create_note(Some("Shared"), Some("# public\nbody"), None).await.unwrap();
        let token = s.share("shared").await.unwrap();
        assert_eq!(token.len(), 32);
        assert_eq!(s.share("shared").await.unwrap(), token); // idempotent

        let (title, body) = s.note_by_token(&token).await.unwrap();
        assert_eq!(title, "Shared");
        assert!(body.contains("public"));

        s.unshare("shared").await.unwrap();
        assert!(s.note_by_token(&token).await.is_err());
    }

    #[tokio::test]
    async fn export_then_import_round_trips() {
        let s = store().await;
        let c = s.create_category("proj").await.unwrap();
        s.create_note(Some("Alpha"), Some("# A\nbody"), Some(vec![c.id.clone()])).await.unwrap();
        s.create_note(Some("Beta"), Some("B"), None).await.unwrap();

        let files = s.export_all().await.unwrap();
        assert_eq!(files.len(), 2);

        // Import the export into a fresh pod → notes, bodies, and the category
        // tag are preserved (the cloud→pod migration path).
        let s2 = store().await;
        assert_eq!(s2.import_markdown(files).await.unwrap(), 2);
        assert_eq!(s2.list_notes(None, None).await.unwrap().len(), 2);
        let alpha = s2.get_note("alpha").await.unwrap();
        assert_eq!(alpha.body, "# A\nbody");
        assert!(alpha.categories.iter().any(|c| c.name == "proj"));
    }

    #[tokio::test]
    async fn mutations_broadcast_events() {
        let s = store().await;
        let mut rx = s.events().subscribe();

        s.create_note(Some("Live"), Some("body"), None).await.unwrap();
        assert_eq!(rx.recv().await.unwrap()["type"], "note.upserted");

        let c = s.create_category("live-cat").await.unwrap();
        assert_eq!(rx.recv().await.unwrap()["type"], "category.upserted");

        s.delete_note("live").await.unwrap();
        assert_eq!(rx.recv().await.unwrap()["type"], "note.deleted");

        s.delete_category(&c.id).await.unwrap();
        assert_eq!(rx.recv().await.unwrap()["type"], "category.deleted");
    }

    #[tokio::test]
    async fn favorite_search_and_category_edit() {
        let s = store().await;
        s.create_note(Some("Grocery list"), Some("milk eggs bread"), None).await.unwrap();
        s.create_note(Some("Trip plan"), Some("flights and hotels"), None).await.unwrap();

        // favorite toggles and lists
        s.favorite_note("grocery-list", true).await.unwrap();
        let favs = s.list_favorites().await.unwrap();
        assert_eq!(favs.len(), 1);
        assert_eq!(favs[0].slug, "grocery-list");

        // FTS prefix search finds by body/title
        let hits = s.search("gro", None).await.unwrap();
        assert!(hits.iter().any(|h| h.slug == "grocery-list"));
        let body_hits = s.search("hotel", None).await.unwrap();
        assert!(body_hits.iter().any(|h| h.slug == "trip-plan"));
        assert!(s.search("   ", None).await.unwrap().is_empty());

        // category rename + delete
        let c = s.create_category("misc").await.unwrap();
        let renamed = s.update_category(&c.id, Some("miscellaneous"), None).await.unwrap();
        assert_eq!(renamed.name, "miscellaneous");
        s.delete_category(&c.id).await.unwrap();
        assert!(s.update_category(&c.id, Some("x"), None).await.is_err());
    }

    #[tokio::test]
    async fn category_tagging_and_limits() {
        let s = store().await;
        let c = s.create_category("ideas").await.unwrap();
        let note = s.create_note(Some("Tagged"), None, Some(vec![c.id.clone()])).await.unwrap();
        assert_eq!(note.categories.len(), 1);
        assert_eq!(note.categories[0].name, "ideas");

        // filter by category
        let listed = s.list_notes(None, Some(&c.id)).await.unwrap();
        assert_eq!(listed.len(), 1);

        // duplicate name rejected
        assert!(s.create_category("Ideas").await.is_err());
    }
}
