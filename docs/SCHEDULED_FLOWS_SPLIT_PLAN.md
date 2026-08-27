# Splitting flows and schedules into two artifacts

Status: **built** (2026-08-27) across four repos, all green; **not committed, and
`metalcraft-flows` 0.5.0 is not published** — every repo still carries a
`[patch.crates-io]` pointing at the local crate. Supersedes the
`schedules[]`-on-the-flow model from `docs/FLOW_SCHEDULES_PLAN.md`.

Verified end to end against a throwaway pod: a legacy v2 flow with two schedules
(one off) and an armed binding migrated at boot into two documents — enabled and
disabled respectively — the flow rewrote itself to spec v3, `flow_bindings.json`
kept its preset and lost its `instances`, a second boot created nothing, and
arm → pause → disarm over HTTP minted a persistent agent and left it alive.

Today one JSON document answers two unrelated questions — *what work is this?* and
*when does it run, as whom, with what inputs?* — and a third store
(`flow_bindings.json`) answers *which agent runs it*. That is why "when does this
fire" needs three tiers of fallback (`effective_schedules()`), why "is it on"
needs two booleans (`flow.enabled` **and** `schedule.enabled`), and why a pack
install has to defensively force both off.

Split them:

| Artifact | File | Answers |
| --- | --- | --- |
| **`SavedFlow`** (spec v3) | `flows/{id}.json` | *What work is this?* A graph. Nothing else. |
| **`ScheduledFlow`** | `scheduled_flows/{id}.json` | *When does it run, as whom, on which agent, with what inputs?* |

---

## 1. The two artifacts

### 1.1 `SavedFlow` (spec_version `"3"`)

```json
{
  "spec_version": "3",
  "id": "morning-brief",
  "name": "Morning brief",
  "created_at": "…", "updated_at": "…",
  "requires": { "packs": [ … ] },
  "flow": { "nodes": [ … ], "edges": [ … ] }
}
```

Removed: `schedules[]`, `enabled`, and any meaning for the entry node's
`data.schedule_type` / `cron` / `interval`.

### 1.2 `ScheduledFlow`

```json
{
  "id": "sf_9c31a4",
  "flow_id": "morning-brief",
  "enabled": true,
  "instance_id": "inst_4f2…",
  "from_suggestion": "morning",
  "schedule": {
    "name": "Morning brief",
    "type": "cron",
    "cron": "0 0 8 * * *",
    "timezone": "America/Detroit",
    "inputs": { "depth": "short" },
    "persona": "morning-briefer"
  },
  "created_at": "…", "updated_at": "…"
}
```

- `id` is **pod-unique** and authoritative everywhere: the daemon's `last_started`
  key, the API path segment, the file name. It collapses today's composite
  `(flow_id, schedule_id)` key.
- `enabled` is now the *only* switch. There is no flow-level master switch,
  because a flow with no `ScheduledFlow` pointing at it already cannot fire.
- `instance_id` is the armed agent, moved out of `flow_bindings.json`. **Creating a
  `ScheduledFlow` is the act of arming** — the "yes, run this in the background"
  consent point, which is now a file that exists rather than two booleans flipped.
- `schedule` is a **pure trigger** and carries no identifier of its own. It keeps
  the `type`-tagged shape flat (`manual | minutes | hours | cron`), so
  `{ "type": "cron", "cron": "…" }` and `{ "type": "minutes", "interval": 15 }`
  both read as written.
- `schedule.persona` / `schedule.inputs` stay per-schedule, which is what lets one
  flow run 08:00-short and 18:00-long.
- `schedule.name` is the display label. Every UI shows it; nothing shows the `id`.
  It is also what `create` uses to name a minted agent (`"{preset} — {name}"`),
  which is what `flow_bindings::arm` does with `FlowScheduleSpec::name` today.
- `from_suggestion` is **provenance, not identity**: the author-assigned key of the
  pack/registry suggestion this artifact was created from, absent when a person
  made it by hand. This is the only surviving job of today's
  `FlowScheduleSpec::id`, and the only thing that can answer "the pack moved its
  morning suggestion to 07:30 — is the user's 08:00 artifact the one it means?"
  Nothing keys off it; nothing requires it to be unique.

### 1.3 Identifier policy

`id` is **opaque and generated** — `sf_<uuid8>` — with an author-chosen slug
permitted at create time for hand-authored artifacts. It is not derived from the
flow id or the trigger.

An id that encodes a fact goes stale the moment the fact changes: an artifact
called `morning-brief-0800` whose cron someone moved to 09:00 is a lie in every
log line and API path that mentions it. The readable handle is `schedule.name`,
which is *meant* to be edited; the id is a pointer, which is not.

### 1.4 What dies

- `SavedFlow::schedules`, `SavedFlow::enabled`
- `FlowScheduleSpec::id` and `FlowScheduleSpec::enabled` — the first was a
  within-flow key nothing keys off any more, the second moves up to the artifact
- `SavedFlow::effective_schedules()` and `entry_schedule_from_node()` — the whole
  three-tier fallback
- `FlowSummary::enabled`, `FlowSummary::schedule_count`
- `flow_bindings.json`'s `instances` map (the file keeps `preset` only)
- `flows::RunnableFlow` / `ScheduledTrigger` (collapse into one struct)
- `flow_bindings::has_schedule()`, `flow_bindings::instance_for()`
- the `flow_set_schedules` tool and all six `/flows/{id}/schedules*` endpoints
- `agent_packs::install_flow_unscheduled`'s defensive `enabled = false` loops —
  structurally unnecessary once nothing schedulable ships inside a flow

---

## 2. Phase 1 — `metalcraft-flows` 0.5.0 (spec v3)

Breaking crate release. `metalcraft-agent` currently pins `metalcraft-flows =
"0.4.0"` from crates.io and only patches `metalcraft`, so:

> **Do this first and publish before merging the agent.** Develop against
> `[patch.crates-io] metalcraft-flows = { path = "../metalcraft-flows" }`, then
> `cargo publish` and drop the patch. (The 0.4.0 release stalled in exactly this
> spot.)

**`src/model.rs`**
- Delete `schedules`, `enabled`, `effective_schedules`, `entry_schedule_from_node`.
- Bump `SPEC_VERSION` to `"3"`; `SUPPORTED_SPEC_VERSIONS` keeps `1`/`2` so old
  documents still *parse* (they must, for migration and for anything already
  published).
- Capture a legacy doc's `schedules` / `enabled` into
  `#[serde(default, skip_serializing)] legacy: LegacyScheduling` — readable by the
  migrator, never written back out.
- `FlowSummary` loses `enabled` + `schedule_count`.

**New `src/scheduled.rs`**
- `ScheduledFlow`, `ScheduleSpec` (today's `FlowScheduleSpec` minus `id` and
  `enabled` — the first is gone, the second moves up a level), reuse
  `ScheduleTrigger` verbatim.
- `ScheduledFlow::new_id()` — `sf_<uuid8>`. Ids are generated, never derived.
- `validate_scheduled_flow()` — id shape, non-empty `flow_id`, positive interval,
  non-empty cron, parseable IANA timezone. Cron *syntax* stays a host concern
  (the agent's `cron::Schedule::from_str`), same as today. `from_suggestion` is
  free-form and unvalidated: it names something in another document's namespace.
- `describe()` / `next_runs(n)` — move `workshop_api::schedule_preview`'s logic
  here so pod, front and flows-web describe a trigger identically.

**New `src/migrate.rs`**
- `extract_scheduled(doc: &serde_json::Value) -> (SavedFlow, Vec<ScheduledFlow>)`
  — pure, unit-testable, no I/O. Handles all three legacy tiers.

**`src/store.rs`** — `save_scheduled_flow`, `load_scheduled_flow`,
`list_scheduled_flows`, `list_for_flow`, `delete_scheduled_flow`.

**`src/validate.rs`** — drop `validate_schedules` from the flow path; a v3 doc
carrying `schedules` is a validation error (v1/v2 docs are exempt: they're inputs
to the migrator, not authored artifacts).

Also: `SPEC.md` §1.3 rewritten, `CHANGELOG.md`, `lib.rs` re-exports.

---

## 3. Phase 2 — agent core

- **`src/paths.rs`** — `scheduled_flows_dir()` → `<data>/scheduled_flows`.

- **New `src/scheduled_flows.rs`** — the store + lifecycle:
  `list()`, `for_flow(flow_id)`, `for_instance(instance_id)`, `get/save/delete`,
  `create(flow, spec, instance: Option<&str>)` (validates the flow exists,
  enforces the preset roster via `flow_bindings::check_personas`, mints or
  attaches the persistent `AgentInstance` — the body of today's
  `flow_bindings::arm`), `disarm(id)`.

- **`src/flows.rs`** — `load_enabled_flows()` → `load_due_candidates()` returning
  `Vec<RunnableSchedule { scheduled: ScheduledFlow, flow: SavedFlow, trigger:
  FlowSchedule }>`; `parse_schedules(&SavedFlow)` → `parse_schedule(&ScheduledFlow)`.
  Graph validation stays; the schedule-parsing half moves. A `ScheduledFlow`
  pointing at a missing flow logs once and is skipped (never silently dropped).

- **`src/daemon.rs`** — `last_started: HashMap<String, DateTime<Local>>` keyed by
  scheduled-flow id. The loop iterates scheduled flows, skips `!enabled` and
  `manual`, and takes the instance straight from `sf.instance_id` instead of
  calling `flow_bindings::instance_for`. `is_due` is unchanged.

- **`src/flow_bindings.rs`** — keeps `preset`, `check_personas`,
  `bind_to_a_capable_preset`, `personas_named` (now reading node personas from the
  flow and schedule personas from that flow's `ScheduledFlow`s). `arm`,
  `has_schedule`, `instance_for` and the `instances` map are removed.

- **`src/flow_install.rs`** — drop the "preserve the user's schedules on
  re-install" block (there is nothing on the flow to clobber). `InstallResult`
  gains `suggested_schedules: Vec<Suggestion>` — `{ key, schedule }` — taken from
  the registry payload and **never** materialized. The install report offers them;
  if the user accepts one, the created artifact records `from_suggestion: key`.
  That key lives in the author's namespace, which is why a *suggestion* keeps an
  id and a `ScheduledFlow`'s trigger does not.

- **`src/agent_packs/mod.rs`** — `install_flow_unscheduled` → `install_flow`; the
  two forced-`false` loops go. Packs may ship `suggested_schedules/<flow-id>.json`
  (an array of `{ key, schedule }`) as an inert sidecar next to `flows/<name>.json`.

- **`src/agent_packs/bundle.rs`** — `flow_personas` reads the sidecar instead of
  `flow.schedules[].persona`, so the containment check still sees every persona a
  pack's background work could reach.

- **`src/flow_exec.rs`** — untouched except the `schedules: vec![]` test fixture.

- **`src/approval.rs`** — swap `flow_set_schedules` out of the allowlist for the
  new tool names.

- **Delete-agent guard** (`workshop_api.rs` ~1700–1900) — "this agent still runs
  scheduled flows" now comes from `scheduled_flows::for_instance()`.

---

## 4. Phase 3 — agent tools

In `src/tools/meta_flow.rs` + `tools/mod.rs`, replace `flow_set_schedules` with:

| Tool | Does |
| --- | --- |
| `scheduled_flow_list` | all, or filtered by `flow_id` / `instance_id` |
| `scheduled_flow_create` | create + arm in one call (`flow_id`, `schedule`, optional `instance_id`, optional `from_suggestion`); the pod generates the `id` |
| `scheduled_flow_update` | edit trigger / inputs / persona / `enabled` |
| `scheduled_flow_delete` | disarm + remove |
| `scheduled_flow_preview` | describe + next 3 fire times for a candidate spec |

`flow_list / flow_read / flow_write / flow_run / flow_resume / flow_run_status /
flow_runs_list / flow_install / flow_check_dependencies / flow_templates_*` are
unchanged. Update `seed/skills/authoring-flows.md` — it teaches the old shape.

---

## 5. Phase 4 — workshop API (clean cutover)

**New**
```
GET    /api/v1/scheduled-flows            ?flow_id= &instance_id=
POST   /api/v1/scheduled-flows            create + arm (server-generated `id`)
GET    /api/v1/scheduled-flows/{id}
PUT    /api/v1/scheduled-flows/{id}       trigger / inputs / persona / enabled / instance
DELETE /api/v1/scheduled-flows/{id}       disarm + delete
GET    /api/v1/scheduled-flows/{id}/preview
POST   /api/v1/scheduled-flows/preview    describe an unsaved spec
```

Each response row carries the artifact plus the two derived fields the UI needs
today: `description` ("Cron `0 0 8 * * *` (America/Detroit)") and `next_fire_at`,
plus `instance_name` (absent if the instance was deleted out from under it) and
`flow_name`.

`POST` ignores a client-supplied `id` and generates one, except for an explicit
author slug (§1.3) — which is validated as a slug and rejected on collision, so a
create can never silently overwrite an existing schedule.

**Removed**: `/flows/{id}/schedules` (GET/PUT/POST), `/schedules/preview`,
`/schedules/{sid}` (DELETE), `/schedules/{sid}/arm` (POST/DELETE).

**Changed**: `FlowListItem` drops `enabled`, `armed`, `schedules[]` and gains
`scheduled_count`. `GET /flows` becomes a listing of *graphs*; the Automations
view makes one extra call to `/scheduled-flows` and joins client-side.
`/agents/{id}/flows` (`InstanceFlows`) is rebuilt from `for_instance()`.

Regenerate OpenAPI: `cargo run --example dump_openapi`.

---

## 6. Phase 5 — migration (the risky part)

`scheduled_flows::migrate_from_flows()`, called at daemon boot beside
`metalcraft_gateway::migrate_legacy_keys()`, guarded by
`<data>/.migrations/scheduled_flows_v1` and idempotent per flow (skip anything
already at `spec_version: "3"`).

Idempotency needs care now that ids are generated: a re-run must not mint a second
artifact for a schedule it already migrated. The per-flow skip is the primary
guard (a migrated flow no longer carries `schedules`, so there is nothing to
extract twice); the secondary guard is `(flow_id, from_suggestion)` — if an
artifact with that pair already exists, the schedule is already migrated. The
"run it twice" test in §8 covers exactly this.

For each `flows/*.json`, read as raw JSON and for every effective schedule
(top-level array → legacy entry node → nothing):

- `id` = a fresh `sf_<uuid8>` (§1.3 — never derived from the flow id or trigger)
- `from_suggestion` = the legacy `schedule.id`, preserved as provenance. This is
  the one place the old within-flow key is worth keeping: it is exactly the
  author-assigned key a published flow's suggestions were written against.
- **`enabled = flow.enabled && schedule.enabled`** — the safety property: *migration
  must never make something fire that was not already firing*
- `instance_id` = `flow_bindings.json → flows[flow_id].instances[schedule.id]`
- `schedule.name` = the legacy `schedule.name`, falling back to the legacy
  `schedule.id` (`"morning"`) so nothing migrates into a nameless row in the UI
- `manual` triggers: create an artifact **only if it was armed** (an armed manual
  schedule means "when I run this by hand, be this agent" — real state). Unarmed
  manual schedules produce nothing; hand-runs still work.

Then rewrite the flow doc without `schedules`/`enabled` at `spec_version: "3"`,
and strip `instances` from `flow_bindings.json` (keep `preset`). A flow whose
extraction fails is left **untouched** and logged at error — never half-migrated.
Log one line per artifact created plus a summary count.

Also strip `schedule_type` from the three `seed/flow_templates/*.json` and bump
them to spec 3. No `seed/scheduled_flows/` — nothing should be scheduled on a
fresh pod.

---

## 7. Phase 6 — clients

**metalcraft-front**
`crates/front-core/src/models.rs` (`FlowSchedule` → `ScheduledFlow`),
`crates/front-core/src/pod.rs` (`schedules` / `set_schedules` / `arm_schedule` /
`disarm_schedule` → the CRUD above), `frontend/src/types.ts`,
`features/automations/AutomationsView.tsx` + `ArmDialog.tsx`,
`stores/sessions.ts`, `app/CommandPalette.tsx`,
`features/settings/DangerZoneCard.tsx`; tests `AutomationsView.test.tsx` and
`crates/front-core/tests/live_pod.rs`.

**metalcraft-mobile**
`Scripts/sync-openapi.sh` → regenerate `Sources/PodKit/Generated/PodSchemas.swift`;
hand-typed shapes in `Undescribed.swift`; `PodClient+Flows.swift`;
`Features/Automations/{AutomationsView,FlowDetailView}.swift`;
`Tests/PodKitLiveTests/LivePodTests.swift`.

**metalcraft-flows-web**
Migration adding `suggested_schedules JSONB` to `flows` (an array of
`{ key, schedule }`, the `key` taken from the uploaded document's old
`schedules[].id`); the publish/seed path lifts `schedules[]` out of the uploaded
document into that column and stores the `SavedFlow` clean; `backend/src/models/flow.rs` (`Flow`, `FlowDetail`);
`frontend/src/pages/FlowDetail.tsx` reads the column instead of digging into the
document; re-seed the `flows/` fixtures.

**Display rule for all three** — a schedule is labelled by `schedule.name`, with
its `description` ("Cron `0 0 8 * * *`…") as the secondary line. The `id` appears
only where a machine needs it (URLs, logs, error text). Today's UIs key their list
rows off `schedule_id`; those become the artifact `id` as an opaque React/SwiftUI
key, not something rendered.

**Version skew** — a clean cutover means old client ↔ new pod (and the reverse)
both break. Add a 404-on-`/scheduled-flows` branch in front + mobile that says
"this pod needs updating" rather than showing an empty Automations list, and ship
in the order in §9.

---

## 8. Phase 7 — tests

Update: crate tests in `model.rs` / `validate.rs`; agent `tests/flow_list_test.rs`,
`flow_binding_test.rs`, `flow_preset_autobind_test.rs`, `flow_conversation_test.rs`,
`agent_pack_install_test.rs`.

New `tests/scheduled_flow_migration_test.rs`, with a fixture per legacy shape:

1. v2 flow, two enabled crons, one armed → two artifacts, one carrying the instance
2. v2 flow with `enabled: false` → artifacts exist, all `enabled: false`
3. v1 flow, legacy entry-node cron → one artifact
4. manual + armed → artifact; manual + unarmed → none
5. flow with neither → no artifact, flow still hand-runnable
6. run the migration twice → byte-identical result (no second artifact minted
   despite ids being generated — see the `(flow_id, from_suggestion)` guard in §6)
7. a legacy schedule with no `name` migrates to `schedule.name` = its old id, not
   to an empty label

Plus a daemon test that a disabled artifact never fires and an enabled one fires
once per due window.

---

## 9. Rollout order

1. `metalcraft-flows` 0.5.0 → `cargo publish`
2. agent PR (phases 2–5 + tests) → build image → **roll pods** (migration runs at boot)
3. flows-web deploy (independent of the pod)
4. front release, then mobile TestFlight build

Steps 2 and 4 are the skew window; the 404 branch in §7 keeps it legible.

## 10. Deviations taken while building

Recorded here rather than silently: each is a place the plan said one thing and
the code does another, with the reason.

1. **No `legacy` field on `SavedFlow`.** The plan had the crate capture a pre-v3
   document's `enabled`/`schedules` into a skipped field. Unnecessary: serde
   ignores unknown fields, so a v1/v2 document already parses, and `migrate`
   reads the scheduling from the **raw JSON** instead. Keeps the published type
   clean. The cost is that saving an un-migrated flow through the v3 types drops
   its scheduling — which is why migration runs from `seed::ensure_defaults()`
   (both binaries, before anything reads a flow) rather than from the daemon
   alone.
2. **`new_id()` and `next_runs()` live in the agent, not the crate.** The crate
   has no `uuid`, `chrono` or `cron` dependency and is meant to stay pure; id
   minting and firing-time projection need all three. The crate keeps
   `describe()` (pure string) so every client phrases a trigger identically.
3. **No `GET /scheduled-flows/{id}/preview`.** The row already carries
   `description` + `next_fire_at`, so the only unserved question was "when would
   *this unsaved* trigger fire" — which is `POST /scheduled-flows/preview`.
4. **`fetch_flow` returns `(flow, suggestions)`.** A registry that still serves
   pre-v3 documents has its `schedules[]` turned into *suggestions* on the way
   in, so a published flow that used to arrive pre-scheduled stops doing that
   without flows-web having to migrate first.
5. **`Suggestion` is `{ key, schedule }`,** and lives in the crate, since packs
   and the registry both serve it.
6. **The invalid-cron diagnosis moved into `preview`.** `describe()` cannot
   parse cron, so the host prefixes `"Invalid cron `…`: …"` when its own parser
   rejects one. Preserves today's behaviour: a schedule that will never fire
   reads as broken rather than as merely having nothing scheduled.
7. **`flow_bindings::FlowBinding::instances` survives as a read-only legacy
   field**, drained by `clear_instances` during migration and never written.
   Removing it outright would make un-migrated pods unreadable.

## 11. Docs to update alongside

`docs/FLOWS_ARCHITECTURE.md` (and fix its stale §9 — wiring, `http`/`sub_agent`
and pause/resume all shipped), `docs/FLOW_SCHEDULES_PLAN.md` (mark superseded),
`metalcraft-flows/SPEC.md`, `seed/skills/authoring-flows.md`, `how_it_works.md`.
