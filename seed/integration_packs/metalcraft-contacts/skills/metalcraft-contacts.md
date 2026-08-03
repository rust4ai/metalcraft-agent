# Metalcraft Contacts

Read and manage the user's personal contacts / address book.

These tools call the Metalcraft Contacts REST API at `https://contacts.metalcraftai.com/api/v1`
using the configured `METALCRAFT_TOKEN`, sent as `Authorization: Bearer $METALCRAFT_TOKEN`.
The base URL is fixed — the only thing to configure is the token. That single token is the
user's **Metalcraft account** credential and works across every ecosystem app; there are no
per-service keys.

## The model: account → flat contact list → tags
- **The token implies the account.** You never pass a user id.
- Contacts are a **flat list** — there are no folders or address books. Grouping is done
  entirely with **`tags`** (e.g. `family`, `client`, `dnd-crew`).
- Each contact is addressed by a **`slug`** (derived from the display name, auto-deduped).
- A contact's fields:
  - `display_name` (required), `given_name`, `family_name`, `nickname`
  - `organization`, `job_title`
  - `birth_month` (1–12), `birth_day` (1–31), `birth_year` — **year is optional**
  - `tags` (array of strings), `notes` (freeform markdown)
  - `phones`, `emails`, `urls` — arrays of `{label, value}`; `addresses` — arrays of
    `{label, street, city, region, postal, country}`

## Scopes (read vs write)
`mcon_whoami` returns the token's `scopes`. Creating, updating, or deleting contacts requires
**`write`**. Without it those calls return 403; tell the user to mint a token with `write` at
id.metalcraftai.com → Account → Tokens.

## Workflow
1. **`mcon_whoami`** — validate the token, read `scopes`.
2. **Find the person:** `mcon_list_contacts` (optionally `q` / `tag`) or `mcon_search` (full-text)
   → get their `slug`.
3. **Read:** `mcon_get_contact(slug)` → the full record.
4. **Write (needs `write`):** `mcon_create_contact(display_name, …)`,
   `mcon_update_contact(slug, …)`, `mcon_delete_contact(slug)`.
5. **Birthdays:** `mcon_upcoming_birthdays(within)` for who's coming up.

## Editing rules
- `mcon_update_contact` **replaces** the fields you send and leaves the rest untouched. The
  repeatable fields (`phones`, `emails`, `addresses`, `urls`) and `tags` are **whole arrays** —
  to add one phone, `mcon_get_contact` first, append to the array, and resend the **complete**
  array. Sending a partial array overwrites the list.
- Set `birth_month` and `birth_day` **together**; include `birth_year` only if you know it.
- Slugs are stable — renaming `display_name` does not change the slug.

## Good practice
- Put durable facts about a person (how you met, preferences, last conversation) in `notes` as
  clean markdown.
- Use `tags` deliberately so the user can filter later.
- Confirm the exact contact (`display_name`) before deleting; summarize what changed afterward.
- **Compose across the ecosystem:** upcoming birthdays → create a Calendar event or schedule a
  reminder; a contact's primary phone → text them via the messaging gateway.
- Never reveal the token or raw tool URLs.
