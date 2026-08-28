//! When a schedule fires, and what to remember about it having fired.
//!
//! This is the arithmetic the daemon polls on: given what a schedule last did
//! and what time it is, decide whether it runs now. It lives apart from the
//! daemon because the daemon's loop is untestable by construction — it reads the
//! wall clock, blocks on a flow, and never returns — and this is the part where
//! being wrong means somebody's morning brief arrives at 4am, or twice, or not
//! at all. Every input is a parameter, including `now`.
//!
//! ## The bookmark
//!
//! Each schedule has one: the last occurrence it reached. It is persisted, which
//! is the fix for the loudest thing that was wrong here. The daemon used to hold
//! the map in memory, so a restart erased it — and an interval schedule with no
//! record of a previous run fires *immediately*, which meant "every 24 hours"
//! fired again on every pod roll, and pods roll on every image upgrade.
//!
//! A bookmark records the **occurrence**, not the moment the daemon noticed it.
//! Polling is periodic, so noticing happens up to a poll-interval late; bookmarking
//! `now` would push each firing later than the last and an 08:00 brief would walk
//! into the afternoon over enough days.
//!
//! ## Timezones
//!
//! A cron is evaluated in its declared zone, so "08:00 America/Detroit" is 08:00
//! there in both June and January. Without a zone it falls back to the pod's own
//! clock, which in the cluster is UTC — the reason both clients state the
//! reader's zone rather than leaving it empty, and the reason an unparseable zone
//! is now refused at the door (see [`crate::scheduled_flows::save`]) instead of
//! silently meaning UTC.
//!
//! ## Daylight saving
//!
//! A cron expression matches a **clock face**, not an instant: the underlying
//! parser advances through local wall-clock times and asks the zone what instant
//! each one is. That answers both edges, and the tests below pin them because
//! they are the two days a year this is allowed to be surprising:
//!
//! * **Spring forward** — 02:30 does not exist on the day the clocks jump from
//!   02:00 to 03:00, so that day's 02:30 run is skipped. The conventional cron
//!   answer, and the next day is unaffected.
//! * **Fall back** — 01:30 arrives twice, an hour apart. It fires on the first
//!   and not the second, because the search is for the next clock face *after*
//!   the one it last fired at, and 01:30 is not after 01:30. The consequence for
//!   a sub-hourly schedule is that it stands still through the repeated hour
//!   rather than running it twice, which is the same trade every wall-clock
//!   scheduler makes.

use std::str::FromStr;

use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::flows::FlowSchedule;

/// What a schedule should do at this instant.
///
/// Carries the occurrence reached even when nothing runs, because a first
/// sighting still has to move the bookmark — a schedule that keeps re-deciding
/// the same starting point never gets to a second one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// The occurrence reached. Record this as the schedule's new bookmark.
    pub occurrence: DateTime<Utc>,
    /// Whether to actually run the flow.
    pub run: bool,
}

impl Decision {
    /// Run it, and bookmark this occurrence.
    fn run(occurrence: DateTime<Utc>) -> Self {
        Self {
            occurrence,
            run: true,
        }
    }
    /// Note where a schedule starts from, without running it.
    fn seed(occurrence: DateTime<Utc>) -> Self {
        Self {
            occurrence,
            run: false,
        }
    }
}

/// Decide what `trigger` does at `now`, given the occurrence it last reached.
///
/// `bookmark` is `None` the first time a schedule is ever seen. That seeds
/// rather than fires: an interval schedule armed at 3pm means "every 24 hours
/// **from now**", which is what the arming screen's preview projects, and a cron
/// schedule must not open by catching up on occurrences that predate its
/// existence.
pub fn decide(
    bookmark: Option<DateTime<Utc>>,
    trigger: &FlowSchedule,
    timezone: Option<&str>,
    now: DateTime<Utc>,
) -> Option<Decision> {
    match trigger {
        FlowSchedule::Manual => None,
        FlowSchedule::EveryMinutes(minutes) => {
            every(bookmark, Duration::minutes(*minutes as i64), now)
        }
        FlowSchedule::EveryHours(hours) => every(bookmark, Duration::hours(*hours as i64), now),
        FlowSchedule::Cron(expr) => {
            let schedule = cron::Schedule::from_str(expr).ok()?;
            // An unparseable zone name reaches here only from data written
            // before the save-time check; firing it on the pod's clock is the
            // historical behaviour, and `crate::flows::parse_schedule` now
            // refuses it loudly before the daemon ever asks.
            match timezone.and_then(|name| name.parse::<chrono_tz::Tz>().ok()) {
                Some(zone) => cron_decision(&schedule, &zone, bookmark, now),
                None => cron_decision(&schedule, &chrono::Local, bookmark, now),
            }
        }
    }
}

/// Interval triggers: fire once a full interval has passed since the last one.
fn every(
    bookmark: Option<DateTime<Utc>>,
    interval: Duration,
    now: DateTime<Utc>,
) -> Option<Decision> {
    // A non-positive interval would fire every poll forever. `parse_schedule`
    // rejects zero, so this is a floor, not a policy.
    if interval <= Duration::zero() {
        return None;
    }
    match bookmark {
        None => Some(Decision::seed(now)),
        Some(last) if now - last >= interval => Some(Decision::run(now)),
        Some(_) => None,
    }
}

/// Cron triggers, in one zone.
///
/// Generic over the zone so the declared-timezone path and the pod's-clock
/// fallback are the same code — the fallback used to be a second copy of this
/// logic, which is how two paths drift.
fn cron_decision<Z: TimeZone>(
    schedule: &cron::Schedule,
    zone: &Z,
    bookmark: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<Decision> {
    let now_there = now.with_timezone(zone);
    let Some(last) = bookmark else {
        return Some(Decision::seed(now));
    };
    let last_there = last.with_timezone(zone);
    // `after` searches clock faces, not instants: the next local time matching
    // the pattern that is strictly later than the one just fired. That is what
    // makes a fall-back repeat a non-event — 01:30 is not after 01:30 — and what
    // makes a spring-forward gap a skipped day.
    let next = schedule.after(&last_there).next()?;
    if next > now_there {
        return None;
    }
    Some(Decision::run(next.with_timezone(&Utc)))
}

/// Where each schedule last got to, across restarts.
///
/// A whole-map rewrite on every change. There are a handful of schedules on a
/// pod and a firing is a rare event next to the flow run it starts, so the
/// simplest thing that cannot half-write a record is the right one.
pub mod bookmarks {
    use super::*;
    use std::collections::HashMap;

    /// Read the bookmarks, dropping any whose schedule is gone.
    ///
    /// A missing or unreadable file is an empty map, not an error: the first run
    /// after this landed has no file, and a pod that cannot read it should seed
    /// fresh bookmarks rather than refuse to schedule anything.
    pub fn load() -> HashMap<String, DateTime<Utc>> {
        let path = crate::paths::schedule_state_file();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return HashMap::new();
        };
        let stored: HashMap<String, String> = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                log::warn!(
                    "schedule state at {} is unreadable ({e}); every schedule will \
                     bookmark itself afresh and fire one interval from now",
                    path.display()
                );
                return HashMap::new();
            }
        };
        let live: std::collections::HashSet<String> = crate::scheduled_flows::list()
            .into_iter()
            .map(|sf| sf.id)
            .collect();
        stored
            .into_iter()
            .filter(|(id, _)| live.contains(id))
            .filter_map(|(id, at)| {
                DateTime::parse_from_rfc3339(&at)
                    .ok()
                    .map(|t| (id, t.with_timezone(&Utc)))
            })
            .collect()
    }

    /// Write the bookmarks. Failure is logged, not propagated: losing the file
    /// costs one seeded interval, and refusing to run the flow costs the run.
    pub fn save(map: &HashMap<String, DateTime<Utc>>) {
        let path = crate::paths::schedule_state_file();
        let stored: std::collections::BTreeMap<&str, String> = map
            .iter()
            .map(|(id, at)| (id.as_str(), at.to_rfc3339()))
            .collect();
        match serde_json::to_string_pretty(&stored) {
            Ok(text) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(&path, text) {
                    log::warn!("could not write schedule state to {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("could not serialize schedule state: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// `2026-08-27T12:00:00Z`, and friends — absolute instants, so a test says
    /// what it means regardless of where it runs.
    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn detroit(expr: &str) -> (FlowSchedule, Option<&'static str>) {
        (FlowSchedule::Cron(expr.into()), Some("America/Detroit"))
    }

    #[test]
    fn a_manual_schedule_never_fires_on_its_own() {
        assert_eq!(
            decide(None, &FlowSchedule::Manual, None, utc(2026, 8, 27, 12, 0)),
            None
        );
    }

    // ── Intervals ───────────────────────────────────────────────────────────

    #[test]
    fn an_interval_first_seen_is_bookmarked_not_fired() {
        // The bug this pins: an interval with no record fired immediately, and
        // the record lived in memory, so "every 24 hours" ran again on every pod
        // roll. It also disagreed with the arming preview, which projects the
        // first run one whole interval out.
        let now = utc(2026, 8, 27, 15, 0);
        let d = decide(None, &FlowSchedule::EveryHours(24), None, now).expect("seeds");
        assert!(!d.run, "first sighting must not fire");
        assert_eq!(d.occurrence, now);
    }

    #[test]
    fn an_interval_fires_once_the_interval_has_passed() {
        let armed = utc(2026, 8, 27, 15, 0);
        let s = FlowSchedule::EveryHours(24);
        assert_eq!(decide(Some(armed), &s, None, utc(2026, 8, 28, 14, 59)), None);
        let d = decide(Some(armed), &s, None, utc(2026, 8, 28, 15, 0)).expect("due");
        assert!(d.run);
    }

    #[test]
    fn an_interval_survives_a_restart_without_refiring() {
        // Same schedule, same bookmark, a pod that has restarted twice in the
        // meantime: still not due until the interval is actually up.
        let last = utc(2026, 8, 27, 15, 0);
        let s = FlowSchedule::EveryMinutes(30);
        assert_eq!(decide(Some(last), &s, None, utc(2026, 8, 27, 15, 10)), None);
        assert_eq!(decide(Some(last), &s, None, utc(2026, 8, 27, 15, 29)), None);
        assert!(
            decide(Some(last), &s, None, utc(2026, 8, 27, 15, 30))
                .unwrap()
                .run
        );
    }

    // ── Cron, in a declared zone ────────────────────────────────────────────

    #[test]
    fn a_cron_fires_at_its_local_hour_not_the_pods() {
        // 08:00 Detroit is 12:00 UTC in August. A pod whose own clock is UTC
        // must not fire this at 08:00 UTC.
        let (s, tz) = detroit("0 0 8 * * *");
        let yesterday = utc(2026, 8, 26, 12, 0);
        assert_eq!(decide(Some(yesterday), &s, tz, utc(2026, 8, 27, 8, 0)), None);
        let d = decide(Some(yesterday), &s, tz, utc(2026, 8, 27, 12, 0)).expect("due");
        assert!(d.run);
        assert_eq!(d.occurrence, utc(2026, 8, 27, 12, 0));
    }

    #[test]
    fn the_same_local_hour_follows_the_clocks_across_a_dst_boundary() {
        // 08:00 Detroit is 13:00 UTC in winter and 12:00 UTC in summer. The
        // schedule is the local hour; the UTC offset is what moves.
        let (s, tz) = detroit("0 0 8 * * *");
        let winter = decide(Some(utc(2027, 3, 12, 13, 0)), &s, tz, utc(2027, 3, 13, 13, 0))
            .expect("winter firing");
        assert_eq!(winter.occurrence, utc(2027, 3, 13, 13, 0));
        // 2027-03-14 is the spring-forward day; the 08:00 firing lands at 12:00Z.
        let summer = decide(Some(utc(2027, 3, 13, 13, 0)), &s, tz, utc(2027, 3, 14, 12, 0))
            .expect("summer firing");
        assert_eq!(summer.occurrence, utc(2027, 3, 14, 12, 0));
    }

    #[test]
    fn a_firing_is_bookmarked_at_its_occurrence_so_it_cannot_drift() {
        // The daemon polls, so it notices late. Bookmarking the moment of
        // *noticing* would push tomorrow's firing later than today's, and an
        // 08:00 brief would walk into the afternoon over a few hundred days.
        let (s, tz) = detroit("0 0 8 * * *");
        let d = decide(
            Some(utc(2026, 8, 26, 12, 0)),
            &s,
            tz,
            utc(2026, 8, 27, 12, 0) + Duration::seconds(29),
        )
        .expect("due");
        assert_eq!(d.occurrence, utc(2026, 8, 27, 12, 0), "the hour, not the tick");
    }

    #[test]
    fn a_cron_first_seen_does_not_backfill_history() {
        // Arming a daily 08:00 at noon must not immediately run this morning's.
        let (s, tz) = detroit("0 0 8 * * *");
        let d = decide(None, &s, tz, utc(2026, 8, 27, 16, 0)).expect("seeds");
        assert!(!d.run);
    }

    #[test]
    fn a_firing_missed_while_the_pod_was_down_runs_once_when_it_returns() {
        // The bookmark outlives the process, so a pod that was down at 08:00
        // knows it owes a run — and owes exactly one, however long it was away.
        let (s, tz) = detroit("0 0 8 * * *");
        let last = utc(2026, 8, 24, 12, 0); // Monday's firing
        let d = decide(Some(last), &s, tz, utc(2026, 8, 27, 16, 0)).expect("owed");
        assert!(d.run);
        assert_eq!(d.occurrence, utc(2026, 8, 25, 12, 0), "the oldest one owed");
        // Having run it, the next poll owes the next one — not all of them at once.
        let d2 = decide(Some(d.occurrence), &s, tz, utc(2026, 8, 27, 16, 0)).expect("still owed");
        assert_eq!(d2.occurrence, utc(2026, 8, 26, 12, 0));
    }

    // ── Daylight saving ─────────────────────────────────────────────────────

    #[test]
    fn a_daily_run_in_the_repeated_hour_runs_once() {
        // 2026-11-01, America/Detroit: 02:00 EDT falls back to 01:00 EST, so the
        // clock reads 01:30 twice — at 05:30Z and again at 06:30Z — and both are
        // instants a naive reading of "every day at 01:30" would fire on.
        let (s, tz) = detroit("0 30 1 * * *");
        let first = decide(Some(utc(2026, 10, 31, 5, 30)), &s, tz, utc(2026, 11, 1, 5, 30))
            .expect("the first 01:30");
        assert!(first.run);
        assert_eq!(first.occurrence, utc(2026, 11, 1, 5, 30));

        // An hour later the clock says 01:30 again. Nothing is owed: the search
        // is for the next clock face after 01:30, and that is tomorrow's.
        assert_eq!(
            decide(Some(first.occurrence), &s, tz, utc(2026, 11, 1, 6, 30)),
            None,
            "01:30 already happened today"
        );

        let tomorrow = decide(Some(first.occurrence), &s, tz, utc(2026, 11, 2, 6, 30))
            .expect("the next day");
        assert!(tomorrow.run);
        assert_eq!(tomorrow.occurrence, utc(2026, 11, 2, 6, 30));
    }

    #[test]
    fn a_sub_hourly_schedule_stands_still_through_the_repeated_hour() {
        // The other side of the same coin, written down so it is a decision
        // rather than a surprise: a 15-minute schedule does not replay the hour
        // the clocks hand back. It resumes at the first clock face after the one
        // it last fired at, which is 02:00 EST — a real 02:00, an hour later.
        let (s, tz) = detroit("0 0,15,30,45 * * * *");
        let last = decide(Some(utc(2026, 11, 1, 5, 30)), &s, tz, utc(2026, 11, 1, 5, 45))
            .expect("01:45 EDT");
        assert_eq!(last.occurrence, utc(2026, 11, 1, 5, 45));
        let next = decide(Some(last.occurrence), &s, tz, utc(2026, 11, 1, 7, 0)).expect("next");
        assert_eq!(
            next.occurrence,
            utc(2026, 11, 1, 7, 0),
            "02:00 EST — the repeated 01:00 hour is not run again"
        );
    }

    #[test]
    fn an_hour_that_does_not_exist_is_skipped_for_that_day_only() {
        // 2027-03-14, America/Detroit: 02:00 EST jumps to 03:00 EDT, so there is
        // no 02:30 at all. The day's run is skipped — the conventional cron
        // answer — and the day after is unaffected.
        let (s, tz) = detroit("0 30 2 * * *");
        let before = utc(2027, 3, 13, 7, 30); // 02:30 EST on the 13th
        let next = decide(Some(before), &s, tz, utc(2027, 3, 16, 0, 0)).expect("due");
        assert_eq!(
            next.occurrence,
            utc(2027, 3, 15, 6, 30),
            "the 14th has no 02:30; the 15th is 02:30 EDT = 06:30Z"
        );
    }

    // ── Zones ───────────────────────────────────────────────────────────────

    #[test]
    fn zones_ahead_of_utc_are_handled_by_the_same_arithmetic() {
        // 09:00 Tokyo is 00:00 UTC the same day — the case where "local" and
        // "UTC" are not even the same date.
        let s = FlowSchedule::Cron("0 0 9 * * *".into());
        let tz = Some("Asia/Tokyo");
        let yesterday = utc(2026, 8, 26, 0, 0);
        assert_eq!(decide(Some(yesterday), &s, tz, utc(2026, 8, 26, 23, 0)), None);
        let d = decide(Some(yesterday), &s, tz, utc(2026, 8, 27, 0, 0)).expect("due");
        assert_eq!(d.occurrence, utc(2026, 8, 27, 0, 0));
    }

    #[test]
    fn a_half_hour_offset_zone_lands_on_its_own_half_hour() {
        // 08:00 in Kolkata is 02:30 UTC. Zones whose offset is not a whole hour
        // are where "just add hours" implementations give themselves away.
        let s = FlowSchedule::Cron("0 0 8 * * *".into());
        let tz = Some("Asia/Kolkata");
        let d = decide(
            Some(utc(2026, 8, 26, 2, 30)),
            &s,
            tz,
            utc(2026, 8, 27, 2, 30),
        )
        .expect("due");
        assert_eq!(d.occurrence, utc(2026, 8, 27, 2, 30));
    }

    #[test]
    fn an_unparseable_cron_fires_nothing() {
        // Five-field POSIX crons land here. They are refused at save and skipped
        // loudly by `parse_schedule`; this is the last line of that defence.
        let s = FlowSchedule::Cron("0 8 * * *".into());
        assert_eq!(
            decide(Some(utc(2026, 8, 26, 12, 0)), &s, None, utc(2026, 8, 27, 12, 0)),
            None
        );
    }
}
