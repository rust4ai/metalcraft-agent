# Metalcraft Drive pack

Persona, skill, and HTTP API tools for **Metalcraft Drive** (`drive.metalcraftai.com`) — a
personal Google-Drive-style file store (folders + files on DigitalOcean Spaces) in the
Metalcraft ecosystem. Authenticated by one `METALCRAFT_TOKEN` (`Authorization: Bearer`),
the same account token used across every Metalcraft subapp.

## Env
- `METALCRAFT_TOKEN` — a Metalcraft account token. Reads need any valid token; writes
  (create/upload/move/rename/trash/delete) need the **write** scope.

## Tools (`mdrv_` prefix)
| Tool | Method | Purpose |
| --- | --- | --- |
| `mdrv_whoami` | GET | Validate token, read scopes |
| `mdrv_list_folder` | GET | List a folder's subfolders + files (+breadcrumb) |
| `mdrv_create_folder` | POST | Create a folder (write) |
| `mdrv_get_file` | GET | One file's metadata |
| `mdrv_update_file` | PATCH | Rename / move / star / trash-toggle (write) |
| `mdrv_delete_file` | DELETE | Permanent delete — object + record (write) |
| `mdrv_presign_upload` | POST | Step 1: mint a direct-to-Spaces PUT URL (write) |
| `mdrv_confirm_upload` | POST | Step 2: finalize after the PUT (write) |
| `mdrv_list_starred` | GET | Starred files |
| `mdrv_list_trash` | GET | Trashed files |

Reads and idempotent lookups auto-approve in the agent; create/upload/move/delete require
approval (see `src/approval.rs`, the `mdrv_` branch).

## Upload flow
`mdrv_presign_upload` → raw HTTP **PUT** of the bytes to the returned `upload_url` →
`mdrv_confirm_upload` with the `s3_key`. The bytes never pass through the API or the agent.
