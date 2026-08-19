//! Secret scrubbing, applied to every memory before it reaches the log.
//!
//! The agent holds real credentials (`key_store`, `METALCRAFT_TOKEN`, provider
//! keys) and talks about them in conversation. Turn capture is automatic, so
//! without this a single "here's my key, try it" message would be distilled into
//! a durable memory and then *re-injected into the prompt on every future turn
//! that mentions keys*. That is a worse leak than the original message, because
//! it persists and recurs.
//!
//! Deliberately hand-rolled rather than regex-based: the patterns are prefix and
//! shape checks, which are clearer as code, and it keeps the dependency list at
//! zero additions.
//!
//! The bias is toward **false positives over false negatives** in one direction
//! only — a wrongly-redacted token costs a slightly worse memory; a missed key
//! costs a leaked credential. But the thresholds are still chosen to avoid
//! obvious collisions with legitimate content (see the hex rule).

/// Result of scrubbing one string.
#[derive(Debug, Clone)]
pub struct Redaction {
    pub content: String,
    pub count: usize,
    /// Distinct kinds found, for logging (never the values themselves).
    pub kinds: Vec<String>,
}

impl Redaction {
    pub fn is_clean(&self) -> bool {
        self.count == 0
    }
}

/// Known secret prefixes and the label to replace them with. A prefixed token is
/// only redacted if the tail is long enough to plausibly be a key, so prose like
/// "the sk- prefix" survives.
const PREFIXES: &[(&str, &str, usize)] = &[
    ("sk-", "openai-key", 20),
    ("mck_", "metalcraft-token", 16),
    ("ghp_", "github-token", 16),
    ("gho_", "github-token", 16),
    ("github_pat_", "github-token", 16),
    ("xoxb-", "slack-token", 16),
    ("xoxp-", "slack-token", 16),
    ("AKIA", "aws-key-id", 12),
    ("AIza", "google-key", 20),
];

/// Scrub secrets out of `input`.
pub fn redact(input: &str) -> Redaction {
    let mut kinds: Vec<String> = Vec::new();
    let mut count = 0usize;

    // PEM blocks first — they span lines, so they must be handled before any
    // token-wise pass chews them into unrecognizable pieces.
    let (mut text, pem_hits) = redact_pem(input);
    if pem_hits > 0 {
        count += pem_hits;
        push_kind(&mut kinds, "private-key");
    }

    // Token-wise pass. `split_inclusive` keeps each piece's trailing whitespace,
    // so re-emitting pieces reproduces the original spacing exactly.
    let mut out = String::with_capacity(text.len());
    let mut redact_next = false;
    for raw in text.split_inclusive(char::is_whitespace) {
        let (core, lead, trail) = split_affixes(raw);

        if redact_next && !core.is_empty() {
            out.push_str(lead);
            out.push_str("[REDACTED:bearer-token]");
            out.push_str(trail);
            redact_next = false;
            count += 1;
            push_kind(&mut kinds, "bearer-token");
            continue;
        }

        // `Authorization: Bearer <token>` — the value is in the *next* token.
        if core.eq_ignore_ascii_case("bearer") {
            redact_next = true;
            out.push_str(raw);
            continue;
        }

        match classify(core) {
            Some(kind) => {
                out.push_str(lead);
                out.push_str(&format!("[REDACTED:{kind}]"));
                out.push_str(trail);
                count += 1;
                push_kind(&mut kinds, kind);
            }
            None => out.push_str(raw),
        }
    }
    text = out;

    Redaction { content: text, count, kinds }
}

fn push_kind(kinds: &mut Vec<String>, kind: &str) {
    if !kinds.iter().any(|k| k == kind) {
        kinds.push(kind.to_string());
    }
}

/// What kind of secret this bare token is, if any.
fn classify(core: &str) -> Option<&'static str> {
    if core.len() < 12 {
        return None;
    }
    for (prefix, kind, min_tail) in PREFIXES {
        if let Some(tail) = core.strip_prefix(prefix)
            && tail.len() >= *min_tail
            && tail.chars().all(is_key_char)
        {
            return Some(kind);
        }
    }
    // `KEY=value` / `KEY: value` inline assignments of a secret-looking name.
    if let Some((name, value)) = core.split_once('=')
        && is_secret_name(name)
        && value.len() >= 8
    {
        return Some("credential");
    }
    // A long pure-hex run. Threshold is 48 rather than 40 so a full git SHA
    // (40 hex) — which shows up in perfectly legitimate memories about commits —
    // is not mistaken for a key, while sha256 digests and hex-encoded secrets are.
    if core.len() >= 48 && core.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some("hex-secret");
    }
    None
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_secret_name(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "APIKEY", "credential"]
        .iter()
        .any(|needle| n.contains(&needle.to_ascii_uppercase()))
}

/// Strip surrounding punctuation so `"sk-abc..."` or `(sk-abc...)` still match,
/// returning `(core, leading, trailing)` so the original framing is preserved.
fn split_affixes(raw: &str) -> (&str, &str, &str) {
    let is_affix = |c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '=';
    let start = raw.find(|c: char| !is_affix(c)).unwrap_or(raw.len());
    let end = raw.rfind(|c: char| !is_affix(c)).map(|i| i + raw[i..].chars().next().unwrap().len_utf8()).unwrap_or(start);
    (&raw[start..end], &raw[..start], &raw[end..])
}

/// Replace whole PEM blocks. Returns the text and how many blocks were removed.
fn redact_pem(input: &str) -> (String, usize) {
    const BEGIN: &str = "-----BEGIN";
    const END: &str = "-----END";
    if !input.contains(BEGIN) {
        return (input.to_string(), 0);
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    let mut hits = 0;
    while let Some(start) = rest.find(BEGIN) {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find(END).and_then(|e| after[e..].find("-----").map(|d| e + d + 5).and_then(|p| after[p..].find("-----").map(|q| p + q + 5))) {
            Some(block_end) => {
                out.push_str("[REDACTED:private-key]");
                rest = &after[block_end..];
                hits += 1;
            }
            None => {
                // Unterminated block — redact to end of input rather than leaking a
                // truncated key.
                out.push_str("[REDACTED:private-key]");
                rest = "";
                hits += 1;
                break;
            }
        }
    }
    out.push_str(rest);
    (out, hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_is_untouched() {
        let r = redact("Andrew prefers Rust over Go for pod services.");
        assert!(r.is_clean());
        assert_eq!(r.content, "Andrew prefers Rust over Go for pod services.");
    }

    #[test]
    fn openai_key_is_redacted_in_place() {
        let r = redact("the key is sk-proj-abcdefghijklmnopqrstuvwxyz012345 ok?");
        assert_eq!(r.count, 1);
        assert!(r.content.contains("[REDACTED:openai-key]"));
        assert!(!r.content.contains("abcdefghijkl"));
        assert!(r.content.starts_with("the key is "));
        assert!(r.content.trim_end().ends_with("ok?"));
    }

    #[test]
    fn metalcraft_and_github_tokens_are_redacted() {
        let r = redact("use mck_aaaaaaaaaaaaaaaaaaaa and ghp_bbbbbbbbbbbbbbbbbbbb");
        assert_eq!(r.count, 2);
        assert!(r.content.contains("[REDACTED:metalcraft-token]"));
        assert!(r.content.contains("[REDACTED:github-token]"));
    }

    #[test]
    fn bearer_value_is_redacted_even_when_opaque() {
        let r = redact("Authorization: Bearer abc123def456ghi789jkl");
        assert_eq!(r.count, 1);
        assert!(r.content.contains("Bearer [REDACTED:bearer-token]"));
        assert!(!r.content.contains("abc123def456"));
    }

    #[test]
    fn prose_mentioning_a_prefix_survives() {
        // No long tail ⇒ not a key.
        let r = redact("OpenAI keys start with the sk- prefix");
        assert!(r.is_clean(), "got: {}", r.content);
    }

    #[test]
    fn surrounding_punctuation_is_preserved() {
        let r = redact("(sk-abcdefghijklmnopqrstuvwxyz012345),");
        assert_eq!(r.count, 1);
        assert!(r.content.starts_with('('), "got: {}", r.content);
        assert!(r.content.ends_with("),"), "got: {}", r.content);
    }

    #[test]
    fn git_sha_is_not_mistaken_for_a_secret() {
        // 40 hex chars — a real commit id, and legitimate memory content.
        let r = redact("fixed in 93bf92205485efe508f3ab5b92317488ca3116ff yesterday");
        assert!(r.is_clean(), "git SHAs must survive; got: {}", r.content);
    }

    #[test]
    fn long_hex_secret_is_redacted() {
        let hex = "a".repeat(64);
        let r = redact(&format!("token {hex} here"));
        assert_eq!(r.count, 1);
        assert!(r.content.contains("[REDACTED:hex-secret]"));
    }

    #[test]
    fn inline_assignment_of_a_secret_name_is_redacted() {
        let r = redact("run with OPENAI_API_KEY=supersecretvalue123 set");
        assert_eq!(r.count, 1);
        assert!(r.content.contains("[REDACTED:credential]"));
        assert!(!r.content.contains("supersecretvalue"));
    }

    #[test]
    fn pem_block_is_removed_whole() {
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\nkqhkiG9w0BAQ\n-----END PRIVATE KEY-----";
        let r = redact(&format!("here it is:\n{pem}\nthanks"));
        assert_eq!(r.count, 1);
        assert!(r.content.contains("[REDACTED:private-key]"));
        assert!(!r.content.contains("MIIEvQIBADANBg"));
        assert!(r.content.contains("thanks"));
    }

    #[test]
    fn unterminated_pem_does_not_leak_a_partial_key() {
        let r = redact("oops -----BEGIN RSA PRIVATE KEY-----\nMIIEvQIBADANBg");
        assert_eq!(r.count, 1);
        assert!(!r.content.contains("MIIEvQIBADANBg"));
    }

    #[test]
    fn kinds_are_deduplicated() {
        let r = redact("sk-aaaaaaaaaaaaaaaaaaaaaaaa and sk-bbbbbbbbbbbbbbbbbbbbbbbb");
        assert_eq!(r.count, 2);
        assert_eq!(r.kinds, vec!["openai-key".to_string()]);
    }

    #[test]
    fn multiline_content_keeps_its_shape() {
        let r = redact("line one\nline two\nline three");
        assert!(r.is_clean());
        assert_eq!(r.content, "line one\nline two\nline three");
    }
}
