use async_trait::async_trait;
use reqwest::Client;
use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: u64 = 15;

fn make_error(tool: &str, message: impl std::fmt::Display) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: message.to_string(),
    }
}

fn get_gateway_client() -> Result<Client, metalcraft::GraphError> {
    let api_key = std::env::var("AGENT_GATEWAY_API_KEY").map_err(|_| {
        make_error(
            "discord",
            "AGENT_GATEWAY_API_KEY environment variable not set",
        )
    })?;

    Client::builder()
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {api_key}")
                    .parse()
                    .expect("invalid api key header"),
            );
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            headers
        })
        .build()
        .map_err(|e| make_error("discord", format!("Failed to create HTTP client: {e}")))
}

fn gateway_base_url() -> Result<String, metalcraft::GraphError> {
    std::env::var("AGENT_GATEWAY_URL").map_err(|_| {
        make_error(
            "discord",
            "AGENT_GATEWAY_URL environment variable not set",
        )
    })
}

async fn gateway_request(
    client: &Client,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> metalcraft::Result<serde_json::Value> {
    let base = gateway_base_url()?;
    let url = format!("{base}/api/v1{path}");
    let mut req = client.request(method, &url);
    if let Some(body) = body {
        req = req.json(&body);
    }

    let response = req.send().await.map_err(|e| {
        make_error("discord", format!("Request to {url} failed: {e}"))
    })?;

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Ok(serde_json::json!({
            "error": format!("HTTP {status}"),
            "body": body_text,
        }));
    }

    if body_text.is_empty() {
        Ok(serde_json::json!({ "ok": true }))
    } else {
        serde_json::from_str(&body_text).map_err(|_| {
            make_error("discord", format!("Invalid JSON in response: {body_text}"))
        })
    }
}

// ---------------------------------------------------------------------------
// discord_send_message
// ---------------------------------------------------------------------------

pub struct DiscordSendMessageTool;

#[async_trait]
impl metalcraft::Tool for DiscordSendMessageTool {
    fn name(&self) -> &str {
        "discord_send_message"
    }
    fn description(&self) -> &str {
        "Send a message to a Discord channel. Optionally reply to a specific message by providing message_reference_id."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel_id": {
                    "type": "string",
                    "description": "The Discord channel ID to send the message to"
                },
                "content": {
                    "type": "string",
                    "description": "The message content (max 2000 characters)"
                },
                "message_reference_id": {
                    "type": "string",
                    "description": "Optional: message ID to reply to"
                }
            },
            "required": ["channel_id", "content"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let channel_id = args["channel_id"]
            .as_str()
            .ok_or_else(|| make_error("discord_send_message", "Missing channel_id"))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| make_error("discord_send_message", "Missing content"))?;

        let mut body = serde_json::json!({
            "channel_id": channel_id,
            "content": content
        });
        if let Some(ref_id) = args["message_reference_id"].as_str() {
            body["message_reference_id"] = serde_json::json!(ref_id);
        }

        let client = get_gateway_client()?;
        gateway_request(
            &client,
            reqwest::Method::POST,
            "/messages",
            Some(body),
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// discord_edit_message
// ---------------------------------------------------------------------------

pub struct DiscordEditMessageTool;

#[async_trait]
impl metalcraft::Tool for DiscordEditMessageTool {
    fn name(&self) -> &str {
        "discord_edit_message"
    }
    fn description(&self) -> &str {
        "Edit a message previously sent by the bot in a Discord channel."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel_id": {
                    "type": "string",
                    "description": "The Discord channel ID"
                },
                "message_id": {
                    "type": "string",
                    "description": "The message ID to edit"
                },
                "content": {
                    "type": "string",
                    "description": "The new message content"
                }
            },
            "required": ["channel_id", "message_id", "content"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let channel_id = args["channel_id"]
            .as_str()
            .ok_or_else(|| make_error("discord_edit_message", "Missing channel_id"))?;
        let message_id = args["message_id"]
            .as_str()
            .ok_or_else(|| make_error("discord_edit_message", "Missing message_id"))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| make_error("discord_edit_message", "Missing content"))?;

        let client = get_gateway_client()?;
        gateway_request(
            &client,
            reqwest::Method::PATCH,
            &format!("/messages/{message_id}"),
            Some(serde_json::json!({
                "channel_id": channel_id,
                "content": content
            })),
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// discord_add_reaction
// ---------------------------------------------------------------------------

pub struct DiscordAddReactionTool;

#[async_trait]
impl metalcraft::Tool for DiscordAddReactionTool {
    fn name(&self) -> &str {
        "discord_add_reaction"
    }
    fn description(&self) -> &str {
        "Add a reaction emoji to a message."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel_id": {
                    "type": "string",
                    "description": "The Discord channel ID"
                },
                "message_id": {
                    "type": "string",
                    "description": "The message ID to react to"
                },
                "emoji": {
                    "type": "string",
                    "description": "The emoji to react with"
                }
            },
            "required": ["channel_id", "message_id", "emoji"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let channel_id = args["channel_id"]
            .as_str()
            .ok_or_else(|| make_error("discord_add_reaction", "Missing channel_id"))?;
        let message_id = args["message_id"]
            .as_str()
            .ok_or_else(|| make_error("discord_add_reaction", "Missing message_id"))?;
        let emoji = args["emoji"]
            .as_str()
            .ok_or_else(|| make_error("discord_add_reaction", "Missing emoji"))?;

        let client = get_gateway_client()?;
        gateway_request(
            &client,
            reqwest::Method::PUT,
            &format!("/messages/{message_id}/reactions"),
            Some(serde_json::json!({
                "channel_id": channel_id,
                "emoji": emoji
            })),
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// discord_get_messages
// ---------------------------------------------------------------------------

pub struct DiscordGetMessagesTool;

#[async_trait]
impl metalcraft::Tool for DiscordGetMessagesTool {
    fn name(&self) -> &str {
        "discord_get_messages"
    }
    fn description(&self) -> &str {
        "Get recent messages from a Discord channel for context. Returns up to `limit` messages (default 10, max 50)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel_id": {
                    "type": "string",
                    "description": "The Discord channel ID"
                },
                "limit": {
                    "type": "integer",
                    "description": "Number of messages to fetch (default 10, max 50)"
                }
            },
            "required": ["channel_id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let channel_id = args["channel_id"]
            .as_str()
            .ok_or_else(|| make_error("discord_get_messages", "Missing channel_id"))?;
        let limit = args["limit"].as_u64().unwrap_or(10).min(50);

        let client = get_gateway_client()?;
        gateway_request(
            &client,
            reqwest::Method::GET,
            &format!("/channels/{channel_id}/messages?limit={limit}"),
            None,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// discord_get_channel_info
// ---------------------------------------------------------------------------

pub struct DiscordGetChannelInfoTool;

#[async_trait]
impl metalcraft::Tool for DiscordGetChannelInfoTool {
    fn name(&self) -> &str {
        "discord_get_channel_info"
    }
    fn description(&self) -> &str {
        "Get metadata about a Discord channel (name, topic, type, etc.)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "channel_id": {
                    "type": "string",
                    "description": "The Discord channel ID"
                }
            },
            "required": ["channel_id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let channel_id = args["channel_id"]
            .as_str()
            .ok_or_else(|| make_error("discord_get_channel_info", "Missing channel_id"))?;

        let client = get_gateway_client()?;
        gateway_request(
            &client,
            reqwest::Method::GET,
            &format!("/channels/{channel_id}"),
            None,
        )
        .await
    }
}
