---
description: Use a Sprite Builder Docuspace — an S3-backed file store attached to a project, with no sprite or worker. Create it, then read/list/write/delete files and folders over the REST API.
---

# Sprite Builder — Docuspaces

A **Docuspace** is plain S3-backed file storage owned by a project. There is **no
sprite and no worker** — creation and all file ops are synchronous/instant. Use it
to store and serve files (markdown, images, assets) without a running sandbox.

> Requires the Sprite Builder server's `S3_*` env to be configured. If it isn't,
> file ops return `400 "S3 is not configured"` (the docuspace record still exists).

## Tools

- `sprite_builder_create_docuspace` (with `project_id`) — instant, no provisioning.
- `sprite_builder_list_docuspaces`, `sprite_builder_get_docuspace`,
  `sprite_builder_delete_docuspace` (drops all its objects — confirm first).
- `sprite_builder_docuspace_read` — read a file or list a dir (omit `path` for root).
- `sprite_builder_docuspace_write` — create/overwrite a file. `encoding` is
  `utf8` (default) or `base64` (upload binaries as text). Max 5 MiB. Content-type
  is inferred from the extension so files render/download correctly.
- `sprite_builder_docuspace_delete_path` — delete a file, or a folder and all under it.
- `sprite_builder_docuspace_create_folder` — make an empty folder visible (folders
  are otherwise implicit — they exist when something lives under `<path>/`).

## Gotchas

- Folders are **implicit**; you only need `create_folder` to show an empty one.
- Paths are validated/jailed (no `..`, no absolute paths).
