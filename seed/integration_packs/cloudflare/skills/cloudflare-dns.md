---
description: How to manage Cloudflare DNS over the Cloudflare API
---

# Cloudflare DNS Operations

These tools call the Cloudflare API (`https://api.cloudflare.com/client/v4`) using the
account tied to the configured `CLOUDFLARE_API_TOKEN`. The token should be a **scoped API
token** (not the legacy global API key) with at least:

- `Zone → Zone → Read` — to list zones
- `Zone → DNS → Edit` — to read and modify DNS records

Every response is wrapped in Cloudflare's envelope: `{ "success": bool, "errors": [...],
"messages": [...], "result": ... }`. Always check `success`; on failure the `errors` array
explains why (e.g. `code 10000` = bad/insufficient token, `81044` = record name mismatch).

## The core workflow

Almost every DNS operation needs a **zone id**, and mutating/deleting a record needs a
**record id**. The normal flow is:

1. `cloudflare_verify_token` — confirm the token is valid and `status` is `active`.
2. `cloudflare_list_zones` — find the zone (domain). Filter with `name` (e.g.
   `example.com`); the zone's `id` is what every record call needs as `zone_id`.
3. `cloudflare_list_dns_records` — list records in that zone (filter by `type` and/or
   `name`). Each record has an `id` (the `record_id`), plus `type`, `name`, `content`,
   `ttl`, `proxied`.
4. Mutate: `cloudflare_create_dns_record`, `cloudflare_update_dns_record` (full replace),
   `cloudflare_patch_dns_record` (partial), or `cloudflare_delete_dns_record`.

## Tools

| Tool | Method | What it does |
|------|--------|--------------|
| `cloudflare_verify_token` | GET | Verify the token is valid/active. Takes no parameters. |
| `cloudflare_list_zones` | GET | List zones (domains). Optional `name`, `per_page`. |
| `cloudflare_list_dns_records` | GET | List DNS records in a zone. Requires `zone_id`; optional `type`, `name`, `per_page`. |
| `cloudflare_get_dns_record` | GET | Get one record by `zone_id` + `record_id`. |
| `cloudflare_create_dns_record` | POST | Create a record. Requires `zone_id`, `type`, `name`, `content`. |
| `cloudflare_update_dns_record` | PUT | **Replace** a record. Requires `zone_id`, `record_id`, `type`, `name`, `content`. |
| `cloudflare_patch_dns_record` | PATCH | Partially update a record. Requires `zone_id`, `record_id`; send only changed fields. |
| `cloudflare_delete_dns_record` | DELETE | Delete a record by `zone_id` + `record_id`. Irreversible. |

## Record fields

- `type` — `A`, `AAAA`, `CNAME`, `TXT`, `MX`, `NS`, `SRV`, `CAA`, etc.
- `name` — the full record name, e.g. `www.example.com` (use `@` or the zone apex name for
  the root). Cloudflare also accepts the bare subdomain and appends the zone.
- `content` — the record value: an IPv4 for `A`, IPv6 for `AAAA`, a hostname for `CNAME`/`MX`,
  the quoted string for `TXT`.
- `ttl` — seconds; **`1` means "automatic"**. When a record is proxied, TTL is forced to auto.
- `proxied` — `true` routes the record through Cloudflare's proxy (orange cloud). Only valid
  for proxiable types (`A`, `AAAA`, `CNAME`). Leave `false`/omit for mail, TXT, NS records.
- `priority` — required for `MX` and `SRV` records (lower = higher priority).
- `comment` — optional free-text note stored on the record.

## PUT vs PATCH

`cloudflare_update_dns_record` (PUT) replaces the **entire** record — any field you don't
send is reset to default. For a small change (flip `proxied`, change `content`, update
`ttl`), prefer `cloudflare_patch_dns_record` (PATCH), which only touches the fields you pass.

## Safety

DNS edits affect a live domain — a wrong `content`, a deleted `A` record, or a broken `MX`
can take a site or its email offline. Before any write or delete:

- Confirm you resolved the **right zone** (`cloudflare_list_zones` by name) and the **right
  record** (`cloudflare_get_dns_record` / list, by id).
- Echo back the intended change (record name, type, old → new value) when intent is ambiguous.
- Treat deletes as irreversible — verify the `record_id` maps to the record the user means.
