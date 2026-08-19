# Agent Presets, Agent Instances & Agent Packs — Implementation Plan (master)

> Three new concepts in `metalcraft-agent`, and one change to what "install" means.
>
> - **Agent preset** — what you pick when you start a chat, instead of a persona. Names a
>   default persona plus the roster it can call, the skills and integration packs they need,
>   and a bundle of seed memories. Default: **General Agent** (`orchestrator-agent`).
> - **Agent instance** — a live agent. Holds its own memory and **many conversations**. Created
>   from a preset; the preset is fixed for its life.
> - **Agent pack** — the distribution unit. A zip of one or more presets plus **every** persona,
>   skill, and integration pack they require. **Agent packs are what get installed and
>   uninstalled.** Integration packs stop being an independent install unit.
>
> Targets `master` (`0.29.0`, `Cargo.toml:3`). Breaking; ships as **`0.30.0`**.
> Companion doc: `axoniac-prime/PLAN.md`.

---

## 0. The shape

```
                     installed / uninstalled as one unit
        ┌──────────────────── Agent Pack ─────────────────────┐
        │  agent_pack.json                                     │
        │  agent_presets/amy-kitchen.json  + memories/         │
        │  personas/{amy, amy-shopper, amy-critic}.json        │
        │  skills/{knife-skills, menu-planning}.md             │
        │  integration_packs/{metalcraft-calendar, instacart}/ │
        └──────────────────────────────────────────────────────┘
                                  │ install
                                  ▼
                    Agent Preset  "amy-kitchen"
              default_persona: amy · roster: [amy-shopper, amy-critic]
                                  │ instantiate
                     ┌────────────┴────────────┐
                     ▼                          ▼
             Agent Instance                Agent Instance
             "Amy"  (SMS channel)          "Sunday prep"
             own memory ──────────┐        own memory
             ├─ conversation      │        ├─ conversation
             ├─ conversation      │        └─ conversation
             └─ conversation      │
                                  └── memory persists across all of them
```

**A pack ships presets. A preset is a template. An instance is a running agent that
remembers. A conversation is one thread with it.**

### 0.1 Why this is smaller than it looks

Almost every piece exists. A preset is a *composition object over existing primitives* — it
references personas by slug, and `Persona` already carries `tools`, `packs`, `skills`, and
`system_prompt` (`src/persona.rs:5-26`). `Persona.packs` already means "adopt every tool from
these integration packs" (`:11-16`, resolved at `:144`). Sub-agent delegation by persona
already exists (`src/tools/sub_agent.rs`). Conversations already persist (`<data>/chats/`,
`src/workshop_api.rs:1948` `persist_chat`).

Genuinely new: the preset object and its resolution, the instance record and per-instance
memory, and the agent-pack installer. The rest is renaming and rewiring.

### 0.2 Naming discipline

| Term | On disk | In Rust | Tools |
|---|---|---|---|
| agent pack | `<data>/agent_packs/<id>/`, `agent_pack.json` | `AgentPackManifest` | `agentpack_*` |
| agent preset | `agent_presets/<slug>.json` | `AgentPreset` | `preset_*` |
| agent instance | `<data>/agent_instances/<id>/instance.json` | `AgentInstance` | `instance_*` |
| conversation | `…/<id>/conversations/<cid>.json` | `Conversation` | `conversation_*` |
| integration pack | `integration_packs/<id>/`, `pack.json` | `PackManifest` | `pack_*` (read-only) |

Never write bare "pack".

---

## 1. Agent preset

### 1.1 The artifact

One JSON file plus a sidecar directory — shaped like a persona, not a tree, because it is a
*manifest of references*, not a container.

```
agent_presets/amy-kitchen.json
agent_presets/amy-kitchen/memories.jsonl      seed memories (§3)
agent_presets/amy-kitchen/vectors.bin         optional precomputed embeddings (§3.5)
agent_presets/amy-kitchen/avatar.png
```

```json
{
  "manifest_version": 1,
  "slug": "amy-kitchen",
  "name": "Amy's Kitchen Agent",
  "tagline": "Cooks like Amy. Shops like Amy. Nags you about mise en place.",
  "description": "…",
  "avatar": "agent_presets/amy-kitchen/avatar.png",

  "default_persona": "amy",
  "personas": [
    { "slug": "amy",          "role": "default" },
    { "slug": "amy-shopper",  "role": "subagent", "description": "Builds and places grocery orders" },
    { "slug": "amy-critic",   "role": "internal"  }
  ],

  "skills": ["knife-skills", "menu-planning", "substitutions"],
  "integration_packs": ["metalcraft-calendar", "instacart"],

  "memories": { "file": "agent_presets/amy-kitchen/memories.jsonl", "count": 214,
                "embed_model": "text-embedding-3-small", "dims": 384 },

  "model": { "tier": "standard", "needs": ["tool_calling"], "min_context": 128000,
             "prefer": "gpt-5.4" },
  "requires_env": ["METALCRAFT_TOKEN", "INSTACART_TOKEN"],
  "version": "1.4.0"
}
```

- **`personas[].role`** — `default` (what a new instance starts as), `subagent` (offered to
  `sub_agent`'s persona mode), `internal` (callable only from within this preset; never in a
  picker). This is "a default persona and others it can call upon", made explicit — and it
  becomes the **enum that scopes `sub_agent`**, which today can reach any persona on the pod.
- **`skills` / `integration_packs`** — declared requirements, validated at install: every
  listed persona, skill, and pack must be present in the agent pack or already on the pod, or
  the install fails naming what's missing.
- **`model` is a capability floor, not a model name.** A hard `"gpt-5.4"` breaks on a pod that
  doesn't have it, and axoniac's old `inference_source` had the same problem. Declare what the
  agent *needs* (`tool_calling`, context size, a coarse `tier`) plus a non-binding `prefer`;
  the pod maps that onto what it has, including `metalcraft-inference`. A pod that can't meet
  the floor warns at install and still installs.

### 1.2 Storage & resolution

```
<data>/agent_presets/<slug>.json           user-authored (top precedence)
<data>/agent_packs/<pack>/agent_presets/   pack-provided
```

Layered exactly like personas today (`src/integration_packs.rs:512-610` `resolve_file` /
`list_files_layered`) — a new `subdir` argument to machinery that already exists — plus a real
fix: **collisions error, never shadow.**

**The `AmbiguousSlug` rule applies to presets, personas, *and* skills.** Skills matter most
here: the first-party seeds already ship `planning.md`, `summarize.md`, `memory.md`, and two
packs shipping `planning` is likely rather than hypothetical. A bare slug defined by exactly
one installed pack resolves; by two, errors naming both qualified ids
(`amy-kitchen-agent/planning`); user-local always wins over both.

### 1.3 The default preset

```json
{
  "slug": "general-agent",
  "name": "General Agent",
  "default_persona": "orchestrator-agent",
  "personas": [
    { "slug": "orchestrator-agent", "role": "default" },
    { "slug": "coding-agent",   "role": "subagent" },
    { "slug": "research-agent", "role": "subagent" },
    { "slug": "devops-agent",   "role": "subagent" },
    { "slug": "config-agent",   "role": "subagent" },
    { "slug": "workshop-agent", "role": "subagent" }
  ]
}
```

`runtime::DEFAULT_PERSONA = "orchestrator-agent"` (`src/runtime.rs:27`) becomes
`DEFAULT_PRESET = "general-agent"`; its duplicated fallbacks (`src/main.rs:181`,
`src/workshop_api.rs:3783`) collapse onto one resolver. **A pod with nothing installed behaves
exactly as 0.29 does** — the migration's acceptance test.

---

## 2. Agent instance

### 2.1 An instance owns conversations

An instance is **the agent**: a preset, a name, and a memory. A conversation is one thread with
it. This is the distinction that makes a long-lived agent possible — today a gateway session
resets its whole state on idle (`src/workshop_api.rs:4253` sets `s.state = None`,
`DEFAULT_GATEWAY_SESSION_TTL_SECS` at `:3951`), so if an instance were a single conversation,
Amy would forget you between text messages. Instead the idle reset ends a *conversation*; the
instance and its memory carry on.

```
<data>/agent_instances/<id>/
  instance.json
  conversations/<conversation-id>.json
```

```json
// instance.json
{
  "id": "inst_01J…",
  "preset": "amy-kitchen",
  "agent_pack": "amy-kitchen-agent",
  "created_from_version": "1.4.0",
  "name": "Amy",
  "persona": "amy",
  "memory": { "base": "amy-kitchen@1.4.0", "delta": "inst_01J…" },
  "origin": "gateway:sms-amy",
  "persistent": true,
  "created_at": "…", "last_active_at": "…"
}
```

```json
// conversations/<cid>.json
{ "id": "conv_01J…", "instance_id": "inst_01J…", "title": "Sunday prep",
  "messages": [ … ], "created_at": "…", "last_active_at": "…" }
```

**The preset is immutable for the life of the instance** — its memory was seeded from that
preset, so switching mid-life is incoherent. `/persona set` still moves within the roster;
switching preset means a new instance.

`created_from_version` is diagnostics only. Personas and skills **follow the installed pack
version** (§5.4), they are not pinned here.

### 2.2 Every surface creates instances

| Surface | Today | After |
|---|---|---|
| Workshop chat | `POST /api/v1/chats` with a persona (`:431`) | **default: new instance + its first conversation** — the session feel is preserved. A named instance offers "new conversation" instead |
| CLI interactive | in-memory `AgentState` | a conversation in an instance, so a REPL session survives `/quit`. `--preset`, `--instance`, `METALCRAFT_PRESET` |
| CLI one-shot | ephemeral | ephemeral instance (§2.3) unless `--instance` given |
| Gateway channel | session keyed by channel + TTL reset (`:4253`) | **one permanent instance per channel** (`preset` on the channel record); each idle window is a new conversation, memory persists |
| Flow schedule | ephemeral one-shot per firing | a **persistent instance minted at arm time**; each firing is a conversation in it, so scheduled work remembers its own history (see the flows plan) |

`TurnRunner` (`src/runtime.rs:112`) stays the single turn chokepoint and gains the instance
handle, so recall and capture hit the right memory.

### 2.3 Ephemeral vs persistent

Not every instance deserves a durable memory. An instance is **ephemeral** unless named,
channel-bound, or explicitly kept: it recalls against the preset base but writes no delta, and
the daemon reaps it after a TTL. Without this, every one-shot CLI invocation mints an embedding
job. `persistent: true` is the flag; "keep this agent" is the UI action that sets it.

---

## 3. Memory: a shared base and a per-instance delta

Today memory is one process-global index: `static MEMORY: OnceLock<Arc<RwLock<MemoryIndex>>>`
(`src/memory/mod.rs:131` `handle()`), one set of files (`src/paths.rs:143-166`). It becomes a
**two-layer, copy-on-write** structure.

### 3.1 Layout

```
<data>/memory/
  presets/<slug>@<version>/    BASE — built once at pack install, immutable, shared
    snapshot.json · vectors.bin · manifest.json
  instances/<instance-id>/     DELTA — this agent's own
    snapshot.json · wal.jsonl · vectors.bin · capture.jsonl · tombstones.json
  dreams/<ts>.json
```

### 3.2 Why copy-on-write, not copy

The obvious implementation is "copy the preset's 214 records into the new instance". Across
twenty instances that's 4,280 duplicated records and vectors, and instance creation becomes
O(memories) — on the interactive path, since creating an instance is *starting a chat*.

Instead the instance index is a **delta over the shared base**: recall queries both and fuses;
a write goes to the delta; an edit materializes that one record into the delta, shadowing the
base; a `mem_forget` writes a tombstone. Instance creation is O(1) — a pointer and an empty
delta.

**And this is what makes "live instances follow the pack" (§5.4) correct rather than
alarming.** Updating the pack swaps the instance's `memory.base` pointer to the new version:
new seed memories appear automatically, tombstones persist so anything the user forgot stays
forgotten, and everything the instance learned is untouched in its delta. Without the delta
layer, "follow the pack" for memories would mean either re-copying (clobbering the agent's
own learning) or nothing at all. The optimization *is* the semantics.

### 3.3 The handle registry

`memory::handle()` → `memory::handle(instance_id)`, backed by a
`RwLock<HashMap<InstanceId, InstanceMemory>>` (base `Arc` + delta) with lazy load and **LRU
eviction** — propose 8 resident, flush on evict. Base layers are refcounted and shared: twenty
instances of Amy hold one copy of her 214 vectors in RAM.

Every call site in `recall.rs`, `capture.rs`, `inject.rs`, `tools.rs` takes the instance from
the turn context instead of reaching for the global. **Write `recall()` to fuse N indexes from
day one** — it needs two now, and a future shared-across-instances layer (§8.1) would need a
third. Written against a single index, adding either means reworking the hot path.

### 3.4 Provenance

Every record carries `source: Seeded { preset, version } | Learned`. This drives the
`[from Amy's Kitchen Agent]` label in the recall tail block (`src/memory/inject.rs`, which
already guarantees the block is ephemeral and stripped before persist), the dream's decay
exemptions, and the base/delta reconciliation on update (§5.4). It also keeps the door open for
harvest (§8.4) without committing to it.

### 3.5 Embeddings

Base layers are built **once per (preset, version)** at pack install, not per instance —
twenty instances, one embedding bill.

A pack MAY ship a precomputed `vectors.bin`. axoniac already embeds every seed memory at 384
dims for its own search (`axoniac-prime/PLAN.md` §7.7), so shipping that file is free on the
producing side and **skips the embedding cost entirely** on any pod whose `(model, dims)`
match. A mismatch ignores it and rebuilds. If no embedder is available, build BM25-only and let
`backfill_embeddings` (`src/memory/mod.rs:396`) fill in later — **recall degrades, never
fails**, the invariant `MEMORY_SYSTEM_PLAN.md` already states.

Redaction (`src/memory/redact.rs`) runs at pack **build** time, not per seed.

### 3.6 What this costs, deliberately

- **Learning is per-instance.** Two instances of Amy don't share what they learn. For
  conversation-shaped agents that's right; §8.1 proposes an opt-in shared layer if it isn't.
- **`mem_*` become instance-scoped.** `mem_search` gains `scope: "instance" | "base" | "all"`.
- **Memory earns its keep on long-lived instances.** For a ten-turn chat the conversation *is*
  the context and memory is overhead — which is exactly why §2.3 exists.

---

## 4. Integration packs — dependencies, not installables

**Integration packs are no longer installed or uninstalled.** They ship inside agent packs.

### 4.1 What a pack is now

```
integration_packs/<id>/
  pack.json      manifest_version: 2 — id, name, description, version,
                 requires_env[], tags[], tools[], domains[]
  api_tools/*.json
  README.md
```

**Removed:** `personas/`, `skills/`, `flow_templates/` — they live in agent packs now, where
presets curate them. **Added:** `tools[]` (declared names, validated against `api_tools/` at
build and install — a mismatch is a hard error, which stops a pack quietly growing a
`github_delete_repo` after you approved it) and `domains[]` (every origin its tools reach,
derived from the `url` fields). Both feed the install-time permission summary.

### 4.2 Content-addressed store

Ten agent packs vendoring `metalcraft-calendar` should not mean ten copies. Store packs by
content hash and reference them:

```
<data>/pack_store/<sha256>/{pack.json, api_tools/, README.md}
<data>/agent_packs/<id>/integration_packs.json   → { "metalcraft-calendar": "sha256:ab12…" }
```

Dedup for free, **and two versions coexist without conflict** — which deletes the
"highest-version-wins" ambiguity entirely. Refcounted; a store entry is GC'd when the last
agent pack referencing it is uninstalled.

### 4.3 Resolution, and the death of enable/disable

A tool resolves if: some installed agent pack provides that integration pack, **and** the
current persona references it via `Persona.packs` (`src/persona.rs:144`), **and** the current
preset lists it in `integration_packs`.

So **`integration_packs.json` enable-state goes away** (`src/paths.rs:81`,
`src/integration_packs.rs:155-256`), along with `pack_enable`/`pack_disable`. Scoping becomes
structural — three declarations — instead of a mutable global flag. `pack_list`/`pack_read`
survive as read-only introspection.

### 4.4 What packs.metalcraftai.com is for now

Two jobs, and only two:

1. **The home of integration packs** — where they're authored, published, and versioned, and
   where build tooling fetches them to vendor into an agent pack. The pod never fetches them at
   runtime: `PACKS_BASE_URL`, the `registry::fetch_zip` pack path, and `pack_install` are retired.
2. **A host that also serves agent packs**, implementing the §5.6 contract like any other. The
   first-party ecosystem packs live there; axoniac is the social host. Two registries, one
   protocol, neither depending on the other.

Integration-pack authoring stays there rather than moving to axoniac's knowledgebase editor:
`api_tools` are JSON tool definitions, credentials, and HTTP schemas — a developer artifact with
a developer audience, not social content.

---

## 5. The agent pack

### 5.1 Layout

```
amy-kitchen-agent-1.4.0.agentpack        (zip)
  agent_pack.json
  agent_presets/amy-kitchen.json  +  amy-kitchen/{memories.jsonl, vectors.bin, avatar.png}
  personas/{amy, amy-shopper, amy-critic}.json
  skills/{knife-skills, menu-planning, substitutions}.md
  integration_packs/{metalcraft-calendar, instacart}/
  flows/, flow_templates/                  optional, installed unscheduled (§5.5)
  README.md
  SIGNATURE                                optional, detached, over agent_pack.json
```

**Self-contained by construction.** Every persona a preset names, every skill those personas
load, every integration pack they call — in the zip. Installing works with no network. There is
no thin/fat variant: a pack that doesn't carry its dependencies isn't valid.

### 5.2 `agent_pack.json`

```json
{
  "manifest_version": 1,
  "id": "amy-kitchen-agent",
  "handle": "amy_kitchen",
  "name": "Amy's Kitchen Agent",
  "version": "1.4.0",
  "license": "MIT",
  "author": { "handle": "ethereumdegen", "display_name": "Andrew", "sub": "…" },
  "category": "food", "tags": ["cooking"],

  "presets": ["amy-kitchen"],
  "provides": {
    "personas": ["amy", "amy-shopper", "amy-critic"],
    "skills":   ["knife-skills", "menu-planning", "substitutions"],
    "integration_packs": [
      { "id": "metalcraft-calendar", "version": "1.7.1", "content_sha256": "…",
        "source": "https://packs.metalcraftai.com" },
      { "id": "instacart", "version": "0.3.4", "content_sha256": "…",
        "source": "https://packs.metalcraftai.com" }
    ]
  },

  "requires_env": [
    { "name": "METALCRAFT_TOKEN", "needed_by": ["metalcraft-calendar"], "required": true },
    { "name": "INSTACART_TOKEN",  "needed_by": ["instacart"], "required": false }
  ],
  "domains": ["calendar.metalcraftai.com", "api.instacart.com"],

  "content_sha256": "…",
  "parent": { "id": "generic-chef-agent", "version": "0.9.2", "content_sha256": "…" }
}
```

`requires_env` is flattened and attributed because the question a human answers at install is
*"which credentials do I paste in?"*. Missing keys are a **warning with a list**, never a
failure. `parent` is fork lineage, from axoniac's `parent_hash`
(`axoniac-monorepo/migrations/027_parent_hash.sql`).

### 5.3 Install

`src/agent_packs/install.rs`, modelled on `integration_packs::install_from_zip`
(`src/integration_packs.rs:321`) — reuse its traversal rejection, size cap (raise to 64 MB for
assets and memories), and version gate.

1. Unzip in memory; verify `content_sha256`, and `SIGNATURE` when the origin requires it.
2. Validate: every preset's personas/skills/integration_packs present; every persona's
   `packs[]` ⊆ its preset's `integration_packs` (cheap check, real containment); every pack's
   `tools[]` matches its `api_tools/`.
3. Extract → `<data>/agent_packs/<id>/`; integration packs → `pack_store/` by hash (§4.2).
4. Build the preset memory **base layers** → `<data>/memory/presets/<slug>@<version>/`.
5. Record in `<data>/agent_packs.json` + `lockfile::record_agent_pack`.
6. **Report**: presets, personas, skills, packs (new / deduped / new version alongside old),
   missing env keys, flows installed-unscheduled, base memories indexed, vectors reused or
   built, slug collisions, unmet model floor.

`uninstall(id)` drops the directory, deref-counts the pack store, drops base layers, removes
lock entries — and **refuses while any persistent instance references one of its presets**,
listing them. `--force` orphans them: they keep their deltas and fall back to `general-agent`.

### 5.4 Updates — live instances follow the pack

**Updating is explicit. There is no auto-update.** `agentpack_update` is an approval-gated
action the user runs; nothing changes underneath a running agent because someone published.
This matters: without it, "instances follow the pack" would let an author push a persona prompt
to every pod that installed them.

Once updated, **live instances follow the new version**:

| Element | On update |
|---|---|
| Persona prompts, tools, skills | **Follow.** The instance resolves against the installed version. A fix reaches every agent, including your SMS one. |
| Preset roster, declared packs | **Follow.** |
| Seed memories | **Follow, additively.** `memory.base` repoints to `<slug>@<new-version>`; new records appear, tombstoned ones stay forgotten, the instance's delta is untouched (§3.2). |
| Learned memories, conversations, name, `persistent` | **Never touched.** |

Two edge rules the installer must handle, both reported:

- **A persona the instance is currently using was removed** → fall back to the preset's new
  `default_persona`, note it on the instance.
- **A preset an instance uses was removed** → the instance is *orphaned*: it keeps its delta and
  conversations, resolves against a synthesized frozen copy of the old preset, and is flagged in
  the UI. Never silently deleted; someone's agent is in there.

### 5.5 Flows install unscheduled

A pack may ship flows; **install never arms a schedule.** They land disabled and the report says
so. Installing an identity must not silently start background work — and **arming is what mints
the agent that runs it**, which makes it the second consent moment, not a checkbox.

Flows are bound to a preset, may only name personas from its roster, and run as a persistent
instance with each firing a conversation — so a scheduled agent finally accumulates memory
across runs. That's a design of its own: **[FLOWS_AND_AGENT_PRESETS_PLAN.md](FLOWS_AND_AGENT_PRESETS_PLAN.md)**.

### 5.6 Registries are a protocol, not a host

**Any host implementing the contract can serve agent packs.** axoniac.com is the social
discovery host; packs.metalcraftai.com serves first-party ecosystem packs; a company can
self-host. The pod treats them as interchangeable — the crates.io-alternative-registries model,
and why the manifest and endpoints live in a shared spec crate rather than in one server.

```
GET /api/v1/agent-packs/{id}/version    → { id, version, content_sha256 }
GET /api/v1/agent-packs/{id}/manifest   → the raw agent_pack.json
GET /api/v1/agent-packs/{id}/download   → the .agentpack bytes
GET /api/v1/agent-packs/search?q=       → results (optional; a host may be fetch-only)
```

`<data>/registries.json`:

```json
{
  "default": "axoniac",
  "registries": {
    "axoniac":    { "url": "https://axoniac.com",             "trust": "verified-only" },
    "metalcraft": { "url": "https://packs.metalcraftai.com",  "trust": "first-party" },
    "acme":       { "url": "https://agents.acme.internal",    "trust": "explicit", "token_key": "ACME_TOKEN" }
  }
}
```

**Qualified references.** `@amy_kitchen` resolves against `default`; `axoniac:@amy_kitchen` is
explicit. **An id present in more than one configured registry is an error unless qualified** —
never a silent first-match, because that is the supply-chain substitution attack.

Trust is per registry: `first-party` installs like today; `verified-only` refuses unverified
packs unless overridden; `explicit` requires the user to have added the host by hand and prompts
every install. Signature verification is keyed on each host's published signing key, so a pack
signed by axoniac doesn't validate served from elsewhere.

`LockEntry` already carries `source` (`src/lockfile.rs:20-30`), so the pin is
`(id, version, sha, source)` and a restored pod re-fetches from the same origin.

### 5.7 Authoring: memories are content, written like skills

**Seed memories are authored, not harvested.** They are written entry by entry in axoniac's
knowledgebase editor — the same shape, same editor, same versioning as a skill
(`axoniac-monorepo/ax-backend/src/models/skill.rs`: name, description, markdown `content`,
tags, `content_hash`, `parent_hash`). A skill is a methodology the persona loads on demand; a
memory is a thing the agent simply knows. Two content types, one authoring surface. See
`axoniac-prime/PLAN.md` §7.2.

**What the pod receives.** At pack build, each authored memory entry compiles to one record in
`memories.jsonl`: frontmatter supplies `kind` / `entity` / `importance` / `tags`, the body
becomes `content`, the title becomes `summary`. Long entries are fine — `summary` is what
recall uses when filling its token budget (`src/memory/mod.rs:537`), so a paragraph-length
memory costs a line, not a paragraph, until it's the relevant one.

**One escape hatch:** frontmatter `split: by-heading` compiles a single entry into one memory
per `##` section, each inheriting the entry's metadata and carrying `source_entry`. That covers
"I want to write one article about braising, not six fragments" without needing a compile-preview
UI on either side.

`agentpack_export { preset, out }` therefore just packages what's on the pod — presets,
personas, skills, vendored packs, authored memories — and bumps the version. It does **not**
reach into instance memory: an instance's delta is the operator's, not the author's.

### 5.8 Lockfile

`Lock` (`src/lockfile.rs:33-42`) drops `packs`, gains `agent_packs: Vec<LockEntry>`
(`record_agent_pack` / `remove_agent_pack`; templates at `:117`/`:146`). Packs are
self-contained, so `/lockfile/restore` is a flat replay — no dependency ordering.

---

## 6. Tools & API

### 6.1 Tools (`src/agent_packs/tools.rs`, registered at `src/tools/mod.rs:65`)

| Tool | Approval | |
|---|---|---|
| `agentpack_list` / `agentpack_read` / `agentpack_search` | auto (`MetaRead`) | |
| `agentpack_install { ref, version? }` | **prompt** | summary: presets, personas, **domains**, credentials, memory count |
| `agentpack_update` / `agentpack_uninstall` | prompt | update reports what followed (§5.4) |
| `agentpack_export { preset, out }` | prompt | package a local preset (§5.7) |
| `agentpack_submit` | prompt | publish |
| `preset_list` / `preset_read` | auto | |
| `preset_save` / `preset_delete` | prompt | author a local preset |
| `instance_list` / `instance_read` / `instance_create` / `instance_delete` | mixed | create is prompt |
| `conversation_list` / `conversation_new` | auto / prompt | within an instance |

`src/approval.rs` gains `AgentPackWrite`; `pack_enable`/`pack_disable`/`pack_install` retire.

### 6.2 Workshop API (`src/workshop_api.rs:392-474`)

```
GET/POST/DELETE /api/v1/agent-packs[/{id}]      · POST …/install · POST …/{id}/update
GET    /api/v1/presets[/{slug}]                 · PUT /api/v1/presets/{slug}
GET    /api/v1/agents/instances                 list
POST   /api/v1/agents/instances                 { preset, name?, persistent? }
GET    /api/v1/agents/instances/{id}            record + conversations
DELETE /api/v1/agents/instances/{id}
GET    /api/v1/agents/instances/{id}/memory/stats
GET    /api/v1/agents/instances/{id}/conversations
POST   /api/v1/agents/instances/{id}/conversations
POST   /api/v1/agents/instances/{id}/conversations/{cid}/turn
GET    /api/v1/agents/instances/{id}/conversations/{cid}/events    SSE
GET    /api/v1/registry/agent-packs/search?q=
```

`/api/v1/chats*` stays a deprecated alias for one minor version, mapping a chat id onto
`(instance, conversation)`. `/api/v1/snapshot` (`:393`) gains presets and instances. Channels
(`:449-451`) gain `preset` and their bound `instance`.

### 6.3 Discovery is declarative; install is native

Search and read are pure HTTP, so they're a declarative integration pack — and because
registries are a protocol, it's **one `agent-registry` pack parameterized by host**, not an
axoniac-specific one: `ar_search`, `ar_get`, `ar_check_update`, `ar_trending`, each taking an
optional `registry`. Install stays native because it touches the filesystem — the split
`metalcraft-packs-web/PACK_INSTALL_PLAN.md` already landed on.

### 6.4 One preset per instance is a prompt-cache decision

The framework tracks `cached_input_tokens` (`metalcraft/src/prebuilt.rs`) and the design
deliberately injects recall at the *tail* rather than the system prompt, so the prefix is
stable. Pinning the preset for the instance's life keeps it stable across every turn of every
conversation. **Write this down as a reason, not just a consequence** — otherwise someone later
adds "switch preset mid-chat" and quietly multiplies token cost.

---

## 7. Migration

Runs in `src/bin/migrate.rs`, **never on boot**.

### 7.1 Seeds become first-party agent packs

| Today | Becomes |
|---|---|
| `seed/personas/*.json`, `seed/skills/*.md`, `seed/flow_templates/*` | **`metalcraft-core`**, providing **`general-agent`** (§1.3) |
| `seed/integration_packs/metalcraft-{calendar,notes,email,drive,contacts,code,packs}` + their personas/skills | **`metalcraft-ecosystem`**: a `metalcraft-assistant` preset, those personas/skills, the packs vendored, plus `agent-registry` |
| `seed/integration_packs/email` (IMAP) | into `metalcraft-ecosystem` |

Both are embedded seeds written on first run (`seed.rs:58` `ensure_defaults`, `:141`
`write_versioned_seeds` — the version-gated force-upgrade path extends to agent packs).

### 7.2 Existing pods

1. Wrap each `<data>/integration_packs/<id>/` into `<data>/agent_packs/<id>-legacy/`: pack
   becomes a vendored dep in `pack_store/`; its `personas/`/`skills/` move up; a `<id>-legacy`
   preset is synthesized.
2. `<data>/personas/` and `<data>/skills/` — the user's own — **untouched**, top precedence. A
   synthesized `my-agent` preset lists them so they're reachable from the picker.
3. Each `<data>/chats/*.json` → **one instance holding one conversation**, `preset:
   "general-agent"`, `persistent: true`, no seeding (they predate presets).
4. Existing global `<data>/memory/` → the delta of a `legacy` instance with no base.
   **Nobody loses memories.**
5. Drop `integration_packs.json`. Report to `<data>/migrations/<ts>-presets.json`.

Pods that skip the step: the server refuses to start, naming the command.

### 7.3 Provisioning hook (cross-repo)

`paths.rs:91` `ecosystem_packs_seeded_marker()` shows the k3 control plane already preloads
packs on a pod's first boot. Extending that to *"provision a pod with agent pack X
preinstalled"* gives axoniac a conversion path for visitors who don't have a pod yet — see
`axoniac-prime/PLAN.md` §10.2. Low effort here, high leverage there.

---

## 8. Open questions

1. **Cross-instance learning.** Instances don't share what they learn (§3.6). If that's wrong,
   the fix is a third layer — `<data>/memory/shared/<preset>/` that instances read and
   optionally write via a `promote_memory` tool or a dream stage. Cheap **only if `recall()`
   fuses N indexes from day one** (§3.3), which is why that's specified now.
2. **Does `--auto-approve` cover `agentpack_install`?** It adds personas whose prompts the model
   follows and tools that use the user's credentials. Honour it (consistent), always prompt
   (breaks headless), or gate on `METALCRAFT_ALLOW_UNATTENDED_INSTALL`? **Leaning: third.**
3. **Prompt injection.** Personas are untrusted text that becomes instructions; seed memories are
   untrusted text that becomes *beliefs*, and are worse because nobody reads a corpus. Structural
   containment is in the design (§5.3 step 2); detection and review are registry-side. **Own doc.**
4. **Harvest as a second authoring path (deferred).** Memories are authored (§5.7), but an
   operator who has run an agent for months holds a corpus the author never wrote. Lifting
   `Learned` records out of an instance into a new pack version is a natural follow-on — and the
   provenance field (§3.4) already makes it possible. It is deferred because the redaction
   problem is real: those memories are about the *operator* — their kitchen, their allergies,
   their address — so shipping them needs a personal-data pass and a line-by-line confirm, not a
   button. Revisit once authoring-by-writing has users.
5. **Ephemeral TTL and resident-index cap** — 8 resident, 24h TTL? Needs numbers and a
   `mem_stats`-shaped observable.
6. **Can a user edit a pack-provided preset?** Pack contents are read-only today
   (`PersonaSummary.read_only`, `src/persona.rs:36`). Proposal: **fork-on-edit** — copies to
   `<data>/agent_presets/` with `parent` set. That's also the authoring on-ramp and the social loop.
7. **Preset- or instance-scoped keys?** The key store is global (`src/key_store.rs`) with channel
   scoping already present. Two instances wanting different credentials for the same service is a
   real case (two Instacart accounts).
8. ~~Do multi-preset agent packs exist in practice?~~ **Decided: one preset per agent pack.** The
   installer validates it and the pod may assume a single preset when naming things in the UI.
   The manifest keeps `presets` as an array so multi-preset "crews" stay additive later.
9. **Memory pays off unevenly, by design.** "New chat" mints a new instance, so a Workshop chat's
   learning dies with it; what carries is the authored knowledgebase, which every instance gets
   from turn one. Accumulated learning only accrues where an instance is long-lived — gateway
   channels, named agents, and armed flows. That's a coherent split (the knowledgebase does the
   work; learning is a bonus for agents you keep), but it means **the memory subsystem's cost is
   mostly paid by chats that never benefit from it** — hence the ephemeral rule in §2.3. Worth
   re-checking once there's usage data.

---

## 9. Phasing

| Phase | Scope |
|---|---|
| **AP1** | `metalcraft-packs` spec crate: `PackManifest` v2, `AgentPackManifest`, `AgentPreset`, validation, canonical hashing. Shared by agent, registries, Workshop. |
| **AP2** | Preset object + resolution (incl. the skill collision rule) + `general-agent`. `sub_agent` scoped to the roster. CLI `--preset`. **Presets over today's packs — no install changes.** |
| **AP3** | Instances + conversations: records, lifecycle, chats→(instance, conversation) migration, every surface, `TurnRunner` carries the handle, gateway channels get permanent instances, flow schedules gain `instance` + arm/disarm. |
| **AP4** | Two-layer memory: base/delta, handle registry + LRU, N-index `recall()`, provenance, instance-scoped `mem_*`, dream per instance. |
| **AP5** | Agent pack format + installer + `pack_store` + lockfile + update semantics + retirement of `pack_enable`/`pack_install`/`PACKS_BASE_URL`. Integration pack v2. First-party packs. `migrate`. **The breaking release.** |
| **AP6** | Registries config + `agent-registry` pack + install-from-registry + submit + `agentpack_export`. |
| **AP7** | Workshop UI: preset picker on new chat, instance list with conversations, pack browser + permission summary, fork-on-edit, local preset + memory editing. |

AP2–AP3 are additive and shippable alone — **the preset picker lands before any breaking work**,
which is the visible half of the idea.

---

## 10. Acceptance tests

- Fresh pod, nothing installed: CLI, Workshop, gateway behave byte-identically to 0.29.
- `migrate` preserves every chat as an instance+conversation and every memory in a `legacy`
  instance; every previously-working persona resolves by bare slug.
- Installing an agent pack with the network disabled succeeds end to end.
- A tampered `content_sha256` fails **before** anything is written to the data dir.
- Two instances of one preset: independent learning, **one shared base on disk and in RAM**.
- Creating an instance of a 5,000-memory preset is O(1) — no per-instance embedding, no copy.
- Update the pack: prompts change in a live instance, its learned memories survive, a memory it
  forgot stays forgotten, and new seed memories appear.
- Remove a persona in an update → the instance using it falls back and says so.
- A gateway channel idle-resets: a new conversation starts, memory persists.
- An authored entry with `split: by-heading` and three `##` sections compiles to three memories,
  each carrying `source_entry`.
- Ten packs vendoring the same integration pack → one `pack_store` entry.
- A persona referencing a pack its preset doesn't declare → **install fails**, naming it.
- Uninstalling a pack with a persistent instance fails, listing the instances.
- A pack shipping flows arms **zero** schedules.
