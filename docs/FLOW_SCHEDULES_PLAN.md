# Flow Schedules Plan — move "when a flow runs" off the entry node

**Status:** IMPLEMENTED + builds/tests green (2026-08-11). Not yet published/deployed.
See "Implementation status" at the bottom.
**Repos touched:** `metalcraft-flows` (0.3.0 → 0.4.0), `metalcraft-agent` (0.28.1 →),
`metalcraft-mobile`, `metalcraft-workshop-web`, `metalcraft-flows-web`

## Problem

Today a flow's schedule lives **inside the entry node's `data`**:

```jsonc
{ "id": "entry", "node_type": "entry",
  "data": { "schedule_type": "cron", "cron": "0 8 * * *", "persona": "..." } }
```

- `metalcraft-flows/src/model.rs` — `SavedFlow.flow.nodes[entry].data` holds `schedule_type`
  (`manual | minutes | hours | cron`), `interval`, `cron`.
- `metalcraft-agent/src/flows.rs::parse_schedule()` reads `entry.data` → exactly **one**
  `FlowSchedule` per flow.
- `metalcraft-agent/src/daemon.rs` keeps `state_by_flow: HashMap<flow_id, FlowRunState>`
  (in-memory `{last_started_at, is_running}`) and calls `is_due(state, schedule)` once per flow.
- `metalcraft-flows/src/validate.rs` never touches scheduling — it is purely an
  agent-runtime concern, which makes it cheap to move.

Two problems:
1. "When it runs" is trigger metadata, but it is buried in a graph node.
2. The entry node structurally allows only **one** schedule → **two crons on one flow is
   impossible**.

## Goal

- Scheduling is flow-level metadata (a "manifest" wrapper), not a graph node.
- A flow can carry **N schedules** (e.g. run at 8:00 AM *and* 6:00 PM) — clean in data and UI.
- Each schedule is independently toggleable, nameable, and can carry its own inputs/persona.
- A published flow can ship **default schedules** that seed onto the pod at install time.
- Fully back-compatible: existing flows (schedule in the entry node) keep working with no
  migration.

## Design

### 1. Data model — `metalcraft-flows` crate (the single home)

Add a top-level field to `SavedFlow`:

```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub schedules: Vec<FlowScheduleSpec>,

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowScheduleSpec {
    /// Stable id within the flow, e.g. "morning". Author-assigned for published
    /// defaults so upgrades can diff. Must be unique per flow.
    pub id: String,
    /// Toggle this trigger without deleting it. Defaults true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The trigger. Tagged by `type`: cron | minutes | hours | manual.
    #[serde(flatten)]
    pub trigger: ScheduleTrigger,
    /// UI label ("Morning brief", "Evening recap").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// IANA tz for cron evaluation (chrono-tz). None = server/local time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Per-schedule inputs handed to run_flow_v2 (so the same flow can run with
    /// different inputs on different schedules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Value>,
    /// Optional per-schedule persona override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleTrigger {
    Manual,                          // no-op trigger; flow runs only via Run/agent
    Minutes { interval: u64 },
    Hours   { interval: u64 },
    Cron    { cron: String },
}
```

**Back-compat helper (no file migration):**

```rust
impl SavedFlow {
    /// The normalized schedule list the runtime should honor.
    /// - If `schedules` is non-empty, it wins (entry-node schedule ignored).
    /// - Else synthesize one from the entry node's `schedule_type` (today's behavior).
    /// - Else a single Manual.
    pub fn effective_schedules(&self) -> Vec<FlowScheduleSpec> { ... }
}
```

**Precedence rule (document in SPEC.md):** non-empty `schedules[]` wins; the entry node's
`schedule_type` is legacy fallback only.

**Validation (`validate.rs`):** unique schedule ids; well-formed trigger shape. Cron *string*
parsing stays agent-side (the flows crate has no `cron` dep) — same boundary as today.

Bump crate to **0.4.0**. Update `SPEC.md` §on scheduling + `FlowSummary` gets
`schedule_count: usize`.

### 2. Agent runtime — `metalcraft-agent`

- `flows.rs`: replace `parse_schedule() -> FlowSchedule` with
  `parse_schedules(flow) -> Vec<ScheduledTrigger>` (one per **enabled** spec), each carrying
  `{ schedule_id, FlowSchedule, inputs, persona, timezone }`. Reads via `effective_schedules()`.
- `load_enabled_flows()` returns one `RunnableSchedule` per **(flow × enabled schedule)** rather
  than one per flow.
- `daemon.rs`:
  - Key due-tracking on **`(flow_id, schedule_id)`** so each cron fires independently.
  - Keep a **per-flow run lock** so two schedules cannot run the same flow concurrently
    (preserves today's `is_running` guard). State becomes:
    - `last_started: HashMap<(flow_id, schedule_id), DateTime>` (due calc)
    - `running: HashSet<flow_id>` (concurrency lock)
  - Pass each schedule's `inputs` / `persona` into `run_flow_v2`.
- Optional: honor `timezone` via `chrono-tz` in `is_due` cron eval (today uses `Local::now()`).
  Ties into the existing calendar-timezone plan. **Decision below: ship tz in v1.**
- `meta_flow` tool: add a `set_schedules` action so the agent itself can add/edit crons
  ("run my morning brief at 8am and 6pm"). Generated entry nodes stay `schedule_type: manual`.

### 3. API — `metalcraft-agent/src/workshop_api.rs`

`put_flow` already accepts a full `SavedFlow`, so `schedules` rides along. Add focused endpoints
so mobile/workshop edit only the schedule (never rewrite the graph):

- `GET    /api/v1/flows/{id}/schedules` — list
- `PUT    /api/v1/flows/{id}/schedules` — replace the whole array
- `POST   /api/v1/flows/{id}/schedules` — add one
- `PATCH  /api/v1/flows/{id}/schedules/{sid}` — edit / toggle
- `DELETE /api/v1/flows/{id}/schedules/{sid}` — remove
- `GET    /api/v1/flows/{id}/schedules/preview` — next N fire times per schedule
  (for "Runs 8:00 AM & 6:00 PM daily" UI summaries)

Each loads the `SavedFlow`, mutates `.schedules`, `save_flow`s. Regenerate OpenAPI
(`cargo run --example dump_openapi`) so `types.ts` picks up the new shapes.

### 4. Published defaults + install (the "ship defaults with the flow" path)

Because `schedules[]` is a field on `SavedFlow`, defaults travel end-to-end with **no new
plumbing**:

```
flows-web author sets defaults
  → registry stores the SavedFlow as raw serde_json::Value (flows-web backend/src/models/flow.rs:23) verbatim
  → agent install_flow_from_registry downloads + validate() + save_flow  → schedules land on the pod
```

Semantics:
- Published `schedules[]` become the pod's **initial/default** schedules on fresh install.
- Install still respects the document's `enabled` flag — registry flows publish **disabled**
  (master switch off) → default crons **do not fire until the user enables the flow**. Right
  safety default: "here are the suggested schedules; turn it on when ready."

**Upgrade / re-install reconciliation — NON-DESTRUCTIVE (chosen):**
- Fresh install (no local flow): take published defaults.
- Upgrade / re-install (local flow exists): **preserve the user's schedules**; do not clobber
  their crons with the author's. Mirrors how install treats an author `requires` block without
  overwriting user intent. Stable author-assigned schedule ids leave room to later offer an
  "author changed the suggested schedule — adopt?" diff, but default stays non-destructive.

`flow_install.rs` change: when a local flow already exists, merge — keep existing
`.schedules`; only seed published defaults when local `.schedules` is empty.

### 5. UI

- **Mobile (`FlowsApp.swift` / `PodClient.swift`)**: a Schedules sheet on a flow — rows
  (name + human-readable cron + enable toggle), "Add schedule" with cron presets + custom,
  delete. Row subtitle shows the summary ("Runs 8:00 AM & 6:00 PM daily"). Uses the new
  `/schedules` endpoints (same read-modify-write pattern as today's `setFlowEnabled`, but
  server-side).
- **Workshop-web + flows-web `FlowDetail`**: a "Schedules" panel with the same add-multiple-crons
  UX; flows-web catalog card / detail surfaces a read-only "Suggested: 8am, 6pm daily".
- **flows-web AdminPublish** (optional): schedules editor so authors set defaults deliberately.
  Registry backend needs **no schema change** (raw JSON already carries `schedules`); a
  denormalized `default_schedules` column is optional for cheap catalog listing.

## Two `enabled` levels (keep both)

- `SavedFlow.enabled` = master switch; gates whether the daemon loads the flow at all.
- `FlowScheduleSpec.enabled` = per-trigger toggle.
- Daemon: skip flow if `!flow.enabled`; else run each **enabled** schedule.

## Edge cases

- Empty `schedules[]` + entry `manual` → no scheduled runs (runnable manually). Unchanged.
- Duplicate schedule ids → validation error.
- `Manual` inside `schedules[]` is a no-op trigger.
- State is still **in-memory** (rebuilt on daemon restart), same as today — on restart
  `last_started` resets so a cron won't double-fire. Persisting next-run across restarts is a
  separate optional follow-up.

## Open decision — RESOLVED

- **Upgrade reconciliation:** non-destructive (preserve user schedules on upgrade). ✅
- **Timezone in v1:** include per-schedule IANA `timezone` (chrono-tz) now. ✅

## Rollout order (each step back-compatible; no lockstep redeploy)

1. `metalcraft-flows` — model + `effective_schedules()` + validation + SPEC.md → **0.4.0**.
2. `metalcraft-agent` — `parse_schedules`, daemon `(flow_id, schedule_id)` keying + per-flow
   run lock, `/schedules` API, install merge, `meta_flow set_schedules`, OpenAPI regen.
3. UIs — mobile `FlowsApp`, workshop-web `FlowDetail`, flows-web `FlowDetail` (+ optional
   AdminPublish authoring).

---

## Implementation status (2026-08-11)

All code written, compiles, and passes tests. **Nothing published or deployed yet.**

- **metalcraft-flows 0.4.0** — `schedules` on `SavedFlow`, `FlowScheduleSpec`/`ScheduleTrigger`,
  `effective_schedules()`, `schedule_count` on `FlowSummary`, `InvalidSchedule` validation,
  SPEC.md §1.3. 72 tests pass. **Needs `cargo publish`.**
- **metalcraft-agent** — dep bumped to `0.4.0` with a **TEMPORARY `[patch.crates-io]` → local
  path** (remove after flows 0.4.0 is on crates.io). `flows::parse_schedules()` (multi-trigger,
  drops disabled/manual, parses cron), `daemon.rs` re-keyed on `(flow_id, schedule_id)` with
  per-schedule tz (chrono-tz)/inputs/persona and no `FlowRunState`, `/api/v1/flows/{id}/schedules`
  GET/PUT/POST + `/{sid}` DELETE + `/preview`, non-destructive install merge in `flow_install.rs`,
  `flow_set_schedules` meta tool (registered in tools/mod.rs, approval.rs, workshop-agent persona).
  147 lib tests pass. OpenAPI regenerated.
- **metalcraft-workshop-web** — `FlowScheduleSpec`/`SchedulePreview` types, `podApi` schedule
  methods, a **Schedule tab** in `views/Flows.tsx` (add/edit/toggle/delete multiple crons +
  next-run preview + "flow is off" warning). tsc + build pass. `api-types.ts` regenerated.
- **metalcraft-mobile** — `FlowScheduleSpec`/`SchedulePreview` models (JSONValue made Encodable so
  `inputs` round-trips), `PodClient` schedule methods, `FlowSchedulesView` + `ScheduleRowEditor`
  in `FlowsApp.swift` (Schedules screen off the flow detail). xcodebuild SUCCEEDED.
- **metalcraft-flows-web** — read-only "Suggested schedules" section in `FlowDetail.tsx` (reads
  `schedules[]` with entry-node fallback). No backend change (registry stores raw JSON). Build passes.

### Publish / deploy order
1. `cd metalcraft-flows && cargo publish` (0.4.0).
2. In `metalcraft-agent/Cargo.toml`, delete the `[patch.crates-io]` block; `cargo build` to
   re-lock against the registry; commit.
3. Build agent image, roll pods ≥ new version.
4. Deploy workshop-web + flows-web frontends.
5. Mobile → TestFlight.
Each step is backward-compatible; no lockstep required.
