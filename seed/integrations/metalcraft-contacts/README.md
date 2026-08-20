# Metalcraft Contacts pack

Persona, skill, and HTTP API tools for **Metalcraft Contacts** (contacts.metalcraftai.com) — the
ecosystem's agent-first personal address book / CRM.

## Setup
1. Mint a Metalcraft account token at `id.metalcraftai.com → Account → Tokens`. Give it the
   **write** scope if the agent should create/update/delete contacts (reads work with any token).
2. Set `METALCRAFT_TOKEN` in the agent environment (the same token used by the Notes and
   Calendar packs — it's one account credential across the whole ecosystem).
3. Enable this pack. The API base is fixed to `https://contacts.metalcraftai.com`.

## Model
One account owns a **flat list of contacts**; grouping is via **tags** (no folders). Each contact
is addressed by a `slug` and carries name fields, organization/title, an optional-year birthday,
tags, markdown notes, and repeatable phones/emails/addresses/links.

## Tools (`mcon_*`)
| Tool | Purpose |
|---|---|
| `mcon_whoami` | validate token, read scopes |
| `mcon_list_contacts` | list / filter (q, tag, sort) |
| `mcon_search` | full-text search |
| `mcon_get_contact` | read one contact in full |
| `mcon_create_contact` | add a person *(write)* |
| `mcon_update_contact` | edit — replaces sent fields *(write)* |
| `mcon_delete_contact` | remove a person *(write)* |
| `mcon_upcoming_birthdays` | who's coming up |
| `mcon_set_photo_from_url` | set a photo from an image URL *(write)* |

## Composing across the ecosystem
- **Birthdays → Calendar / reminders:** poll `mcon_upcoming_birthdays`, then create a Calendar
  event or schedule a nudge for each.
- **Message a person:** resolve a contact's primary phone/email and hand it to the messaging
  gateway to text/email them.
- **Enrich:** keep durable facts about a person in their markdown `notes`.

See `skills/metalcraft-contacts.md` for the full workflow and editing rules.
