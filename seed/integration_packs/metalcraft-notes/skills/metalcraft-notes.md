# Metalcraft Notes

Save and manage the user's markdown notes.

These tools call the Metalcraft Notes REST API at `https://notes.metalcraftai.com/api/v1`
using the configured `METALCRAFT_TOKEN`, sent as `Authorization: Bearer $METALCRAFT_TOKEN`.
The base URL is fixed — the only thing to configure is the token. That single token is the
user's **Metalcraft account** credential and works across every ecosystem app; there are no
per-service keys.

## The model: account → flat notes (markdown) + category tags
- **The token implies the account.** You never pass a user id.
- **Notes are flat** — there are no folders/notebooks and no nesting. Each note is
  addressed by its `slug`. Discover them with `mnote_list_notes`.
- A note's **body is plain markdown** — the source of truth you read and write.
- **Categories** are color-coded tags (at most 12 per account; defaults `home`, `work`,
  `personal`). A note can have several. List them with `mnote_list_categories`; tag notes
  by passing category **ids** to `mnote_create_note` / `mnote_update_note`.

## Scopes (read vs write)
`mnote_whoami` returns the token's `scopes`. Creating, updating, or deleting notes — and
creating categories — requires **`write`**. Without it those calls return 403; tell the
user to mint a token with `write` at id.metalcraftai.com → Account → Tokens.

## Workflow
1. **`mnote_whoami`** — validate the token, read `scopes`.
2. **`mnote_list_notes`** — see the notes; find a note's `slug`. Optionally `sort` by
   `updated` (last edited) or `accessed` (last opened), or filter by a `category` id.
3. **`mnote_list_categories`** — resolve category names to ids for tagging/filtering.
4. **Read:** `mnote_get_note(slug)` → the note incl. its markdown `body`.
   `mnote_links(slug)` → what it links to, what links back, and which `[[targets]]` are
   still missing.
5. **Write (needs `write`):** `mnote_create_note(title, body, categories?, slug?)`,
   `mnote_update_note(slug, …)`, `mnote_delete_note(slug)`,
   `mnote_create_category(name)` (auto-assigns a color; 409 at the 12-category cap).

## Linking notes together
Notes reference each other with **`[[slug]]`** — or **`[[slug|Display Text]]`** to control
the wording — written inline in the markdown body (Obsidian's syntax; it survives export).
This is the main way a vault becomes more than a pile of files, and you are usually the one
writing it.

- **Link instead of restating.** If a note touches a topic another note already covers, link
  to it rather than summarizing it again.
- **Get the slug first.** `mnote_list_notes` is the lookup table — call it before writing a
  note that should reference existing ones. Never guess at a slug's spelling.
- **Forward links are fine.** `[[a-note-that-doesnt-exist-yet]]` is legal; it shows as a
  to-be-created link and resolves the moment that note exists. Use it when you know the
  note *should* exist.
- **Creating a linked-to note:** `mnote_links(slug)` lists a note's `broken` targets. Pass
  that exact target as `slug` to `mnote_create_note` so every existing link to it resolves.
- **Never put `|` or `]` inside the display text.** `[[plan|Q3|Q4]]` silently parses to
  nothing at all — no link, no error. Rewrite the wording instead.
- **Traverse with `mnote_links`.** Backlinks answer "what already refers to this?", which
  `mnote_list_notes` cannot. Following links and reading neighbours is usually a better way
  to assemble context than re-listing everything.

## Writing good notes
- Author clean markdown: a clear `# title`-less body (the `title` is a separate field),
  headings, bullet/number lists, ` ``` ` code fences, tables, and `- [ ]` task lists.
- **Link deliberately:** weave `[[slug]]` references into the prose where they belong (see
  above) instead of appending a bare "Related" list.
- **Tag deliberately:** pass `categories` (an array of category ids) so notes land under
  the right tags. `mnote_update_note`'s `categories` REPLACES the whole tag set — send the
  full list the note should have.
- `mnote_update_note` **replaces** the fields you send. To append or edit the body, call
  `mnote_get_note` first, modify the returned markdown, and resend the complete `body`.
- Confirm the exact note (title) before deleting; summarize what changed afterward.
- Never reveal the token or raw tool URLs.
