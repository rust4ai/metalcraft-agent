//! Small helpers for the Calendar app.

use chrono::{DateTime, SecondsFormat, Utc};

use super::{CalError, CalResult};

pub fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Canonical UTC timestamp: RFC3339, millisecond precision, trailing `Z`. Fixed
/// width and Z-suffixed, so **lexicographic order == chronological order** — the
/// property that lets range queries (`starts_at <= ?`) work over TEXT columns.
pub fn canon(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn now_iso() -> String {
    canon(Utc::now())
}

/// Parse a UTC ISO-8601 timestamp from the agent/client into a canonical string.
/// Rejects unparseable input with a 400 so bad times never enter the store.
pub fn parse_ts(s: &str, field: &str) -> CalResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| CalError::bad_request(format!("{field} must be a UTC ISO-8601 timestamp (e.g. 2026-08-12T14:00:00Z)")))
}

/// Slug from a name: lowercase, non-alphanumeric runs → `-`, trimmed.
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
    if slug.is_empty() { "calendar".to_string() } else { slug }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_timestamps_sort_chronologically() {
        // Mixed input precision normalizes to the same shape, so string compare works.
        let a = canon(parse_ts("2026-08-12T14:00:00Z", "t").unwrap());
        let b = canon(parse_ts("2026-08-12T14:00:00.500Z", "t").unwrap());
        let c = canon(parse_ts("2026-08-12T15:00:00Z", "t").unwrap());
        assert!(a < b && b < c);
        assert!(a.ends_with("Z") && a.contains(".000"));
    }

    #[test]
    fn rejects_bad_timestamp() {
        assert!(parse_ts("not-a-time", "starts_at").is_err());
    }
}
