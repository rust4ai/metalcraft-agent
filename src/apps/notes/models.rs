//! Data shapes. `*Row` structs deserialize straight from SQLite rows (field
//! names == column names; SQLite INTEGER → i64). The output structs are the
//! JSON contract the SPA and the `mnote_` pack already expect — byte-compatible
//! with the cloud metalcraft-notes.

use serde::Serialize;
use sqlx::FromRow;

// ── rows read from SQLite ────────────────────────────────────────────────────

#[derive(Debug, Clone, FromRow)]
pub struct NoteRow {
    pub id: String,
    pub title: String,
    pub slug: String,
    pub body: String,
    pub is_favorite: i64,
    pub public_token: Option<String>,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
    pub last_accessed_at: String,
}

/// A sidebar row (no body).
#[derive(Debug, Clone, FromRow)]
pub struct NoteSummaryRow {
    pub id: String,
    pub title: String,
    pub slug: String,
    pub is_favorite: i64,
    pub created_at: String,
    pub updated_at: String,
    pub last_accessed_at: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct CategoryRow {
    pub id: String,
    pub name: String,
    pub color: String,
    pub created_at: String,
}

/// A category joined to a note id, for batch-loading tags across many notes.
#[derive(Debug, Clone, FromRow)]
pub struct NoteCategoryRow {
    pub note_id: String,
    pub id: String,
    pub name: String,
    pub color: String,
    pub created_at: String,
}

// ── JSON output (the contract) ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Category {
    pub id: String,
    pub name: String,
    pub color: String,
    pub created_at: String,
}

impl From<CategoryRow> for Category {
    fn from(r: CategoryRow) -> Self {
        Category { id: r.id, name: r.name, color: r.color, created_at: r.created_at }
    }
}

/// Full note + its category tags. `owner_user_id` is injected from the pod's
/// identity so the shape matches metalcraft-notes exactly.
#[derive(Debug, Clone, Serialize)]
pub struct NoteView {
    pub id: String,
    pub owner_user_id: String,
    pub title: String,
    pub slug: String,
    pub body: String,
    pub is_favorite: bool,
    pub public_token: Option<String>,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
    pub last_accessed_at: String,
    pub categories: Vec<Category>,
}

impl NoteView {
    pub fn build(r: NoteRow, owner: &str, categories: Vec<Category>) -> Self {
        NoteView {
            id: r.id,
            owner_user_id: owner.to_string(),
            title: r.title,
            slug: r.slug,
            body: r.body,
            is_favorite: r.is_favorite != 0,
            public_token: r.public_token,
            version: r.version,
            created_at: r.created_at,
            updated_at: r.updated_at,
            last_accessed_at: r.last_accessed_at,
            categories,
        }
    }
}

/// A sidebar row + its category tags.
#[derive(Debug, Clone, Serialize)]
pub struct NoteSummaryView {
    pub id: String,
    pub title: String,
    pub slug: String,
    pub is_favorite: bool,
    pub created_at: String,
    pub updated_at: String,
    pub last_accessed_at: String,
    pub categories: Vec<Category>,
}

impl NoteSummaryView {
    pub fn build(r: NoteSummaryRow, categories: Vec<Category>) -> Self {
        NoteSummaryView {
            id: r.id,
            title: r.title,
            slug: r.slug,
            is_favorite: r.is_favorite != 0,
            created_at: r.created_at,
            updated_at: r.updated_at,
            last_accessed_at: r.last_accessed_at,
            categories,
        }
    }
}
