use serde::Deserialize;

/// Normalized event received from the gateway's pub/sub system.
#[derive(Clone, Debug, Deserialize)]
pub struct GatewayEvent {
    pub id: String,
    pub platform: String,
    pub event_type: String,
    pub channel_id: Option<String>,
    pub author: Option<EventAuthor>,
    pub content: Option<String>,
    pub timestamp: String,
    pub raw: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EventAuthor {
    pub id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub is_bot: bool,
}

impl GatewayEvent {
    /// Convert the event into a natural-language prompt for the agent.
    pub fn to_agent_prompt(&self) -> String {
        let author_name = self
            .author
            .as_ref()
            .map(|a| {
                a.display_name
                    .as_deref()
                    .unwrap_or(&a.username)
            })
            .unwrap_or("unknown");

        let content = self.content.as_deref().unwrap_or("");
        let channel = self
            .channel_id
            .as_deref()
            .unwrap_or("unknown");

        // Extract message ID from raw payload for reply references
        let message_id = self.raw.get("id").and_then(|v| v.as_str()).unwrap_or("");

        // Platform-specific send tool instruction
        let send_instruction = match self.platform.as_str() {
            "discord" => format!(
                "Respond using discord_send_message with channel_id \"{channel}\".{}",
                if !message_id.is_empty() {
                    format!(" Use message_reference_id \"{message_id}\" to reply to the original message.")
                } else {
                    String::new()
                }
            ),
            "whatsapp" => format!(
                "Respond using whatsapp_send_message with channel_id \"{channel}\" (the sender's phone number)."
            ),
            "slack" | "github" => {
                // For non-Discord platforms, give a generic instruction
                // The agent's available tools determine what it can actually do
                format!("The channel/context is \"{channel}\" on {platform}.", platform = self.platform)
            }
            _ => format!("Platform: {}, channel: {channel}.", self.platform),
        };

        match self.event_type.as_str() {
            "message_create" => {
                format!(
                    "{platform} message from @{author_name} in channel {channel}:\n\n\
                     {content}\n\n\
                     {send_instruction}",
                    platform = self.platform,
                )
            }
            "reaction_add" => {
                format!(
                    "{platform} reaction from @{author_name} in channel {channel}: {content}",
                    platform = self.platform,
                )
            }
            _ => {
                format!(
                    "{platform} event '{event_type}' in channel {channel} from @{author_name}.\n\n\
                     {send_instruction}\n\nRaw payload:\n{raw}",
                    platform = self.platform,
                    event_type = self.event_type,
                    raw = serde_json::to_string_pretty(&self.raw).unwrap_or_default(),
                )
            }
        }
    }
}
