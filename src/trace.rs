//! OpenTelemetry trace logger.
//!
//! A sibling of [`crate::diagnostics::DiagnosticsLogger`]: where diagnostics
//! writes bespoke per-turn JSON snapshots under `sessions/<id>/`, this writes a
//! single OTLP/JSON document under `traces/<id>/otlp-trace.json` following the
//! OpenTelemetry **GenAI semantic conventions**. The two directories share the
//! same `<id>` (the diagnostics session-dir name) so they line up 1:1.
//!
//! The logger is **session-scoped** and accumulates across turns: the whole
//! chat is one trace, each user turn is a child span, and each LLM call / tool
//! execution nests under its turn. Span timings are real — they reuse the same
//! stopwatches the chat-turn task already keeps to emit client `*Completed`
//! events (see `workshop_api.rs`), so no metalcraft change is required.
//!
//! What is *not* available without an upstream library change: per-call token
//! usage (`gen_ai.usage.*`). The current `LlmCallHook` fires before `.send()`,
//! so the response (and its usage) never reaches us. That is the sole driver
//! for the metalcraft `LlmResponseHook` follow-up.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// OTLP span kind. Only the two we emit.
const KIND_INTERNAL: i32 = 1;
const KIND_CLIENT: i32 = 3;

/// OTLP status codes.
const STATUS_UNSET: i32 = 0;
const STATUS_ERROR: i32 = 2;

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// A finished or in-flight span, in our own shape; serialized to OTLP at write.
struct Span {
    span_id: String,
    parent: String,
    name: String,
    kind: i32,
    start_ns: u128,
    end_ns: u128,
    attrs: Vec<(String, Value)>,
    /// `(time_ns, event_name, attributes)` — used for prompt/response content.
    events: Vec<(u128, String, Vec<(String, Value)>)>,
    status: i32,
    status_msg: Option<String>,
}

struct OpenTurn {
    span: Span,
}

struct Inner {
    /// Completed spans, flushed to disk after every turn.
    spans: Vec<Span>,
    session_span_id: String,
    session_start_ns: u128,
    turn: Option<OpenTurn>,
    open_llm: Option<Span>,
    open_tools: HashMap<String, Span>,
    turn_counter: u64,
    span_counter: u64,
}

/// Session-scoped OTLP trace writer.
pub struct TraceLogger {
    file: PathBuf,
    trace_id: String,
    model: String,
    inner: Mutex<Inner>,
}

impl TraceLogger {
    /// Create a logger writing `traces/<session_id>/otlp-trace.json`.
    ///
    /// `session_id` should be the diagnostics session-dir name so the two
    /// output trees align. `model` populates `gen_ai.request.model`.
    pub fn new(session_id: &str, model: &str) -> std::io::Result<Self> {
        let dir = crate::paths::traces_dir().join(session_id);
        std::fs::create_dir_all(&dir)?;
        let trace_id = new_hex(32);
        let session_span_id = new_hex(16);
        Ok(Self {
            file: dir.join("otlp-trace.json"),
            trace_id,
            model: model.to_string(),
            inner: Mutex::new(Inner {
                spans: Vec::new(),
                session_span_id,
                session_start_ns: now_ns(),
                turn: None,
                open_llm: None,
                open_tools: HashMap::new(),
                turn_counter: 0,
                span_counter: 0,
            }),
        })
    }

    /// Open a turn span for one user message. Closes any still-open turn first.
    pub fn start_turn(&self, user_message: &str) {
        let mut st = self.inner.lock().unwrap();
        // Defensive: close a previous turn that never got end_turn'd.
        if let Some(prev) = st.turn.take() {
            let mut s = prev.span;
            s.end_ns = now_ns();
            st.spans.push(s);
        }
        st.turn_counter += 1;
        let n = st.turn_counter;
        let span_id = self.next_span_id(&mut st);
        let parent = st.session_span_id.clone();
        st.turn = Some(OpenTurn {
            span: Span {
                span_id,
                parent,
                name: format!("agent turn {n}"),
                kind: KIND_INTERNAL,
                start_ns: now_ns(),
                end_ns: 0,
                attrs: vec![
                    ("gen_ai.operation.name".into(), json!("invoke_agent")),
                    ("metalcraft.turn.index".into(), json!(n)),
                ],
                events: vec![(
                    now_ns(),
                    "gen_ai.user.message".into(),
                    vec![("content".into(), json!(user_message))],
                )],
                status: STATUS_UNSET,
                status_msg: None,
            },
        });
    }

    /// An LLM `.send()` is about to start (from `LlmCallHook`).
    pub fn on_llm_start(&self) {
        let mut st = self.inner.lock().unwrap();
        if let Some(prev) = st.open_llm.take() {
            // Shouldn't happen (calls are serial), but don't leak it.
            let mut s = prev;
            s.end_ns = now_ns();
            st.spans.push(s);
        }
        let span_id = self.next_span_id(&mut st);
        let parent = st.current_parent();
        let model = self.model.clone();
        st.open_llm = Some(Span {
            span_id,
            parent,
            name: format!("chat {model}"),
            kind: KIND_CLIENT,
            start_ns: now_ns(),
            end_ns: 0,
            attrs: vec![
                ("gen_ai.system".into(), json!("openai")),
                ("gen_ai.operation.name".into(), json!("chat")),
                ("gen_ai.request.model".into(), json!(model)),
            ],
            events: Vec::new(),
            status: STATUS_UNSET,
            status_msg: None,
        });
    }

    /// Record token usage on the open LLM span. Fired by the `LlmResponseHook`
    /// after `.send()` returns but before the step_guard closes the span, so the
    /// span is still in `open_llm`. No-op if no LLM span is open.
    pub fn on_llm_usage(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
        cached_input_tokens: u64,
        reasoning_tokens: u64,
    ) {
        let mut st = self.inner.lock().unwrap();
        if let Some(span) = st.open_llm.as_mut() {
            span.attrs
                .push(("gen_ai.usage.input_tokens".into(), json!(input_tokens)));
            span.attrs
                .push(("gen_ai.usage.output_tokens".into(), json!(output_tokens)));
            span.attrs
                .push(("gen_ai.usage.total_tokens".into(), json!(total_tokens)));
            if cached_input_tokens > 0 {
                span.attrs.push((
                    "gen_ai.usage.cache_read.input_tokens".into(),
                    json!(cached_input_tokens),
                ));
            }
            if reasoning_tokens > 0 {
                span.attrs.push((
                    "gen_ai.usage.reasoning_tokens".into(),
                    json!(reasoning_tokens),
                ));
            }
        }
    }

    /// The LLM call produced output (assistant text and/or tool calls). Closes
    /// the open LLM span. `assistant_text` is recorded as a response event.
    pub fn on_llm_complete(&self, assistant_text: Option<&str>) {
        let mut st = self.inner.lock().unwrap();
        if let Some(mut span) = st.open_llm.take() {
            span.end_ns = now_ns();
            if let Some(text) = assistant_text {
                if !text.is_empty() {
                    span.events.push((
                        now_ns(),
                        "gen_ai.assistant.message".into(),
                        vec![("content".into(), json!(text))],
                    ));
                }
            }
            st.spans.push(span);
        }
    }

    /// A tool execution started (matches a `ToolStarted` client event).
    pub fn on_tool_start(&self, tool_call_id: &str, name: &str, args: &Value) {
        let mut st = self.inner.lock().unwrap();
        let span_id = self.next_span_id(&mut st);
        let parent = st.current_parent();
        st.open_tools.insert(
            tool_call_id.to_string(),
            Span {
                span_id,
                parent,
                name: format!("execute_tool {name}"),
                kind: KIND_INTERNAL,
                start_ns: now_ns(),
                end_ns: 0,
                attrs: vec![
                    ("gen_ai.operation.name".into(), json!("execute_tool")),
                    ("gen_ai.tool.name".into(), json!(name)),
                    ("gen_ai.tool.call.id".into(), json!(tool_call_id)),
                    ("gen_ai.tool.call.arguments".into(), json!(args.to_string())),
                ],
                events: Vec::new(),
                status: STATUS_UNSET,
                status_msg: None,
            },
        );
    }

    /// A tool execution finished. `result` is the raw tool result string; a
    /// leading `ERROR:` marks the span as failed (mirrors diagnostics).
    pub fn on_tool_complete(&self, tool_call_id: &str, result: &str) {
        let mut st = self.inner.lock().unwrap();
        if let Some(mut span) = st.open_tools.remove(tool_call_id) {
            span.end_ns = now_ns();
            span.events.push((
                now_ns(),
                "gen_ai.tool.message".into(),
                vec![("content".into(), json!(result))],
            ));
            if result.starts_with("ERROR:") {
                span.status = STATUS_ERROR;
                span.status_msg = Some(truncate(result, 512));
            }
            st.spans.push(span);
        }
    }

    /// Record a turn failure on the current turn span (and any open LLM span).
    pub fn on_error(&self, reason: &str) {
        let mut st = self.inner.lock().unwrap();
        let now = now_ns();
        if let Some(mut span) = st.open_llm.take() {
            span.end_ns = now;
            span.status = STATUS_ERROR;
            span.status_msg = Some(truncate(reason, 512));
            span.events.push((
                now,
                "exception".into(),
                vec![("exception.message".into(), json!(reason))],
            ));
            st.spans.push(span);
        }
        if let Some(turn) = st.turn.as_mut() {
            turn.span.status = STATUS_ERROR;
            turn.span.status_msg = Some(truncate(reason, 512));
        }
    }

    /// Close the current turn span and flush the whole trace to disk.
    pub fn end_turn(&self, ok: bool) {
        let mut st = self.inner.lock().unwrap();
        // Close any tool spans that never completed (e.g. interruption).
        let leftover: Vec<String> = st.open_tools.keys().cloned().collect();
        for id in leftover {
            if let Some(mut s) = st.open_tools.remove(&id) {
                s.end_ns = now_ns();
                s.status = STATUS_ERROR;
                s.status_msg = Some("tool did not complete".into());
                st.spans.push(s);
            }
        }
        if let Some(turn) = st.turn.take() {
            let mut s = turn.span;
            s.end_ns = now_ns();
            if !ok && s.status != STATUS_ERROR {
                s.status = STATUS_ERROR;
            }
            st.spans.push(s);
        }
        let doc = self.to_otlp(&st);
        drop(st);
        if let Err(e) = std::fs::write(
            &self.file,
            serde_json::to_string_pretty(&doc).unwrap_or_default(),
        ) {
            eprintln!("trace: failed to write {}: {e}", self.file.display());
        }
    }

    fn next_span_id(&self, st: &mut Inner) -> String {
        st.span_counter += 1;
        // Deterministic-enough, collision-free within a trace: counter → hex.
        format!("{:016x}", st.span_counter.wrapping_mul(0x9E3779B97F4A7C15))
    }

    /// Serialize accumulated spans (+ the synthetic session root) as one OTLP
    /// `TracesData` document.
    fn to_otlp(&self, st: &Inner) -> Value {
        let mut spans: Vec<Value> = Vec::with_capacity(st.spans.len() + 1);
        // Synthetic session root, spanning everything seen so far.
        spans.push(self.span_to_otlp(&Span {
            span_id: st.session_span_id.clone(),
            parent: String::new(),
            name: "chat session".into(),
            kind: KIND_INTERNAL,
            start_ns: st.session_start_ns,
            end_ns: now_ns(),
            attrs: vec![
                ("gen_ai.operation.name".into(), json!("chat")),
                ("gen_ai.request.model".into(), json!(self.model)),
            ],
            events: Vec::new(),
            status: STATUS_UNSET,
            status_msg: None,
        }));
        for s in &st.spans {
            spans.push(self.span_to_otlp(s));
        }
        json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [
                        otlp_attr("service.name", json!("metalcraft-agent")),
                    ]
                },
                "scopeSpans": [{
                    "scope": { "name": "metalcraft-agent.trace", "version": env!("CARGO_PKG_VERSION") },
                    "spans": spans
                }]
            }]
        })
    }

    fn span_to_otlp(&self, s: &Span) -> Value {
        let mut obj = json!({
            "traceId": self.trace_id,
            "spanId": s.span_id,
            "name": s.name,
            "kind": s.kind,
            "startTimeUnixNano": s.start_ns.to_string(),
            "endTimeUnixNano": (if s.end_ns == 0 { now_ns() } else { s.end_ns }).to_string(),
            "attributes": s.attrs.iter().map(|(k, v)| otlp_attr(k, v.clone())).collect::<Vec<_>>(),
            "status": {
                "code": s.status,
                "message": s.status_msg.clone().unwrap_or_default(),
            },
        });
        if !s.parent.is_empty() {
            obj["parentSpanId"] = json!(s.parent);
        }
        if !s.events.is_empty() {
            obj["events"] = json!(s
                .events
                .iter()
                .map(|(t, name, attrs)| json!({
                    "timeUnixNano": t.to_string(),
                    "name": name,
                    "attributes": attrs.iter().map(|(k, v)| otlp_attr(k, v.clone())).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>());
        }
        obj
    }
}

impl Inner {
    /// Spans nest under the open turn if there is one, else the session root.
    fn current_parent(&self) -> String {
        self.turn
            .as_ref()
            .map(|t| t.span.span_id.clone())
            .unwrap_or_else(|| self.session_span_id.clone())
    }
}

/// Encode one attribute as an OTLP `KeyValue` with a typed `AnyValue`.
fn otlp_attr(key: &str, value: Value) -> Value {
    let any = match value {
        Value::String(s) => json!({ "stringValue": s }),
        Value::Bool(b) => json!({ "boolValue": b }),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({ "intValue": n.to_string() }),
        Value::Number(n) => json!({ "doubleValue": n.as_f64().unwrap_or(0.0) }),
        // Arrays/objects: stringify. OTLP supports kvlistValue/arrayValue but
        // a JSON string is universally ingestible and keeps this simple.
        other => json!({ "stringValue": other.to_string() }),
    };
    json!({ "key": key, "value": any })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// A random lowercase-hex id of `len` chars (16 = span id, 32 = trace id).
fn new_hex(len: usize) -> String {
    // Reuse the `uuid` dep already in the tree; concatenate as needed.
    let mut out = String::with_capacity(len);
    while out.len() < len {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(len);
    out
}
