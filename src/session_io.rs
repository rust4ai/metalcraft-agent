//! Session preset — the I/O type of an agentic chat session.
//!
//! A session's preset is the single thing that distinguishes a normal workshop
//! chat from a gateway conversation. Both run the identical agent engine
//! (tool-only output, replying via `say_to_user`); the preset only decides where
//! that reply is delivered:
//!
//! - [`SessionPreset::Workshop`] → streamed to the workshop UI over SSE.
//! - [`SessionPreset::Gateway`] → sent out through the bound channel adapter
//!   (PipeStreamr/Twilio) to the original sender.
//!
//! The preset is persisted with the chat (see `PersistedChat`) so a gateway
//! conversation rehydrates with its routing intact after a restart.

use serde::{Deserialize, Serialize};

/// How a session receives input and delivers its replies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionPreset {
    /// A normal workshop chat. Replies stream to the UI over SSE.
    Workshop,
    /// A gateway conversation bound to one sender on one channel instance.
    /// Replies are sent back out through the channel's adapter.
    Gateway {
        /// The channel this conversation belongs to (channels-model slug; a
        /// synthetic id for the dormant twilio path).
        channel_slug: String,
        /// The reply route: `"gateway"` (channels model) or `"twilio"`.
        adapter: String,
        /// The counterparty — where replies go (the inbound message's `from`).
        recipient: String,
        /// Optional sender identity for outbound (integration id / our number).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
}

impl Default for SessionPreset {
    fn default() -> Self {
        SessionPreset::Workshop
    }
}

impl SessionPreset {
    /// Stable label for diagnostics `session_info.json` (`kind` field) and the
    /// Sessions pane: `"session"` for workshop chats (unchanged from before),
    /// `"gateway"` for gateway conversations.
    pub fn diagnostics_kind(&self) -> &'static str {
        match self {
            SessionPreset::Workshop => "session",
            SessionPreset::Gateway { .. } => "gateway",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_preset_defaults_to_workshop() {
        // Chats persisted before `preset` existed have no such field.
        #[derive(serde::Deserialize)]
        struct OldChat {
            #[serde(default)]
            preset: SessionPreset,
        }
        let old: OldChat = serde_json::from_str("{}").unwrap();
        assert!(matches!(old.preset, SessionPreset::Workshop));
    }

    #[test]
    fn gateway_preset_round_trips() {
        let preset = SessionPreset::Gateway {
            channel_slug: "metalcraft".into(),
            adapter: "gateway".into(),
            recipient: "+15550001234".into(),
            from: Some("integration-uuid".into()),
        };
        let json = serde_json::to_string(&preset).unwrap();
        assert!(json.contains("\"kind\":\"gateway\""));
        let back: SessionPreset = serde_json::from_str(&json).unwrap();
        match back {
            SessionPreset::Gateway { adapter, recipient, .. } => {
                assert_eq!(adapter, "gateway");
                assert_eq!(recipient, "+15550001234");
            }
            _ => panic!("expected gateway preset"),
        }
    }
}
