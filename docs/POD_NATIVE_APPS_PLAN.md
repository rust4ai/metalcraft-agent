# Pod-Native Apps — Competing Plans

Turning **notes** and **calendar** from cloud apps (own Rust service + Neon
Postgres) into **apps that live inside the agent's pod**, with state on the
pod's own disk (`/data`) — JSON files, embedded SQLite, or a KV store — instead
of a shared cloud DB. The larger idea: metalcraft-agent becomes a small **"OS"**
whose installed apps get resources (storage, HTTP mounts, tools, a scheduler)
the way iPhone apps, Warcraft 3 custom maps, or Garry's Mod addons use their
host.

Status: **design / options only.** Nothing built. Pick a direction first.

---

## 1. What we're working with (established by code study)

**The pod already has everything an "OS" needs except the app layer.**

- **Durable disk per user.** Each pod is a k3 `StatefulSet` (replicas:1) with a
  `volumeClaimTemplate` → PVC `data-mck-<slug>-0`, `do-block-storage`, **5Gi
  RWO**, mounted at **`/data`** (`METALCRAFT_DATA_DIR=/data`). It survives
  restart, reschedule, image upgrade, and suspend; it is deleted **only** on
  deprovision. (`cluster-backend/src/kube/client.rs:318-361`.)
  Gap: **no PVC snapshot/backup/resize automation exists** — if `/data` becomes
  system-of-record, backup is net-new work.
- **The agent is already DB-less.** All agent state is flat JSON under `/data`
  (`keys.json`, `integration_packs.json`, `flows/`, `runs/`, `chats/`, …), using
  a consistent **advisory-lock + write-temp + fsync + atomic-rename** idiom
  (`src/paths.rs`, `src/integration_packs.rs:167-186`). No SQLite anywhere today.
- **Packs are already the "installed app" runtime.** A pack is a directory
  (`pack.json` manifest + `personas/` + `skills/` + declarative `api_tools/*.json`
  + `flow_templates/`). Packs are embedded at compile time (`include_dir!`,
  `src/seed.rs:30`) **or** installed from a registry as ZIPs at runtime
  (`integration_packs.rs:321`). Most packs are pure JSON HTTP-tool specs; a few
  ship **native Rust tools** compiled into the binary (`s3`, `email`/IMAP),
  declared in the manifest's `native_tools` and drift-checked
  (`src/tools/mod.rs:207-235`). **There is no dynamic code loading / WASM / plugin
  ABI today.**
- **The notes/calendar apps are just packs pointing at cloud URLs.** Each ships
  `api_tools/*.json` whose `url` is hardcoded to `notes.metalcraftai.com` /
  `calendar.metalcraftai.com`, authed with one shared `$METALCRAFT_TOKEN`
  bearer. `HttpApiTool` implements `metalcraft::Tool`; unknown tool names fall
  through to load as HTTP tools (`src/tools/http_api.rs`, `tools/mod.rs:180`).
  **The agent's contract to each app is a small, stable HTTP surface:** notes = 8
  endpoints, calendar = 16. Repointing an app = changing a base URL (or swapping
  the tool implementation), nothing more.

**How hard is each app to make single-user / pod-local / no Postgres?**

- **Notes — low/moderate, largely de-risked.** 4 small tables
  (`users`, `notes`, `categories`, `note_categories`), ~800 lines of
  owner-scoped CRUD, **no transactions, no `FOR UPDATE`, no row locks** —
  concurrency is a single `version` integer (optimistic, 409-with-current). The
  only cross-user read in the whole app is the public share page `/p/{token}`.
  Realtime is an **in-process** per-user `broadcast` hub (no Redis/LISTEN) — one
  process per user makes it *simpler*. A **working SQLite port already exists**
  at `~/ai/metalcraft-notes-r2/` (workers-rs Durable-Object SQLite: FTS5+triggers
  replacing tsvector/GIN, TEXT ids, portable.rs export reused). That port is
  directly harvestable.
- **Calendar — modest for the core, but two features resist pod-locality.**
  5 tables (`users`, `google_connections`, `calendars`, `calendar_events`,
  `event_guests`), no recurrence, all timezone math is pure Rust (`chrono-tz`),
  the clock endpoint (`/now`) is already DB-free. The hard parts:
  1. **Cross-user invites as shared-by-reference events** — the invites inbox and
     "placed invite" reads deliberately JOIN across events **owned by other
     users** and render them read-only on the guest's calendar
     (`services/events.rs:105-185`). A single-user pod has no local row for
     "someone else's event."
  2. **External-guest RSVP + email** — arbitrary email addresses RSVP against a
     public `/rsvp/{token}` and are notified via Resend. This is inherently
     multi-party and internet-facing.
  3. **Google two-way sync** and **metalcraft-meet** room provisioning are
     external-network couplings (orthogonal to Postgres, but a pod inherits them).

**Prior art to reconcile.** `~/ai/metalcraft-notes-r2/` already answers "kill the
cloud DB" — but by moving state to **Cloudflare Durable-Object SQLite (per user),
external to the pod.** This document's ask is different: state lives **in the
pod**. The r2 SQLite schema/port is a reusable asset for the SQLite-based plans
below even though its runtime target (CF Worker) differs.

---

## 2. The "agent OS" abstraction (shared by several plans)

If the agent is an OS, an **app** is more than a pack. Define an **App SDK** — the
set of "syscalls"/resources the agent-OS lends an installed app:

| Resource ("syscall") | What the OS provides | Backed by today |
|---|---|---|
| **Storage** | A namespaced handle under `/data/apps/<app_id>/` — a JSON dir, an embedded SQLite file, or a KV namespace. The OS owns locking + atomic writes. | the `/data` PVC + existing atomic-write idiom |
| **HTTP mount** | The app contributes an `axum::Router` mounted at `/apps/<app_id>/*` on the pod's Workshop server (REST + its embedded SPA). | `src/workshop_api.rs` router |
| **Tools** | The app registers native `metalcraft::Tool`s (e.g. `mnote_*`), auto-added to personas that enable the app. | `ToolRegistry`, `tools/mod.rs` |
| **Identity** | "The pod is the user." The app gets the owner identity for free; no per-request hub introspection. | pod token / `POD_PUBLIC_URL` |
| **Scheduler** | Register timers / cron ticks (reminders, snapshots). | the daemon flow scheduler (`daemon.rs`) |
| **Event bus** | Publish/subscribe app events (drives WebSocket push to the SPA). | today's in-process `broadcast` hub |
| **Manifest** | `pack.json` gains an `app` block: declared storage kind, mounted routes, bundled SPA assets, native tools. | `PackManifest` (add fields) |

The manifest's `native_tools` field + the drift test are the seed of this: the
OS already distinguishes "compiled-in capability" from "declarative data." An App
SDK generalizes that into storage + routes + SPA + scheduler grants.

**Cross-cutting decision — the cross-tenant problem (D).** Sharing (notes
`/p/{token}`), calendar invites/RSVP, and Google sync cannot be purely pod-local.
Three postures, choose one per app:

- **D1 — Drop for v1.** Single-user, no external sharing/invites. Simplest.
- **D2 — Thin cloud relay.** Pods hold *all real state*; a small stateless cloud
  service does only the cross-tenant routing: share-token → pod lookup, public
  RSVP page, pod→pod invite delivery, outbound email. Mirrors the notes-r2 D1
  index and the existing gateway model. **Recommended** — keeps the win (no
  always-on DB of record) while preserving sharing.
- **D3 — Pod-to-pod federation.** Organizer pod serves the event; guest pod
  fetches it. Pods are already addressable (`<slug>.pods.metalcraftai.com`) with
  connection tokens. Most "pure" but the most protocol to design.

**Cross-cutting decision — backup (B).** `/data` becomes system-of-record with no
snapshots today. Reuse each app's **export format** (notes already has
Obsidian-frontmatter `portable.rs`; calendar has `.ics`) as a periodic
**snapshot to R2/Spaces** on a scheduler tick — same pattern as notes-r2's
`alarm → R2`. Optionally add DO block-storage `VolumeSnapshot` CronJobs in k3.

---

## 3. Competing plans

Two orthogonal axes drive the design:
**(1) storage engine** — JSON files vs embedded SQLite vs KV;
**(2) runtime model** — sidecar process vs in-agent-process native app vs
dynamic installable app. The four plans below are coherent picks, ordered from
least to most ambitious.

### Plan A — "Sidecar Apps" (fastest, lowest-risk, least OS-like)

Keep each app as its **own Rust/Axum binary**, but run it **as a co-located
process inside the pod** instead of a cloud service. Swap `sqlx-postgres` →
`sqlx-sqlite` pointing at `/data/apps/notes/notes.db` (harvest the notes-r2
SQLite schema). The k3 StatefulSet gains extra containers (or the agent
supervises child processes); the app serves on `127.0.0.1`. The pack's
`api_tools` repoint from `notes.metalcraftai.com` → `http://127.0.0.1:9300`. Auth
collapses to loopback-trust / a shared local secret; drop hub JWKS. The SPA is
still served by the sidecar unchanged.

- **Storage:** SQLite file per app under `/data`.
- **Runtime:** separate process(es), localhost HTTP.
- **Cross-tenant:** D1 or D2.
- **Pros:** smallest rewrite — the existing apps survive almost verbatim (Postgres
  → SQLite is the only real change, already prototyped in r2); SPA + `merge.ts` +
  REST contract identical; apps stay independent repos with independent deploys.
- **Cons:** N extra processes per pod (memory/footprint × premium users);
  multi-container image + supervision complexity in k3; **doesn't realize the OS
  vision** — it's "localhost microservices," not "installed apps sharing one
  runtime"; each app re-implements its own auth/scheduler/storage plumbing.

### Plan B — "Native In-Process Apps" (the OS vision, first-party) — **recommended**

Promote notes and calendar to **native packs compiled into the agent binary**
(the way `s3`/`email` already are), on top of the **App SDK** from §2. Each app:
registers its `metalcraft::Tool`s (`mnote_*`, `mcal_*`) that read/write pod-local
storage **directly** (no HTTP hop); mounts an `axum::Router` at `/apps/<id>/*` on
the pod's Workshop server for REST + its embedded SPA (via `rust-embed`); and
declares its needs in an `app` block in `pack.json`. One binary, one process, one
`/data` dir. The ~800-line CRUD core of each app is ported to native
tools + mounted routes backed by pod-local SQLite (or JSON — see B-var).

- **Storage:** one embedded **SQLite** file per app under `/data/apps/<id>/`
  (`sqlx-sqlite` or `rusqlite`), FTS5 for notes search. **B-var (JSON):** notes as
  frontmatter `.md` files + calendar as JSON/ICS files, no SQLite — see Plan D.
- **Runtime:** in-agent-process; tools call storage directly; SPA served by the
  agent.
- **Cross-tenant:** D2 (thin relay) recommended, or D1 for v1.
- **Pros:** actually realizes "agent OS with installed apps"; single binary /
  process / data dir; tool calls skip the network round-trip; reuses pack /
  persona / skill / scheduler infra; the App SDK becomes the foundation for a
  third-party ecosystem later (Plan C is a superset).
- **Cons:** apps become part of the agent binary (build-time coupling; can't
  deploy notes without shipping the agent); the app web layer is rewritten into
  the agent's router (the SPA/REST *shapes* stay, the plumbing moves); introduces
  a SQLite dependency into the agent binary; still must solve cross-tenant.

### Plan C — "Installable App Platform" (full OS ambition; dynamic)

Generalize Plan B's App SDK into a **real platform where apps are installed at
runtime** from the packs/flows registry and get **sandboxed resource grants** —
the iPhone / Garry's-Mod / WC3-custom-map model. Two sub-variants:

- **C-WASM:** apps ship as **WASM components** with a host interface exposing the
  §2 syscalls (kv/sql storage, http-mount, tool-register, timers). Truly
  sandboxed, language-agnostic, installable. **But:** WASM has no tokio/axum
  (r2's plan already hit this), the host↔guest ABI is real work, and the
  tooling/perf story is immature.
- **C-Subprocess:** apps ship as **binary/OCI bundles** the agent supervises as
  child processes, declaring a manifest (data-dir grant, localhost port, tool
  proxy, lifecycle). Effectively Plan A **generalized + installable + lifecycle-
  managed by the agent-OS** rather than hand-wired into k3.
- **Storage:** granted per-app namespace (KV or SQLite), enforced by the host.
- **Runtime:** sandboxed WASM guest, or supervised subprocess.
- **Cross-tenant:** D2/D3.
- **Pros:** the fullest expression of the vision — install apps like maps/apps;
  third-party ecosystem; strong isolation (WASM); apps decoupled from the agent
  release cycle.
- **Cons:** by far the most engineering; WASM immaturity vs subprocess overhead;
  a security/sandboxing surface to own; overkill if the near-term goal is just
  "notes+calendar off Neon." **Best treated as the destination Plan B evolves
  toward, not v1.**

### Plan D — "Documents-as-Files" (most minimal, most agent-native)

Lean fully into the agent's existing **flat-file idiom** — no SQLite, no web
server for the agent to use the app. Notes = markdown files with YAML frontmatter
under `/data/apps/notes/*.md` (**exactly the existing `portable.rs` export
format**). Calendar = JSON or `.ics` files under `/data/apps/calendar/`. A thin
native pack exposes `mnote_*` / `mcal_*` tools over these files; **search is
`ripgrep`** (the agent already has `grep`/`find`); date-range queries scan the
dir. Optionally a small read-only web viewer.

- **Storage:** plain files on the PVC.
- **Runtime:** native tools over the filesystem; minimal/no HTTP.
- **Cross-tenant:** D1 (or D2 for a share/publish endpoint).
- **Pros:** zero new storage engine; maximally matches "maybe via json file";
  human-readable, git-able, **trivially backed up (it's just files)**;
  export/import is the identity function; the agent can already read/write/grep
  them with existing tools; smallest possible surface.
- **Cons:** no rich web editing UI unless rebuilt (loses the BlockNote SPA);
  search is grep, not ranked FTS; weaker concurrent-edit/version story;
  calendar date-range and invite logic get awkward over loose files.

---

## 4. Comparison

| | A · Sidecar | B · Native in-process | C · Installable platform | D · Documents-as-files |
|---|---|---|---|---|
| Storage | SQLite/app | SQLite/app (or JSON) | granted KV/SQL | JSON/MD files |
| Runtime | separate process | in agent binary | WASM / subprocess | in agent, files |
| Realizes "OS" vision | ✗ | ✓ | ✓✓ | partial |
| Rewrite size | **small** | medium | **large** | small |
| Reuses r2 SQLite port | ✓✓ | ✓ | ✓ | ✗ |
| Keeps BlockNote SPA | ✓ | ✓ (remount) | ✓ | ✗ (rebuild) |
| Per-pod footprint | N processes | 1 process | 1 + guests | 1 process |
| Backup story | export→R2 | export→R2 | host-managed | **just files** |
| Third-party apps later | ✗ | foundation | ✓✓ | ✗ |

**Recommendation.** **Plan B**, storage = embedded SQLite, cross-tenant = D2 thin
relay, backup = scheduled export→R2. It genuinely delivers the "agent OS with
installed apps" idea, keeps one binary/process/disk, harvests the r2 SQLite port,
and its App SDK is the exact foundation Plan C would later build on. Use **Plan
D's file model as the storage variant for notes** if human-readable/git-able
notes matter more than a ranked-search web editor. Plan A is the fallback if
speed-to-ship dominates and the OS vision can wait; Plan C is the 2.0 destination,
not v1.

---

## 5. Migration & sequencing (applies to B; adaptable to A/D)

1. **App SDK skeleton** — `App` trait + `AppContext` (storage handle, router
   mount, tool register, scheduler, event bus); `pack.json` `app` block +
   drift test. No app logic yet.
2. **Notes first** (lower risk, SQLite port exists). Port schema (r2's
   FTS5 schema), CRUD → native tools, mount REST+SPA at `/apps/notes/*`,
   pod-local auth. Keep the 8-tool contract identical so personas don't change.
3. **One-time data migration** — export from cloud Neon (existing
   `/api/v1/export`) → import into the pod (`/api/v1/import`). Both endpoints
   already exist; the format is portable.
4. **Backup** — scheduler tick exports → R2/Spaces snapshot.
5. **Calendar** — same, plus decide invites/RSVP/Google: v1 = D1 (drop external
   invites) or wire the D2 relay for share-token/RSVP/email + keep Google sync as
   an outbound network call from the pod.
6. **Deprecate cloud services** — once pods are the system of record, the Neon-
   backed `metalcraft-notes` / `metalcraft-calendar` services can be retired (or
   kept only as the D2 relay shell).

**Open decisions to lock before building:** (a) storage engine — SQLite vs JSON
files (B vs D-variant); (b) cross-tenant posture — D1 vs D2 vs D3; (c) do we
build the general App SDK now (B) or hand-wire two apps and generalize later; (d)
PVC size (5Gi today) + backup mechanism; (e) whether apps stay first-party
compiled-in (B) or must be runtime-installable (C).
