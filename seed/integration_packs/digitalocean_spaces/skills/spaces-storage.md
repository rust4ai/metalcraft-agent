---
description: How to read and write files in DigitalOcean Spaces object storage
---

# DigitalOcean Spaces (S3) File Storage

These tools store and retrieve files in **DigitalOcean Spaces**, an S3-compatible object
store. They talk to the S3 REST API at `https://{region}.digitaloceanspaces.com` and sign
every request with **AWS Signature V4** using the configured credentials:

- `DO_SPACES_KEY` — the Spaces access key id
- `DO_SPACES_SECRET` — the Spaces secret access key
- `DO_SPACES_REGION` — the region slug, e.g. `nyc3`, `sfo3`, `ams3`, `fra1`, `sgp1`, `syd1`
  (defaults to `nyc3` if unset)

Create a key pair under **API → Spaces Keys** in the DigitalOcean control panel. The region
must match the region your bucket (Space) lives in, or requests fail to authenticate.

## The model: buckets and keys

- A **bucket** (DigitalOcean calls it a "Space") is the top-level container, e.g.
  `my-app-assets`.
- An **object** is a single file, addressed by its **key** — the full path within the
  bucket, e.g. `reports/2026/q1.pdf`. Spaces has no real folders; the `/` in a key is just
  part of the name, and a `prefix` like `reports/` matches every key that starts with it.

## The core workflow

1. `spaces_list_buckets` — confirm the credentials/region work and find the bucket. This is
   the cheapest connectivity check; run it first if anything seems misconfigured.
2. `spaces_list_objects` — list keys in a bucket (optionally filtered by `prefix`). If the
   response has `is_truncated: true`, pass its `next_continuation_token` back as
   `continuation_token` to fetch the next page.
3. Read or write:
   - `spaces_get_object` — download an object (to a file via `dest_path`, or inline as text).
   - `spaces_put_object` — upload `content` or a local `file_path` to a key.
   - `spaces_delete_object` — remove an object.

## Tools

| Tool | What it does |
|------|--------------|
| `spaces_list_buckets` | List all Spaces (buckets) in the account for the region. No parameters. Returns `{name, creation_date}` per bucket. |
| `spaces_list_objects` | List objects in a `bucket`. Optional `prefix`, `max_keys` (≤1000), `continuation_token`. Returns `{key, size, last_modified, etag}` plus `is_truncated` / `next_continuation_token`. |
| `spaces_get_object` | Download `bucket`/`key`. With `dest_path`, saves the bytes to a local file; without it, returns small UTF-8 text inline (~100 KB cap). |
| `spaces_put_object` | Upload to `bucket`/`key` from `content` (text) **or** `file_path` (local file). Optional `content_type`, `acl`. Overwrites an existing key. |
| `spaces_delete_object` | Delete `bucket`/`key`. Irreversible; idempotent (missing key still succeeds). |

## Reading files

Use **inline text** for small, human-readable files — call `spaces_get_object` with just
`bucket` and `key` and read the returned `content`:

> get the object `config/app.json` from bucket `my-app`

For **binary or large files** (images, PDFs, archives), pass `dest_path` to save to the
agent's upload directory and work with the file:

> get `backups/db.sql.gz` from `my-app` and save it to `db.sql.gz`

`dest_path` is relative to the agent's upload directory; absolute paths must resolve inside
it. You cannot read or write arbitrary files on the host — that jail is intentional.

## Writing files

Provide **exactly one** source:

- `content` — inline text you generate (a report, JSON, a manifest). Set `content_type`
  appropriately, e.g. `application/json` or `text/markdown`.
- `file_path` — a local file already in the upload directory.

```
spaces_put_object(bucket="my-app", key="reports/2026/q1.md",
                  content="# Q1 ...", content_type="text/markdown")
```

### Public vs private

Objects are **private** by default — only signed requests can read them. To make an object
publicly downloadable at `https://{bucket}.{region}.digitaloceanspaces.com/{key}`, pass
`acl: "public-read"`. The tool returns that `public_url` when you do. Only make objects
public when the user explicitly wants a shareable link.

## Safety

- `spaces_put_object` **overwrites** an existing key silently, and `spaces_delete_object`
  is **irreversible**. When the bucket/key is ambiguous or the user didn't clearly ask to
  replace/remove a specific object, confirm before the write or delete.
- Errors come back as the S3 error code and message (e.g. `NoSuchBucket`, `AccessDenied`,
  `SignatureDoesNotMatch`). `AccessDenied` / `SignatureDoesNotMatch` usually mean the key,
  secret, or region is wrong — re-check them and that the region matches the bucket.
- Never reveal the access key or secret.
