# Metalcraft Images — intrinsic pack, image editing, and letting the agent see

**Status:** plan. Nothing here is built yet.

Today an agent cannot generate an image at all. The capability is written and
correct — `packs/metalcraft_images` in `metalcraft-agent-external-packs`, four
`mimg_*` tools against `images.metalcraftai.com`, which spends the caller's
credits through `metalcraft-inference` on-behalf — but it was never published to
`packs.metalcraftai.com` (the live catalog has 16 packs; this is not one of
them) and it is not embedded in the binary. So there is no path by which any pod
obtains it.

This plan does four things, in dependency order:

1. **Make image editing real** — the pack advertises image-to-image; no tool
   parameter reaches it, and the inference model registry has no edit model.
2. **Let the agent see what it made** — port the render-check verdict pattern
   from buildr.space so the agent gets *words about the image*, not the image.
3. **Ship the pack intrinsically** — `seed/agent_packs/metalcraft-images/`,
   ecosystem-tagged, so every pod arrives holding it.
4. **Fix what is broken in the pack as written** — a 30-second timeout on a
   synchronous generation being the one that will bite first.

## The pipe, as it actually is

```
mimg_* (declarative HTTP tool, Bearer METALCRAFT_TOKEN)
  └─> images.metalcraftai.com  POST /api/v1/generations
        ├─ Principal: mck_ PAT (write scope) or mc_session; premium-gated at the edge
        ├─> inference.metalcraftai.com  POST /v1/fal/run
        │     X-Metalcraft-Service-Secret + X-Metalcraft-Act-As: <owner uuid>
        │     authorize → fal.ai → meter → settle against the hub ledger
        ├─ downloads each ephemeral fal image, PUTs bytes to R2 (private)
        └─ rows in `generations` + `images`, returns per-image download URL + credits
```

Everything above the fal call already works. The gaps are at the two ends: the
agent cannot reach it, and the result comes back as a URL nobody can look at.

---

## Phase 0 — decisions to settle first

| Decision | Recommendation | Why |
|---|---|---|
| Pack id | `metalcraft-images` (hyphen), tools stay `mimg_*` | Matches every other ecosystem pack (`metalcraft-email`, `metalcraft-calendar`, `metalcraft-notes`). The current `metalcraft_images` is the odd one out, and it was never published, so renaming costs nothing — no registry slug to purge, no installed pod to migrate. |
| Where it lives | `metalcraft-agent/seed/agent_packs/metalcraft-images/`, and **delete** `metalcraft-agent-external-packs/packs/metalcraft_images` | `seed/agent_packs/` is what `include_dir!` compiles in and `seed::install_seed_agent_packs` installs on every boot, version-gated on `agent_pack.json`. Two copies of one pack in two repos is how they drift. |
| Preset shape | Spawnable, not `library` | "Image Studio" is a coherent agent to start, unlike `metalcraft-packs` which is a library the orchestrator borrows. The orchestrator can delegate to the persona either way. |
| How the agent "sees" | A server-side vision verdict returning **text** | The agent's chat loop is text-only, and so is the `metalcraft` framework's message type. Adding multimodal input to the agent is a much larger change for a worse result: a *falsifiable* verdict ("what is on screen, then pass/fail") is more useful to an agent than a picture it must interpret. Precedent is built and shipped: `buildr-space/backend/src/services/vision.rs`. |

> **On the buildr.space question — yes, you built this.** `services/vision.rs`
> is the opt-in half of a render check: base64 the PNG into a
> `chat/completions` call with an `image_url` content part, routed to inference
> as a first-party service acting for the workspace owner, billed to that
> owner's credits, answering a stated `expect` as
> `{evidence, result: pass|fail|unsure}` and never returning `Err` — an
> unavailable verdict is a verdict with a reason, never a 500 and never a
> silent pass. The act-as identity is read at that single call site from the
> owner's link row, never from anything the caller sent. Phase 2.4 ports that
> module almost verbatim, with the screenshot swapped for a stored image.

---

## Phase 1 — `metalcraft-inference`: edit models must exist and be discoverable

Migration `002_models.sql` seeds exactly one image model,
`bytedance/seedream/v5/pro/text-to-image`. There is nothing to run an edit
against, and `/v1/models` exposes no way to tell an editable model from a
text-to-image one — so neither the images app nor the agent can pick correctly.

**1.1 `migrations/007_image_edit_models.sql`**

- Add the edit model row(s). Confirm the exact fal slug and per-unit price
  before seeding — the Starflask pack's model list is evidence that
  `nano-banana-edit` and friends are live, but the seedream edit slug should be
  read off fal's catalog, not guessed.
- `ALTER TABLE models ADD COLUMN modes TEXT[] NOT NULL DEFAULT '{}'` —
  `{text-to-image}` / `{image-to-image}` (a model may declare both).
- `ALTER TABLE models ADD COLUMN source_input_key TEXT` — which fal `input` key
  carries the source image (`image_url` for most, `image_urls` for some). Put
  it in the registry, not in the images app: then adding an edit model is one
  SQL row and no deploy.

**1.2 `src/controllers/models.rs`** — include `modes` and `source_input_key` in
the `/v1/models` payload.

**1.3 `src/controllers/fal.rs`** — cost is `per_unit × num_images` today, which
is right for edits too. No change expected; confirm an edit response still
carries `images[]` + `seed` in the same shape.

**Acceptance:** `GET /v1/models` returns at least one entry with
`modes: ["image-to-image"]`, and `POST /v1/fal/run` against it with an
`image_url` input returns images and settles credits.

---

## Phase 2 — `metalcraft-images-web`: sources, upload, sight, and a URL that opens

### 2.1 One-call upload for agents — `POST /api/v1/uploads/direct`

`presign` → browser PUTs to R2 → `confirm` is a browser-shaped handshake. A
declarative HTTP tool cannot do the middle step. Add a multipart endpoint that
takes the bytes, writes them with `S3Service::upload_key`, and returns a ready
`upload_id`. Cap the body (20 MB) and validate the content type. The agent's
`http_api` tool already supports `body_mapping: "multipart"` with a local file
path argument, so this becomes one tool call.

### 2.2 Edit from an image already in the gallery — `source_image_id`

`CreateReq` supports `source_upload_id` and a raw `image_url`. The overwhelmingly
common agent loop is *generate, then change that* — which should need no upload
at all. Add `source_image_id: Option<Uuid>`, resolve it against `images` scoped
to `owner_user_id`, and presign a short GET of its `storage_key` exactly as the
upload path does.

Resolution order: `source_image_id` → `source_upload_id` → `image_url`.

### 2.3 Model-aware, fail-before-billing

Read `modes` / `source_input_key` from the `/v1/models` entry (cache it; it is
already fetched on-behalf for the picker):

- `mode: "image-to-image"` against a model that does not declare it → `400`
  *before* the authorize. Today it would authorize, call fal, fail, and refund —
  a slower, noisier way to say the same thing.
- Insert the source under `source_input_key` rather than the hardcoded
  `"image_url"` in `build_input`.

### 2.4 Sight — `POST /api/v1/images/{id}/describe`

A near-verbatim port of `buildr-space/backend/src/services/vision.rs` as
`src/services/vision.rs`:

```jsonc
POST /api/v1/images/{id}/describe
{ "expect": "a red bicycle against a brick wall, no text" }   // optional
→ { "result": "pass" | "fail" | "unsure" | "unavailable",
    "evidence": "…what is visibly in the image, written before the verdict…",
    "model": "…", "reason": "…only with unavailable…", "credits": 12 }
```

Keep every property that makes the buildr version trustworthy:

- **Evidence before verdict**, and a verdict with empty evidence is downgraded
  to `unsure` — "looks right" is not a finding.
- **Never `Err`.** No verdict is `result: "unavailable"` with a reason. A
  missing verdict must never read as a pass.
- **One act-as call site**, read from the authenticated `Principal`, never from
  the request body.
- Bytes read from R2 and inlined as a `data:` URL — the stored object is
  private and `/download` is owner-authenticated, so a URL handed to a vision
  model would 401.
- With no `expect`, the same call is a plain description (`result: "unsure"`
  carries no meaning there — return `"described"`, or omit `result`; pick one
  and say so in the OpenAPI).
- `IMAGES_VISION_MODEL` config, mirroring buildr's `render_model`.

**This is the phase that makes the agent's loop closed:** generate → describe
against the user's own words → correct once → report.

### 2.5 A URL the user can actually open — `POST /api/v1/images/{id}/share`

`GET /api/v1/images/{id}/download` is owner-authenticated, so the "absolute
permanent download url" the pack tells the agent to relay only opens for a
signed-in owner in a browser. Return a time-boxed presigned R2 GET (or a signed
`/s/{token}` route with an expiry) so the link the agent pastes into chat works.

### 2.6 Move the premium gate off read routes

`middleware/auth.rs` rejects `402` for non-premium inside the `Principal`
extractor, which means it fires on `GET /generations` and
`GET /images/{id}/download` too: an account that lapses loses read access to
images it already paid to make. Keep the gate, move it to the spending
handlers — `generations::create` and `images::describe` — where
`require_write()` already lives. (Spending must stay gated at the edge: the
service-secret on-behalf call is trusted as premium downstream.)

### 2.7 While in here

- `generations::list` is a hard `LIMIT 100` with no filters. Add `limit`,
  `before` (cursor), and `mode` so a long-lived agent can page its own history.

---

## Phase 3 — the intrinsic pack in `metalcraft-agent`

```
seed/agent_packs/metalcraft-images/
  agent_pack.json                       manifest_version 2, version 1.0.0,
                                        tags: ["metalcraft-ecosystem"]
  agent_presets/metalcraft-images.json  spawnable; requires_env METALCRAFT_TOKEN
  personas/metalcraft-images-agent.json
  skills/metalcraft-images.md
  integrations/metalcraft-images/
    integration.json                    id metalcraft-images, ecosystem tag
    api_tools/mimg_*.json
```

Copy the shape from `unbundled_packs/metalcraft-email/` — it is the closest
correct example (same auth, same `METALCRAFT_TOKEN`, same ecosystem tag).

### 3.1 Tools

| Tool | Change | Notes |
|---|---|---|
| `mimg_list_models` | keep, extend | Surface `modes` so the model choice is informed; document text-to-image vs edit. |
| `mimg_generate_image` | **`timeout_secs: 300`** | The single most important line in this plan. It declares no timeout today, so it runs at `DEFAULT_TIMEOUT_SECS = 30` while the server synchronously runs fal, downloads every image, and PUTs each to R2. A multi-image run will time out *after* the credits are spent and the row is saved. Ceiling is `MAX_TIMEOUT_SECS = 600`. |
| `mimg_edit_image` | **new** | `model`, `prompt`, one of `source_image_id` \| `upload_id` \| `image_url`, plus `guidance_scale` / `strength` / `seed`. `timeout_secs: 300`. |
| `mimg_upload_image` | **new** | `body_mapping: "multipart"` → `/api/v1/uploads/direct`, for a local file. |
| `mimg_describe_image` | **new** | `image_id` + optional `expect`. This is the seeing tool. |
| `mimg_share_image` | **new** | Returns the openable URL to hand the user. |
| `mimg_get_generation` | keep | |
| `mimg_list_generations` | keep, extend | `limit` / `before` once 2.7 lands. |

### 3.2 Skill (`skills/metalcraft-images.md`)

Rewrite around the closed loop, not just the call:

1. **Pick a model** — `mimg_list_models`; quote the cost (price × `num_images`).
2. **Get a source, if editing** — a prior `image_id` is free and needs no
   upload; `mimg_upload_image` for a local file.
3. **Generate or edit** — with the spending rules that are already right in the
   current skill (confirm an ambiguous model/prompt, never loop speculatively).
4. **Look at it** — `mimg_describe_image` with `expect` set to *the user's own
   words*. On `fail`, one corrective re-roll (reuse the seed, adjust the prompt)
   without asking; if the second attempt also fails, report what the model
   actually saw and stop. Both the describe and the re-roll spend credits — say
   so when quoting cost.
5. **Share / remix** — `mimg_share_image`, and seed reuse for variations.

Plus two failure modes the current skill does not cover:

- **Timeout ≠ failure.** If `mimg_generate_image` times out, the generation is
  probably still running or already saved. Recover with
  `mimg_list_generations`. **Never re-generate** — that double-charges.
- **402** — not premium, or out of credits. Relay plainly, do not retry.

### 3.3 Persona

Keep the existing prompt (it is good) and add the see-then-iterate loop and the
timeout-recovery rule. `tools: ["load_skill"]` + `packs: ["metalcraft-images"]`
is the right shape — the pack's HTTP tools are resolved through the pack.

---

## Phase 4 — agent-side plumbing

**4.1 Approval policy — `src/approval.rs`.** `mimg_*` matches no arm today, so
every one of them falls to the default `Execute` and prompts, including
`mimg_list_models`. Add an arm alongside the `mcal_` / `mnote_` / `mdrv_` ones:

- auto-approve (`ReadFile`): `mimg_list_models`, `mimg_list_generations`,
  `mimg_get_generation`, `mimg_share_image`
- require approval (default `Execute`): `mimg_generate_image`,
  `mimg_edit_image`, `mimg_upload_image`, **and `mimg_describe_image`** — a
  vision call spends credits too, which is exactly why buildr made its verdict
  opt-in.

**4.2 Reach on pods that already booted.** `install_seed_agent_packs` runs every
boot and is version-gated, so the pack *installs* everywhere on upgrade — but
packs default to **disabled**, and the ecosystem auto-enable is a one-shot
guarded by `.metalcraft_packs_seeded` (`paths::ecosystem_packs_seeded_marker`).
Note that `integrations::ecosystem_pack_ids()` currently has **no caller** — the
boot-time auto-enable appears to have been removed when packs moved to the
registry, leaving the marker and its doc comment behind. Decide explicitly:

- **Recommended:** make the marker a record of *which ids* were auto-enabled and
  enable any ecosystem pack not in it, so a newly-shipped first-party pack
  reaches existing pods. Restores `ecosystem_pack_ids()` to having a purpose.
- Otherwise: accept that existing pods need one manual enable, and say so in the
  release note.

**4.3 Dead guards to fix while adjacent** (both mislead the next reader):

- `seed::is_embedded_integration` tests `SEED.get_dir("integrations/{id}")`, and
  `seed/integrations/` no longer exists — it returns `false` for every pack,
  including the vendored ones, and has no callers. Either point it at
  `agent_packs/*/integrations/<id>` and use it to refuse a colliding registry
  install, or delete it.
- The `seed.rs` module doc still documents `integrations/<id>/ ->
  pack-version-gated (see write_integrations)`; `write_integrations` is gone.

**4.4 Tests.** The existing sweeps already cover a new `seed/agent_packs/` entry
(`tests/http_api_tool_test.rs`, the `native_tools` drift guard in
`src/tools/mod.rs`, and `seed.rs`'s `metalcraft_packs_are_tagged_ecosystem`).
Add:

- every `mimg_*` tool that spends declares a `timeout_secs` ≥ 120 — the
  regression that would otherwise silently return;
- `mimg_*` URLs all point at `images.metalcraftai.com`;
- the approval classification of each `mimg_*` tool, asserted by name.

---

## Phase 5 — optional: show the image in the transcript

In `metalcraft-front`, `frontend/src/features/session/linkify.ts` turns bare
URLs into links and nothing more, so a generated image arrives as a link even
when the user is sitting right there. Once 2.5 gives us an openable URL, render
a thumbnail for image URLs from a known host. Small, self-contained, and
independent of every phase above — do it last, or not at all.

---

## Order, and what each phase unblocks

```
1 (inference: edit models) ──┐
                             ├─> 2 (images-web: sources, upload, describe, share)
                             │      └─> 3 (the pack) ──> 4 (approval, reach, tests)
                             │                              └─> 5 (thumbnails)
```

Phase 3 alone — pack in `seed/`, id renamed, `timeout_secs: 300` — is already
worth shipping on its own: it turns "no agent can generate an image" into
"every pod can". Everything else improves it.

## Risks

- **Double-charging on a timeout.** The one that costs real money. `timeout_secs`
  plus the recover-with-`mimg_list_generations` rule are both required; either
  alone leaves the hole open.
- **The service secret can debit any account.** Both new on-behalf call sites
  (edit and describe) must read the act-as identity from the authenticated
  principal at the call site. That is the structural mitigation buildr chose,
  and the thing to be suspicious of is a second call site appearing.
- **A vision verdict can be wrong.** It is advice with evidence attached, not a
  gate. Never let a `fail` silently discard an image the user is paying for —
  report what the model saw and let them decide.
- **Renaming the pack id** is free only because nothing installed it. Confirm
  the registry has no `metalcraft_images` slug before deleting the external
  copy.
