# Metalcraft Notes

Save and manage the user's markdown notes.

These tools call the Metalcraft Notes REST API at `https://notes.metalcraftai.com/api/v1`
using the configured `METALCRAFT_TOKEN`, sent as `Authorization: Bearer $METALCRAFT_TOKEN`.
The base URL is fixed — the only thing to configure is the token. That single token is the
user's **Metalcraft account** credential and works across every ecosystem app; there are no
per-service keys.

## The model: account → notebooks → page tree → markdown
- **The token implies the account.** You never pass a user id.
- **Notebooks** are the top-level spaces (e.g. `work`, `personal`), addressed by `slug`.
  Discover them with `mnote_list_notebooks`.
- **Pages** form a tree inside a notebook (`parent` nests one under another), addressed by
  `slug` within the notebook.
- A page's **body is plain markdown** — the source of truth you read and write.

## Scopes (read vs write)
`mnote_whoami` returns the token's `scopes`. Creating, updating, or deleting pages — and
creating notebooks — requires **`notes:write`**. Without it those calls return 403; tell the
user to mint a token with `notes:write` at id.metalcraftai.com → Account → Tokens.

## Workflow
1. **`mnote_whoami`** — validate the token, read `scopes`.
2. **`mnote_list_notebooks`** — find the target notebook's `slug` (or `mnote_create_notebook`).
3. **`mnote_list_pages(notebook)`** — see the page tree; find a page's `slug`.
4. **Read:** `mnote_get_page(notebook, slug)` → the page incl. its markdown `body`.
5. **Write (needs `notes:write`):** `mnote_create_page(notebook, title, body, parent?)`,
   `mnote_update_page(notebook, slug, …)`, `mnote_delete_page(notebook, slug)`.

## Writing good notes
- Author clean markdown: a clear `# title`-less body (the `title` is a separate field),
  headings, bullet/number lists, ` ``` ` code fences, tables, and `- [ ]` task lists.
- **Nest deliberately:** pass `parent` (a sibling page's slug) to build a tree rather than
  piling pages at the root.
- `mnote_update_page` **replaces** the fields you send. To append or edit, call
  `mnote_get_page` first, modify the returned markdown, and resend the complete `body`.
- Confirm the exact page (title) before deleting; summarize what changed afterward.
- Never reveal the token or raw tool URLs.
