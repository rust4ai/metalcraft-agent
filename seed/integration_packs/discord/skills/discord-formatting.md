---
description: Discord markdown and formatting reference
---

# Discord Formatting

Discord supports a subset of Markdown:

- **Bold**: `**text**`
- *Italic*: `*text*` or `_text_`
- ~~Strikethrough~~: `~~text~~`
- __Underline__: `__text__`
- `Inline code`: `` `code` ``
- Code blocks: ` ```language\ncode\n``` `
- > Block quotes: `> text`
- Spoilers: `||text||`
- Headings: `# H1`, `## H2`, `### H3` (only in messages, not embeds)
- Lists: `- item` or `1. item`
- Links: bare URLs auto-link, `[text](url)` for named links

## Mentions
- User: `<@USER_ID>`
- Role: `<@&ROLE_ID>`
- Channel: `<#CHANNEL_ID>`

## Emoji
- Unicode emoji: use the emoji character directly
- Custom emoji: `<:name:ID>` or `<a:name:ID>` for animated
- For reactions via API, URL-encode unicode emoji (e.g. `%F0%9F%91%8D` for thumbs up)

## Limits
- Message content: 2000 characters max
- Embed description: 4096 characters
- Embed fields: 25 max
- Total embed size: 6000 characters
