---
description: How to administer a Discord server over the REST API — channels, roles, members, bans, timeouts, moderation, and the audit log, plus permission bitfields and the role hierarchy
---

# Discord Server Administration

These tools call the Discord REST API (`https://discord.com/api/v10`) authenticated
by a single **`DISCORD_BOT_TOKEN`** (sent as the `Authorization: Bot <token>` header —
you never pass it yourself). What you can do is bounded by two things:

1. **The bot's permissions** in the guild (granted via its roles / channel overwrites).
2. **The bot's position in the role hierarchy** — the bot can only manage roles and
   members whose highest role is *below* the bot's highest role, and it can never
   act on the **guild owner**.

## Setup (one-time, done by a human)

- Create an application + bot in the Discord Developer Portal.
- Invite the bot to the server with the `bot` scope and the permissions you need
  (Manage Server, Manage Channels, Manage Roles, Kick Members, Ban Members,
  Moderate Members, Manage Messages, Manage Webhooks, View Audit Log).
- To use `discord_list_members`, enable the **Server Members Intent** (a privileged
  intent) on the application.
- Store the token as the key `DISCORD_BOT_TOKEN`.

## IDs are snowflakes

Every guild, channel, role, user, and message is identified by a numeric
**snowflake** string. Always resolve to an exact id before a mutating call:

- **`discord_get_guild`** — server overview + member counts.
- **`discord_list_guild_channels`** — channels with ids, types, parents, positions.
- **`discord_list_roles`** — roles with ids, permissions, positions.
- **`discord_search_members`** (by name prefix) / **`discord_get_member`** — resolve
  a person to a `user_id`.

Never pass a display name to a mutating tool — look up the id first.

## Channels

- **`discord_create_channel`** — `type`: 0 text, 2 voice, 4 category, 5 announcement,
  13 stage, 15 forum. Nest under a category with `parent_id`.
- **`discord_modify_channel`** / **`discord_delete_channel`** — deletion is permanent.
- **`discord_edit_channel_permissions`** — set an `allow`/`deny` overwrite for a role
  (`type: 0`) or member (`type: 1`) on one channel.

## Roles

- **`discord_create_role`** / **`discord_modify_role`** / **`discord_delete_role`**.
- Assign/remove **one** role with **`discord_add_member_role`** /
  **`discord_remove_member_role`** — these are surgical.
- `discord_modify_member`'s `roles` field **replaces the member's entire role set**.
  Only use it when you intend to set the full list; otherwise you will strip roles.

## Members: kick vs. ban vs. timeout

- **Kick** (`discord_kick_member`) — removes the member; they can rejoin with a new
  invite. Needs Kick Members.
- **Ban** (`discord_create_ban`) — removes and blocks rejoining; optional
  `delete_message_seconds` (0–604800) also purges their recent messages. Reverse with
  **`discord_remove_ban`**. Needs Ban Members.
- **Timeout** (`discord_modify_member` with `communication_disabled_until`) — mutes
  the member for a period without removing them. The value is an **ISO-8601**
  timestamp **at most 28 days** in the future; pass an empty value to clear it. Needs
  Moderate Members.

## Message moderation

- **`discord_delete_message`** — one message.
- **`discord_bulk_delete_messages`** — 2–100 messages in one call; **all must be
  newer than 2 weeks** or the whole call fails.
- **`discord_pin_message`** — max 50 pins per channel.

## Permissions are bitfields

Role permissions and channel overwrites are **bitwise flags** passed as **decimal
strings**. Combine flags by OR-ing their values (e.g. VIEW_CHANNEL `1024` +
SEND_MESSAGES `2048` = `"3072"`). Compute the exact bitfield and sanity-check it
before applying — a wrong value can silently grant or remove access.

## Audit log

**`discord_get_audit_log`** shows who did what. Filter with `action_type` (e.g. 22
= MEMBER_BAN_ADD, 20 = MEMBER_KICK, 10 = CHANNEL_CREATE, 25 = MEMBER_ROLE_UPDATE)
and/or `user_id`. Great for "who deleted #general" investigations and periodic
moderation summaries.

## Operating discipline

- **Confirm destructive actions.** Deleting channels/roles, kicking, banning, and
  bulk-deleting are disruptive or irreversible — state the exact target and get
  explicit confirmation unless already authorized for that specific action.
- **Rate limits.** On HTTP `429`, respect the `retry_after` value before retrying;
  these tools do not auto-retry.
- **Errors.** A `403` with code `50013 Missing Permissions` means the bot lacks the
  permission or is too low in the hierarchy — fix the bot's role/permissions rather
  than retrying.
- **Never** reveal the token, webhook tokens, or invite secrets.
