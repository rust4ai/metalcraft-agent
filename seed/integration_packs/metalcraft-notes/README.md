# Metalcraft Notes pack

Lets an agent save and manage **markdown notes** through **Metalcraft Notes**
(notes.metalcraftai.com) — part of the shared-login Metalcraft ecosystem. Agent-first:
the agent writes notes as plain markdown.

## Connects with

- **`METALCRAFT_TOKEN`** — a Personal Access Token from the user's Metalcraft account
  (id.metalcraftai.com → Account → Tokens), scoped `read` and/or `write`.
  The **only** setting, and the **same token works across every Metalcraft ecosystem app**.

The API base is **fixed to `https://notes.metalcraftai.com`**. Every tool sends
`Authorization: Bearer $METALCRAFT_TOKEN`. The account is implied by the token; a **note
slug** addresses the content.

## Model
Notes are **flat** — there are no folders/notebooks and no nesting. One account owns many
**notes** (body = **markdown**), each addressed by `slug`. Notes are organized by
**categories**: color-coded tags, at most 12 per account (defaults: `home`, `work`,
`personal`). A note can carry several categories. Discover notes with `mnote_list_notes`
and categories with `mnote_list_categories`.

## Tools
| Tool | Method | Path | Scope |
|------|--------|------|-------|
| `mnote_whoami` | GET | `/api/v1/whoami` | read |
| `mnote_list_notes` | GET | `/api/v1/notes` | read |
| `mnote_get_note` | GET | `/api/v1/notes/{slug}` | read |
| `mnote_links` | GET | `/api/v1/notes/{slug}/links` | read |
| `mnote_create_note` | POST | `/api/v1/notes` | **write** |
| `mnote_update_note` | PATCH | `/api/v1/notes/{slug}` | **write** |
| `mnote_delete_note` | DELETE | `/api/v1/notes/{slug}` | **write** |
| `mnote_list_categories` | GET | `/api/v1/categories` | read |
| `mnote_create_category` | POST | `/api/v1/categories` | **write** |

Reads auto-approve; create/update/delete require approval. Writes need a token with the
`write` scope (403 otherwise). Note bodies are plain markdown; categories are addressed by
`id` (from `mnote_list_categories`). *(Search + share tools live in the web app.)*

## Linking
Notes reference each other with **`[[slug]]`** (or `[[slug|Display Text]]`) written inline
in the markdown body — Obsidian's syntax, so it survives export. Nothing special is sent
over the wire: links are just characters in `body`, and the server derives the link graph
from them.

`mnote_links` reports a note's outgoing links, its **backlinks** (what points at it —
something `mnote_list_notes` can't tell you), and its `broken` targets. Passing a broken
target as `slug` to `mnote_create_note` creates the note those links were waiting for, and
they all resolve at once.

One sharp edge worth knowing: a `|` or `]` inside the display text makes the link parse to
**nothing at all** — no link, no error. The skill and tool descriptions say so; the web
editor sanitizes it automatically, but an agent writing raw markdown has to avoid it.

## Ships
- `personas/metalcraft-notes-agent.json` — a notes assistant scoped to this pack.
- `skills/metalcraft-notes.md` — the whoami → list_notes / list_categories →
  create/update workflow, scope + markdown conventions, and the `[[slug]]` linking rules.
