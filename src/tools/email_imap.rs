//! Native read-only IMAP email tools.
//!
//! IMAP is not HTTP, so the declarative HTTP-API tool framework
//! ([`crate::tools::http_api`]) cannot speak it — hence these purpose-built
//! native tools (the same reasoning as [`crate::tools::s3`], whose S3 SigV4
//! signing the HTTP framework can't produce). They are **read-only**: every
//! session uses IMAP `EXAMINE` (SELECT without write access) and `BODY.PEEK` /
//! `RFC822.HEADER` fetches, so nothing in the mailbox is ever mutated — no
//! `\Seen` flags, no deletes.
//!
//! Generic across any IMAP provider (Gmail, Fastmail, Zoho, self-hosted).
//! Credentials come from the key store / environment (see [`crate::key_store`]):
//!   * `IMAP_HOST`     — server host, e.g. `imap.gmail.com`
//!   * `IMAP_USER`     — full email address / login
//!   * `IMAP_PASSWORD` — password or app password (Gmail requires an App Password)
//!   * `IMAP_PORT`     — optional, defaults to 993 (implicit TLS)
//!
//! The `imap` crate is blocking, so each tool runs its IMAP work inside
//! `tokio::task::spawn_blocking`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mail_parser::MessageParser;

const DEFAULT_PORT: u16 = 993;
const DEFAULT_MAILBOX: &str = "INBOX";
const SNIPPET_CHARS: usize = 280;

fn err(tool: &str, message: impl std::fmt::Display) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: message.to_string(),
    }
}

type ImapSession = imap::Session<native_tls::TlsStream<std::net::TcpStream>>;

struct Creds {
    host: String,
    port: u16,
    user: String,
    password: String,
}

/// Read the IMAP credentials from the key store. Returns a user-facing error
/// naming exactly which key is missing.
fn creds(tool: &str) -> metalcraft::Result<Creds> {
    let host = crate::key_store::lookup("IMAP_HOST")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(tool, "IMAP_HOST is not set (e.g. imap.gmail.com)"))?;
    let user = crate::key_store::lookup("IMAP_USER")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(tool, "IMAP_USER is not set (your full email address)"))?;
    let password = crate::key_store::lookup("IMAP_PASSWORD")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(tool, "IMAP_PASSWORD is not set (use an App Password for Gmail)"))?;
    let port = crate::key_store::lookup("IMAP_PORT")
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    Ok(Creds { host, port, user, password })
}

/// Connect over implicit TLS and log in. Returns a logged-in session; the caller
/// selects a mailbox (always via `examine`, read-only) as needed.
fn connect(c: &Creds) -> Result<ImapSession, String> {
    let tls = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| format!("building TLS connector: {e}"))?;
    let client = imap::connect((c.host.as_str(), c.port), c.host.as_str(), &tls)
        .map_err(|e| format!("connecting to {}:{}: {e}", c.host, c.port))?;
    client
        .login(&c.user, &c.password)
        .map_err(|(e, _)| format!("IMAP login failed (check user + App Password / 2FA): {e}"))
}

/// Parse a fetched header blob (RFC822.HEADER) into a compact JSON summary row.
fn header_row(uid: u32, header_bytes: &[u8], internal: Option<DateTime<Utc>>) -> serde_json::Value {
    let parsed = MessageParser::default().parse(header_bytes);
    let (from_addr, from_name, subject, date) = match &parsed {
        Some(m) => {
            let from = m.from().and_then(|a| a.first());
            let sent = m
                .date()
                .and_then(|d| DateTime::<Utc>::from_timestamp(d.to_timestamp(), 0))
                .or(internal);
            (
                from.and_then(|a| a.address()).map(str::to_string),
                from.and_then(|a| a.name()).map(str::to_string),
                m.subject().map(str::to_string),
                sent,
            )
        }
        None => (None, None, None, internal),
    };
    serde_json::json!({
        "uid": uid,
        "from_addr": from_addr,
        "from_name": from_name,
        "subject": subject,
        "date": date.map(|d| d.to_rfc3339()),
    })
}

/// Build a compact `uid_fetch` sequence string ("1,5,9") from a set of UIDs,
/// keeping only the most recent `limit` (highest UIDs), sorted ascending.
fn top_uids(uids: impl IntoIterator<Item = u32>, limit: usize) -> Vec<u32> {
    let mut v: Vec<u32> = uids.into_iter().collect();
    v.sort_unstable();
    if v.len() > limit {
        v = v.split_off(v.len() - limit);
    }
    v
}

/// Fetch header rows for the given UIDs in one mailbox (read-only). Shared by
/// search and list_recent.
fn fetch_header_rows(
    session: &mut ImapSession,
    mailbox: &str,
    uids: &[u32],
) -> Result<Vec<serde_json::Value>, String> {
    session
        .examine(mailbox)
        .map_err(|e| format!("EXAMINE {mailbox}: {e}"))?;
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let seq = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
    let fetches = session
        .uid_fetch(&seq, "(UID INTERNALDATE RFC822.HEADER)")
        .map_err(|e| format!("UID FETCH: {e}"))?;
    let mut rows = Vec::new();
    for f in fetches.iter() {
        let uid = match f.uid {
            Some(u) => u,
            None => continue,
        };
        let internal = f.internal_date().map(|d| d.with_timezone(&Utc));
        let header = f.header().unwrap_or(&[]);
        rows.push(header_row(uid, header, internal));
    }
    // Newest first.
    rows.sort_by(|a, b| b["uid"].as_u64().cmp(&a["uid"].as_u64()));
    Ok(rows)
}

/// Escape a value for use inside an IMAP quoted string (search criteria).
fn imap_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

// ---------------------------------------------------------------------------
// email_list_mailboxes
// ---------------------------------------------------------------------------

pub struct EmailListMailboxesTool;

#[async_trait]
impl metalcraft::Tool for EmailListMailboxesTool {
    fn name(&self) -> &str { "email_list_mailboxes" }
    fn description(&self) -> &str {
        "List the mailboxes (folders) available on the IMAP account, e.g. 'INBOX', '[Gmail]/Sent Mail'. The cheapest way to confirm the IMAP_HOST/IMAP_USER/IMAP_PASSWORD credentials work before reading mail. Read-only. Takes no parameters. Returns an array of mailbox names."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let tool = self.name();
        let c = creds(tool)?;
        let res = tokio::task::spawn_blocking(move || -> Result<Vec<String>, String> {
            let mut session = connect(&c)?;
            let names = session
                .list(Some(""), Some("*"))
                .map_err(|e| format!("LIST: {e}"))?;
            let out = names.iter().map(|n| n.name().to_string()).collect();
            session.logout().ok();
            Ok(out)
        })
        .await
        .map_err(|e| err(tool, format!("task join error: {e}")))?
        .map_err(|e| err(tool, e))?;
        Ok(serde_json::json!({ "count": res.len(), "mailboxes": res }))
    }
}

// ---------------------------------------------------------------------------
// email_search
// ---------------------------------------------------------------------------

pub struct EmailSearchTool;

#[async_trait]
impl metalcraft::Tool for EmailSearchTool {
    fn name(&self) -> &str { "email_search" }
    fn description(&self) -> &str {
        "Search a mailbox and return matching message headers (read-only), newest first: { uid, from_addr, from_name, subject, date }. Combine any of `from`, `subject`, `text` (full-text/body), and `since` (only mail on/after this date; format 'DD-Mon-YYYY' e.g. '01-Jul-2026'); all given criteria must match (AND). `mailbox` defaults to 'INBOX'. `limit` caps results (default 25, most recent). The returned `uid` is what email_get_message needs to read a full message. Fetches headers only, so it's light."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "mailbox": { "type": "string", "description": "Mailbox to search. Default 'INBOX'." },
                "from": { "type": "string", "description": "Match messages whose From contains this (address or name)." },
                "subject": { "type": "string", "description": "Match messages whose Subject contains this." },
                "text": { "type": "string", "description": "Full-text match anywhere in the message (headers + body)." },
                "since": { "type": "string", "description": "Only messages on/after this date. IMAP date format 'DD-Mon-YYYY', e.g. '01-Jul-2026'." },
                "limit": { "type": "integer", "description": "Max results, most recent first (default 25)." }
            }
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let tool = self.name();
        let c = creds(tool)?;
        let mailbox = args.get("mailbox").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_MAILBOX).to_string();
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(25).max(1) as usize;

        // Assemble IMAP search criteria; ALL is the neutral base if none given.
        let mut criteria: Vec<String> = Vec::new();
        if let Some(v) = args.get("from").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            criteria.push(format!("FROM {}", imap_quote(v)));
        }
        if let Some(v) = args.get("subject").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            criteria.push(format!("SUBJECT {}", imap_quote(v)));
        }
        if let Some(v) = args.get("text").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            criteria.push(format!("TEXT {}", imap_quote(v)));
        }
        if let Some(v) = args.get("since").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            // Date is a bare IMAP token (not quoted), e.g. SINCE 01-Jul-2026.
            criteria.push(format!("SINCE {v}"));
        }
        let query = if criteria.is_empty() { "ALL".to_string() } else { criteria.join(" ") };

        let res = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>, String> {
            let mut session = connect(&c)?;
            session.examine(&mailbox).map_err(|e| format!("EXAMINE {mailbox}: {e}"))?;
            let found = session.uid_search(&query).map_err(|e| format!("UID SEARCH ({query}): {e}"))?;
            let uids = top_uids(found, limit);
            let rows = fetch_header_rows(&mut session, &mailbox, &uids)?;
            session.logout().ok();
            Ok(rows)
        })
        .await
        .map_err(|e| err(tool, format!("task join error: {e}")))?
        .map_err(|e| err(tool, e))?;
        Ok(serde_json::json!({ "count": res.len(), "query": query_echo(&args), "messages": res }))
    }
}

/// Echo the effective search back to the caller for transparency.
fn query_echo(args: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "mailbox": args.get("mailbox").and_then(|v| v.as_str()).unwrap_or(DEFAULT_MAILBOX),
        "from": args.get("from"),
        "subject": args.get("subject"),
        "text": args.get("text"),
        "since": args.get("since"),
    })
}

// ---------------------------------------------------------------------------
// email_list_recent
// ---------------------------------------------------------------------------

pub struct EmailListRecentTool;

#[async_trait]
impl metalcraft::Tool for EmailListRecentTool {
    fn name(&self) -> &str { "email_list_recent" }
    fn description(&self) -> &str {
        "List the most recent messages in a mailbox (read-only), newest first: { uid, from_addr, from_name, subject, date }. `mailbox` defaults to 'INBOX'. `hours` limits to mail received in the last N hours (default 24). `limit` caps how many are returned (default 50, most recent). Headers only — use email_get_message with a uid to read a full message."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "mailbox": { "type": "string", "description": "Mailbox to read. Default 'INBOX'." },
                "hours": { "type": "integer", "description": "Look back this many hours (default 24)." },
                "limit": { "type": "integer", "description": "Max messages, most recent first (default 50)." }
            }
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let tool = self.name();
        let c = creds(tool)?;
        let mailbox = args.get("mailbox").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_MAILBOX).to_string();
        let hours = args.get("hours").and_then(|v| v.as_i64()).unwrap_or(24).max(1);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50).max(1) as usize;
        let cutoff = Utc::now() - chrono::Duration::hours(hours);
        // IMAP SINCE is date-granular; refine to the exact window after fetch.
        let since = cutoff.format("%d-%b-%Y").to_string();
        let mailbox_out = mailbox.clone();

        let res = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>, String> {
            let mut session = connect(&c)?;
            session.examine(&mailbox).map_err(|e| format!("EXAMINE {mailbox}: {e}"))?;
            let found = session
                .uid_search(format!("SINCE {since}"))
                .map_err(|e| format!("UID SEARCH: {e}"))?;
            let uids = top_uids(found, limit);
            let rows = fetch_header_rows(&mut session, &mailbox, &uids)?;
            session.logout().ok();
            // Refine to the precise cutoff (drop rows older than `hours`).
            let refined = rows
                .into_iter()
                .filter(|r| {
                    r["date"].as_str()
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|d| d.with_timezone(&Utc) >= cutoff)
                        .unwrap_or(true)
                })
                .collect();
            Ok(refined)
        })
        .await
        .map_err(|e| err(tool, format!("task join error: {e}")))?
        .map_err(|e| err(tool, e))?;
        Ok(serde_json::json!({ "count": res.len(), "mailbox": mailbox_out, "hours": hours, "messages": res }))
    }
}

// ---------------------------------------------------------------------------
// email_get_message
// ---------------------------------------------------------------------------

pub struct EmailGetMessageTool;

#[async_trait]
impl metalcraft::Tool for EmailGetMessageTool {
    fn name(&self) -> &str { "email_get_message" }
    fn description(&self) -> &str {
        "Fetch one full message by its `uid` in a mailbox (read-only): { uid, message_id, from_addr, from_name, to, subject, date, body_text, snippet }. Get the uid from email_search or email_list_recent. `mailbox` defaults to 'INBOX'. Returns the plain-text body (HTML-only mails may have an empty body_text)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "uid": { "type": "integer", "description": "The message UID (from email_search / email_list_recent)." },
                "mailbox": { "type": "string", "description": "Mailbox the uid belongs to. Default 'INBOX'." }
            },
            "required": ["uid"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let tool = self.name();
        let uid = args.get("uid").and_then(|v| v.as_u64())
            .ok_or_else(|| err(tool, "missing required integer `uid`"))? as u32;
        let mailbox = args.get("mailbox").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_MAILBOX).to_string();
        let c = creds(tool)?;

        let res = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
            let mut session = connect(&c)?;
            session.examine(&mailbox).map_err(|e| format!("EXAMINE {mailbox}: {e}"))?;
            let fetches = session
                .uid_fetch(uid.to_string(), "(UID INTERNALDATE RFC822)")
                .map_err(|e| format!("UID FETCH: {e}"))?;
            let f = fetches.iter().next().ok_or_else(|| format!("no message with uid {uid} in {mailbox}"))?;
            let internal = f.internal_date().map(|d| d.with_timezone(&Utc));
            let raw = f.body().ok_or_else(|| "message had no body".to_string())?;
            let parsed = MessageParser::default().parse(raw)
                .ok_or_else(|| "failed to parse message".to_string())?;

            let from = parsed.from().and_then(|a| a.first());
            let to: Vec<String> = parsed.to()
                .map(|addrs| addrs.iter().filter_map(|a| a.address().map(str::to_string)).collect())
                .unwrap_or_default();
            let sent_at = parsed.date()
                .and_then(|d| DateTime::<Utc>::from_timestamp(d.to_timestamp(), 0))
                .or(internal);
            let body_text = parsed.body_text(0).map(|c| c.into_owned());
            let snippet = body_text.as_deref().map(|b| {
                b.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(SNIPPET_CHARS).collect::<String>()
            });
            let out = serde_json::json!({
                "uid": uid,
                "message_id": parsed.message_id(),
                "from_addr": from.and_then(|a| a.address()),
                "from_name": from.and_then(|a| a.name()),
                "to": to,
                "subject": parsed.subject(),
                "date": sent_at.map(|d| d.to_rfc3339()),
                "body_text": body_text,
                "snippet": snippet,
            });
            session.logout().ok();
            Ok(out)
        })
        .await
        .map_err(|e| err(tool, format!("task join error: {e}")))?
        .map_err(|e| err(tool, e))?;
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imap_quote_escapes_quotes_and_backslashes() {
        assert_eq!(imap_quote("hi"), "\"hi\"");
        assert_eq!(imap_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(imap_quote("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn top_uids_keeps_highest_sorted_ascending() {
        assert_eq!(top_uids(vec![3u32, 1, 9, 5, 7], 3), vec![5, 7, 9]);
        assert_eq!(top_uids(vec![2u32, 1], 10), vec![1, 2]);
        assert!(top_uids(Vec::<u32>::new(), 5).is_empty());
    }

    #[test]
    fn header_row_parses_from_and_subject() {
        let raw = b"From: Jane Doe <jane@example.com>\r\nSubject: Hello there\r\nDate: Wed, 01 Jul 2026 10:00:00 +0000\r\n\r\n";
        let row = header_row(42, raw, None);
        assert_eq!(row["uid"], 42);
        assert_eq!(row["from_addr"], "jane@example.com");
        assert_eq!(row["from_name"], "Jane Doe");
        assert_eq!(row["subject"], "Hello there");
        assert!(row["date"].as_str().unwrap().starts_with("2026-07-01"));
    }
}
