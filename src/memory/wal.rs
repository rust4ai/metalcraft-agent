//! The append-only event log and its periodic snapshot.
//!
//! Durability model, and why it is shaped this way:
//!
//! * **Writes are one `O_APPEND` line.** A turn must be able to record something
//!   without rewriting the store, so the hot path never pays O(n).
//! * **Boot is snapshot + tail replay.** The dream writes a fresh snapshot and
//!   truncates the log nightly ([`super::dream`]), so the tail holds at most a
//!   day of events. A busy agent that outgrows that between dreams compacts on
//!   load as well — see `instance::load_delta`.
//! * **A torn final line is skipped, not fatal.** A crash mid-append leaves a
//!   partial JSON line; it fails to parse and is dropped with a `warn`. This is
//!   the same tolerance `scheduled_tasks::load_unlocked` shows for a corrupt
//!   file — a damaged store degrades, it does not prevent boot.
//! * **Snapshots are written tmp-then-rename**, the atomic-write idiom from
//!   `key_store::save`, so an interrupted snapshot can never truncate the good one.
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use super::types::{Event, Snapshot};

/// Append one event as a single JSON line. Callers hold the index write lock, so
/// interleaving is impossible within this process.
pub fn append(path: &Path, event: &Event) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Append and `fsync`. Used for writes a human explicitly asked for
/// (`mem_remember`), where losing the last few seconds is not acceptable. Bulk
/// machine-generated writes use [`append`] and accept the OS buffer.
pub fn append_durable(path: &Path, event: &Event) -> std::io::Result<()> {
    append(path, event)?;
    // Reopening to sync is cheaper than holding a handle across the process and
    // is only on the explicit-write path, which is rare.
    let f = File::open(path)?;
    f.sync_all()
}

/// Replay every parseable event from the log, in file order.
///
/// Returns the events plus the number of unparseable lines skipped, so the caller
/// can log a single summary rather than one line per casualty. A missing file is
/// not an error — it is a store that has never been written.
pub fn replay(path: &Path) -> (Vec<Event>, usize) {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return (Vec::new(), 0),
    };
    let mut events = Vec::new();
    let mut skipped = 0usize;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            skipped += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(&line) {
            Ok(e) => events.push(e),
            Err(_) => skipped += 1,
        }
    }
    (events, skipped)
}

/// Number of lines currently in the log (for `mem_stats`; cheap enough at the
/// sizes involved, and only called on demand).
pub fn count(path: &Path) -> u64 {
    match File::open(path) {
        Ok(f) => BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .count() as u64,
        Err(_) => 0,
    }
}

/// Read the snapshot, if one exists and parses. A corrupt snapshot is treated as
/// absent: the log replay alone then rebuilds whatever it can.
pub fn read_snapshot(path: &Path) -> Option<Snapshot> {
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Snapshot>(&raw) {
        Ok(s) => Some(s),
        Err(e) => {
            log::warn!(
                "memory: snapshot at {} is corrupt ({e}); rebuilding from log",
                path.display()
            );
            None
        }
    }
}

/// Write the snapshot atomically (tmp + rename).
pub fn write_snapshot(path: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(snapshot).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// Drop the log after its contents have been folded into a snapshot.
///
/// Ordering matters and is the caller's responsibility: write the snapshot
/// **first**, then truncate. A crash between the two replays a few already-applied
/// events, which is harmless because every event is idempotent under
/// `MemoryIndex::apply`. A crash in the other order would lose them.
pub fn truncate(path: &Path) -> std::io::Result<()> {
    match OpenOptions::new().write(true).truncate(true).open(path) {
        Ok(_) => Ok(()),
        // Nothing to truncate is success.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::{Memory, MemoryKind, Source};
    use chrono::Utc;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mem-log-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn upsert(seq: u64, content: &str) -> Event {
        Event::Upsert {
            seq,
            at: Utc::now(),
            memory: Box::new(Memory::new(MemoryKind::Semantic, content, Source::Tool)),
        }
    }

    #[test]
    fn append_then_replay_round_trips_in_order() {
        let dir = tmpdir();
        let p = dir.join("log.jsonl");
        append(&p, &upsert(1, "one")).unwrap();
        append(&p, &upsert(2, "two")).unwrap();
        append(&p, &upsert(3, "three")).unwrap();

        let (events, skipped) = replay(&p);
        assert_eq!(skipped, 0);
        assert_eq!(events.len(), 3);
        assert_eq!(
            events.iter().map(|e| e.seq()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_torn_final_line_is_skipped_not_fatal() {
        let dir = tmpdir();
        let p = dir.join("log.jsonl");
        append(&p, &upsert(1, "good")).unwrap();
        append(&p, &upsert(2, "also good")).unwrap();
        // Simulate a crash mid-append: a partial JSON line with no newline.
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(br#"{"op":"upsert","seq":3,"at":"2026-08-18T00:00:0"#)
            .unwrap();
        drop(f);

        let (events, skipped) = replay(&p);
        assert_eq!(events.len(), 2, "the two intact events must survive");
        assert_eq!(skipped, 1, "the torn line is counted, not panicked on");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn blank_lines_are_ignored_and_not_counted_as_damage() {
        let dir = tmpdir();
        let p = dir.join("log.jsonl");
        append(&p, &upsert(1, "x")).unwrap();
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"\n\n   \n").unwrap();
        drop(f);
        let (events, skipped) = replay(&p);
        assert_eq!(events.len(), 1);
        assert_eq!(skipped, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_log_is_an_empty_store_not_an_error() {
        let dir = tmpdir();
        let (events, skipped) = replay(&dir.join("nope.jsonl"));
        assert!(events.is_empty());
        assert_eq!(skipped, 0);
        assert_eq!(count(&dir.join("nope.jsonl")), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snapshot_round_trips_and_rename_is_atomic() {
        let dir = tmpdir();
        let p = dir.join("snapshot.json");
        let snap = Snapshot {
            seq: 42,
            written_at: Utc::now(),
            memories: vec![Memory::new(
                MemoryKind::Preference,
                "prefers rust",
                Source::User,
            )],
            links: vec![],
        };
        write_snapshot(&p, &snap).unwrap();
        // No stray tmp file left behind.
        assert!(!dir.join("snapshot.json.tmp").exists());

        let back = read_snapshot(&p).unwrap();
        assert_eq!(back.seq, 42);
        assert_eq!(back.memories.len(), 1);
        assert_eq!(back.memories[0].content, "prefers rust");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_snapshot_reads_as_absent() {
        let dir = tmpdir();
        let p = dir.join("snapshot.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(read_snapshot(&p).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_empties_the_log_and_tolerates_a_missing_one() {
        let dir = tmpdir();
        let p = dir.join("log.jsonl");
        append(&p, &upsert(1, "x")).unwrap();
        assert_eq!(count(&p), 1);
        truncate(&p).unwrap();
        assert_eq!(count(&p), 0);
        truncate(&dir.join("absent.jsonl")).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn durable_append_is_readable() {
        let dir = tmpdir();
        let p = dir.join("log.jsonl");
        append_durable(&p, &upsert(1, "explicit")).unwrap();
        let (events, _) = replay(&p);
        assert_eq!(events.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
