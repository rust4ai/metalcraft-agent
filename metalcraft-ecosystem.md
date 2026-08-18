# The Metalcraft Ecosystem

*How `metalcraft-agent` works, how identity flows through the platform, and what the cloud
services around it do.*

> **Ground truth: mainline only.** Everything below was read from each repo's default branch —
> `master` for `metalcraft-agent` and the `metalcraft` framework crate, `main` for every other
> service. Work living on feature branches is deliberately excluded, even where it is nearly
> ready to land. Where a repo's own committed docs disagree with its committed code, the **code**
> wins and the disagreement is recorded in [§8](#8-doc-drift-on-mainline).
>
> Snapshot: `metalcraft-agent` v0.29.0.

---

## Table of contents

1. [The shape of the system](#1-the-shape-of-the-system)
2. [How metalcraft-agent works](#2-how-metalcraft-agent-works)
3. [Capabilities: packs, tools, keys](#3-capabilities-packs-tools-keys)
4. [Messaging: channels and the gateway](#4-messaging-channels-and-the-gateway)
5. [The auth ecosystem](#5-the-auth-ecosystem)
6. [The cloud apps](#6-the-cloud-apps)
7. [Deployment topology](#7-deployment-topology)
8. [Doc drift on mainline](#8-doc-drift-on-mainline)
9. [Repo index](#9-repo-index)

---

## 1. The shape of the system

Metalcraft is a **single-tenant agent pod surrounded by first-party cloud services**, all hanging
off one identity hub.

```
                        ┌───────────────────────────────┐
                        │  metalcraft-id                │  id.metalcraftai.com
                        │  users · PATs (mck_) · JWKS   │  root of trust
                        │  /verify · /internal/tokens   │  credit ledger · billing
                        └───────┬───────────────┬───────┘
             mints pod tokens   │               │  every subapp verifies here
            (service secret)    │               │  (JWKS offline · /verify for PATs)
                        ┌───────▼───────┐       │
                        │ k3-cluster    │       ├── calendar · contacts · notes
                        │ control plane │       ├── drive · email · photos · music
                        │ pods.metal…   │       ├── meet · code · images-web
                        └───────┬───────┘       ├── flows-web · packs-web (registries)
          provisions 1 pod/user │               └── inference (credit-metered LLM)
                        ┌───────▼─────────────────────────────┐
                        │  metalcraft-agent  (the pod)        │  <slug>.pods.metalcraftai.com
                        │  personas · tools · flows · packs   │  all state = files on a PVC
                        └───────┬─────────────────────────────┘
                    inbound msg │ ▲ outbound reply
                        ┌───────▼─┴───────┐
                        │ gateway         │  gateway.metalcraftai.com
                        │ SMS / WhatsApp  │  holds the Twilio credentials
                        └─────────────────┘
```

Four principles show up over and over in the code, and they explain most of the design decisions
that follow.

**One authoritative owner per fact.** The hub owns accounts, billing and tokens. The control plane
owns pod *workload* lifecycle and nothing else. The pod owns its own runtime state — channels,
keys, flows, chats. The gateway owns messaging. Nobody keeps a display copy of another service's
truth; when the gateway needed to show a pod's connection status it was made a live proxy rather
than a cache, and the control plane's `gateway_connections` table was created in migration 003 and
dropped again in migration 004 for exactly this reason.

**No god-scoped user token.** Cross-account authority is only ever a shared service secret
(`X-Metalcraft-Service-Secret`), never a user credential carrying elevated scope. There are three
scopes in the entire ecosystem — `read`, `write`, `admin` — and `metalcraft-id`'s
`SUBAPP_STANDARD.md` explicitly forbids inventing per-app ones.

**Sleep-friendly by default.** Nearly every service runs on Neon Postgres, which autosuspends when
idle. That constraint is visible everywhere: token introspection is Redis-cached with a one-day
TTL, `last_used` writes are throttled to five-minute granularity, the gateway throttles user-mirror
upserts to 30 minutes, and long-polls are backed by Redis doorbells so a held connection doesn't
keep the database awake.

**Fail closed, fail loud.** Service-secret middleware refuses everything if the secret is unset.
Token verification returns "no" on any transport or parse error. Unknown tool names default to
requiring human approval. Misconfiguration is meant to stop the process, not degrade silently.

---

## 2. How metalcraft-agent works

`metalcraft-agent` is a Rust binary (edition 2024) built on three published crates that the rest of
the platform also depends on: `metalcraft` 0.9 (a LangGraph-style typed, cyclic graph orchestrator —
`Reducer` / `Node` / `Graph` / `Executor`, with checkpointing and human-in-the-loop interrupts),
`metalcraft-flows` 0.4 (the flow wire format), and `metalcraft-packs` 0.1 (the pack manifest spec).
Around those sit `rig` 0.37 for LLM calls, Axum 0.8 for HTTP, and `utoipa` for the OpenAPI contract.

### 2.1 Entrypoints and run modes

Two binaries on master:

| Binary | Source | Role |
|---|---|---|
| `metalcraft-agent` | `src/main.rs` | Interactive REPL or one-shot task. `--persona/-p <slug>`, `--auto-approve`, `--api [KEY]`, `--api-port <n>`. |
| `metalcraft-daemon` | `src/bin/metalcraft-daemon.rs` | Flow scheduler + follow-up firer, optionally serving the HTTP API. `--flows-dir`, `--persona`, `--model`, `--poll-seconds` (default 30), `--once`, `--auto-approve`, `--api <KEY>`, `--api-oidc`, `--api-port` (default 3002). |

Both call `seed::ensure_defaults()` and `dotenvy::dotenv()` before anything else — `.env` must load
before the first `paths::data_dir()` call, because the data directory is memoized on first access.

The daemon still accepts a set of retired flags (`--event-port`, `--event-host`, `--event-persona`,
`--events`, `--platforms`, `--admin-user-ids`) and ignores them with a deprecation warning: "the
external gateway was removed."

Run modes in practice: interactive REPL · one-shot task · `--once` (run what's due, exit) ·
continuous poll loop · API-only.

### 2.2 The turn loop

Every code path that runs the agent — CLI, one-shot task, API chat turn — funnels through a single
`runtime::TurnRunner`. That consolidation is deliberate: it exists so compaction, step limits and
guard logic cannot be present in one path and missing from another.

Setup, in `build_agent_runtime()`:

1. `AgentRuntimeContext::from_environment()` loads config and requires `OPENAI_API_KEY`.
2. The persona is resolved and `persona.build_system_prompt()` renders its prompt.
3. `tools::create_registry_for_with_config` builds the tool registry from the persona's resolved
   tool names.
4. An OpenAI client is built through `rig`, honoring `OPENAI_BASE_URL`.
5. `approval::build_hook` wires the approval gate.
6. `metalcraft::create_react_agent_with_options` compiles the graph.

Then each turn:

```
context::compact_if_needed          // best-effort; failure logs and proceeds uncompacted
  → Executor::new_from_arc(graph)
      .max_steps(MAX_TURN_STEPS)    // 90
      .with_step_guard(step_guard)
      .run(state, "agent")
```

One non-obvious detail: `build_openai_client` deliberately does **not** call `.completions_api()`,
leaving rig on the Responses API (`POST {base}/responses`). Chat Completions rejects the agent's
parallel-tool-call message layout with a 400.

Model and persona resolution both walk an env precedence chain:

- Model: `METALCRAFT_MODEL` → `STARKBOT_MODEL` → `DEFAULT_MODEL = "gpt-5.4"`.
  `AVAILABLE_MODELS = ["gpt-5.4-mini", "gpt-5.4", "gpt-5.5"]`.
- Persona: `METALCRAFT_PERSONA` → `METALCRAFT_DEFAULT_PERSONA` → `STARKBOT_PERSONA` →
  `DEFAULT_PERSONA = "orchestrator-agent"`.

The `STARKBOT_*` names are legacy aliases still honored throughout the daemon.

**Approval** (`src/approval.rs`). `OperationKind::classify(name, args)` sorts each tool call into a
sensitivity kind, and `default_permission()` decides. Auto-approved: `ReadFile`, `ListFiles`,
`Search`, `LoadSkill`, `MetaRead`, `WriteNewFile`. Requires approval: `OverwriteFile`, `EditFile`,
`Execute`, `NetworkFetch`, `SubAgent`, `DiscordAction`, `MetaWrite`. Pack-specific read/write splits
are hardcoded for the `discord_*`, `calcom_*`, `mcal_*`, `mnote_*` and `mdrv_*` prefixes.
`flow_run` classifies as `SubAgent` (approval required); `flow_install`, `flow_set_schedules` and
`pack_enable` classify as `MetaWrite`. Once a path has been approved for edit or overwrite within a
session it is not re-prompted. Diffs are previewed before write (`src/diff_preview.rs`), with an
alt-screen scroll view for large ones.

**Context compaction** (`src/context.rs`). `context_window = 128_000`, `compact_threshold = 0.6`
(so compaction triggers above roughly 76,800 estimated tokens), `keep_recent_messages = 10`, with
tokens estimated as `total_chars / 4`. `safe_split()` walks the boundary backward so the retained
window never begins mid-tool-block — a `ToolResult` with no preceding call, or a `ToolCall` missing
its paired `Reasoning` item, are both invalid provider payloads.

**Guard** (`src/guard.rs`). `max_consecutive_errors = 3`, `max_identical_repeats = 4`,
`max_poll_repeats = 60`, `verbose = true`. Error spirals and identical-call loops both stop the run.
Tools whose JSON sets `"poll": true` get the looser 60-call budget so a status-polling tool isn't
mistaken for a loop. A call hash is retracted from the tally if its result was a denial.

**Observability.** `src/diagnostics.rs` writes per-session JSON — LLM calls, turns, config switches
— under `<data>/sessions/<timestamp>/`. `src/trace.rs` emits one OTLP/JSON trace per chat session at
`<data>/traces/<session>/otlp-trace.json` using GenAI semantic conventions. Worth knowing:
**per-call token usage is not currently captured**, because the `LlmCallHook` fires before `.send()`
and the response never reaches the logger. The module doc names a future `LlmResponseHook` as the
fix.

### 2.3 Personas

A persona is the unit of least privilege: a named bundle of tools, a system prompt, and skills.
Every run happens under exactly one.

On disk at `personas/<slug>.json`:

```rust
struct Persona {
    name: String, description: String,
    tools: Vec<String>,        // native / meta / HTTP tool names
    packs: Vec<String>,        // pack ids this persona is scoped to
    skills: Vec<String>,
    version: Option<String>,   // drives seed force-upgrade
    system_prompt: String,
}
```

**Resolution and shadowing.** `Persona::load`, `list_available` and `list_summaries` all go through
`integration_packs::resolve_or_explain` / `list_files_layered`: the user's local `personas/` is
checked first, then any *enabled* pack's `personas/`. A local file of the same slug shadows the pack
copy, so a shipped persona can be tweaked without forking its pack.

**`resolved_tool_names()`** = the explicit `tools` list, plus every HTTP-API tool belonging to each
pack named in `packs`, plus that pack's native tools — the latter only if the pack is enabled.

**System-prompt templating** (`build_system_prompt`) substitutes `{{cwd}}`, `{{available_skills}}`,
`{{available_personas}}`, `{{installed_packs}}` and `{{now_utc}}` (formatted
`%Y-%m-%dT%H:%M:%SZ (%A)`). Anything the author didn't reference is appended afterward under a
default heading — which is why personas written before templating existed still get the current
time, the skill list, and (when they have `sub_agent`) the live persona list. That last one matters:
injecting real installed slugs stops the model from guessing a persona name that doesn't exist.

Seeded personas: `orchestrator-agent` (default), `coding-agent`, `config-agent`, `devops-agent`,
`research-agent`, `workshop-agent`, `morning-briefer`, `video-script-agent`. The orchestrator ships
at version 1.4.0 with tools `[read_file, list_files, grep, find_files, sub_agent, schedule_followup,
load_skill]`, skills `[planning, explore-codebase, research-methodology, summarize]`, and no packs —
it delegates rather than doing.

**Delegation** (`src/tools/sub_agent.rs`). The `sub_agent` tool takes `task` (required), an optional
`persona` (preferred — the child inherits that persona's resolved tools, prompt and skills), and a
`tool_set`:

- `read_only` (default) — `read_file`, `list_files`, `grep`, `find_files`
- `full` — adds `write_file`, `edit_file`, `bash`
- `all` — full plus every installed integration tool, optionally narrowed by `pack`

Children run with `max_steps(90)` under a 120-second `tokio::time::timeout`. They get
`reply_sink: None` and `session_binding: None`, so a sub-agent can neither talk directly to the user
nor arm a follow-up. If a persona names a pack that isn't enabled, the tool fails fast with a clear
message rather than producing an orphaned tool call the provider would reject with a 400.

### 2.4 Skills

A skill is a methodology, not code: `skills/<slug>.md` with a YAML frontmatter `description` and a
Markdown body. `load_skill(slug)` strips the frontmatter and returns `{slug, description, body,
pack_id, read_only}`. The `load_skill` tool's JSON Schema restricts its argument to the persona's
declared skills, so the model can only load what it was granted. Local skills shadow pack skills the
same way personas do, and `pack_owner_blocking_write()` refuses a user write or delete of a slug that
currently only exists inside a pack.

Seeded: `authoring-flows`, `authoring-personas`, `authoring-skills`, `ci-cd`, `code-review`,
`commit-message`, `debugging`, `dockerfile-best-practices`, `edit-workflow`, `explore-codebase`,
`managing-integrations`, `planning`, `research-methodology`, `summarize`, `video-scripting`,
`workshop-overview`.

### 2.5 Flows

A flow is a saved state machine the daemon can run on a schedule. The wire format lives in the
separate `metalcraft-flows` crate — a pure spec with no I/O — so the agent, the Workshop and
`flows.metalcraftai.com` all agree on it.

`flow_exec::is_v2_flow(flow)` returns true if any node type is v2. v2 flows run through
`flow_exec::run_flow_v2` as a stateful machine; legacy v1 flows still run through
`flows::collect_reachable_prompts` (a BFS over reachable `Prompt` nodes) with each prompt executed as
an isolated one-shot task, sharing no state.

**Node types implemented** in `FlowExecutor::run_node`: `Entry`, `End`, `SetVariable`, `Conditional`,
`Prompt`, `Tool`, `Http`, `SubAgent`, `Branch`, `Approval`, `Wait`. Only `ForEach` still falls
through to the "not implemented yet" arm.

- **State** is one `variables` JSON object. Nodes return `Route::Handle(Option<String>)` to follow a
  named output edge (or the unlabeled default), `Route::End(status)`, or `Route::Pause(PauseSpec)`.
- **`conditional`** is deterministic: each condition resolves an `Operator` from the flows crate and
  evaluates it against a variable, routing to the first match, else to `default_handle`.
- **`branch`** is the LLM counterpart. It builds a tool-only sub-agent with `ToolChoice::Required`
  and the node's output handles as terminal tools, where each declared output becomes a synthetic
  `HandleTool` (scalar outputs get wrapped as `{"value": <schema>}`). The model must call exactly one
  handle tool to terminate, and its arguments become the edge payload, structurally validated against
  the handle's declared `required` fields. `max_steps(30)`. Malformed payloads, timeouts, errors and
  no-selection all route to the reserved `BRANCH_ERROR_HANDLE` (or the flow's `default_handle`)
  instead of silently appearing to succeed.
- **`approval`** pauses with `resume_handles` defaulting to `["approve", "reject"]` and a `wake_at`
  timeout. **`wait`** pauses with `["after"]` and a `wake_at` from `data.until` or a parsed duration.

**Durability.** A paused run is a `FlowRun` JSON file at `<data>/runs/{id}.json`:

```
{ id, flow_id, status: running|paused|completed|failed, current_node_id, variables,
  pause: { reason, resume_handles, message, wake_at }, persona, model, cwd,
  steps, flow: Option<SavedFlow>, warnings, created_at, updated_at }
```

That `flow` field is a **snapshot of the graph taken at pause time** — resuming routes against the
graph as it was, not against a version the user has since edited. Each daemon poll scans for paused
runs whose `wake_at` has passed and resumes them with `"after"` (wait) or `"timeout"` (an approval
that expired). A paused run whose flow no longer exists and has no snapshot is marked `failed` once
rather than erroring forever.

**Scheduling — reworked in v0.29.0.** Scheduling used to hang off the entry node. It now lives in a
flow-level `schedules: Vec<FlowScheduleSpec>` array in `SavedFlow`, each spec being:

```
{ id, enabled (default true), trigger: manual | minutes{interval} | hours{interval} | cron{cron},
  name, timezone (IANA, via chrono-tz), inputs, persona }
```

`SavedFlow::effective_schedules()` prefers a non-empty `schedules[]`; failing that it synthesizes one
from the entry node's legacy `schedule_type`; failing that it returns a single `Manual`. So old flows
keep working untouched. `flows::parse_schedules` then drops disabled and manual entries, validates
cron strings and positive intervals, and yields one `ScheduledTrigger` per flow × enabled schedule.
The daemon tracks due-ness per `(flow_id, schedule_id)` and runs triggers inline and sequentially
within a poll tick, which is why no concurrency lock is needed.

API surface for schedules: `GET` (effective list), `PUT` (replace the whole array), `POST` (add one),
`GET …/preview` (next-fire preview), `DELETE …/{sid}` (remove one). There is deliberately **no
PATCH** — editing a single schedule means a whole-array `PUT`. Registry-published flows may ship
default schedules, and install is non-destructive on upgrade: defaults are only seeded when the local
`schedules` array is empty.

### 2.6 Scheduled follow-ups

Distinct from flows: this is the agent deferring its own work — "check back on this in an hour" —
rather than a saved workflow. State is a JSON array at `<data>/scheduled_tasks.json`, written
atomically under a process-wide mutex.

Bounds are tight on purpose: `MIN_DELAY_SECS = 10`, `MAX_DELAY_SECS = 86_400` (a day — past that,
write a flow), `MAX_PENDING_PER_CHAT = 20`, `MAX_RESCHEDULE_DEPTH = 12` to bound self-rearming
chains. `TaskStatus` is `Pending | Running | Done | Failed | Cancelled`. Delivery is decided by an
`IoBinding` of `WorkshopChat { chat_id }`, `Gateway { channel_id, address }`, or `Unbound` (result
logged only — a one-shot CLI run has nowhere to reply). Delays parse as `"90s"`, `"3m"`, `"2h"` or a
bare integer of seconds. Due tasks fire on the same daemon poll tick as flow scheduling, immediately
after the auto-resume pass.

### 2.7 The HTTP API

`src/workshop_api.rs`. Authentication accepts a Bearer token that is either the static
`WORKSHOP_API_KEY` (byte-equal comparison) or a Metalcraft ID `mck_…` token
([§5.3](#53-how-a-pod-authenticates-its-callers)). Both modes may be active at once.

A structural detail worth remembering when adding routes: the auth middleware is applied with
`.layer(...)` partway through the router chain, so it covers only the routes registered **before**
that call. Routes registered after it are public by construction.

Public routes: `GET /health`, `GET /` (landing), `GET /api/v1/openapi.json` and `/api/v1/docs` (the
Scalar UI), and `POST /webhook/gateway` — which is HMAC-verified per request instead.

```
GET    /api/v1/info
GET    /api/v1/snapshot
GET|PUT|DELETE  /api/v1/personas/{slug}
GET|PUT|DELETE  /api/v1/skills/{slug}
POST   /api/v1/flows/install
GET|PUT|DELETE  /api/v1/flows/{id}
POST   /api/v1/flows/{id}/run
GET|PUT|POST    /api/v1/flows/{id}/schedules
GET    /api/v1/flows/{id}/schedules/preview
DELETE /api/v1/flows/{id}/schedules/{sid}
POST   /api/v1/flows/{id}/install-dependencies
GET    /api/v1/flow-runs
GET    /api/v1/flow-runs/{run_id}
POST   /api/v1/flow-runs/{run_id}/resume
GET    /api/v1/flow-templates[/{slug}]
GET    /api/v1/diagnostics[/{id}]
GET|PUT|DELETE  /api/v1/api-tools[/{name}]
GET    /api/v1/keys
GET    /api/v1/keys/recommended
PUT|DELETE      /api/v1/keys/{name}
GET    /api/v1/keys/{name}/reveal
GET|POST        /api/v1/chats
GET|DELETE      /api/v1/chats/{id}
POST   /api/v1/chats/{id}/turn
GET    /api/v1/chats/{id}/events            (SSE)
GET|DELETE      /api/v1/scheduled-tasks[/{id}]
GET    /api/v1/integration-packs
POST   /api/v1/integration-packs/install
GET|DELETE      /api/v1/integration-packs/{id}
PUT    /api/v1/integration-packs/{id}/enabled
GET    /api/v1/lockfile
POST   /api/v1/lockfile/restore
GET    /api/v1/gateway/activity
GET|POST        /api/v1/channels
PUT|DELETE      /api/v1/channels/{slug}
GET    /api/v1/channels/{slug}/events
GET    /api/v1/gateway/metalcraft/status
POST   /api/v1/gateway/metalcraft/register|connect|disconnect
```

### 2.8 State: files, not a database

`data_dir()` resolves once per process and is memoized: `METALCRAFT_DATA_DIR` →
`dirs::data_dir()/metalcraft-agent` → `./data`.

```
<data>/
  personas/*.json        skills/*.md            flows/*.json
  flow_templates/*.json  api_tools/*.json       runs/{id}.json
  sessions/<ts>/         traces/<ts>/otlp-trace.json
  chats/                 integration_packs/<id>/  integration_packs.json
  channels.json          keys.json              inbound_dedup.json
  scheduled_tasks.json   gateway_activity.jsonl
  uploads/               (override METALCRAFT_UPLOAD_ROOT)
  .metalcraft_packs_seeded
```

Everything is flat JSON written tmp-then-atomic-rename. There is no database: backing up a pod means
backing up one volume. Two legacy artifacts are actively cleaned up rather than read —
`<data>/gateway_channels/` (the old channel-*type* manifest directory) is retired at startup by
`seed::retire_dir`, and `<data>/gateway_channels.json` (the old channel-*instance* file) is migrated
into the current channel model once and then deleted.

### 2.9 Deploying a single agent

- **Dockerfile** — two-stage `rust:1.91-bookworm` → `debian:bookworm-slim`, builds only
  `metalcraft-daemon` using the stub-`main.rs` dependency-cache trick, copies `seed/` to
  `/opt/metalcraft/seed`, `CMD ["metalcraft-daemon", "--auto-approve"]`. No `--api` is baked in — the
  API turns on purely via env.
- **docker-compose.yml** — one `daemon` service on `ghcr.io/rust4ai/metalcraft-agent:${TAG:-latest}`,
  publishing `3002:3002` directly, `METALCRAFT_DATA_DIR=/data`.
- **docker-compose.caddy.yml + Caddyfile** — the daemon runs `--api-port 8080` and is only
  `expose`d, never published; `caddy:2-alpine` takes 80/443 and reverse-proxies to `daemon:8080`,
  provisioning Let's Encrypt certs from `{$DOMAIN}` / `{$TLS_EMAIL}`.
- **render.yaml** — Render blueprint on the GHCR image, `plan: starter`, health check `/health`, 1 GB
  persistent disk at `/data`.
- **railway.toml** — Dockerfile builder, restart on failure, max 3 retries.
- **.do/app.yaml** — DigitalOcean App Platform, `http_port: 8080`, with an explicit warning that the
  platform's filesystem is ephemeral: only embedded seed content survives a redeploy, so
  runtime-created chats, personas and keys are lost.
- **start-agent.sh / update-agent.sh** — `start-agent.sh` creates `.env` from the example on first
  run, treats a missing or placeholder `OPENAI_API_KEY` as fatal, and warns (but proceeds) when
  `WORKSHOP_API_KEY` is unset, since the flow scheduler still runs without an API. Defaults to
  `COMPOSE_FILE=docker-compose.caddy.yml`; `TAG=` pins a release.

---

## 3. Capabilities: packs, tools, keys

### 3.1 Integration packs

A pack is a versioned directory bundling everything one integration needs:

```
<pack>/
  pack.json          # PackManifest: id, name, description, version,
                     #   requires_env, tags, native_tools, …
  README.md
  personas/*.json
  skills/*.md
  api_tools/*.json
  flow_templates/*.json
```

`PackManifest`, `ECOSYSTEM_TAG` and `is_ecosystem` are re-exported from the shared `metalcraft-packs`
crate, so the agent and the registry validate against the same spec (including the id regex
`^[a-z0-9][a-z0-9_-]{0,63}$` and the canonical content hash).

**Seeding.** `src/seed.rs` embeds the whole `seed/` tree at compile time with `include_dir!`, so the
binary is self-contained — a consequence worth internalizing: *the binary cannot ship a pack that
isn't under `seed/`*. Packs install to `<data>/integration_packs/<id>/` and default to **disabled**.
`set_enabled(id, true)` always re-installs from the embedded seed first, so "enabled" is a guarantee
that the files exist.

**Enable state** lives separately in `<data>/integration_packs.json`:

```json
{ "metalcraft-notes": { "enabled": true, "enabled_at": "…", "source": null } }
```

`source: "registry"` marks a pack pulled from `packs.metalcraftai.com`; null means built-in. Writes
are atomic (tmp + fsync + rename) under an advisory file lock, so the daemon and the Workshop can't
lose an update to each other.

**Registry installs** go through `install_from_zip`, which enforces: a top-level `pack.json`; no path
traversal (`..`, absolute paths, drive components); `MAX_PACK_BYTES = 16 MiB`; no shadowing a
built-in id; no downgrading an installed pack; and an optional `expected_sha256` verified against
`metalcraft_packs::canonical_sha256` — all checked before a single byte hits disk.

**Packs on master — seven, all first-party:**

| id | What it is |
|---|---|
| `metalcraft-calendar` | Calendars, events, guest invites; timezone-aware (`mcal_now`) |
| `metalcraft-contacts` | Address book / CRM, birthdays, tags |
| `metalcraft-notes` | Markdown notes with categories |
| `metalcraft-drive` | Drive-style file and folder store |
| `metalcraft-email` | The hosted read-only IMAP cache service |
| `metalcraft-packs` | Read-only discovery and search over the pack registry |
| `email` | Generic read-only IMAP against any provider — **native Rust tools**, not HTTP |

Third-party packs are not bundled. Thirteen of them — `calcom`, `cloudflare`, `s3`, `discord`,
`discord_admin`, `github`, `linear`, `railway`, `render`, `sentry`, `solarabase`, `sprite_builder`,
`starflask` — live in `metalcraft-agent-external-packs` and install on demand from the registry via
`POST /api/v1/integration-packs/install`.

**Native tools.** `native_pack_tool_names()` covers exactly two packs: `s3`
(`s3_list_buckets|list_objects|get_object|put_object|delete_object`, which need SigV4 request
signing) and `email` (`email_list_mailboxes|search|list_recent|get_message`, which speak IMAP).
Everything else is declarative JSON. A unit test, `native_tools_drift`, cross-checks every seeded
manifest against that map so enable/disable state can never diverge from the tools that actually
exist.

### 3.2 Declarative HTTP tools

`HttpApiToolConfig` (`src/tools/http_api.rs`): `name`, `description`, `method`, `url` (with `{param}`
placeholders), `headers`, `parameters` (JSON Schema), `body_mapping` (`params` default, plus
`template`, `params_nested`, `multipart`, `none`), `body_template`, `body_defaults`, `param_paths`,
`poll`, `multipart { file_param, file_field }`.

```json
{
  "name": "mcal_create_event",
  "method": "POST",
  "url": "https://calendar.metalcraftai.com/api/v1/calendars/{calendar}/events",
  "headers": { "Authorization": "Bearer $METALCRAFT_TOKEN", "Content-Type": "application/json" },
  "parameters": {
    "type": "object",
    "properties": { "calendar": {…}, "title": {…}, "starts_at": {…}, "ends_at": {…} },
    "required": ["calendar", "title", "starts_at", "ends_at"]
  },
  "body_mapping": "params"
}
```

`expand_env` scans header and URL values for `$WORD` tokens and resolves each through
`key_store::lookup`; unknown names expand to empty. URL `{param}` placeholders fill from arguments,
and `clean_unexpanded_placeholders` strips the leftovers from the query string so an unset optional
filter doesn't become `?name=`. Requests carry a 30-second timeout and responses are truncated at
50,000 characters.

### 3.3 The key store

`<data>/keys.json`, schema v2:

```json
{ "version": 2,
  "global":   { "OPENAI_API_KEY": "sk-…" },
  "channels": { "metalcraft": { "WEBHOOK_SECRET": "whsec_…" } } }
```

Pre-v2 flat files migrate into `global` transparently on load. Scope is `KeyScope::Global` or
`Channel(id)`. Masking renders values of 8 characters or fewer as `••••` and longer ones as
first-four…last-four; the raw value is only ever returned by `GET /api/v1/keys/{name}/reveal`.

Resolution is store-first, env-fallback — with one exception. `ENV_AUTHORITATIVE =
["METALCRAFT_TOKEN"]`: for that key alone a non-empty process-env value wins over the stored one,
because the control plane injects a fresh token into every pod and a stale token someone once pasted
into the key store must never shadow it. `lookup_scoped(channel, name)` layers channel scope on top:
channel → global → env, preserving the exception.

`GET /api/v1/keys/recommended` merges the `requires_env` of every *enabled* pack into a sorted list of
`{ name, configured, managed, packs[] }`, where `managed` is true only for `METALCRAFT_TOKEN`. The
agent-callable equivalents are `pack_list`, `pack_read` and `pack_enable`, which return the same
"still missing these keys" view so the agent can ask for what it needs and then set it with
`key_set`.

Secrets are deliberately *not* environment variables. Integration-pack credentials are stored at
runtime through the key store and referenced as `$NAME`, which is what makes a pack installable
without redeploying the container.

---

## 4. Messaging: channels and the gateway

### 4.1 The channel model

A channel is a named outbound connection plus an inbound route:

```rust
struct Channel { slug, name, url, enabled, managed,
                 integration_id, persona, model, active_number, connected }
```

There is always one managed channel, `DEFAULT_SLUG = "metalcraft"`, pointed at
`DEFAULT_GATEWAY_URL = "https://gateway.metalcraftai.com"` (override `METALCRAFT_GATEWAY_URL`). Its
secret is a live reference to `METALCRAFT_TOKEN` — never copied into the channel record — and it
can't be edited or deleted. Custom channels store only `{slug, name, url, enabled}` in
`<data>/channels.json`; their secrets live channel-scoped in the key store under `SECRET` and
`WEBHOOK_SECRET`, never in the channels file.

This is a substantial simplification of an earlier design that had channel *types* (JSON manifests)
and channel *instances*; both legacy artifacts are now migration-and-delete paths only
([§2.8](#28-state-files-not-a-database)).

### 4.2 Connecting

`POST /api/v1/gateway/metalcraft/connect` on the pod is zero-copy: using the pod's own
`METALCRAFT_TOKEN`, it fetches the base URL, integration id, webhook secret and active number from
the gateway, registers the pod's inbound webhook, and writes the channel-scoped secrets. Nothing is
pasted by hand.

A `heal_loop()` re-syncs every `METALCRAFT_GATEWAY_HEAL_SECS` (default 600). Separately, a failed
inbound signature check triggers an immediate reactive resync, rate-limited to once per 30 seconds —
a rejected HMAC is precisely the symptom of a rotated secret.

### 4.3 Inbound

The route is `POST /webhook/gateway`, public at the router layer and verified per request instead:
header `x-metalcraft-signature`, hex HMAC-SHA256 over the raw body, keyed by the resolved channel's
webhook secret, constant-time compared. Routing is by the gateway integration UUID (`source_id`)
matched against a channel's `integration_id` — not by URL path or phone number, so adding a channel
never means adding a route.

A long-poll pull transport also exists but is **off by default**, gated behind
`GATEWAY_INBOUND_PULL=1` for a dual-transport bake period. Both transports funnel into
`route_gateway_inbound` and share one dedup window: `src/inbound_dedup.rs` persists the most recent
`MAX_IDS = 2000` message ids at `<data>/inbound_dedup.json`, so dual delivery still runs the agent
once. Unknown or empty ids fail open and are processed.

Replies go out through the `say_to_user` and `gateway_send_message` tools, resolving the channel by
slug and `POST`ing to `{url}/api/v1/messages/send` with the channel secret as bearer, 30-second
timeout. Everything inbound and outbound is appended to `<data>/gateway_activity.jsonl` as a
`GatewayEvent` (bodies truncated at 500 chars), including "unrouted" records for messages that
matched no channel — surfaced by `GET /api/v1/gateway/activity`, which is what makes a
misconfigured integration id debuggable.

### 4.4 The gateway service

`metalcraft-gateway` is a wire-compatible replacement for the hosted PipeStreamr SMS/WhatsApp relay,
re-authenticated through Metalcraft ID and holding the Twilio credentials so consumers don't. For an
existing PipeStreamr consumer the migration is three environment variables.

Auth is the standard `Principal` — Bearer `mck_…` via the hub's `/verify`, or the `mc_session` cookie
verified offline against cached JWKS. `require_write()` gates mutations; `require_audience()` is
where audience enforcement is actually switched on (the inbound-pull routes require `gateway` in the
token's audience list).

**Pod connection is stateless proxying.** `src/controllers/agent.rs` never stores the pod↔gateway
link. Each call forwards the caller's own credential to the control plane to resolve the pod URL,
mints a short-lived `pod:{slug}`-scoped connection token, and then calls the *pod's* own
`/api/v1/gateway/metalcraft/{status,connect,disconnect}` with it. The pod remains the single source
of truth, so deleting a channel in the Workshop shows up here immediately with no drift.

Two inbound delivery modes exist on the gateway side. Push signs the body with
`gateway_sign(secret, body) = hex(HMAC-SHA256(...))` and delivers it to the consumer webhook. Pull —
the newer design — has the pod long-poll `GET /api/v1/agent/inbound/next?wait=25` (server cap 50s)
and acknowledge via `POST /api/v1/agent/inbound/ack`, with Postgres as the durable queue
(`received` → `delivered`), at-least-once semantics and pod-side idempotency on `external_id`. With
`QUEUE_BACKEND=redis` a Redis doorbell (`LPUSH`/`BRPOP`) replaces polling so Neon can autosuspend
even while a long-poll is held. Note the asymmetry worth tracking: the gateway has built the pull
path, while the agent still has it disabled by default.

Carrier-side, `twilio_validate` implements Twilio's own `X-Twilio-Signature` scheme —
base64(HMAC-SHA1(auth_token, url + sorted name+value concatenation)).

The gateway also runs an optional platform number pool. Claiming a number is premium-gated through
the hub's `/internal/membership`, and `src/premium.rs` reconciles membership periodically
(default daily) with a deliberate fail-safe: a number is released only on a definitive `premium:
false`, never on a hub error, so an outage can't mass-release everyone's numbers. `REPLY_WINDOW_SECS`
(default 86,400) enforces a reply-only guard on shared numbers — which conveniently coincides with
WhatsApp's 24-hour service window — and STOP/START/HELP opt-out handling is enforced gateway-side for
both SMS and WhatsApp.

---

## 5. The auth ecosystem

### 5.1 metalcraft-id — the identity hub

`id.metalcraftai.com`. Rust + Axum + sqlx against Neon Postgres, RS256 via `jsonwebtoken`, with a
React SPA served from the same origin. Sign-in is Google OAuth only, using identity-only scopes
(`openid email profile`) so the consent screen carries no verification warning; `return_to` is
validated against `*.metalcraftai.com`, localhost, or `ALLOWED_RETURN_HOSTS`.

Two credential types, never mixed:

| | Session (`mc_session`) | PAT (`mck_…`) |
|---|---|---|
| Format | RS256 JWT, `kid` in header | `mck_` + 32 random bytes hex |
| Cookie attrs | `Domain=.metalcraftai.com`, HttpOnly, Secure | n/a |
| Verified by | JWKS, offline | `POST {hub}/verify` |
| Stored server-side | nothing | SHA-256 hash only (`token_hash`) |
| Claims/fields | `iss, sub, email, name?, admin, iat, exp` | resolved server-side |
| Scope semantics | `scopes == None` → full-access owner | `Some([...])` → limited |
| Revocation | short TTL + re-login | instant (DB flag) |

Signing keys load from `JWT_PRIVATE_PEM` (tolerant of env-mangled newlines and quotes). If unset, an
ephemeral 2048-bit key is generated at boot — dev only, since every session and the JWKS reset on
restart. `kid` is the first 8 bytes of SHA-256 over the base64 modulus.

**PAT kinds** (`personal_access_tokens.kind`, `CHECK (kind IN ('user','pod','connection'))`):

| kind | Minted by | Lifetime | Audience |
|---|---|---|---|
| `user` | `/account`-style self-service; also the device flow | 30 days default (device flow: 90) | usually unscoped |
| `pod` | `/internal/tokens`, service-secret gated | **no expiry** — revoked, never rotated | unscoped, named `pod:{slug}` |
| `connection` | `/apps/token`, or `/internal/tokens` with `ttl_secs` | 3600s | scoped, e.g. `["pod:{slug}"]` |

**Scopes** are exactly `read`, `write`, `admin`; `write` implies `read`; `admin` is only mintable
through the hub's own admin endpoint, never the self-service page. `SUBAPP_STANDARD.md` states the
rule directly: do not invent `<app>:write` scopes, and there are no god-scoped user tokens.

**Audience scoping** (`aud TEXT[]`, migration 009). `NULL` means valid for every audience — the
back-compat default. A populated list restricts the token to exactly those free-form audience strings
(`pod:andrew-a1b2c3`, `gateway`, `app:contacts`). Critically, **the hub does not enforce audience** —
it only returns `aud` from `/verify`. Enforcement is opt-in per relying party, and each one has
chosen differently: the agent requires `pod:{slug}` membership, the gateway's `require_audience()`
passes unscoped tokens and checks containment otherwise, and the control plane rejects *any*
audience-scoped token outright.

**`/verify`** returns `{ active, sub?, email?, scopes?, aud?, expires_at?, premium }` — `premium` is
always present, account-level, and cached. Results are cached in Redis under `mid:verify:tok:{hash}`
with `VERIFY_CACHE_SECONDS` (default 86,400 — a full day). That long TTL is safe because every
mutating event does an explicit `DEL`, and `expires_at` plus `current_period_end` are re-checked live
on each cache hit. If Redis is missing or unreachable the code falls through to Postgres uncached and
never fails to boot.

**Service-to-service authority.** `X-Metalcraft-Service-Secret`, constant-time compared against
`METALCRAFT_SERVICE_SECRET`, failing closed when unset. It gates `/internal/tokens`,
`/internal/tokens/revoke`, `/internal/membership` and the entire `/credits/*` ledger. This is the
only privileged cross-account mechanism that exists.

A narrower delegation primitive sits alongside it: `POST /apps/token` takes a registered app's
`X-App-Secret` plus the acting user's own credential and returns a `connection` PAT scoped
`aud=["app:{slug}"]` with a 3600s TTL. Scopes never escalate beyond the acting credential's own, and
the audience is derived from the secret rather than from request input. This is what lets Drive hand
Photos a token to read one user's files.

**Device pairing** (`device_logins`, migration 008) covers native clients that can't register a
redirect URI. `POST /auth/device/start` returns `{device_code, user_code, verify_url, interval_secs:
2, expires_at}` with a 10-minute TTL; the user approves at `GET /device?uc=`, bouncing through Google
if needed; `POST /device/approve` mints a 90-day `read+write` PAT and parks the raw value in
`device_logins.token_raw`; `POST /auth/device/poll` returns it exactly once, then nulls the column.
The high-entropy `device_code` never appears in a URL — only the short `user_code`, which is useless
without an authenticated approval.

**Endpoints** (`build_router`): `/health`, `/ready`, `/api/account`, `/api/onboarding`,
`/api/tokens` (POST), `/api/tokens/{id}/revoke`, `/api/admin/tokens`, `/api/redeem[/{code}]`,
`/api/admin`, `/api/admin/promo`, `/api/admin/trial-codes[/{id}/revoke]`, `/auth/google[/callback]`,
`/auth/logout`, `/auth/device/{start,poll}`, `/device`, `/device/approve`,
`/billing/{checkout,portal,webhook}`, `/me`, `/verify`,
`/credits/{balance,authorize,settle,refund,grant}`, `/apps/token`, `/internal/tokens`,
`/internal/tokens/revoke`, `/internal/membership`, `/.well-known/jwks.json`, plus the SPA fallback.

**Schema highlights.** `users(id, google_sub, email, name, avatar_url, created_at)`;
`personal_access_tokens(id, user_id, name, token_hash, token_prefix, scopes[], expires_at, last_used,
revoked_at, created_at, kind, aud[])`; billing (`stripe_customers`, `subscriptions`, `app_settings`,
`promo_redemptions`); credits (`credit_accounts` in micro-credits, append-only `credit_transactions`
idempotent on `(service, ref)`, `credit_holds` with `held|settled|released`); `trial_codes` and
`premium_grants` — premium is always *derived* (an active subscription or an unexpired grant), never
a column on `users`; and `device_logins`. Migrations run 001, 002, 005–009; **003 and 004 don't exist
in the repo** — presumably squashed early, but unverified.

### 5.2 The Subapp Standard

There is **no shared auth crate**. A dependency scan across all five service repos finds no
workspace-internal or `path = "../…"` dependency; each pulls `jsonwebtoken`, `sha2` and `hex`
straight from crates.io and reimplements its own `Principal` extractor, JWKS cache and introspection
client.

That is the documented pattern, not an oversight — `SUBAPP_STANDARD.md` tells new subapps to clone
the calendar app's spine, which keeps every service a standalone Railway deployable with its own
Cargo.toml and Dockerfile. The cost is real though: each copy has drifted to add local extensions
(on-behalf headers in inference, audience rejection in the control plane, `require_audience` in the
gateway), so a change to a hub-side auth semantic — a new `aud` convention, say — has to be
propagated by hand across at least four independently maintained implementations.

What genuinely *is* shared is the vocabulary, treated as a protocol: the `mck_` prefix, the
`mc_session` cookie, `X-Metalcraft-Service-Secret`, `X-Metalcraft-Act-As`, the `pod:{slug}` and
`app:{slug}` audience conventions, and the three scope names.

### 5.3 How a pod authenticates its callers

`src/hub_auth.rs`. The hub URL resolves as `METALCRAFT_ID_URL` (key store) → `HUB_INTERNAL_URL`
(env) → `https://id.metalcraftai.com`. The pod's own slug is the first DNS label of
`POD_PUBLIC_URL`; when that's unset (dev, standalone) audience-scoped acceptance is simply
unavailable.

```
accept if token starts with "mck_" AND /verify says active AND (
      aud is a non-empty list containing "pod:{slug}"      // minted for this pod
   OR aud is null/empty AND sub == owner_sub               // a broad PAT, but the owner's
)
```

`owner_sub` is learned once by the pod self-verifying its own `METALCRAFT_TOKEN` and cached in a
`OnceLock`; an unlinked pod therefore rejects all unscoped tokens. Positive verifications are cached
for 60 seconds keyed by SHA-256 of the token. Any transport or parse error rejects.

The effect is a clean two-way property: a broad user PAT reaches its owner's pod and nobody else's,
and a connection token reaches exactly the one pod it was minted for.

Outbound, the managed channel resolves its credential as adopted connection token
(`lookup_scoped("metalcraft", "SECRET")`) → the pod's broad `METALCRAFT_TOKEN`.

### 5.4 metalcraft-inference

`inference.metalcraftai.com` — an OpenAI-compatible router that meters credits. Pointing an agent's
`OPENAI_BASE_URL` here turns "bring your own OpenAI key" into authenticated, metered inference.

Auth is the standard `Principal` requiring the `write` scope (spending is a write; owner sessions
always pass), and non-premium principals get a `402` regardless of credential type.

For trusted first-party callers there's an on-behalf mode: `X-Metalcraft-Service-Secret` plus
`X-Metalcraft-Act-As: <user-uuid>` (and optionally `X-Metalcraft-Act-As-Email`) lets, say,
`images.metalcraftai.com` spend a named user's credits without holding that user's credential. The
secret alone is the trust boundary, so an on-behalf principal is treated as premium unconditionally
and carries `on_behalf: true`.

Credits move only through the hub: `authorize(user, amount, ref_id)` → `settle(ref_id, amount)` →
`refund(ref_id)`, each a service-secret-authenticated call to `/credits/*`, each idempotent on
`ref_id` (the request id). A provider failure refunds fire-and-forget. Amounts are integer
micro-credits — 1 credit = 1000 µc ≈ $0.001 — priced per model as `input_credits_per_1k` /
`output_credits_per_1k` for chat and `per_unit_credits × unit` for media. Charges come back inline as
`usage.credits` plus an `x-metalcraft-credits` header.

Endpoints: `/health`, `/ready`, `/login`, `/config`, `/account/{me,usage,settings}`,
`/admin/models[/delete]`, `/admin/credits/grant`, `/v1/models`, `/v1/whoami`, `/v1/chat/completions`,
`/v1/responses`, `/v1/fal/run`. (`/v1/responses` — OpenAI Responses API compatibility — exists on
mainline but is missing from the README's endpoint table.)

### 5.5 metalcraft-k3-cluster — the control plane

`pods.metalcraftai.com`, crate `cluster-backend`, running on DO App Platform and driving a DOKS
cluster through `KUBE_CONFIG_BASE64`. This — not `metalcraft-coordinator` — is the platform's control
plane.

Provisioning (`provision.rs`): get-or-create the user's single `pods` row → if `mc_token_pat_id` is
already set, treat it as provisioned and just converge the workload → otherwise mint a pod token
(`kind=pod`, `scopes=["write"]`, no expiry) via the hub's `/internal/tokens` → build the Kubernetes
secret `mck-{slug}-env` → apply PVC, StatefulSet, Service and Ingress → persist the PAT **id** (never
the raw token) on the row. If the workload apply fails after minting, the fresh token is revoked
before returning the error, so no orphan tokens accumulate.

One detail worth highlighting: the secret sets `METALCRAFT_TOKEN` **and** `OPENAI_API_KEY` to the
same minted token, because the StatefulSet also sets `OPENAI_BASE_URL` to
`metalcraft-inference`. Pods never hold a real OpenAI key.

`reconcile::decide` is a pure function from `(status, premium, workload_exists)` to an action:

| Condition | Action |
|---|---|
| Active/Pending, not premium | **Suspend** — revoke token, scale to 0, keep the PVC |
| Suspended, premium | **Restore** — mint a fresh token, scale up |
| Active, premium, workload missing | **Reprovision** — heal a ghost row |
| Active, premium, workload present | **Converge** — declarative re-apply, no-op if unchanged |

The sweep runs every `RECONCILE_SECONDS` (default 300; `0` disables) plus on demand after
provisioning actions, and a per-pod failure is skipped rather than aborting the sweep.

Connection tokens come from two routes. `POST /api/pods/{slug}/connection/mint` is the general
primitive: any owner-authenticated caller gets a 3600-second token with audience exactly
`["pod:{slug}"]` — this is what the desktop Workshop, the web Workshop and the gateway proxy all use.
`POST /api/pods/{slug}/connection/refresh` is a legacy path for pods that once adopted a connection
token as their outbound gateway credential; it mints audience `["pod:{slug}", "gateway"]` and is
described in code as draining to zero as pods roll forward.

And the control plane **refuses audience-scoped tokens for itself**: if `/verify` returns a non-empty
`aud`, the `Principal` extractor 401s with "audience-scoped token is not accepted by the control
plane." A leaked pod-connection token therefore cannot be replayed against `/api/pods` for its whole
lifetime. The one legitimate pod→control-plane call uses the pod's own unscoped `METALCRAFT_TOKEN`.

Storage is deliberately thin: `users` (a mirror of hub identity), `pods(id, user_id, slug UNIQUE,
status CHECK(pending|active|suspended|deleted), mc_token_pat_id, pvc_name, …)` with a
one-pod-per-user partial unique index, and `app_settings`. Migration 002 adds an encrypted-at-rest
`workshop_key_enc` (XChaCha20-Poly1305) for a legacy static per-pod API key; current `pods.rs`
docstrings describe pod API auth as OIDC-only, so that column looks vestigial — flagged rather than
asserted, since the code path wasn't traced end to end.

### 5.6 metalcraft-coordinator

The cross-tenant relay — the small piece of state a single-tenant pod structurally cannot own.

**On `main` there is exactly one commit**, and it is worth being precise about what that includes,
because three further capabilities exist only on feature branches. Mainline is: one table,
`shares(token PK, pod_slug, kind DEFAULT 'note', ref, created_at)`; three routes plus health —
`POST /api/v1/shares` (upsert), `DELETE /api/v1/shares/{token}`, and a **public**
`GET /p/{token}` that proxies to `https://{pod_slug}.{PODS_DOMAIN}/apps/metalcraft-notes/p/{token}`.
Writes are gated by `x-metalcraft-service-secret`, failing closed when unconfigured; there is no PAT
or cookie path at all on main, because the coordinator only ever authenticates pods, never end users.
It stores routing rows and never user content.

Calendar external invites, RSVP, email sending via Resend, Drive public links and invite revocation
are all on branches (C2–C4), not on `main`.

---

## 6. The cloud apps

| Repo | Purpose | Stack | Kind | Routes | Auth | Storage | Pack |
|---|---|---|---|---|---|---|---|
| `metalcraft` | The framework crate: typed cyclic-graph orchestrator with checkpointing and interrupts | Rust, crates.io | library | — | — | — | — |
| `metalcraft-flows` | Flow DAG wire-format spec | Rust crate | library | — | — | — | — |
| `metalcraft-packs` | Pack manifest spec + canonical hash | Rust crate | library | — | — | — | — |
| `metalflow` | Vendor-neutral JSON Schema spec for personas + flows | JSON Schema + docs | spec | — | — | — | — |
| `metalcraft-calendar` | Calendars by slug, two-way Google Calendar sync | Axum + sqlx + React SPA | service | `/api/v1` | session / PAT | Neon | ✅ |
| `metalcraft-contacts` | Agent-first address book, birthdays, tags | Axum + React | service | `/api/v1` | session / PAT | Neon + DO Spaces (photos) | ✅ |
| `metalcraft-notes` | Markdown notes, BlockNote editor, public shares | Axum + React | service | `/api/v1` | session / PAT | Neon | ✅ |
| `metalcraft-drive` | Personal file store, presigned upload, proxied download | Axum + React | service | `/api/v1` | session / PAT | Neon + Cloudflare R2 | ✅ |
| `metalcraft-email` | Read-only IMAP cache and index (never sends) | Axum + React | service (scaffold) | `/api/v1` | session / PAT | Neon | ✅ |
| `metalcraft-photos` | Photo library indexed out of Drive | Axum + sqlx | headless service | `/v1` | delegated app tokens | Neon (derived only) | — |
| `metalcraft-music` | Music index / likes / playlists for the iOS app | Axum + sqlx | headless service | `/v1` | JWKS / PAT | Neon (derived only) | — |
| `metalcraft-meet` | Self-hosted WebRTC meetings | Axum + React + coturn | service | `/api/v1`, `/ws/room/{id}` | session / PAT, guest `invite_token` | Neon | ✗ (README claims `mmeet_*`) |
| `metalcraft-code` | Remote coding workspaces on sprites.dev | Axum + React + `codeworker` | service | `/api/v1` | session / PAT + GitHub App | Neon + R2 | ✗ (README claims `mcode_*`) |
| `metalcraft-images-web` | Prompt → image over inference credits | Axum + React | service | `/api/v1` | session / PAT, premium | Neon + R2 | — |
| `metalcraft-flows-web` | Registry: browse / publish / install flows | Axum + React + `@xyflow/react` | registry | `/api/v1` | session / PAT; admin publish | Neon (JSONB) | — |
| `metalcraft-packs-web` | Registry: browse / submit packs | Axum + React | registry | `/api/v1` | public read; `packs:write` to submit | Neon + `pack_files` BYTEA | ✅ |
| `metalcraft-workshop` | Desktop app: author personas/skills/flows, drive pods | Rust + Tauri + React | desktop | — | OIDC → per-pod connection token | local FS | — |
| `metalcraft-workshop-web` | Browser Workshop — stateless pod proxy | Axum + React | frontend | `/api/pod/{*rest}` | session / PAT | **none** | — |
| `metalcraft-ai-web` | metalcraftai.com marketing + docs | Axum + React | frontend | `/healthz` | — | — | — |
| `metalcraft-mobile` | iOS client: sign in, today's calendar, push | SwiftUI | mobile | — | PAT in Keychain | — | — |
| `metalcraft-design-guide` | Shared tokens + Tailwind preset + React components | CSS/JSON + React | design system | — | — | — | — |
| `metalcraft-agent-external-packs` | Canonical source of the 13 third-party packs | JSON + scripts | content | — | admin PAT to publish | — | — |
| `metalcraft-ecosystem` | Local compose harness for hub + calendar SSO | docker-compose | tooling | — | — | — | — |
| `metalcraft-events` | Meetup-style groups/events/RSVP | `DESIGN.md` only | design | — | — | — | — |

Four repos have **no git history at all** and were read from the working tree: `metalcraft-photos`,
`metalcraft-music`, `metalcraft-events`, `metalcraft-cf-cluster` and `metalcraft-do-cluster`.

Details worth knowing:

- **Calendar** is tenanted on *a calendar*, not a user: `/api/v1/calendars`,
  `/calendars/{slug}/events`, `/google/calendars`, `/google-link`. Writes require the `write` scope.
  Its Google OAuth covers calendar access only, since login is the hub's job.
- **Contacts** leads with `GET /upcoming-birthdays?within=&tag=` — a year-agnostic days-until sort,
  which is exactly the shape an agent reminder needs rather than a raw date list.
- **Notes** keeps markdown as the source of truth under a block editor, with full-text search and
  public read-only share links at `/p/{token}` — the links the coordinator routes.
- **Drive** is the storage backbone the other apps build on: single-owner, no sharing in v1, uploads
  going **direct to R2** by presigned PUT (`/files/presign` → `/files/confirm`) and downloads
  **proxied** with Range support so objects stay private but remain seekable and streamable.
- **Email** is an E1 scaffold on mainline — it boots and serves `/whoami`, and the IMAP ingestion,
  mailbox schema and REST API are still ahead of it. The design is `EXAMINE`-only and explicitly
  never sends mail.
- **Photos and Music** are sibling derived-data services with no UI of their own. They store only
  metadata keyed on content SHA-256 and stream bytes from R2 through Drive's signed URLs using
  delegated tokens. Both break convention by serving at `/v1` rather than `/api/v1`.
- **Meet** runs its own WebRTC mesh — no Zoom, LiveKit or Daily — with signaling over
  `/ws/room/{room_id}`, a host-controlled lobby for guests, delegated-token mirroring into Calendar,
  and Resend for invites. Room size stays capped until an SFU is built.
- **Code** provisions ephemeral sprites.dev workspaces and issues per-repo GitHub App installation
  tokens rather than long-lived credentials, split across three binaries (API, `codeworker`
  provisioner, `migrate`).
- **Images-web** is a thin product layer: it never calls fal.ai or touches credits itself, it calls
  inference's `/v1/fal/run` on-behalf-of and then persists results to its own R2 bucket, because the
  provider's URLs expire.
- **Workshop-web** is deliberately stateless — explicitly no database — caching a per-pod credential
  in memory and streaming SSE straight through `/api/pod/{*rest}`.
- **Flows-web and packs-web** are architectural twins: Axum lib plus `migrate`/`main` binaries, sqlx
  against Neon, raw-SQL migrations, one binary serving both API and embedded SPA, admin-gated
  publishing, and installation into a pod through the control-plane proxy.

**`metalcraft-ecosystem`** is a one-command local harness: the hub on `:9200` and Calendar on
`:9100`, each with its own Postgres, migrations run as one-shot jobs gated on database health, using
sibling checkouts as build contexts. Its README calls out the wiring that trips everyone up once —
`HUB_BASE_URL=http://localhost:9200` for the browser's login redirect versus
`HUB_INTERNAL_URL=http://id:9200` for the calendar container's JWKS and `/verify` calls, because
`localhost` inside a container means the container itself. In production both are just
`https://id.metalcraftai.com`.

---

## 7. Deployment topology

Production today, from `metalcraft-k3-cluster/ARCHITECTURE.md`:

| Component | Runtime | Origin | Datastore |
|---|---|---|---|
| metalcraft-id (hub) | Railway | `id.metalcraftai.com` | Neon |
| k3-cluster control plane | DO App Platform, driving DOKS | `pods.metalcraftai.com` | Neon + optional Upstash Redis |
| metalcraft-agent (pod) | DOKS StatefulSet, one per premium user, ns `metalcraft-pods` | `<slug>.pods.metalcraftai.com` | per-pod PVC at `/data` |
| metalcraft-gateway | Railway | `gateway.metalcraftai.com` | Neon + Upstash Redis |
| metalcraft-inference | inference gateway | `inference.metalcraftai.com` | — |
| metalcraft-workshop-web | Railway | `workshop.metalcraftai.com` | none |
| all other subapps | Railway (Dockerfile, auto-deploy on default branch) | `*.metalcraftai.com` | one Neon DB each |

DNS and TLS come from a wildcard ingress on `*.pods.metalcraftai.com`. Thin clients — the desktop
Workshop, the web Workshop, the gateway site — are pure reverse proxies to the pod's own API, which
is what keeps "one authoritative owner per fact" true in practice.

`ENABLE_METALCRAFT_PACKS=1` auto-enables every pack tagged `metalcraft-ecosystem` on a pod's first
boot only, guarded by a one-shot marker on the PVC. That's the control-plane mechanism behind a new
pod arriving with its first-party packs already on.

Two successor designs exist for replacing the DOKS layer; **neither has git history and neither is
deployed**. Both chase the same goal — Cloudflare Workers, Durable Objects and Containers so an idle
pod costs nothing instead of holding a StatefulSet on a reserved node pool.

- **`metalcraft-do-cluster`** is the spike with real code: `worker.ts` as a front door, `agent-do.ts`
  and `session-do.ts` Durable Objects, a container wrapping a `metalcraft-agent-r2` fork that uses
  SQLite + Litestream to R2, D1 migrations, `wrangler.jsonc`. Self-labeled "M0 spike scaffold (not
  deployed, not built)" with unfilled placeholders and an open go/no-go checklist on durability,
  cold-start latency and RSS sizing.
- **`metalcraft-cf-cluster`** is design-only — a single `PLAN.md`, explicitly "not a repo scaffold."
  It supersedes and renames the spike (whose "DO" reads as DigitalOcean rather than Durable Objects)
  and differs in one deliberate way: it does **not** fork the agent image, demoting the
  SQLite+Litestream variant to an optional v2 hardening track rather than a v1 requirement.

---

## 8. Doc drift on mainline

Committed docs versus committed code, mainline only. Each is a small, contained cleanup.

1. **The channel architecture was replaced, but README and `docs/architecture.md` still describe the
   old one** — channel *types* as JSON manifests plus channel *instances*, inbound webhooks at
   `/webhook/<adapter>` (naming `/webhook/pipestreamr` and `/webhook/twilio`), and "two channel types
   ship." Master has a flat `{slug, url, secret}` channel model, one `POST /webhook/gateway` route,
   and per-channel HMAC. This is documentation for a *removed* subsystem, not a lag on an active one:
   `seed::retire_dir()` deletes the old type-manifest directory at boot and `metalcraft_gateway.rs`
   carries one-time migration code for leftover instances. The drift reaches into the binary — the
   daemon's own `--help` text still mentions `/webhook/<adapter>` — and dead `seed/gateway_channels/`
   manifests still ship.
2. **The README's shipped-packs list names 17 packs; `seed/` contains 7.** `docs/architecture.md`
   separately claims sixteen, including a `vestaloop` that appears nowhere. Because packs are embedded
   with `include_dir!` at compile time, anything absent from `seed/` cannot ship — these aren't
   "coming soon," they're gone (most are installable from the registry, which is a different claim).
   `approval.rs` still carries classification arms for `discord_*` and `calcom_*` tools with no seeded
   pack behind them.
3. **README's project structure names `src/gateway_channels.rs`,** which doesn't exist on master. The
   file is `src/channels.rs`.
4. **`docs/architecture.md` points at `<data>/gateway_channels.json`** as the live channel store; the
   live store is `<data>/channels.json` and the former is migration-only.
5. **`docs/FLOWS_ARCHITECTURE.md` still describes v2 pause/resume as "(planned)."** It is fully
   shipped: `run_approval` and `run_wait`, `FlowRun` persistence, daemon auto-resume, and
   `POST /api/v1/flow-runs/{run_id}/resume`.
6. **`src/flow_exec.rs`'s own module doc contradicts the code beneath it** — it says `branch`, `http`,
   `sub_agent`, `approval`, `wait` and `foreach` all return "not yet implemented," while the match
   arms implement everything except `foreach`. `docs/overview.md` and `docs/architecture.md` repeat
   the same stale staging.
7. **`docs/FLOW_SCHEDULES_PLAN.md` proposes a `PATCH /api/v1/flows/{id}/schedules/{sid}`** that was
   never built; editing one schedule requires a whole-array `PUT`. The plan also proposes a per-flow
   `running: HashSet` concurrency lock that the daemon doesn't need, since triggers run sequentially
   within a tick.
8. **`src/tools/twilio.rs`'s module doc claims a `/webhook/twilio` route.** No such route is
   registered; `session_io.rs` correctly calls this the dormant Twilio path.
9. **`docs/SCOPED_KEYS_PLAN.md` and `docs/METALCRAFT_GATEWAY_CONNECT_PLAN.md`** describe an
   intermediate design with per-channel `PIPESTREAMR_*` key names, superseded by the generic
   `SECRET` / `WEBHOOK_SECRET` constants in `channels.rs`.
10. **In the gateway, `X-PipeStreamr-Signature` survives only in comments.** The code sends and the
    agent verifies `x-metalcraft-signature`; `services/signing.rs` and `controllers/inbound.rs` both
    still document the old header name alongside the new one.
11. **Cross-repo README claims about agent packs are stale in both directions.** `metalcraft-meet`
    documents an `mmeet_*` pack and `metalcraft-code` an `mcode_*` pack, neither of which is seeded;
    `metalcraft-calendar` describes its own pack as "next" when `metalcraft-calendar` has shipped for
    a while.
12. **`metalcraft-inference`'s README endpoint table omits `/v1/responses`,** which exists on
    mainline.
13. **Structural, not drift, but worth a test:** the agent's HTTP API auth middleware protects only
    routes registered before the `.layer()` call, so a route added in the wrong place is silently
    public. That is currently correct for `/health`, `/`, the OpenAPI docs and the HMAC-verified
    webhook — and entirely invisible to a reviewer reading a diff.

---

## 9. Repo index

**Core runtime** — `metalcraft` (framework crate) · `metalcraft-agent` (the pod) ·
`metalcraft-flows`, `metalcraft-packs`, `metalflow` (spec crates) · `metalcraft-workshop`,
`metalcraft-workshop-web` (authoring) · `metalcraft-agent-external-packs` (third-party packs)

**Identity and platform** — `metalcraft-id` (hub) · `metalcraft-gateway` (messaging) ·
`metalcraft-inference` (credit-metered LLM) · `metalcraft-k3-cluster` (control plane) ·
`metalcraft-coordinator` (cross-tenant relay)

**Apps** — `metalcraft-calendar` · `-contacts` · `-notes` · `-drive` · `-email` · `-photos` ·
`-music` · `-meet` · `-code` · `-images-web` · `-events` (design only)

**Registries and web** — `metalcraft-flows-web` · `metalcraft-packs-web` · `metalcraft-ai-web` ·
`metalcraft-mobile` · `metalcraft-design-guide`

**Infrastructure** — `metalcraft-do-cluster` (spike) · `metalcraft-cf-cluster` (plan) ·
`metalcraft-ecosystem` (local compose harness)
