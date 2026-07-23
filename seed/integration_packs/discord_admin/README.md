# Discord Admin integration pack

Administer a Discord server from a metalcraft agent through Discord's REST API
(`https://discord.com/api/v10`) using a single **bot token**. The pack bundles a
persona (`discord-admin-agent`), a skill (`discord-server-admin`), ~30 HTTP tools,
and an optional weekly audit-log flow template. No relay service, no WebSocket.

## What it can do

Manage channels and permission overwrites; create/edit/delete roles and
assign/remove member roles; manage members (nickname, roles, kick, ban, timeout);
moderate messages (delete, bulk-delete, pin); manage invites and webhooks; and read
the audit log. Read-only tools (`discord_list_*`, `discord_get_*`,
`discord_search_*`) auto-approve; every mutating tool requires approval.

## Which credential? A **bot token**.

Discord has three kinds of credentials, and only one works here:

| Credential | Header | Use case | Use here? |
|---|---|---|---|
| **Bot token** | `Authorization: Bot <token>` | Automation acting as a bot user in guilds | ✅ **Yes** |
| OAuth2 bearer (user) | `Authorization: Bearer <token>` | Acting on behalf of a logged-in user (needs an OAuth flow) | ❌ Can't do guild management |
| User account token | (raw) | — | 🚫 Self-botting — violates Discord's ToS |

The guild-management endpoints this pack calls are **only** supported with a bot
token. The literal word `Bot ` in front of the token is mandatory — a raw token
returns `401 Unauthorized`. Every tool sends this header for you; you never pass the
token in a prompt.

## Setup (one-time, done by a human)

1. **Create the application + bot.** Go to the
   [Discord Developer Portal](https://discord.com/developers/applications) →
   **New Application**, then open the **Bot** tab (a bot user is created with the app).
2. **Copy the token.** On the Bot tab, click **Reset Token** and copy the value —
   this is your `DISCORD_BOT_TOKEN`. It is shown only once; reset again if lost. Treat
   it like a password: anyone with it controls the bot.
3. **Enable privileged intents (only if needed).** To use `discord_list_members`,
   turn on the **Server Members Intent** under *Privileged Gateway Intents* on the Bot
   tab. The other admin tools need no intent.
4. **Build an invite link.** Go to **OAuth2 → URL Generator**, tick the **`bot`**
   scope, then tick the permissions the bot should have — for full admin,
   **Administrator**; otherwise the specific ones (Manage Server, Manage Channels,
   Manage Roles, Kick Members, Ban Members, Moderate Members, Manage Messages, Manage
   Webhooks, View Audit Log). Copy the generated URL.
5. **Invite the bot.** Open that URL, pick the server, and authorize (you need Manage
   Server on that guild). The bot now appears in the member list.
6. **Position the bot's role.** In **Server Settings → Roles**, drag the bot's role
   **above** any role it must manage — a bot can only edit roles and moderate members
   *below* its own highest role, and can never act on the server owner. Getting this
   wrong is the usual cause of `403 / 50013 Missing Permissions`.
7. **Store the token.** Save it as the key **`DISCORD_BOT_TOKEN`** (workshop key store,
   or exported in the environment). The pack's `requires_env` surfaces it in the
   key-store UI once the pack is enabled.

## Enable and use

1. Enable the pack (workshop Integration Packs UI, or the `integration_enable` meta
   tool). It ships **disabled** by default.
2. Make sure `DISCORD_BOT_TOKEN` is set (step 7 above).
3. Run the persona, e.g.:

   ```bash
   metalcraft-agent -p discord-admin-agent "list the channels and roles in guild <guild_id>"
   ```

   Discord IDs are numeric "snowflakes"; the agent resolves names to ids with
   `discord_search_members` / `discord_list_*` before acting.

## Notes & limits

- **Permissions come from the bot's roles**, not the token — a valid token with too
  few permissions (or a role positioned too low) still gets `403`.
- **Rate limits:** on HTTP `429`, respect the `retry_after` value; the tools do not
  auto-retry.
- **Audit-log reasons** (`X-Audit-Log-Reason`) can't be set per call — tool headers
  only expand environment/key-store variables, not per-call arguments.
- **Never** commit or paste the token; if it leaks, reset it in the Developer Portal.

See the `discord-server-admin` skill for endpoint details, permission bitfields, the
role hierarchy, and the kick/ban/timeout distinctions.
