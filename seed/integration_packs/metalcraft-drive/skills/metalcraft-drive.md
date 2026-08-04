# Metalcraft Drive skill

Read/manage a user's files on **Metalcraft Drive** (`https://drive.metalcraftai.com/api/v1`),
authenticated by a single `METALCRAFT_TOKEN` (`Authorization: Bearer`). The account is
implied by the token — never pass a user id.

## Model
- A personal tree of **folders** and **files**, each with a UUID `id`.
- A file's `folder_id` is `null` at the drive **root**.
- Files can be **starred**. **Trash** is a reversible soft-delete; permanent delete is separate.

## Workflow
1. **`mdrv_whoami`** — confirm the token and read `scopes`. Reads work with any token;
   **create/upload/move/rename/trash/delete need `write`**. If write is missing, tell the
   user to mint a write token at id.metalcraftai.com → Account → Tokens, and stop.
2. **Navigate** with **`mdrv_list_folder`** (`folder='root'`, or a folder UUID). Returns
   `{ folder, breadcrumb, folders, files }`. Resolve the exact `id` from here before acting.
3. **Act:**
   - New folder → **`mdrv_create_folder`** (`name`, optional `parent_id`).
   - Get metadata → **`mdrv_get_file`**.
   - Rename / move / star / trash → **`mdrv_update_file`** (send only the fields that change;
     `folder_id` — a UUID or `null` for root — ONLY to move; `trashed` true/false).
   - Permanent delete → **`mdrv_delete_file`** (irreversible — confirm the name first).
   - Starred / Trash listings → **`mdrv_list_starred`** / **`mdrv_list_trash`**.

## Uploading (two steps + a raw PUT)
1. **`mdrv_presign_upload`** (`name`, `content_type`, optional `folder_id`) → `{ upload_url, s3_key }`.
2. **HTTP PUT** the file's raw bytes to `upload_url` — no auth header, `Content-Type` = the file's type.
   The bytes go straight to DigitalOcean Spaces, not through the API.
3. **`mdrv_confirm_upload`** (`s3_key`) → creates the file record and returns it.

If you cannot perform a raw byte PUT, give the user the `upload_url` (or point them at the
Drive web app) rather than pretending the upload finished.

## Rules of thumb
- Don't guess ids — always list first.
- Prefer **Trash** (`mdrv_update_file trashed=true`) over permanent delete.
- Summarize what changed afterward; never reveal the token or raw tool URLs.
