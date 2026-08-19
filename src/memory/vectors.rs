//! The vector sidecar: an append-only binary file of embeddings, plus the
//! similarity math.
//!
//! Vectors do not live in the JSON log. A 384-dim `f32` embedding is 1.5 KB of
//! binary; base64'd into JSON it would roughly double, and it would bloat every
//! log replay and snapshot read with data that is never inspected by eye. So
//! embeddings get their own file with a compact fixed-shape record, written
//! append-only for the same reason the log is: writing one vector must not
//! rewrite the others.
//!
//! Record layout, little-endian:
//!
//! ```text
//!   u16  dims
//!   u8   id_len          (uuid text is 36 bytes, so one byte is plenty)
//!   [u8] id              id_len bytes, UTF-8
//!   [f32] vec            dims * 4 bytes
//! ```
//!
//! A truncated final record (crash mid-append) stops the read; everything before
//! it is kept. Later records for the same id win, which is what makes re-embedding
//! a memory a plain append.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

/// Cosine similarity of two equal-length vectors. Returns 0.0 for mismatched or
/// zero-magnitude input rather than NaN, so a bad vector can never poison a
/// ranking with an unorderable score.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let mag = na.sqrt() * nb.sqrt();
    if mag == 0.0 { 0.0 } else { dot / mag }
}

/// Append one vector. Callers hold the index write lock, so records never interleave.
pub fn append(path: &Path, id: &str, vec: &[f32]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = BufWriter::new(OpenOptions::new().create(true).append(true).open(path)?);
    write_record(&mut f, id, vec)?;
    f.flush()
}

fn write_record<W: Write>(w: &mut W, id: &str, vec: &[f32]) -> std::io::Result<()> {
    let id_bytes = id.as_bytes();
    if id_bytes.len() > u8::MAX as usize {
        return Err(std::io::Error::other("memory id too long for the vector record header"));
    }
    if vec.len() > u16::MAX as usize {
        return Err(std::io::Error::other("embedding has more dimensions than the record header allows"));
    }
    w.write_all(&(vec.len() as u16).to_le_bytes())?;
    w.write_all(&[id_bytes.len() as u8])?;
    w.write_all(id_bytes)?;
    for v in vec {
        w.write_all(&v.to_le_bytes())?;
    }
    Ok(())
}

/// Read every intact record. Later records for an id replace earlier ones.
///
/// Returns the map plus the number of bytes that could not be parsed as a
/// complete record (a torn tail), so the caller can log it once.
pub fn load(path: &Path) -> (HashMap<String, Vec<f32>>, usize) {
    let Ok(file) = File::open(path) else {
        return (HashMap::new(), 0);
    };
    let mut r = BufReader::new(file);
    let mut out: HashMap<String, Vec<f32>> = HashMap::new();

    loop {
        let mut dims_buf = [0u8; 2];
        match r.read_exact(&mut dims_buf) {
            Ok(()) => {}
            // Clean EOF on a record boundary is the normal exit.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return (out, 0),
            Err(_) => return (out, 1),
        }
        let dims = u16::from_le_bytes(dims_buf) as usize;

        let mut len_buf = [0u8; 1];
        if r.read_exact(&mut len_buf).is_err() {
            return (out, 1);
        }
        let mut id_buf = vec![0u8; len_buf[0] as usize];
        if r.read_exact(&mut id_buf).is_err() {
            return (out, 1);
        }
        let Ok(id) = String::from_utf8(id_buf) else {
            return (out, 1);
        };

        let mut vec_buf = vec![0u8; dims * 4];
        if r.read_exact(&mut vec_buf).is_err() {
            return (out, 1);
        }
        let vec: Vec<f32> = vec_buf
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        out.insert(id, vec);
    }
}

/// Rewrite the file from scratch, dropping ids that no longer exist and
/// collapsing superseded records. Atomic (tmp + rename), like the snapshot.
///
/// This is what stops the file growing without bound as memories are re-embedded
/// and purged; compaction calls it alongside the snapshot write.
pub fn rewrite<'a>(
    path: &Path,
    entries: impl Iterator<Item = (&'a String, &'a Vec<f32>)>,
) -> std::io::Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("bin.tmp");
    let mut written = 0usize;
    {
        let mut w = BufWriter::new(File::create(&tmp)?);
        for (id, vec) in entries {
            write_record(&mut w, id, vec)?;
            written += 1;
        }
        w.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mem-vec-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        let v = vec![0.1, 0.2, 0.3, 0.4];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_of_orthogonal_is_zero_and_opposite_is_negative() {
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_degrades_to_zero_rather_than_nan() {
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), 0.0, "length mismatch");
        assert_eq!(cosine(&[], &[]), 0.0, "empty");
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0, "zero magnitude");
        assert!(!cosine(&[0.0, 0.0], &[0.0, 0.0]).is_nan());
    }

    #[test]
    fn append_then_load_round_trips() {
        let dir = tmpdir();
        let p = dir.join("vectors.bin");
        append(&p, "id-one", &[1.0, 2.0, 3.0]).unwrap();
        append(&p, "id-two", &[-0.5, 0.25, 0.125]).unwrap();

        let (map, torn) = load(&p);
        assert_eq!(torn, 0);
        assert_eq!(map.len(), 2);
        assert_eq!(map["id-one"], vec![1.0, 2.0, 3.0]);
        assert_eq!(map["id-two"], vec![-0.5, 0.25, 0.125]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_later_record_supersedes_an_earlier_one() {
        let dir = tmpdir();
        let p = dir.join("vectors.bin");
        append(&p, "same", &[1.0, 1.0]).unwrap();
        append(&p, "same", &[9.0, 9.0]).unwrap();
        let (map, _) = load(&p);
        assert_eq!(map.len(), 1);
        assert_eq!(map["same"], vec![9.0, 9.0], "re-embedding is a plain append");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_torn_trailing_record_is_reported_and_earlier_ones_survive() {
        let dir = tmpdir();
        let p = dir.join("vectors.bin");
        append(&p, "good", &[1.0, 2.0]).unwrap();
        // Header claiming 4 dims, then not enough bytes for them.
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&4u16.to_le_bytes()).unwrap();
        f.write_all(&[3u8]).unwrap();
        f.write_all(b"abc").unwrap();
        f.write_all(&[0u8; 5]).unwrap(); // 5 bytes where 16 are needed
        drop(f);

        let (map, torn) = load(&p);
        assert_eq!(torn, 1, "the truncated record is reported");
        assert_eq!(map.len(), 1);
        assert_eq!(map["good"], vec![1.0, 2.0]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tmpdir();
        let (map, torn) = load(&dir.join("absent.bin"));
        assert!(map.is_empty());
        assert_eq!(torn, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rewrite_drops_stale_ids_and_leaves_no_tmp_file() {
        let dir = tmpdir();
        let p = dir.join("vectors.bin");
        append(&p, "keep", &[1.0, 2.0]).unwrap();
        append(&p, "drop", &[3.0, 4.0]).unwrap();
        append(&p, "drop", &[5.0, 6.0]).unwrap();

        let mut live: HashMap<String, Vec<f32>> = HashMap::new();
        live.insert("keep".into(), vec![1.0, 2.0]);
        let written = rewrite(&p, live.iter()).unwrap();
        assert_eq!(written, 1);
        assert!(!dir.join("vectors.bin.tmp").exists());

        let (map, torn) = load(&p);
        assert_eq!(torn, 0);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("keep"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn realistic_384_dim_vector_round_trips() {
        let dir = tmpdir();
        let p = dir.join("vectors.bin");
        let v: Vec<f32> = (0..384).map(|i| (i as f32) / 384.0 - 0.5).collect();
        append(&p, "d0e7f2a1-0000-4000-8000-000000000001", &v).unwrap();
        let (map, torn) = load(&p);
        assert_eq!(torn, 0);
        let back = &map["d0e7f2a1-0000-4000-8000-000000000001"];
        assert_eq!(back.len(), 384);
        assert!((cosine(back, &v) - 1.0).abs() < 1e-6);
        // 2 + 1 + 36 + 384*4 bytes
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 2 + 1 + 36 + 384 * 4);
        std::fs::remove_dir_all(&dir).ok();
    }
}
