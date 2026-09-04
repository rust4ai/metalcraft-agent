//! A ceiling on what any one tool can put into a conversation.
//!
//! Individual tools already truncate themselves — `bash` at 30 000 characters,
//! `read_file` at 50 000, `grep` at a hundred matches. Those are good numbers
//! chosen by someone who knew what the tool returns. This is for the tools that
//! *can't* know: an HTTP tool pointed at an unexpectedly large export, a pack
//! tool wrapping a third-party API that answered with a whole dataset, a native
//! integration added later by somebody who did not read this file.
//!
//! The cost of getting it wrong is not "a big allocation". A tool result is
//! appended to [`metalcraft::AgentState`], persisted with the chat, and replayed
//! into the next LLM request — and the one after that. A single unbounded result
//! is therefore carried for the rest of the conversation and written to disk on
//! every turn of it. Bounding it at the source is the only place one fix covers
//! all of that.
//!
//! Applied by wrapping at registration ([`RegisterCapped::register_capped`]) so
//! it holds for every tool in the registry rather than for the ones somebody
//! remembered.

use metalcraft::{Tool, ToolRegistry};
use serde_json::Value;

/// Wraps a tool so its result can't exceed
/// [`crate::resources::max_tool_result_bytes`].
pub struct CappedTool<T> {
    inner: T,
}

impl<T: Tool> CappedTool<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl<T: Tool> Tool for CappedTool<T> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        let out = self.inner.call(args).await?;
        Ok(cap_result(self.inner.name(), out))
    }
}

/// Replace an oversized result with a middle-elided preview of it.
///
/// A preview rather than an error because the model asked for something and
/// deserves an answer: the head and tail of a large response usually carry the
/// shape of it (a JSON envelope's fields, the first and last rows), which is
/// enough to decide what to ask for next. The note says plainly that this is not
/// the whole thing, so the model narrows its request instead of concluding the
/// data ends there — the failure mode a silent truncation would cause.
pub fn cap_result(tool_name: &str, value: Value) -> Value {
    let limit = crate::resources::max_tool_result_bytes();
    let serialized = match serde_json::to_string(&value) {
        Ok(s) => s,
        // Unserializable results are the executor's problem, not this wrapper's.
        Err(_) => return value,
    };
    if serialized.len() <= limit {
        return value;
    }

    let original_bytes = serialized.len();
    crate::resources::record_tool_result_truncated((original_bytes - limit) as u64);
    log::warn!(
        "tool '{tool_name}' returned {original_bytes} bytes; truncated to {limit} \
         (raise MAX_TOOL_RESULT_BYTES to keep more)"
    );

    serde_json::json!({
        "truncated": true,
        "original_bytes": original_bytes,
        "limit_bytes": limit,
        "note": format!(
            "The '{tool_name}' result was {original_bytes} bytes, over this agent's \
             {limit}-byte ceiling for a single tool result. Below is its beginning and \
             end, with the middle removed. Narrow the request — a filter, a range, a \
             smaller page — rather than assuming the data ends where this preview does."
        ),
        "preview": super::truncate_output(&serialized, limit),
    })
}

/// Register a tool with the result ceiling applied.
///
/// An extension trait rather than a free function so the call sites in
/// [`super::create_registry_for_with_config`] stay a readable list of
/// registrations — `registry.register_capped(x)` reads the same as the
/// `registry.register(x)` it replaced, which is what keeps the next person from
/// quietly adding an uncapped one back.
pub trait RegisterCapped {
    fn register_capped<T: Tool + 'static>(self, tool: T) -> Self;
}

impl RegisterCapped for ToolRegistry {
    fn register_capped<T: Tool + 'static>(self, tool: T) -> Self {
        self.register(CappedTool::new(tool))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_result_passes_through_untouched() {
        let v = serde_json::json!({"ok": true, "rows": [1, 2, 3]});
        assert_eq!(cap_result("demo", v.clone()), v);
    }

    #[test]
    fn an_oversized_result_becomes_a_preview() {
        let limit = crate::resources::max_tool_result_bytes();
        let huge = serde_json::json!({ "body": "x".repeat(limit * 2) });
        let capped = cap_result("demo", huge);

        assert_eq!(capped["truncated"], serde_json::json!(true));
        assert!(capped["original_bytes"].as_u64().unwrap() > limit as u64);

        // The point of the whole exercise: what goes into the conversation is
        // bounded, whatever the tool did.
        let bytes = serde_json::to_string(&capped).unwrap().len();
        assert!(
            bytes < limit * 2,
            "a capped result should be far smaller than the original, got {bytes}"
        );
    }
}
