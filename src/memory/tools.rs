//! The `mem_*` agent tools.
//!
//! These are the *explicit* surface: the agent deciding, in the moment, to save
//! or look something up. Automatic recall injection and automatic turn capture
//! land in later phases and do not go through these tools — which is why the
//! write tools being approval-gated is not a usability problem.
//!
//! Conventions follow the core tools (`read_file`, `grep`), not the pack tools:
//! a bad parameter is a `GraphError::ToolCallFailed` via
//! [`crate::tools::missing_param`], and a successful call returns a bare JSON
//! object rather than an HTTP-style `{status, data}` envelope.
use async_trait::async_trait;
use serde_json::{Value, json};

use super::recall::{Mode, RecallOptions};
use super::types::{Memory, MemoryKind, Source};
use super::{RememberRequest, forget, get, recall, remember, stats};

/// Every tool name this module contributes, for registry wiring.
pub const TOOL_NAMES: &[&str] = &[
    "mem_remember",
    "mem_search",
    "mem_get",
    "mem_forget",
    "mem_stats",
];

/// The compact projection of a memory used in list results — full content is
/// only returned by `mem_get`, so a broad search can't flood the context.
fn brief(m: &Memory, score: Option<f32>) -> Value {
    let text = m.display_text();
    let snippet: String = if text.chars().count() > 240 {
        format!("{}…", text.chars().take(240).collect::<String>())
    } else {
        text.to_string()
    };
    let mut v = json!({
        "id": m.id,
        "kind": m.kind.as_str(),
        "importance": m.importance,
        "created_at": m.created_at.to_rfc3339(),
        "text": snippet,
    });
    if m.pinned {
        v["pinned"] = json!(true);
    }
    if let Some(e) = &m.entity {
        v["entity"] = json!(e);
    }
    if let Some(s) = score {
        // Round in f64, not f32: serializing a rounded f32 widens back to noise
        // like 1.2350000143051147, which is unreadable in tool output.
        v["score"] = json!(((s as f64) * 1000.0).round() / 1000.0);
    }
    v
}

/// `brief` plus the provenance of the hit, so "why did this come back?" is
/// answerable from the tool output alone.
fn brief_scored(s: &super::recall::Scored) -> Value {
    let mut v = brief(&s.memory, Some(s.score));
    let matched = s.signals.describe();
    if !matched.is_empty() {
        v["matched"] = json!(matched);
    }
    v
}

fn full(m: &Memory) -> Value {
    json!({
        "id": m.id,
        "kind": m.kind.as_str(),
        "content": m.content,
        "summary": m.summary,
        "entity": m.entity,
        "importance": m.importance,
        "confidence": m.confidence,
        "pinned": m.pinned,
        "source": m.source.as_str(),
        "chat_id": m.chat_id,
        "persona": m.persona,
        "occurred_at": m.occurred_at.map(|t| t.to_rfc3339()),
        "created_at": m.created_at.to_rfc3339(),
        "updated_at": m.updated_at.to_rfc3339(),
        "last_accessed_at": m.last_accessed_at.to_rfc3339(),
        "access_count": m.access_count,
        "archived": m.archived_at.is_some(),
        "superseded_by": m.superseded_by,
    })
}

// ── mem_remember ─────────────────────────────────────────────────────────────

pub struct MemRememberTool {
    /// The agent whose memory this tool acts on. `None` targets the
    /// pod-global store — the CLI and any pre-instance caller.
    instance_id: Option<String>,
}

impl MemRememberTool {
    pub fn new(instance_id: Option<String>) -> Self {
        Self { instance_id }
    }
}

#[async_trait]
impl metalcraft::Tool for MemRememberTool {
    fn name(&self) -> &str {
        "mem_remember"
    }
    fn description(&self) -> &str {
        "Save something to long-term memory so it survives this conversation. Use for durable facts, \
         the user's preferences, and methods that worked — not for details only true right now. \
         Prefer one self-contained sentence that will still make sense months from now, with no \
         pronouns referring to this conversation. Secrets are stripped automatically. Saving \
         something already remembered reinforces it instead of duplicating."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The memory, as one self-contained statement."
                },
                "kind": {
                    "type": "string",
                    "enum": ["semantic", "preference", "procedural", "entity", "episodic", "insight"],
                    "description": "semantic = a durable fact; preference = how the user wants things done; \
                                    procedural = a method that worked; entity = a person/repo/service; \
                                    episodic = something that happened. Defaults to semantic."
                },
                "entity": {
                    "type": "string",
                    "description": "Canonical name of the thing this is about, if it is about one thing."
                },
                "importance": {
                    "type": "number",
                    "description": "0-10, default 5. Higher survives decay longer."
                },
                "pinned": {
                    "type": "boolean",
                    "description": "Never forget this automatically. Use sparingly."
                }
            },
            "required": ["content"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        let content = args["content"]
            .as_str()
            .ok_or_else(|| crate::tools::missing_param("mem_remember", "content"))?;
        let kind = args["kind"]
            .as_str()
            .and_then(MemoryKind::parse)
            .unwrap_or(MemoryKind::Semantic);

        let mut req = RememberRequest::new(kind, content, Source::Tool);
        // An agent that recalls from its own memory must write there too.
        req.instance_id = self.instance_id.clone();
        req.entity = args["entity"]
            .as_str()
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        req.importance = args["importance"].as_f64().map(|v| v as f32);
        req.pinned = args["pinned"].as_bool().unwrap_or(false);

        match remember(req).await {
            Ok(r) => Ok(json!({
                "id": r.memory.id,
                "kind": r.memory.kind.as_str(),
                "status": if r.deduplicated { "already_known" } else { "saved" },
                "redactions": r.redactions,
                "note": if r.deduplicated {
                    "An identical memory already existed; it was reinforced rather than duplicated."
                } else {
                    "Saved."
                },
            })),
            Err(e) => Err(metalcraft::GraphError::ToolCallFailed {
                tool: "mem_remember".into(),
                message: e,
            }),
        }
    }
}

// ── mem_search ───────────────────────────────────────────────────────────────

pub struct MemSearchTool {
    /// The agent whose memory this tool acts on. `None` targets the
    /// pod-global store — the CLI and any pre-instance caller.
    instance_id: Option<String>,
}

impl MemSearchTool {
    pub fn new(instance_id: Option<String>) -> Self {
        Self { instance_id }
    }
}

#[async_trait]
impl metalcraft::Tool for MemSearchTool {
    fn name(&self) -> &str {
        "mem_search"
    }
    fn description(&self) -> &str {
        "Search long-term memory for things learned in earlier conversations. Use proactively when \
         the user refers to past decisions, their preferences, or anything you might already have \
         been told. Combines keyword, semantic, and connection-based search, so it finds relevant \
         memories even when the wording differs. Returns ranked snippets — call mem_get with an id \
         for the full text."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to look for. A natural phrase works as well as keywords." },
                "limit": { "type": "integer", "description": "Max results (default 10, max 50)." },
                "mode": {
                    "type": "string",
                    "enum": ["hybrid", "text", "vector"],
                    "description": "hybrid (default) combines keyword, semantic, and linked-memory search. \
                                    text is exact-keyword only. vector is meaning-only. Leave unset unless debugging."
                },
                "kind": {
                    "type": "string",
                    "enum": ["semantic", "preference", "procedural", "entity", "episodic", "insight"],
                    "description": "Restrict to one kind of memory. Omit to search all."
                }
            },
            "required": ["query"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| crate::tools::missing_param("mem_search", "query"))?;
        let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
        let mode = args["mode"]
            .as_str()
            .and_then(Mode::parse)
            .unwrap_or(Mode::Hybrid);
        let opts = RecallOptions {
            mode,
            limit,
            kind: args["kind"].as_str().and_then(MemoryKind::parse),
            // Search where this agent writes. Without it an agent would remember
            // something and then be unable to find it.
            instance_id: self.instance_id.clone(),
            ..Default::default()
        };

        let results = recall(query, opts).await;
        let availability = super::embedding_availability();
        let mut out = json!({
            "query": query,
            "mode": mode.as_str(),
            "count": results.len(),
            "results": results.iter().map(brief_scored).collect::<Vec<_>>(),
        });
        if results.is_empty() {
            out["note"] = json!("Nothing in memory matches. This may simply be new.");
        }
        // Say so when semantic matching is unavailable, rather than letting a
        // thin result set look like a confident "nothing is known".
        if mode != Mode::Text && availability != super::embed::Availability::Ready {
            out["degraded"] = json!(format!(
                "Semantic search is {} — these results are keyword and connection matches only.",
                availability.as_str()
            ));
        }
        Ok(out)
    }
}

// ── mem_get ──────────────────────────────────────────────────────────────────

pub struct MemGetTool {
    /// The agent whose memory this tool acts on. `None` targets the
    /// pod-global store — the CLI and any pre-instance caller.
    instance_id: Option<String>,
}

impl MemGetTool {
    pub fn new(instance_id: Option<String>) -> Self {
        Self { instance_id }
    }
}

#[async_trait]
impl metalcraft::Tool for MemGetTool {
    fn name(&self) -> &str {
        "mem_get"
    }
    fn description(&self) -> &str {
        "Read one memory in full by id, including how it connects to other memories. \
         Get ids from mem_search."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Memory id from mem_search." } },
            "required": ["id"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| crate::tools::missing_param("mem_get", "id"))?;
        let scoped = match &self.instance_id {
            Some(inst) => super::instance_get(inst, id)
                .await
                .map(|m| (m, Vec::new(), Vec::new())),
            None => None,
        };
        match match scoped {
            Some(hit) => Some(hit),
            None if self.instance_id.is_none() => get(id).await,
            // An agent must not read another agent's memory just because the
            // pod-global store happens to hold that id.
            None => None,
        } {
            Some((m, out_links, in_links)) => {
                let mut v = full(&m);
                v["links_out"] = json!(
                    out_links
                        .iter()
                        .map(|l| json!({"kind": l.kind.as_str(), "to": l.dst}))
                        .collect::<Vec<_>>()
                );
                v["links_in"] = json!(
                    in_links
                        .iter()
                        .map(|l| json!({"kind": l.kind.as_str(), "from": l.src}))
                        .collect::<Vec<_>>()
                );
                Ok(v)
            }
            None => Err(metalcraft::GraphError::ToolCallFailed {
                tool: "mem_get".into(),
                message: format!("no memory with id '{id}'"),
            }),
        }
    }
}

// ── mem_forget ───────────────────────────────────────────────────────────────

pub struct MemForgetTool {
    /// The agent whose memory this tool acts on. `None` targets the
    /// pod-global store — the CLI and any pre-instance caller.
    instance_id: Option<String>,
}

impl MemForgetTool {
    pub fn new(instance_id: Option<String>) -> Self {
        Self { instance_id }
    }
}

#[async_trait]
impl metalcraft::Tool for MemForgetTool {
    fn name(&self) -> &str {
        "mem_forget"
    }
    fn description(&self) -> &str {
        "Forget a memory. By default this archives it (reversible, hidden from search). Pass \
         purge=true only when the user explicitly asks for something to be deleted — that is \
         permanent."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Memory id from mem_search." },
                "purge": {
                    "type": "boolean",
                    "description": "Delete permanently instead of archiving. Only on an explicit user request."
                }
            },
            "required": ["id"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| crate::tools::missing_param("mem_forget", "id"))?;
        let purge = args["purge"].as_bool().unwrap_or(false);

        // An agent forgetting inside its own memory is a different operation: its
        // own memories are purged, but a memory its pack shipped is only tombstoned,
        // because that copy is shared with every other agent of the same preset.
        if let Some(inst) = &self.instance_id {
            return match super::instance_forget(inst, id).await {
                Ok(super::instance::Forgotten::Purged) => Ok(json!({
                    "id": id,
                    "status": "purged",
                    "note": "This was your own memory; it is gone.",
                })),
                Ok(super::instance::Forgotten::Tombstoned) => Ok(json!({
                    "id": id,
                    "status": "tombstoned",
                    "note": "This came with your agent pack. You will no longer recall it; \
                             the shared copy other agents use is untouched.",
                })),
                Err(e) => Err(metalcraft::GraphError::ToolCallFailed {
                    tool: "mem_forget".into(),
                    message: e,
                }),
            };
        }

        match forget(id, purge).await {
            Ok(()) => Ok(json!({
                "id": id,
                "status": if purge { "purged" } else { "archived" },
                "note": if purge {
                    "Permanently deleted."
                } else {
                    "Archived — hidden from search, still recoverable."
                },
            })),
            Err(e) => Err(metalcraft::GraphError::ToolCallFailed {
                tool: "mem_forget".into(),
                message: e,
            }),
        }
    }
}

// ── mem_stats ────────────────────────────────────────────────────────────────

pub struct MemStatsTool {
    /// The agent whose memory this tool acts on. `None` targets the
    /// pod-global store — the CLI and any pre-instance caller.
    instance_id: Option<String>,
}

impl MemStatsTool {
    pub fn new(instance_id: Option<String>) -> Self {
        Self { instance_id }
    }
}

#[async_trait]
impl metalcraft::Tool for MemStatsTool {
    fn name(&self) -> &str {
        "mem_stats"
    }
    fn description(&self) -> &str {
        "Report the state of long-term memory: how many memories exist, of what kinds, and how \
         much space they take. Useful for answering 'what do you remember?' at a high level."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: Value) -> metalcraft::Result<Value> {
        if let Some(inst) = &self.instance_id {
            let v = super::instance_view(inst, 0).await;
            return Ok(json!({
                "scope": "agent",
                "instance_id": v.instance_id,
                "base": v.base,
                "shipped": v.shipped,
                "learned": v.learned,
                "forgotten": v.forgotten,
                "total": v.shipped + v.learned,
                "enabled": super::enabled(),
                "embeddings": {
                    "availability": super::embedding_availability().as_str(),
                    "model": super::embed::configured_model(),
                    "dims": super::embed::configured_dims(),
                },
                "pending_captures": super::capture::pending_count(),
            }));
        }
        let s = stats().await;
        Ok(json!({
            "total": s.total,
            "live": s.live,
            "archived": s.archived,
            "superseded": s.superseded,
            "pinned": s.pinned,
            "by_kind": s.by_kind.iter().map(|(k, n)| json!({"kind": k, "count": n})).collect::<Vec<_>>(),
            "links": s.links,
            "pending_log_events": s.log_events,
            // A queue that stops draining means the dream is not running — worth
            // being able to see rather than silently accumulating.
            "pending_captures": super::capture::pending_count(),
            "approx_kb": s.approx_bytes / 1024,
            "enabled": super::enabled(),
            "embeddings": {
                "availability": super::embedding_availability().as_str(),
                "model": super::embed::configured_model(),
                "dims": super::embed::configured_dims(),
                "vectors": s.vectors,
                // An empty store is trivially fully covered.
                "coverage_pct": (s.vectors * 100).checked_div(s.live).unwrap_or(100),
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metalcraft::Tool;

    #[test]
    fn every_declared_tool_name_has_an_implementation() {
        let (remember, search, get, forget, stats) = (
            MemRememberTool::new(None),
            MemSearchTool::new(None),
            MemGetTool::new(None),
            MemForgetTool::new(None),
            MemStatsTool::new(None),
        );
        let built: Vec<&str> = vec![
            remember.name(),
            search.name(),
            get.name(),
            forget.name(),
            stats.name(),
        ];
        assert_eq!(
            built, TOOL_NAMES,
            "TOOL_NAMES must match the registered tools exactly"
        );
    }

    #[test]
    fn schemas_are_objects_and_mark_their_required_params() {
        for (schema, required) in [
            (
                MemRememberTool::new(None).parameters_schema(),
                vec!["content"],
            ),
            (MemSearchTool::new(None).parameters_schema(), vec!["query"]),
            (MemGetTool::new(None).parameters_schema(), vec!["id"]),
            (MemForgetTool::new(None).parameters_schema(), vec!["id"]),
        ] {
            assert_eq!(schema["type"], "object");
            let got: Vec<String> = schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            assert_eq!(got, required);
        }
        // mem_stats takes no parameters at all.
        assert_eq!(
            MemStatsTool::new(None).parameters_schema()["type"],
            "object"
        );
    }

    #[test]
    fn brief_truncates_long_text_but_keeps_short_text_whole() {
        let short = Memory::new(MemoryKind::Semantic, "short one", Source::Tool);
        assert_eq!(brief(&short, None)["text"], "short one");

        let long = Memory::new(MemoryKind::Episodic, "x".repeat(500), Source::Turn);
        let text = brief(&long, None)["text"].as_str().unwrap().to_string();
        assert!(text.ends_with('…'));
        assert_eq!(text.chars().count(), 241);
    }

    #[test]
    fn brief_prefers_the_summary_and_surfaces_flags() {
        let mut m = Memory::new(MemoryKind::Preference, "the long original", Source::User);
        m.summary = "the short version".into();
        m.pinned = true;
        m.entity = Some("Andrew".into());
        let v = brief(&m, Some(1.2345));
        assert_eq!(v["text"], "the short version");
        assert_eq!(v["pinned"], true);
        assert_eq!(v["entity"], "Andrew");
        assert_eq!(v["score"], 1.235, "score is rounded to 3 places, cleanly");
    }

    #[test]
    fn full_exposes_lifecycle_state() {
        let mut m = Memory::new(MemoryKind::Semantic, "content", Source::Tool);
        m.superseded_by = Some("other-id".into());
        let v = full(&m);
        assert_eq!(v["archived"], false);
        assert_eq!(v["superseded_by"], "other-id");
        assert_eq!(v["source"], "tool");
    }
}
