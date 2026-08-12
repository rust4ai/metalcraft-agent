//! Small helpers — the native (non-wasm) equivalents of notes-r2's `util.rs`.
//! Postgres `gen_random_uuid()`/`now()` are replaced by app-generated values.

/// A fresh uuid v4 string (replaces Postgres `gen_random_uuid()`).
pub fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A public-share token: 32 hex chars (matches metalcraft-notes' `Uuid::simple()`).
pub fn token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// RFC3339 UTC timestamp with millisecond precision and a trailing `Z`
/// (replaces Postgres `now()`; string-sortable, matches the SPA's expectations).
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Slug from a title: lowercase, non-alphanumeric runs → `-`, trimmed. Falls
/// back to `untitled` when nothing survives.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug
    }
}

/// `?,?,…` — `n` positional placeholders for a dynamic `IN (…)`.
pub fn placeholders(n: usize) -> String {
    std::iter::repeat("?").take(n).collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  spaced  out  "), "spaced-out");
        assert_eq!(slugify("émoji 🎉 mix"), "moji-mix");
        assert_eq!(slugify("!!!"), "untitled");
        assert_eq!(slugify(""), "untitled");
    }

    #[test]
    fn ids_have_expected_shape() {
        assert_eq!(uuid().len(), 36);
        assert_eq!(token().len(), 32);
        assert!(now_iso().ends_with('Z'));
    }
}
