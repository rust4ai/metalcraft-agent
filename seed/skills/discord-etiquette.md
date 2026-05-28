---
description: Discord communication best practices
---

# Discord Etiquette

Guidelines for interacting in Discord:

1. **Message Length**: Keep messages under 2000 characters (Discord's hard limit). For longer content, split into multiple messages.
2. **Reply Threading**: Always use `message_reference_id` when replying to maintain conversation context.
3. **Reactions vs Replies**: Use reactions (discord_add_reaction) for simple acknowledgments (thumbs up, checkmark). Use replies for substantive responses.
4. **Channel Context**: Use discord_get_messages to read recent history before responding, especially when context might be needed.
5. **Mentions**: Don't unnecessarily @mention users. Reply threading is sufficient for directing responses.
6. **Sensitive Data**: Never post API keys, tokens, passwords, or other credentials in Discord.
7. **Error Handling**: If a tool call fails, inform the user gracefully rather than exposing raw error details.
8. **Conciseness**: Discord is a chat platform — prefer short, clear messages over long essays.
