//! Native S3-compatible object-storage tools.
//!
//! Any S3-compatible service (AWS S3, Cloudflare R2, DigitalOcean Spaces,
//! Backblaze B2, MinIO, …) speaks the S3 REST API, which authenticates every
//! request with an **AWS Signature Version 4** header computed over the
//! request's method, path, query, headers, and a SHA-256 of the body. The
//! declarative HTTP-API tool framework ([`crate::tools::http_api`]) can only
//! substitute static `$ENV` values into headers, so it cannot produce that
//! per-request signature — hence these purpose-built native tools. They also
//! handle the parts of S3 that are not JSON: raw/binary object bodies and the
//! XML `ListBucket` response.
//!
//! Credentials and endpoint come from the key store / environment (see
//! [`crate::key_store`]):
//!   * `S3_ACCESS_KEY_ID`     — the access key id
//!   * `S3_SECRET_ACCESS_KEY` — the secret access key
//!   * `S3_REGION`            — the SigV4 signing region (default `us-east-1`;
//!                              use `auto` for Cloudflare R2, the datacenter
//!                              slug like `nyc3` for DigitalOcean Spaces)
//!   * `S3_ENDPOINT`          — optional host of the S3 service, e.g.
//!                              `s3.us-east-1.amazonaws.com` (AWS, the default
//!                              when unset), `nyc3.digitaloceanspaces.com` (DO
//!                              Spaces), `<account>.r2.cloudflarestorage.com`
//!                              (R2), or `localhost:9000` (MinIO). May include a
//!                              `http://` / `https://` scheme (defaults to
//!                              https) and a port.
//!
//! Requests use **path-style** addressing (`{scheme}://{endpoint}/{bucket}/{key}`),
//! which keeps the signing host constant and is the most portable form across
//! providers (notably MinIO and R2).
//!
//! Local file arguments (`file_path` on put, `dest_path` on get) are constrained
//! to [`crate::paths::upload_root`], the same jail the multipart upload tool
//! uses, so a tool-calling model can't be steered into reading or overwriting
//! arbitrary local files.

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_REGION: &str = "us-east-1";
const SERVICE: &str = "s3";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// SHA-256 of the empty string — the payload hash for bodyless requests.
const EMPTY_PAYLOAD_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// Cap on how large a get_object result we'll inline as text (no `dest_path`).
const MAX_INLINE_TEXT: usize = 100_000;

fn err(tool: &str, message: impl std::fmt::Display) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: message.to_string(),
    }
}

// ---------------------------------------------------------------------------
// SigV4 primitives
// ---------------------------------------------------------------------------

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encode per RFC 3986 for SigV4. Unreserved characters
/// (`A-Z a-z 0-9 - _ . ~`) pass through; everything else is `%XX`. When
/// `keep_slash` is set, `/` is also left untouched (used for object-key paths,
/// where slashes are real path separators).
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && b == b'/');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Build a canonical query string from already-decoded `(key, value)` pairs:
/// each side URI-encoded, pairs sorted by encoded key (then value), joined by
/// `&`. Returns an empty string for no params.
fn canonical_query(params: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// All the inputs needed to sign one S3 request. `signed_headers` must already
/// be lowercased; `host`, `x-amz-content-sha256`, and `x-amz-date` are added by
/// the caller before signing.
struct SignInputs<'a> {
    access_key: &'a str,
    secret_key: &'a str,
    region: &'a str,
    method: &'a str,
    canonical_uri: &'a str,
    canonical_query: &'a str,
    /// `(lowercased name, value)`, in any order — sorted internally.
    signed_headers: &'a [(String, String)],
    payload_hash: &'a str,
    amz_date: &'a str,   // YYYYMMDDTHHMMSSZ
    date_stamp: &'a str, // YYYYMMDD
}

/// Compute the `Authorization` header value for a SigV4 request, returning
/// `(authorization, signed_headers_list)`. Pure (no I/O, no clock) so it can be
/// checked against AWS's published test vectors.
fn sigv4_authorization(inp: &SignInputs) -> (String, String) {
    let mut headers: Vec<(String, String)> = inp.signed_headers.to_vec();
    headers.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed_headers_list = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        inp.method,
        inp.canonical_uri,
        inp.canonical_query,
        canonical_headers,
        signed_headers_list,
        inp.payload_hash,
    );

    let scope = format!("{}/{}/{}/aws4_request", inp.date_stamp, inp.region, SERVICE);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        inp.amz_date,
        scope,
        sha256_hex(canonical_request.as_bytes()),
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", inp.secret_key).as_bytes(),
        inp.date_stamp.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, inp.region.as_bytes());
    let k_service = hmac_sha256(&k_region, SERVICE.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        inp.access_key, scope, signed_headers_list, signature,
    );
    (authorization, signed_headers_list)
}

// ---------------------------------------------------------------------------
// S3 client
// ---------------------------------------------------------------------------

struct S3 {
    access_key: String,
    secret_key: String,
    region: String,
    /// Endpoint host (`host` or `host:port`), used verbatim in the `Host`
    /// header, the SigV4 signature, and the request URL.
    host: String,
    /// `https` (default) or `http`, taken from an explicit scheme in `S3_ENDPOINT`.
    scheme: String,
}

/// Outcome of a signed S3 request: HTTP status plus the raw response bytes.
struct RawResponse {
    status: u16,
    body: Vec<u8>,
}

/// Split an `S3_ENDPOINT` value into `(scheme, host[:port])`. Accepts a bare
/// host (`s3.example.com`), a host with a scheme (`https://s3.example.com`), and
/// trims a trailing slash. Defaults to `https` when no scheme is given.
fn parse_endpoint(ep: &str) -> (String, String) {
    let ep = ep.trim();
    if let Some(rest) = ep.strip_prefix("https://") {
        ("https".to_string(), rest.trim_end_matches('/').to_string())
    } else if let Some(rest) = ep.strip_prefix("http://") {
        ("http".to_string(), rest.trim_end_matches('/').to_string())
    } else {
        ("https".to_string(), ep.trim_end_matches('/').to_string())
    }
}

impl S3 {
    /// Resolve credentials and endpoint from the key store / environment.
    fn from_env(tool: &str) -> metalcraft::Result<Self> {
        let access_key = crate::key_store::lookup("S3_ACCESS_KEY_ID")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                err(
                    tool,
                    "S3_ACCESS_KEY_ID is not set (add it in the workshop's keys, or export it)",
                )
            })?;
        let secret_key = crate::key_store::lookup("S3_SECRET_ACCESS_KEY")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                err(
                    tool,
                    "S3_SECRET_ACCESS_KEY is not set (add it in the workshop's keys, or export it)",
                )
            })?;
        let region = crate::key_store::lookup("S3_REGION")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_REGION.to_string());
        // Explicit endpoint (any S3-compatible provider) or the AWS default.
        let (scheme, host) = match crate::key_store::lookup("S3_ENDPOINT").filter(|s| !s.is_empty())
        {
            Some(ep) => parse_endpoint(&ep),
            None => ("https".to_string(), format!("s3.{region}.amazonaws.com")),
        };
        Ok(Self {
            access_key,
            secret_key,
            region,
            host,
            scheme,
        })
    }

    /// Sign and send one request. `bucket`/`key` build the path-style URI; an
    /// empty `bucket` targets the service root (list buckets). `query` are
    /// decoded `(name, value)` pairs. `extra_headers` (e.g. content-type,
    /// x-amz-acl) are signed alongside the mandatory ones.
    async fn send(
        &self,
        tool: &str,
        method: &str,
        bucket: &str,
        key: &str,
        query: &[(String, String)],
        extra_headers: &[(String, String)],
        body: Vec<u8>,
    ) -> metalcraft::Result<RawResponse> {
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();

        // Path-style URI: /{bucket}/{key}. Slashes inside the key are real
        // separators, so keep them; everything else is percent-encoded.
        let raw_path = if bucket.is_empty() {
            "/".to_string()
        } else if key.is_empty() {
            format!("/{bucket}")
        } else {
            format!("/{bucket}/{key}")
        };
        let canonical_uri = uri_encode(&raw_path, true);
        let cq = canonical_query(query);

        let payload_hash = if body.is_empty() {
            EMPTY_PAYLOAD_HASH.to_string()
        } else {
            sha256_hex(&body)
        };

        // Mandatory signed headers + any extras (lowercased).
        let mut signed: Vec<(String, String)> = vec![
            ("host".to_string(), self.host.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        for (k, v) in extra_headers {
            signed.push((k.to_lowercase(), v.clone()));
        }

        let (authorization, _) = sigv4_authorization(&SignInputs {
            access_key: &self.access_key,
            secret_key: &self.secret_key,
            region: &self.region,
            method,
            canonical_uri: &canonical_uri,
            canonical_query: &cq,
            signed_headers: &signed,
            payload_hash: &payload_hash,
            amz_date: &amz_date,
            date_stamp: &date_stamp,
        });

        let url = if cq.is_empty() {
            format!("{}://{}{}", self.scheme, self.host, canonical_uri)
        } else {
            format!("{}://{}{}?{}", self.scheme, self.host, canonical_uri, cq)
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .user_agent("metalcraft-agent/0.3 (s3)")
            .build()
            .map_err(|e| err(tool, format!("failed to build HTTP client: {e}")))?;

        let http_method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| err(tool, format!("invalid HTTP method: {method}")))?;
        let mut req = client
            .request(http_method, &url)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", &authorization);
        // Re-send the signed extra headers (host is supplied by reqwest).
        for (k, v) in extra_headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if !body.is_empty() {
            req = req.body(body);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| err(tool, format!("request to {url} failed: {e}")))?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| err(tool, format!("failed to read response body: {e}")))?
            .to_vec();
        Ok(RawResponse { status, body })
    }
}

/// Turn a non-2xx S3 XML error response into a tool error message.
fn s3_error(tool: &str, status: u16, body: &[u8]) -> metalcraft::GraphError {
    let text = String::from_utf8_lossy(body);
    let code = extract_tag(&text, "Code").unwrap_or_default();
    let message = extract_tag(&text, "Message").unwrap_or_default();
    let detail = match (code.is_empty(), message.is_empty()) {
        (false, false) => format!("{code}: {message}"),
        (false, true) => code,
        _ => crate::tools::truncate_output(text.trim(), 1_000),
    };
    err(tool, format!("S3 returned HTTP {status} — {detail}"))
}

// ---------------------------------------------------------------------------
// Tiny XML helpers (avoid pulling in an XML crate for two response shapes)
// ---------------------------------------------------------------------------

/// Return the text of the first `<tag>…</tag>` in `xml`, if present.
fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(unescape_xml(&xml[start..end]))
}

/// Return the text of every `<tag>…</tag>` block in `xml`, in document order.
fn extract_all_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(s) = rest.find(&open) {
        let after = s + open.len();
        let Some(e_rel) = rest[after..].find(&close) else {
            break;
        };
        let e = after + e_rel;
        out.push(&rest[after..e]);
        rest = &rest[e + close.len()..];
    }
    out
}

fn unescape_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

// ---------------------------------------------------------------------------
// Local-path jail (mirrors http_api's upload-root constraint)
// ---------------------------------------------------------------------------

/// Resolve an existing local file for upload, constrained to the upload root.
/// A relative path is taken relative to the upload root; the canonicalized
/// result must live inside it (symlink escapes are rejected too).
fn resolve_read_path(tool: &str, path_str: &str) -> metalcraft::Result<PathBuf> {
    let root = crate::paths::upload_root();
    let canon_root = root.canonicalize().map_err(|e| {
        err(
            tool,
            format!("upload root {} is not accessible: {e}", root.display()),
        )
    })?;
    let joined = join_under(&root, path_str);
    let canon = joined
        .canonicalize()
        .map_err(|e| err(tool, format!("cannot access '{path_str}': {e}")))?;
    if !canon.starts_with(&canon_root) {
        return Err(err(
            tool,
            format!(
                "refusing to read '{path_str}': resolves outside the upload root {}",
                canon_root.display()
            ),
        ));
    }
    Ok(canon)
}

/// Resolve a destination path for download, constrained to the upload root. The
/// file need not exist yet; the parent directory is created and its canonical
/// form must live inside the root.
fn resolve_write_path(tool: &str, path_str: &str) -> metalcraft::Result<PathBuf> {
    let root = crate::paths::upload_root();
    let canon_root = root.canonicalize().map_err(|e| {
        err(
            tool,
            format!("upload root {} is not accessible: {e}", root.display()),
        )
    })?;
    let joined = join_under(&root, path_str);
    if joined.components().any(|c| c == Component::ParentDir) {
        return Err(err(
            tool,
            format!("refusing to write '{path_str}': path traversal ('..') is not allowed"),
        ));
    }
    let parent = joined
        .parent()
        .ok_or_else(|| err(tool, format!("invalid destination path '{path_str}'")))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| err(tool, format!("failed to create {}: {e}", parent.display())))?;
    let canon_parent = parent
        .canonicalize()
        .map_err(|e| err(tool, format!("cannot access destination directory: {e}")))?;
    if !canon_parent.starts_with(&canon_root) {
        return Err(err(
            tool,
            format!(
                "refusing to write '{path_str}': resolves outside the upload root {}",
                canon_root.display()
            ),
        ));
    }
    let file_name = joined
        .file_name()
        .ok_or_else(|| err(tool, format!("destination '{path_str}' has no file name")))?;
    Ok(canon_parent.join(file_name))
}

fn join_under(root: &Path, path_str: &str) -> PathBuf {
    let p = Path::new(path_str);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

fn require_str<'a>(
    tool: &str,
    args: &'a serde_json::Value,
    key: &str,
) -> metalcraft::Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| crate::tools::missing_param(tool, key))
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// `s3_list_objects` — list object keys in a bucket (ListObjectsV2).
pub struct S3ListObjectsTool;

#[async_trait]
impl metalcraft::Tool for S3ListObjectsTool {
    fn name(&self) -> &str {
        "s3_list_objects"
    }
    fn description(&self) -> &str {
        "List objects (files) in an S3 bucket. Requires `bucket`. Optional `prefix` to list only keys under a folder-like path (e.g. 'reports/'), and `max_keys` (default 1000, max 1000). Returns an array of {key, size, last_modified, etag} plus `is_truncated` and a `next_continuation_token` you can pass back as `continuation_token` to page through more than max_keys results."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "bucket": { "type": "string", "description": "The S3 bucket name." },
                "prefix": { "type": "string", "description": "Optional key prefix filter, e.g. 'reports/2026/'." },
                "max_keys": { "type": "integer", "description": "Max keys to return (default 1000, max 1000)." },
                "continuation_token": { "type": "string", "description": "Token from a previous truncated response to fetch the next page." }
            },
            "required": ["bucket"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let tool = self.name();
        let bucket = require_str(tool, &args, "bucket")?;
        let s3 = S3::from_env(tool)?;

        let mut query: Vec<(String, String)> = vec![("list-type".into(), "2".into())];
        if let Some(prefix) = args
            .get("prefix")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            query.push(("prefix".into(), prefix.to_string()));
        }
        if let Some(token) = args
            .get("continuation_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            query.push(("continuation-token".into(), token.to_string()));
        }
        if let Some(max) = args.get("max_keys").and_then(|v| v.as_u64()) {
            query.push(("max-keys".into(), max.min(1000).to_string()));
        }

        let resp = s3
            .send(tool, "GET", bucket, "", &query, &[], Vec::new())
            .await?;
        if !(200..300).contains(&resp.status) {
            return Err(s3_error(tool, resp.status, &resp.body));
        }
        let xml = String::from_utf8_lossy(&resp.body);
        let objects: Vec<serde_json::Value> = extract_all_blocks(&xml, "Contents")
            .into_iter()
            .map(|block| {
                serde_json::json!({
                    "key": extract_tag(block, "Key").unwrap_or_default(),
                    "size": extract_tag(block, "Size").and_then(|s| s.parse::<u64>().ok()),
                    "last_modified": extract_tag(block, "LastModified"),
                    "etag": extract_tag(block, "ETag").map(|e| e.trim_matches('"').to_string()),
                })
            })
            .collect();
        let is_truncated = extract_tag(&xml, "IsTruncated").as_deref() == Some("true");
        Ok(serde_json::json!({
            "bucket": bucket,
            "count": objects.len(),
            "objects": objects,
            "is_truncated": is_truncated,
            "next_continuation_token": extract_tag(&xml, "NextContinuationToken"),
        }))
    }
}

/// `s3_get_object` — download an object, either to a local file (under the
/// upload root) or returned inline as text.
pub struct S3GetObjectTool;

#[async_trait]
impl metalcraft::Tool for S3GetObjectTool {
    fn name(&self) -> &str {
        "s3_get_object"
    }
    fn description(&self) -> &str {
        "Download an object (file) from S3. Requires `bucket` and `key`. If `dest_path` is given, the bytes are written to that local file (path is relative to the agent's upload directory; absolute paths must stay inside it) and the result reports the byte count. If `dest_path` is omitted, the content is returned inline as text (UTF-8 only, up to ~100 KB) — use `dest_path` for binary or large files."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "bucket": { "type": "string", "description": "The S3 bucket name." },
                "key": { "type": "string", "description": "Object key (path within the bucket), e.g. 'reports/q1.pdf'." },
                "dest_path": { "type": "string", "description": "Optional local path (within the upload root) to save the file to. Omit to get small text content inline." }
            },
            "required": ["bucket", "key"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let tool = self.name();
        let bucket = require_str(tool, &args, "bucket")?;
        let key = require_str(tool, &args, "key")?;
        let s3 = S3::from_env(tool)?;

        let resp = s3
            .send(tool, "GET", bucket, key, &[], &[], Vec::new())
            .await?;
        if !(200..300).contains(&resp.status) {
            return Err(s3_error(tool, resp.status, &resp.body));
        }

        match args
            .get("dest_path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            Some(dest) => {
                let path = resolve_write_path(tool, dest)?;
                std::fs::write(&path, &resp.body)
                    .map_err(|e| err(tool, format!("failed to write {}: {e}", path.display())))?;
                Ok(serde_json::json!({
                    "bucket": bucket,
                    "key": key,
                    "saved_to": path.display().to_string(),
                    "bytes": resp.body.len(),
                }))
            }
            None => {
                if resp.body.len() > MAX_INLINE_TEXT {
                    return Err(err(
                        tool,
                        format!(
                            "object is {} bytes — too large to return inline; pass `dest_path` to save it to a file",
                            resp.body.len()
                        ),
                    ));
                }
                match String::from_utf8(resp.body) {
                    Ok(text) => Ok(serde_json::json!({
                        "bucket": bucket,
                        "key": key,
                        "content": text,
                    })),
                    Err(e) => Err(err(
                        tool,
                        format!(
                            "object is not valid UTF-8 ({} bytes) — pass `dest_path` to save the binary file instead",
                            e.into_bytes().len()
                        ),
                    )),
                }
            }
        }
    }
}

/// `s3_put_object` — upload content or a local file to an object key.
pub struct S3PutObjectTool;

#[async_trait]
impl metalcraft::Tool for S3PutObjectTool {
    fn name(&self) -> &str {
        "s3_put_object"
    }
    fn description(&self) -> &str {
        "Upload (write) an object to S3, creating or overwriting it. Requires `bucket` and `key`. Provide exactly one source: `content` (inline text) or `file_path` (a local file within the agent's upload directory). Optional `content_type` (e.g. 'text/plain', 'application/pdf'; defaults to a sensible value) and `acl` ('private' default, or 'public-read' to make the object publicly downloadable). Overwrites silently if the key already exists."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "bucket": { "type": "string", "description": "The S3 bucket name." },
                "key": { "type": "string", "description": "Destination object key, e.g. 'reports/q1.pdf'." },
                "content": { "type": "string", "description": "Inline text content to upload. Use this OR file_path." },
                "file_path": { "type": "string", "description": "Local file (within the upload root) to upload. Use this OR content." },
                "content_type": { "type": "string", "description": "MIME type, e.g. 'text/plain', 'application/json', 'image/png'." },
                "acl": { "type": "string", "description": "'private' (default) or 'public-read'." }
            },
            "required": ["bucket", "key"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let tool = self.name();
        let bucket = require_str(tool, &args, "bucket")?;
        let key = require_str(tool, &args, "key")?;
        let s3 = S3::from_env(tool)?;

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let file_path = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        let (body, default_ct): (Vec<u8>, &str) = match (content, file_path) {
            (Some(_), Some(_)) => {
                return Err(err(
                    tool,
                    "provide either `content` or `file_path`, not both",
                ));
            }
            (Some(text), None) => (text.as_bytes().to_vec(), "text/plain; charset=utf-8"),
            (None, Some(path)) => {
                let resolved = resolve_read_path(tool, path)?;
                let bytes = std::fs::read(&resolved).map_err(|e| {
                    err(tool, format!("failed to read {}: {e}", resolved.display()))
                })?;
                (bytes, "application/octet-stream")
            }
            (None, None) => {
                return Err(err(
                    tool,
                    "missing source: provide `content` (text) or `file_path` (a local file)",
                ));
            }
        };

        let content_type = args
            .get("content_type")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(default_ct)
            .to_string();
        let mut extra = vec![("content-type".to_string(), content_type)];
        if let Some(acl) = args
            .get("acl")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            extra.push(("x-amz-acl".to_string(), acl.to_string()));
        }

        let bytes_len = body.len();
        let resp = s3.send(tool, "PUT", bucket, key, &[], &extra, body).await?;
        if !(200..300).contains(&resp.status) {
            return Err(s3_error(tool, resp.status, &resp.body));
        }
        let public = args.get("acl").and_then(|v| v.as_str()) == Some("public-read");
        Ok(serde_json::json!({
            "bucket": bucket,
            "key": key,
            "bytes_written": bytes_len,
            "public_url": if public {
                Some(format!("{}://{}/{bucket}/{key}", s3.scheme, s3.host))
            } else {
                None
            },
        }))
    }
}

/// `s3_delete_object` — delete an object by key.
pub struct S3DeleteObjectTool;

#[async_trait]
impl metalcraft::Tool for S3DeleteObjectTool {
    fn name(&self) -> &str {
        "s3_delete_object"
    }
    fn description(&self) -> &str {
        "Delete an object (file) from S3. Requires `bucket` and `key`. This is irreversible — confirm the exact bucket and key with the user before deleting. S3 delete is idempotent: deleting a non-existent key still returns success."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "bucket": { "type": "string", "description": "The S3 bucket name." },
                "key": { "type": "string", "description": "Object key to delete." }
            },
            "required": ["bucket", "key"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let tool = self.name();
        let bucket = require_str(tool, &args, "bucket")?;
        let key = require_str(tool, &args, "key")?;
        let s3 = S3::from_env(tool)?;

        let resp = s3
            .send(tool, "DELETE", bucket, key, &[], &[], Vec::new())
            .await?;
        if !(200..300).contains(&resp.status) {
            return Err(s3_error(tool, resp.status, &resp.body));
        }
        Ok(serde_json::json!({ "bucket": bucket, "key": key, "deleted": true }))
    }
}

/// `s3_list_buckets` — list the buckets in the account. Doubles as a cheap
/// credential/connectivity check.
pub struct S3ListBucketsTool;

#[async_trait]
impl metalcraft::Tool for S3ListBucketsTool {
    fn name(&self) -> &str {
        "s3_list_buckets"
    }
    fn description(&self) -> &str {
        "List all buckets in the account for the configured endpoint/region. The cheapest way to confirm the S3_ACCESS_KEY_ID/S3_SECRET_ACCESS_KEY/S3_REGION credentials (and S3_ENDPOINT) work before doing file operations. Takes no parameters. Returns an array of {name, creation_date}."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let tool = self.name();
        let s3 = S3::from_env(tool)?;
        let resp = s3.send(tool, "GET", "", "", &[], &[], Vec::new()).await?;
        if !(200..300).contains(&resp.status) {
            return Err(s3_error(tool, resp.status, &resp.body));
        }
        let xml = String::from_utf8_lossy(&resp.body);
        let buckets: Vec<serde_json::Value> = extract_all_blocks(&xml, "Bucket")
            .into_iter()
            .map(|block| {
                serde_json::json!({
                    "name": extract_tag(block, "Name").unwrap_or_default(),
                    "creation_date": extract_tag(block, "CreationDate"),
                })
            })
            .collect();
        Ok(serde_json::json!({
            "region": s3.region,
            "count": buckets.len(),
            "buckets": buckets,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_empty_matches_constant() {
        assert_eq!(sha256_hex(b""), EMPTY_PAYLOAD_HASH);
    }

    #[test]
    fn uri_encode_keeps_unreserved_and_slash() {
        assert_eq!(
            uri_encode("reports/q1 2026.pdf", true),
            "reports/q1%202026.pdf"
        );
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        assert_eq!(uri_encode("AZaz09-_.~", false), "AZaz09-_.~");
    }

    #[test]
    fn canonical_query_sorts_and_encodes() {
        let q = canonical_query(&[
            ("prefix".into(), "a b".into()),
            ("list-type".into(), "2".into()),
        ]);
        assert_eq!(q, "list-type=2&prefix=a%20b");
    }

    #[test]
    fn parse_endpoint_scheme_and_host() {
        assert_eq!(
            parse_endpoint("s3.example.com"),
            ("https".into(), "s3.example.com".into())
        );
        assert_eq!(
            parse_endpoint("https://nyc3.digitaloceanspaces.com/"),
            ("https".into(), "nyc3.digitaloceanspaces.com".into())
        );
        assert_eq!(
            parse_endpoint("http://localhost:9000"),
            ("http".into(), "localhost:9000".into())
        );
    }

    /// AWS's documented **S3 "GET Object" SigV4 example** (service = `s3`,
    /// region = `us-east-1`) — the gold-standard check that the canonical
    /// request, string-to-sign, signing key, and signature are assembled
    /// correctly. Published expected signature:
    /// f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41
    #[test]
    fn sigv4_matches_aws_s3_get_object_vector() {
        let payload = EMPTY_PAYLOAD_HASH;
        let signed = vec![
            (
                "host".to_string(),
                "examplebucket.s3.amazonaws.com".to_string(),
            ),
            ("range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), payload.to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let (auth, signed_headers) = sigv4_authorization(&SignInputs {
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            method: "GET",
            canonical_uri: "/test.txt",
            canonical_query: "",
            signed_headers: &signed,
            payload_hash: payload,
            amz_date: "20130524T000000Z",
            date_stamp: "20130524",
        });
        assert_eq!(signed_headers, "host;range;x-amz-content-sha256;x-amz-date");
        assert!(
            auth.contains("Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request"),
            "unexpected credential scope: {auth}"
        );
        assert!(
            auth.ends_with(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "unexpected authorization: {auth}"
        );
    }

    #[test]
    fn extract_tag_and_blocks() {
        let xml = "<ListBucketResult><Contents><Key>a.txt</Key><Size>10</Size></Contents>\
                   <Contents><Key>b.txt</Key><Size>20</Size></Contents>\
                   <IsTruncated>false</IsTruncated></ListBucketResult>";
        let blocks = extract_all_blocks(xml, "Contents");
        assert_eq!(blocks.len(), 2);
        assert_eq!(extract_tag(blocks[0], "Key").as_deref(), Some("a.txt"));
        assert_eq!(extract_tag(blocks[1], "Size").as_deref(), Some("20"));
        assert_eq!(extract_tag(xml, "IsTruncated").as_deref(), Some("false"));
    }

    #[test]
    fn unescape_xml_entities() {
        assert_eq!(unescape_xml("a&amp;b &lt;c&gt;"), "a&b <c>");
    }
}
