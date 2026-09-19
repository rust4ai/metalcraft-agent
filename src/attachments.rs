//! Images a person attached to a chat message.
//!
//! **Why bytes live here and not in the message.** A chat is persisted whole,
//! every turn, to `<data>/chats/<id>.json`, and `GET /api/v1/chats/{id}` returns
//! every message in it. Inlining a base64 image would therefore grow the file by
//! ~1.37× the photo *and* re-send every photo ever attached each time a client
//! opens the conversation — un-cacheable, and impossible to thumbnail without
//! decoding the whole payload. A message carries an id; the bytes are fetched
//! once and cached forever, because an attachment never changes.
//!
//! **Lifetime is the chat's.** Attachments are stored under the chat that owns
//! them and deleted with it. There is deliberately no "unclaimed" sweep and no
//! cap-driven pruning: an attachment that an old message points at must still
//! render, so quietly deleting one would put a broken image in a transcript that
//! is otherwise permanent. The ceiling is enforced by *refusing* an upload
//! ([`StoreError::ChatFull`]), which is a thing a client can show someone.
//!
//! **The mime is sniffed, never trusted.** `Content-Type` is a claim by the
//! caller, and the mime ends up inside the `data:` URL sent to the provider, so
//! a wrong one is a request that fails at OpenAI rather than at the pod. The
//! allowlist is exactly what the Responses API accepts as `input_image`: JPEG,
//! PNG, WebP and GIF. HEIC is recognised solely so the refusal can say "transcode
//! it" instead of "unsupported file".

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One stored attachment: what it is, and which conversation owns it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, utoipa::ToSchema)]
pub struct Attachment {
    pub id: String,
    /// The image's media type, sniffed from the bytes. Always one of the four
    /// the provider accepts.
    pub mime: String,
    pub size_bytes: u64,
    pub created_at: String,
}

/// Why an upload was refused. Each maps to exactly one HTTP answer, and each
/// says something a client can act on.
#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// A zero-byte body. Almost always a client bug, never a valid image.
    Empty,
    TooLarge { size: usize, max: usize },
    /// Bytes that are not one of the four accepted image types. `detail` names
    /// the case when it is worth naming — HEIC, mostly, which iPhones produce by
    /// default and which the Responses API does not take.
    Unsupported { detail: String },
    /// This conversation is already holding as much as it is allowed to.
    ChatFull { used: u64, max: u64 },
    Io(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "an attachment cannot be empty"),
            Self::TooLarge { size, max } => write!(
                f,
                "attachment is {size} bytes; this pod accepts up to {max}. Scale it down before sending."
            ),
            Self::Unsupported { detail } => write!(f, "{detail}"),
            Self::ChatFull { used, max } => write!(
                f,
                "this conversation is holding {used} bytes of attachments, and the limit is {max}. Delete the conversation, or start a new one."
            ),
            Self::Io(e) => write!(f, "could not store the attachment: {e}"),
        }
    }
}

/// Everything the provider accepts as an `input_image`, and the one type worth
/// refusing by name.
fn sniff(bytes: &[u8]) -> Result<&'static str, StoreError> {
    let starts = |prefix: &[u8]| bytes.starts_with(prefix);
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return Ok("image/jpeg");
    }
    if starts(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Ok("image/png");
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Ok("image/gif");
    }
    if starts(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Ok("image/webp");
    }
    // ISO-BMFF: the brand at bytes 8..12 tells HEIC/HEIF apart from MP4.
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        if matches!(
            brand,
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1"
        ) {
            return Err(StoreError::Unsupported {
                detail: "HEIC is not accepted by the model. Convert it to JPEG before sending."
                    .into(),
            });
        }
    }
    Err(StoreError::Unsupported {
        detail: "not an image this pod can send to the model — JPEG, PNG, WebP or GIF".into(),
    })
}

fn chat_dir(chat_id: &str) -> Option<PathBuf> {
    safe(chat_id).map(|id| crate::paths::attachments_dir().join(id))
}

/// Reject anything that could escape the attachments directory.
///
/// Both ids reach this module straight off the URL, and both are used to build
/// a path. Every id this pod mints is a UUID, so accepting exactly that shape is
/// free — `..`, `/`, and a leading dot are all rejected by construction.
fn safe(id: &str) -> Option<&str> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then_some(id)
}

/// Store one attachment against a chat, returning what the message will carry.
pub fn store(chat_id: &str, bytes: &[u8]) -> Result<Attachment, StoreError> {
    if bytes.is_empty() {
        return Err(StoreError::Empty);
    }
    let max = crate::resources::max_attachment_bytes();
    if bytes.len() > max {
        return Err(StoreError::TooLarge {
            size: bytes.len(),
            max,
        });
    }
    let mime = sniff(bytes)?;
    let Some(dir) = chat_dir(chat_id) else {
        return Err(StoreError::Io(format!("'{chat_id}' is not a chat id")));
    };

    let budget = crate::resources::max_chat_attachment_bytes() as u64;
    let used = usage_bytes(chat_id);
    if used + bytes.len() as u64 > budget {
        return Err(StoreError::ChatFull { used, max: budget });
    }

    std::fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;
    let id = uuid::Uuid::new_v4().to_string();
    let meta = Attachment {
        id: id.clone(),
        mime: mime.to_string(),
        size_bytes: bytes.len() as u64,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    // Bytes first: a meta file with no bytes behind it would be an attachment
    // that lists but cannot be fetched, and the transcript would point at it.
    std::fs::write(dir.join(format!("{id}.bin")), bytes)
        .map_err(|e| StoreError::Io(e.to_string()))?;
    let json = serde_json::to_string(&meta).map_err(|e| StoreError::Io(e.to_string()))?;
    if let Err(e) = std::fs::write(dir.join(format!("{id}.json")), json) {
        let _ = std::fs::remove_file(dir.join(format!("{id}.bin")));
        return Err(StoreError::Io(e.to_string()));
    }
    Ok(meta)
}

/// What one attachment is, without reading the image itself.
pub fn meta(chat_id: &str, id: &str) -> Option<Attachment> {
    let dir = chat_dir(chat_id)?;
    let id = safe(id)?;
    let raw = std::fs::read(dir.join(format!("{id}.json"))).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// The image itself, with what it is.
pub fn read(chat_id: &str, id: &str) -> Option<(Attachment, Vec<u8>)> {
    let meta = meta(chat_id, id)?;
    let dir = chat_dir(chat_id)?;
    let bytes = std::fs::read(dir.join(format!("{}.bin", meta.id))).ok()?;
    Some((meta, bytes))
}

/// Total bytes this chat is holding. Counts the images, not their metadata.
pub fn usage_bytes(chat_id: &str) -> u64 {
    let Some(dir) = chat_dir(chat_id) else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "bin"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Drop everything a chat was holding. Called when the chat is deleted — the
/// attachments have no other owner and nothing else can reach them.
pub fn delete_chat(chat_id: &str) {
    let Some(dir) = chat_dir(chat_id) else { return };
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("failed to delete attachments for chat {chat_id}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.extend_from_slice(b"JFIF and then some bytes");
        v
    }

    #[test]
    fn sniffs_the_four_accepted_types() {
        assert_eq!(sniff(&jpeg()).unwrap(), "image/jpeg");
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]).unwrap(),
            "image/png"
        );
        assert_eq!(sniff(b"GIF89a....").unwrap(), "image/gif");
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 ").unwrap(), "image/webp");
    }

    #[test]
    fn heic_is_refused_by_name() {
        // An iPhone's default format. The refusal has to say what to do about
        // it, or the person is told their photo is "unsupported" with no way
        // forward.
        let mut heic = vec![0, 0, 0, 0x18];
        heic.extend_from_slice(b"ftypheic");
        heic.extend_from_slice(b"\0\0\0\0");
        let Err(StoreError::Unsupported { detail }) = sniff(&heic) else {
            panic!("HEIC must be refused");
        };
        assert!(detail.contains("JPEG"), "must name the way out: {detail}");
    }

    #[test]
    fn a_renamed_pdf_is_not_an_image() {
        // The mime is sniffed precisely so a client's `Content-Type: image/jpeg`
        // cannot put a non-image into a `data:` URL the provider then rejects.
        assert!(matches!(
            sniff(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3"),
            Err(StoreError::Unsupported { .. })
        ));
    }

    #[test]
    fn ids_that_could_escape_the_directory_are_refused() {
        assert!(safe("..").is_none());
        assert!(safe("a/b").is_none());
        assert!(safe("../../etc/passwd").is_none());
        assert!(safe("6f1e9d4c-1e5a-4a3e-9c6b-2f0f7f2a1b33").is_some());
    }
}
