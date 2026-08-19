//! Timezone logic — ported verbatim from the cloud `metalcraft-calendar`
//! (`services/events.rs::day_window`, `controllers/api_v1.rs::now`). Pure
//! `chrono`/`chrono-tz`, no DB, no network — moves to the pod unchanged.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};

use super::{CalError, CalResult};

/// Validate an IANA timezone name (e.g. `America/New_York`).
pub fn validate_tz(tz: &str) -> CalResult<Tz> {
    let t = tz.trim();
    if t.is_empty() {
        return Err(CalError::bad_request(
            "timezone is required (IANA name, e.g. 'America/New_York')",
        ));
    }
    t.parse::<Tz>().map_err(|_| {
        CalError::bad_request("unknown timezone; use an IANA name like 'America/New_York'")
    })
}

/// Resolve a `day` token (`today`/`tomorrow`/`yesterday`/`YYYY-MM-DD`) into a
/// `[start, end)` UTC window covering that whole calendar-day **in `tz`** — so
/// "what's on tomorrow" uses local day boundaries, not UTC.
pub fn day_window(day: &str, tz: &str) -> CalResult<(DateTime<Utc>, DateTime<Utc>)> {
    let tz: Tz = tz
        .parse()
        .map_err(|_| CalError::bad_request("calendar has an invalid timezone"))?;
    let today = Utc::now().with_timezone(&tz).date_naive();
    let date: NaiveDate = match day.trim().to_lowercase().as_str() {
        "today" => today,
        "tomorrow" => today.succ_opt().ok_or_else(|| CalError::bad_request("date out of range"))?,
        "yesterday" => today.pred_opt().ok_or_else(|| CalError::bad_request("date out of range"))?,
        s => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
            CalError::bad_request("day must be today, tomorrow, yesterday, or YYYY-MM-DD")
        })?,
    };
    let next = date.succ_opt().ok_or_else(|| CalError::bad_request("date out of range"))?;
    let start = tz
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .ok_or_else(|| CalError::bad_request("ambiguous local midnight for that day"))?
        .with_timezone(&Utc);
    let end = tz
        .from_local_datetime(&next.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .ok_or_else(|| CalError::bad_request("ambiguous local midnight for that day"))?
        .with_timezone(&Utc);
    Ok((start, end))
}

/// The agent's authoritative "now", localized to `tz` (UTC if None). `date` /
/// `tomorrow` / `yesterday` are LOCAL dates ready to pass to `list_events?day=`.
pub fn now_response(tz_name: Option<&str>) -> CalResult<Value> {
    let tz_name = tz_name.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("UTC");
    let tz: Tz = tz_name.parse().map_err(|_| {
        CalError::bad_request("unknown timezone; use an IANA name like 'America/New_York'")
    })?;
    let now_utc = Utc::now();
    let local = now_utc.with_timezone(&tz);
    let date = local.date_naive();
    Ok(json!({
        "utc": now_utc.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "timezone": tz_name,
        "local": local.to_rfc3339(),
        "date": date.to_string(),
        "weekday": local.format("%A").to_string(),
        "tomorrow": date.succ_opt().map(|d| d.to_string()).unwrap_or_default(),
        "yesterday": date.pred_opt().map(|d| d.to_string()).unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_window_uses_local_boundaries() {
        // New York is UTC-4/5, so a local day starts at 04:00/05:00Z.
        let (start, end) = day_window("2026-08-12", "America/New_York").unwrap();
        assert_eq!(start.format("%H:%M").to_string(), "04:00"); // EDT (summer)
        assert_eq!((end - start).num_hours(), 24);
    }

    #[test]
    fn bad_tz_and_day_rejected() {
        assert!(validate_tz("Nowhere/Void").is_err());
        assert!(validate_tz("").is_err());
        assert!(day_window("someday", "UTC").is_err());
    }

    #[test]
    fn now_localizes() {
        let v = now_response(Some("UTC")).unwrap();
        assert_eq!(v["timezone"], "UTC");
        assert!(v["date"].as_str().unwrap().len() == 10);
    }
}
