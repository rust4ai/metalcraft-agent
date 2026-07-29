# Metalcraft Notes pack

Lets an agent save and manage **markdown notes** through **Metalcraft Notes**
(notes.metalcraftai.com) — part of the shared-login Metalcraft ecosystem. Agent-first:
the agent writes notes as plain markdown.

## Connects with

- **`METALCRAFT_TOKEN`** — a Personal Access Token from the user's Metalcraft account
  (id.metalcraftai.com → Account → Tokens), scoped `notes:read` and/or `notes:write`.
  The **only** setting, and the **same token works across every Metalcraft ecosystem app**.

The API base is **fixed to `https://notes.metalcraftai.com`**. Every tool sends
`Authorization: Bearer $METALCRAFT_TOKEN`. The account is implied by the token; a
**notebook slug** + **page slug** address the content.

## Model
One account owns many **notebooks**, each a **tree of pages** whose body is **markdown**.
Discover notebooks with `mnote_list_notebooks`, then a notebook's pages with
`mnote_list_pages`.

## Tools
| Tool | Method | Path | Scope |
|------|--------|------|-------|
| `mnote_whoami` | GET | `/api/v1/whoami` | read |
| `mnote_list_notebooks` | GET | `/api/v1/notebooks` | read |
| `mnote_create_notebook` | POST | `/api/v1/notebooks` | **write** |
| `mnote_list_pages` | GET | `/api/v1/notebooks/{notebook}/pages` | read |
| `mnote_get_page` | GET | `/api/v1/notebooks/{notebook}/pages/{slug}` | read |
| `mnote_create_page` | POST | `/api/v1/notebooks/{notebook}/pages` | **write** |
| `mnote_update_page` | PATCH | `/api/v1/notebooks/{notebook}/pages/{slug}` | **write** |
| `mnote_delete_page` | DELETE | `/api/v1/notebooks/{notebook}/pages/{slug}` | **write** |

Reads auto-approve; create/update/delete require approval. Writes need a token with the
`notes:write` scope (403 otherwise). Page bodies are plain markdown. *(Search + share
tools arrive with the app's N5/N6.)*

## Ships
- `personas/metalcraft-notes-agent.json` — a notes assistant scoped to this pack.
- `skills/metalcraft-notes.md` — the whoami → list_notebooks → list_pages →
  create/update workflow, scope + markdown conventions.
