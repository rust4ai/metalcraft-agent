---
description: How to read an email mailbox over read-only IMAP
---

# Email over IMAP (read-only)

These tools connect to an IMAP server over implicit TLS using the configured
credentials and read mail. They are **native** tools (not declarative HTTP-API
tools) because IMAP is not HTTP. Every session opens the mailbox with **`EXAMINE`**
(SELECT without write access), so nothing is ever modified — no `\Seen` flags, no
moves, no deletes. There is intentionally **no send / reply / delete** capability
in this pack.

## Credentials (key store)

| Key | Required | Meaning |
|-----|----------|---------|
| `IMAP_HOST` | yes | Server host, e.g. `imap.gmail.com`, `imap.fastmail.com` |
| `IMAP_USER` | yes | Full email address / login |
| `IMAP_PASSWORD` | yes | Password or **App Password** (Gmail requires an App Password) |
| `IMAP_PORT` | no | Defaults to `993` (implicit TLS) |

> **Gmail / Google Workspace:** enable 2-Step Verification, then create an App
> Password at https://myaccount.google.com/apppasswords and use it as
> `IMAP_PASSWORD`. Host is `imap.gmail.com`. Note Gmail's folder names look like
> `[Gmail]/Sent Mail`, `[Gmail]/All Mail` — list them with `email_list_mailboxes`.

## Workflow

1. `email_list_mailboxes` — verify credentials and see folders. No params.
2. Find messages:
   - `email_search` — combine `from`, `subject`, `text` (full-text incl. body),
     and `since` (all AND together). `mailbox` defaults to `INBOX`, `limit`
     default 25.
   - `email_list_recent` — recent mail by `hours` (default 24), `limit`
     default 50.
   Both return header rows `{ uid, from_addr, from_name, subject, date }`, newest
   first. Fetching is header-only, so these are light.
3. `email_get_message` — pass a `uid` (and the same `mailbox`) to read the full
   parsed message: `{ uid, message_id, from_addr, from_name, to, subject, date,
   body_text, snippet }`.

## Search criteria notes

- `since` uses the IMAP date format **`DD-Mon-YYYY`** (e.g. `01-Jul-2026`). IMAP
  `SINCE` is date-granular (it ignores the time of day).
- `from` / `subject` match substrings (server-side `FROM` / `SUBJECT`); `text`
  matches anywhere in the message (`TEXT`). Providing several narrows the result
  (logical AND). Providing none lists everything (bounded by `limit`).
- `uid` values are **per-mailbox** — a uid from INBOX is not valid in another
  folder. Always read a message from the mailbox you searched.

## Tools

| Tool | What it does |
|------|--------------|
| `email_list_mailboxes` | List folders; verify credentials. No params. |
| `email_search` | Search a mailbox → header rows. Optional `mailbox`, `from`, `subject`, `text`, `since`, `limit`. |
| `email_list_recent` | Recent mail by time window → header rows. Optional `mailbox`, `hours`, `limit`. |
| `email_get_message` | Full message by `uid` (+ optional `mailbox`). |

## Notes & limits

- Read-only by design. Asked to send/reply/delete/flag → explain the integration
  can't, rather than attempting it.
- HTML-only messages may have an empty `body_text`; report that instead of
  inventing content.
- Summarize faithfully; don't fabricate senders, dates, or content not present in
  the tool output.
- Never reveal the credentials.
