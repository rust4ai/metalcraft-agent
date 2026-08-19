# Plan B — Native In-Process Apps: Concrete Implementation Plan

Companion to `POD_NATIVE_APPS_PLAN.md`. This is the phased build plan for
**Plan B**: notes, calendar, and drive become **native apps compiled into the
agent binary**, running in the agent's own process, on a reusable **App SDK**
("agent OS") with **two storage tiers** — SQLite on the pod PVC for hot/structured
state, and S3/Spaces `blobs` for large/durable bytes + backups. **Calendar keeps
a slimmed cloud "coordinator"** for external-guest invites/RSVP (the one flow a
single-user pod can't own); everything else is pod-local. The user's actual
notes/events/files live in the pod — only invite-coordination rows + share tokens
stay in the cloud.

Grounded against the real code:
- `Tool` trait: `metalcraft-0.9.0/src/tools.rs:15` — `name()`, `description()`,
  `parameters_schema() -> Value`, `async call(args) -> Result<Value>`.
- Router: `src/workshop_api.rs:366` `build_router()` — flat `.route()` chain;
  auth applied by `.layer(middleware::from_fn_with_state(state, auth_middleware))`
  at `:462`; unauthenticated routes (`/health`, `/`, `/webhook/gateway`) added
  after the layer.
- Native tools: `src/tools/mod.rs:78` `create_registry_for_with_config` big
  `match`; `:207` `native_pack_tool_names(pack_id)` map; `:235` drift test.
- Data paths: `src/paths.rs` (`data_dir()`, atomic-write idiom).
- Harvestable SQLite: `~/ai/metalcraft-notes-r2/backend/src/do_notes/schema.rs`
  (FTS5 + triggers + `seed_defaults_once` via a `meta` marker).

**k3 impact is near-zero** — still one binary, one process, one `/data` PVC. No
new containers, no StatefulSet changes except (optionally) PVC size + a backup
mechanism. That is the whole point of Plan B.

---

## 0. Principles / invariants

- **The pod is the user.** No per-request hub introspection inside an app; the
  app trusts the process identity. HTTP routes still sit behind the existing
  Workshop `auth_middleware` (pod token) for browser/SPA access.
- **Contract preserved.** The agent-facing tool names/shapes (`mnote_*` ×8,
  `mcal_*` ×16) and the SPA's REST shapes stay identical, so personas, skills,
  and the frontend don't change behavior — only the base URL / call path moves
  in-process.
- **One writer per app.** Single-user ⇒ no pool, no `FOR UPDATE`. A single
  `sqlx::SqlitePool` in **WAL** mode (or one `Mutex<Connection>`) per app.
  Notes' `version`-integer optimistic concurrency carries over unchanged.
- **Storage engine:** `sqlx` + `sqlite` feature (async, keeps the apps' existing
  `query`/`query_as` style ⇒ cheapest port). Rejected: `rusqlite` (sync, needs
  `spawn_blocking` everywhere), raw JSON (loses FTS/range queries — that's Plan D).
- **Everything under `/data/apps/<id>/`**, using the agent's atomic-write idiom
  for any sidecar JSON; SQLite gets its own file + WAL.
- **Storage is tiered, not one place.** The OS exposes TWO storage resources and
  apps pick per data-type:
  - **`store`** = structured/hot state (SQLite on the **PVC** block volume). Needs
    POSIX + low latency + WAL — **SQLite does not work over S3.** Notes/calendar
    DBs, indexes, the agent's own `keys.json`/packs/flows all live here.
  - **`blobs`** = large/durable/cross-device bytes (uploads, attachments,
    exports, **backups**) on **S3/Spaces**, via the `s3` SigV4 client already in
    the binary (creds from key store: `S3_ACCESS_KEY_ID`/`S3_SECRET_ACCESS_KEY`/
    `S3_REGION`/`S3_ENDPOINT`).
  Rationale: the PVC's weaknesses (fixed 5Gi, no backup, single-node RWO) are
  exactly S3's strengths (durable 11-9s, unbounded, cross-pod, presigned direct
  browser I/O). S3 is the **durability + blob + backup tier**, not a PVC
  replacement. (Going PVC-less — tar `/data` ⇄ R2 — is only worth it on
  ephemeral-disk runtimes; that path is the separate `metalcraft-agent-r2`
  Litestream→R2 fork, gated `METALCRAFT_STORE=files|sqlite`. On k3, keep PVC + S3
  tier.)
- **Local vs pod stays transparent.** `blobs` points at a local dir when
  self-hosting and at Spaces in-pod, the same way `data_dir()` already hides
  local-dir vs PVC — so app code is identical everywhere.

---

## 1. The App SDK (`src/apps/` — new module)

The reusable "OS" layer. New module tree:

```
src/apps/
  mod.rs          # App trait, AppContext, registry of built-in apps
  storage.rs      # SqliteStore: open/migrate a per-app db under /data/apps/<id>
  blobs.rs        # BlobStore: S3/Spaces (or local dir) object-store primitive
  events.rs       # per-app broadcast hub (SPA WebSocket push)
  notes/          # Phase 1
  calendar/       # Phase 4  (pod core + cloud coordinator client)
  drive/          # Phase 4.5
```

**The `App` trait** — the syscall surface each app implements:

```rust
// src/apps/mod.rs
#[async_trait::async_trait]
pub trait App: Send + Sync {
    /// Stable id, matches the pack id ("metalcraft-notes").
    fn id(&self) -> &'static str;

    /// Native tool names this app contributes (mnote_*, mcal_*).
    fn tool_names(&self) -> Vec<String>;

    /// Register this app's Tools into the shared registry (given its storage).
    fn register_tools(&self, reg: ToolRegistry, ctx: &AppContext) -> ToolRegistry;

    /// The app's REST + embedded-SPA router, to be nested at /apps/<id>.
    fn router(&self, ctx: &AppContext) -> axum::Router;

    /// Called once on pod boot: run migrations, seed defaults.
    async fn init(&self, ctx: &AppContext) -> anyhow::Result<()>;

    /// Optional scheduler ticks (calendar reminders, backup) — Phase 3/4.
    fn schedules(&self) -> Vec<AppSchedule> { vec![] }
}
```

**`AppContext`** — what the OS lends the app:

```rust
pub struct AppContext {
    pub store: SqliteStore,      // structured/hot state — SQLite on the PVC
    pub blobs: BlobStore,        // large/durable bytes — S3/Spaces (or local dir)
    pub events: AppEventHub,     // publish → SPA WebSocket
    pub owner: OwnerIdentity,    // "the pod is the user"
    pub data_dir: PathBuf,       // /data/apps/<id>/  (scratch, WAL, snapshots)
}
```

**`BlobStore`** (`blobs.rs`): a per-app object-store namespace
(`apps/<id>/…` key prefix) with `put`/`get`/`delete`/`list`/`presign_put`/
`presign_get`. Backed by the in-binary `s3` client in-pod (Spaces/R2), and by a
local filesystem impl when self-hosting — same trait, chosen at boot from config,
so app code never branches on environment. This is the OS "object storage
primitive" that notes attachments, calendar `.ics`, exports, and Phase 3 backups
all ride on; the **Drive app (Phase 5)** is the human-facing manager *over* this
primitive, not a separate storage engine.

**`SqliteStore`** (`storage.rs`): opens `sqlx::SqlitePool` with
`create_if_missing(true)`, `PRAGMA journal_mode=WAL`, `foreign_keys=ON`; exposes
`pool()` + an idempotent `apply_schema(&[&str])` (the notes-r2 "one statement per
exec, `IF NOT EXISTS`" pattern — copy `schema.rs` verbatim, minus the worker
binding).

**Built-in app registry** — the compiled-in analogue of `seed/`:

```rust
pub fn builtin_apps() -> Vec<Box<dyn App>> {
    vec![
        Box::new(notes::NotesApp),
        Box::new(calendar::CalendarApp),
        Box::new(drive::DriveApp),
    ]
}
```

**Wiring into the three existing integration points:**

1. **Router** (`workshop_api.rs build_router`): before the `.layer(auth_...)`
   call, fold in each enabled app's router:
   ```rust
   let mut r = Router::new()./* existing routes */;
   for app in apps::enabled_builtin_apps() {
       r = r.nest(&format!("/apps/{}", app.id()), app.router(&ctx_for(app)));
   }
   r.layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
    ./* health, /, webhook after layer */
   ```
2. **Tool registry** (`tools/mod.rs create_registry_for_with_config`): after the
   big `match`, before the HTTP-API fallthrough at `:180`, let an app claim the
   name:
   ```rust
   // if no builtin matched, try app-native tools
   if let Some(app) = apps::app_owning_tool(name) {
       registry = app.register_tools(registry, &ctx_for(app));
       continue;
   }
   ```
3. **Manifest** (`native_pack_tool_names` `:207`): add `metalcraft-notes` /
   `metalcraft-calendar` arms returning the app's `tool_names()`, and keep the
   `:235` drift test green.

**`pack.json` gains an `app` block** (new optional field in `PackManifest`):
```json
"app": { "runtime": "native", "storage": "sqlite",
         "mount": "/apps/metalcraft-notes", "spa": "web/" }
```
`runtime: "native"` tells the OS this pack is served in-process (vs the default
`http` packs that point at a URL). Add the field, keep it `Option`, extend the
drift test to assert every `runtime:native` pack has a `builtin_apps()` entry.

**Deliverable of Phase 0:** the SDK compiles with **zero apps wired** (an empty
`builtin_apps()`), all existing tests green. No behavior change yet.

---

## 2. Phase 1 — Notes app (native, SQLite, single-user)

Lowest risk; the SQLite port already exists in `metalcraft-notes-r2`.

1. **Schema** (`src/apps/notes/schema.rs`): copy `do_notes/schema.rs` STATEMENTS
   verbatim (notes/categories/note_categories + `notes_fts` FTS5 + the 3 triggers
   + `meta`). Drop `owner_user_id` entirely (single-user). `seed_defaults_once`
   → 3 default categories via the `meta` marker.
2. **Store/CRUD** (`src/apps/notes/store.rs`): port `metalcraft-notes/src/
   controllers/api.rs` (~800 lines) to `sqlx-sqlite`. Mechanical swaps:
   `gen_random_uuid()`→`util::uuid()`, `now()`→`now_iso()`, `= ANY`→`IN (…)`,
   `to_tsvector/ts_rank`→`notes_fts MATCH ? ORDER BY bm25(notes_fts)`,
   `RETURNING (xmax=0)`→`changes()`, `ON CONFLICT` stays (SQLite supports it).
   The r2 `store.rs` is a near-complete reference for all of this.
3. **Tools** (`src/apps/notes/tools.rs`): 8 `Tool` impls (`mnote_whoami`,
   `_list_notes`, `_get_note`, `_create_note`, `_update_note`, `_delete_note`,
   `_list_categories`, `_create_category`), each calling `store` directly and
   returning the same JSON the cloud endpoints returned. `parameters_schema()`
   copied from the pack's `api_tools/*.json`. Reuse the existing approval policy
   (`src/approval.rs:93` already auto-approves `mnote_*` reads).
4. **Router** (`src/apps/notes/http.rs`): the same REST surface
   (`/api/v1/notes`, `/categories`, `/search`, `/share`, `/export`, `/import`,
   `/ws`) but nested under `/apps/metalcraft-notes`. Reuse `render.rs`
   (comrak/ammonia) and `portable.rs` (export/import) unchanged from the cloud
   app — pure, no DB. WebSocket push → `AppEventHub` (drop the per-user HashMap
   keying; one pod = one user).
5. **SPA**: embed the built Vite `dist/` via `rust-embed` and serve it at the
   mount root (mirrors how the agent already embeds `seed/`). Set the SPA's API
   base to the mount path.
6. **Pack**: convert `seed/integration_packs/metalcraft-notes` to
   `runtime:native` — the 8 `api_tools/*.json` are now served by the native
   tools, so either delete them or keep them as an OpenAPI reference; keep the
   persona + skill.

**Exit criteria:** agent can `mnote_create/list/get/update/delete` against
pod-local SQLite; the SPA loads at `https://<slug>.pods.metalcraftai.com/apps/
metalcraft-notes/` and round-trips edits via WebSocket; existing notes personas
unchanged.

---

## 3. Phase 2 — One-time data migration (notes)

The cloud app already ships portable export/import.
1. For each user: `GET https://notes.metalcraftai.com/api/v1/export` (full-vault
   zip, Obsidian frontmatter) with their token.
2. `POST /apps/metalcraft-notes/api/v1/import` on their pod.
3. Verify counts (notes, categories) match; log a per-user reconcile report.
Run as a one-shot admin script (or a k3 Job iterating premium users). No schema
coupling — the format is the contract.

---

## 4. Phase 3 — Backup (because `/data` is now system-of-record)

- **App-level snapshot** (portable, cheap): an `AppSchedule` tick (daily) calls
  the app's own export → writes a zip to `/data/apps/<id>/snapshots/` and
  uploads to Spaces/R2 (reuse the `s3` native tool already in the binary). This
  is the notes-r2 `alarm → R2` pattern.
- **Optional volume-level:** a k3 `VolumeSnapshot` CronJob against
  `do-block-storage` (CSI supports it) for whole-PVC point-in-time. Separate,
  infra-side, can come later.
- Add `allowVolumeExpansion` to the StorageClass so 5Gi can grow without
  recreation (one-line k3 change; currently absent).

---

## 5. Phase 4 — Calendar app (native core **+ retained cloud coordinator**)

Calendar is **split across two homes on purpose**: the user's own calendar state
moves into the pod, but a **slimmed cloud calendar backend stays alive** to do the
one thing a single-user pod can't — coordinate **external-guest invites/RSVP**
with people who have no pod. This is not the deferred/optional D2 relay; for
calendar it is a **first-class, permanent component** of the design.

### 4a — Pod-local core (source of truth for the user's events)

Same recipe as notes:
- Schema: `calendars`, `calendar_events`, `event_guests` → SQLite (drop `users`
  / `owner_user_id`; `TIMESTAMPTZ`→TEXT ISO or INTEGER millis; `TEXT[]` scopes →
  JSON column; partial unique indexes + CTEs port straight; `xmax=0`→`changes()`).
- Timezone math (`chrono-tz`, `day_window`, `/now`) is pure Rust — **moves
  verbatim**.
- 16 `mcal_*` tools + REST router nested at `/apps/metalcraft-calendar`, SPA
  embedded.
- Google **outbound** sync stays a direct pod→Google call (encrypted refresh
  token in the pod key store).

The pod is the **system of record** for the user's calendars and events. Local
`event_guests` rows hold the invite *intent* + the last-known RSVP status mirrored
back from the coordinator.

### 4b — Retained cloud calendar coordinator (external-invite plane)

A **much slimmer** version of today's `metalcraft-calendar` service — it **no
longer stores the user's whole calendar** (that's in the pod now). It holds only
the cross-tenant coordination state:

- **Outbound invites:** the organizer's pod POSTs `{event snapshot, guest emails,
  invite_token}` to the coordinator. The coordinator persists just the
  invite/RSVP rows + tokens (a small table, not the full calendar), emails guests
  via **Resend**, and hosts the public **`/rsvp/{token}`** page (internet-facing,
  no account — the one flow a pod behind per-user auth shouldn't serve directly).
- **RSVP capture → pushed back to the pod:** when a guest RSVPs, the coordinator
  notifies the organizer's pod (POST to the pod ingress, connection-token authed —
  reuse the gateway's push-via-k3 route), which updates the local `event_guests`
  row. Pod can also poll as a fallback.
- **Inbound invites (the user is a guest):** the coordinator is the user's invite
  **mailbox** — it matches incoming invites to the user's email and exposes them;
  the pod's `mcal_list_invites`/`mcal_respond_invite` call the coordinator (or a
  pod-side cache synced from it). Accepting can "place" the event onto a local
  calendar as a read-only mirror.
- **Meet rooms** (`metalcraft-meet`) stay a coordinator-side or pod-side external
  call — orthogonal.

Why this shape: external guests have no pod, RSVP is public/internet-facing, and
email is a shared sender — all three genuinely can't live behind per-user pod
auth. Everything *else* (the user's real calendar) does live in the pod. Net: the
calendar's Neon footprint **shrinks to invite-coordination rows**, and the
always-on-DB cost mostly goes away, without dropping invites.

**Contract:** the 16 `mcal_*` tools keep their exact shapes. Local CRUD tools hit
pod SQLite; `mcal_add_guests`/`_list_invites`/`_respond_invite` fan out to the
coordinator. The SPA is unchanged.

**Data migration:** export events from cloud Neon → import into the pod (as notes,
Phase 2 pattern); leave live/pending invite rows in the coordinator DB (they're
already where they belong).

**Exit criteria:** pod-local calendar CRUD + timezone + `/now` + Google outbound;
`mcal_*` create/list/update/delete against SQLite; **external invite → Resend
email → public RSVP → status synced back to the pod** works end-to-end via the
coordinator; SPA loads.

---

## 5.5. Phase 4.5 — Drive app (file manager over the `blobs` primitive)

With `blobs` (S3/Spaces) now an OS primitive (§1), Drive is the **human-facing
file manager on top of it** — folders/trash/starred/upload/share — *not* a new
storage engine. Harvest the existing cloud **`metalcraft-drive`** almost wholesale:

- **Reuse:** its Spaces integration (presign-upload + `confirm`, Range download),
  folders/files/trash/starred model, `mdrv_*` pack, and — importantly — its **"App
  Filespaces"** concept (`/api/v1/apps/{app}/folder` claim + `/apps/{app}/files`),
  which is precisely "other apps store their blobs here." That becomes the SPA/UI
  layer over the OS `blobs` primitive.
- **Storage split:** file **metadata/tree** (folders, names, trash flags, stars) →
  pod **SQLite** (`store`); file **bytes** → **`blobs`** (S3/Spaces), browser I/O
  via presigned URLs so large files never stream through the pod.
- **Native tools:** `mdrv_*` (list/upload/download/move/trash) as `Tool` impls;
  REST + SPA nested at `/apps/metalcraft-drive`.
- **Cross-app benefit:** once Drive backs the `blobs` primitive, notes
  attachments and calendar `.ics`/exports can live in the user's Drive too —
  one file space, many apps (the iPhone Files.app model).

**Exit criteria:** upload/download/organize files in-pod with bytes in Spaces and
metadata in pod SQLite; other apps can `blobs.put/get` into the user's Drive.

---

## 6. Phase 5 — Registry-installable native apps (optional, toward Plan C)

Today native tools must be compiled in. To let a native app be *installed* (not
just shipped in the image) without going full WASM:
- Keep native apps compiled-in (the `builtin_apps()` set) but let the **pack
  enable-state** gate whether an app's routes/tools/schedule are active — so
  "install" = enable an already-in-binary app. This is the pragmatic 80% of the
  "install apps like maps" vision without a plugin ABI.
- True third-party dynamic apps (WASM / supervised subprocess) are **Plan C** —
  the App SDK from Phase 0 is deliberately the seam they'd plug into. Not in this
  plan.

---

## 7. Phase 6 — Public-share relay for notes & Drive (optional)

Calendar's cross-tenant plane already lives in the Phase 4b coordinator. What
remains is **public share links** for notes and Drive files — a small
**stateless** relay (same shape as, and can co-live with, the calendar
coordinator) holding **no app content**, only routing:
- **Notes sharing:** `share` registers `{token → pod_slug, note_slug}` in a tiny
  index; public `/p/{token}` fetches the rendered note from the owner's pod
  (pod→pod, connection-token authed) and serves it. (Mirrors notes-r2's D1
  `shares` index.)
- **Drive sharing:** public file links resolve `{token → pod_slug, file_id}` and
  redirect to a presigned Spaces URL (bytes never transit the relay).
- Pods are already addressable (`<slug>.pods.metalcraftai.com`) with connection
  tokens, so pod→pod is auth-ready.

**The retained coordinator (calendar 4b) + this relay are the same small cloud
tier** — the only always-on cloud state in the whole design, and it holds
coordination rows + share-token indexes, never the user's actual notes/events/
files.

---

## 8. Cross-cutting: testing, rollout, risks

**Testing**
- Port the cloud apps' existing tests to the SQLite store (notes has
  `merge.test.ts` client-side; add Rust store tests for the FTS/version/`changes()`
  swaps — the r2 port is the oracle).
- Drift test (`tools/mod.rs:235`) extended: every `runtime:native` pack ⇒ a
  `builtin_apps()` entry ⇒ `native_pack_tool_names()` arm. Keep the three in sync.
- E2E: boot a pod, run the notes/calendar agent personas end-to-end against
  pod-local storage; assert the SPA loads at the mount path.

**Rollout**
1. Ship the agent image with Phase 0+1 (notes native), apps **disabled** by
   default (pack enable-state off) → zero behavior change for existing pods.
2. Migrate a pilot user's notes (Phase 2), enable the native notes pack on that
   pod, verify, then keep the cloud notes service running in parallel (dual-read)
   until confident.
3. Flip default-enable for new pods; batch-migrate existing; then retire the
   cloud notes DB (keep the shell only if D2 relay lands).
4. Repeat for calendar.

**Risks / watch-items**
- **SQLite in the agent binary**: adds `libsqlite3-sys` (bundled) — verify the
  Dockerfile musl/glibc build still links; WAL needs a real filesystem (the DO
  block PVC qualifies — not tmpfs).
- **Binary/build coupling**: notes+calendar now rebuild the agent; accept it for
  v1 (Plan C decouples later).
- **Backup is a hard gate**: do **not** retire a cloud DB until Phase 3 snapshots
  are verified restorable — losing a PVC currently loses the user's data.
- **Google sync from a pod**: outbound is fine; two-way sync's watch/webhook
  callbacks would need a public pod URL (already have the ingress) — scope to
  outbound + poll for v1.
- **Calendar invites keep a cloud dependency (by design)**: external invites/RSVP
  run through the retained Phase 4b coordinator, so calendar is *not* fully
  cloud-independent. That's the deliberate trade — a small always-on coordination
  DB instead of dropping invites. Keep it slim (invite rows + tokens only).
- **Pod↔coordinator sync**: RSVP status is eventually-consistent (push-back +
  poll fallback). Make the pod tolerant of a lagging/absent coordinator (show
  last-known status, retry).

---

## 9. Sequenced checklist

- [ ] **P0** `src/apps/` SDK: `App` trait, `AppContext` (**`store` + `blobs`**),
  `SqliteStore`, `BlobStore` (S3/Spaces + local impl), `AppEventHub`,
  `builtin_apps()` (empty), router/registry/manifest seams, `app` block in
  `PackManifest` + drift test. **Green, no apps.**
- [ ] **P1** Notes native: schema (harvest r2), store (port api.rs), 8 tools,
  REST+WS router, SPA embed, pack → `runtime:native`.
- [ ] **P2** Notes data migration (export→import) + reconcile report.
- [ ] **P3** Backup: app snapshot→`blobs`(R2) schedule; `allowVolumeExpansion`;
  (opt) VolumeSnapshot CronJob.
- [ ] **P4a** Calendar native core (SQLite + timezone + Google outbound).
- [ ] **P4b** Retained cloud calendar **coordinator**: external invites → Resend
  → public `/rsvp/{token}` → RSVP synced back to pod; invite mailbox.
- [ ] **P4.5** Drive app: metadata in pod SQLite, bytes in `blobs`/Spaces
  (presigned I/O); backs the OS blob primitive for other apps.
- [ ] **P5** (opt) enable-state as "install"; seam for Plan C.
- [ ] **P6** (opt) public-share relay: notes `/p/{token}` + Drive file links.
- [ ] Retire the **full** cloud Neon DBs once pods are system-of-record + backed
  up; keep only the slim coordinator/relay tier (invite rows + share tokens).
