---
description: Read a Metalcraft user's email — search, list, and read messages and threads
---

# Metalcraft Email

Read a Metalcraft user's email — search, list, and read messages and threads.

These tools call the Metalcraft Email REST API at
`https://email.metalcraftai.com/api/v1` using the configured `METALCRAFT_TOKEN`, sent as
`Authorization: Bearer $METALCRAFT_TOKEN`. The base URL is fixed — the only thing to
configure is the token. That single token is the user's **Metalcraft account** credential
and works across every ecosystem app; there are no per-service keys.

## What this is (and isn't)
Metalcraft Email is a **read-only cache** of the user's real mailbox, kept in sync over
IMAP. The remote provider (Gmail, Fastmail, iCloud, …) stays the source of truth; this
service is a fast, searchable copy.

**Read-only — hard limit.** There are no tools to send, reply, forward, delete, mark
read/unread, move, or otherwise change mail, and the backend does not support it. If the
user asks you to send or reply, say the integration is read-only and offer to **draft**
text they can send themselves.

**Attachments are metadata-only.** You can see each attachment's `filename`,
`content_type`, and `size_bytes`, but the bytes are not retrievable (`downloadable` is
false). Never claim to have read an attachment's contents.

## The model: account → mailboxes → messages → threads
- **The token implies the account.** You never pass a user id.
- One account may connect **several mailboxes** (`memail_accounts`), each with an `id`,
  an `email`, and a `status`. Scope a listing to one mailbox by passing its `id` as
  `account`; omit to span all.
- A **message** has a UUID `id`, a `thread_id`, sender/recipients, subject, snippet, and
  (via `memail_get`) full bodies + attachment refs.
- A **thread** groups a conversation; read it all with `memail_thread`.

## Workflow
1. **`memail_whoami`** — confirm the token works. (Everything here needs only read.)
2. **Find the mail:**
   - "Did I get an email about X?", "what did <person> say about Y?" → **`memail_search`**
     with a concise keyword query. This is the primary path — it matches subject, sender,
     and body. Prefer it over paging.
   - "What's new / unread / recent?" → **`memail_list`** (optionally `unread=true`, or
     scoped to one `account`). Page older mail with `before` = the last message's
     `sent_at`.
   - Both return **summaries only — no body.**
3. **Read specifics:** call **`memail_get`** on a message `id` for the full body,
   recipients, and attachment list. Use **`memail_thread`** on a `thread_id` to see the
   whole back-and-forth first.

## Times
Wire times are **UTC ISO-8601** (`sent_at`, `received_at`). Report them in the user's
local time when you know it. `memail_list`/`memail_search` are newest-first; `memail_thread`
is oldest-first (reading order).

## Freshness
If `memail_accounts` shows a mailbox with `status: "auth_error"`, its sync has stopped
(the app password likely needs re-entering in Metalcraft Email) — warn the user that that
mailbox's cached mail may be stale before relying on it.

## Privacy
Email is sensitive. Surface only what was asked, quote sparingly, never reveal the token
or raw tool URLs, and don't fabricate messages — if a search returns nothing, say so
plainly rather than guessing.
