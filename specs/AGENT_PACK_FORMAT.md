# Agent Pack Format — normative specification

**Spec version:** 2
**Status:** normative. This document is the single definition of what a valid agent pack is.
**Implementors:** `metalcraft-agent` (installs), `axoniac-prime` (publishes and serves),
`metalcraft-workshop` (reads and displays), any third-party registry.

> `AGENT_PRESETS_PLAN.md` §9 called for a shared `metalcraft-packs` spec crate so that
> "registry and pod can never disagree about what's valid". A shared Rust crate cannot
> reach a TypeScript client and would not have stopped the drift that actually happened
> — the divergence was in *rules*, not in types. This document is the rule set. The
> crate stays where it earns its keep: `metalcraft_packs::canonical_sha256`, which every
> implementor **must** call rather than reimplement, because a hash that disagrees is
> unrecoverable in a way a validation rule is not.
>
> **Conformance is testable.** §10 lists the checks every implementor must perform and
> names the test that proves it, in each repo. A change to this document is not landed
> until those tests are updated on both sides.

---

## 1. Vocabulary

| Term | Is | On disk |
|---|---|---|
| **agent pack** | the unit of installation — a zip | `<data>/agent_packs/<id>/` |
| **agent preset** | the agent's identity: a default persona, a roster, its declared needs | `agent_presets/<slug>.json` |
| **agent instance** | a live agent created from a preset; owns memory and many conversations | `<data>/agent_instances/<id>/` |
| **integration** | a named, versioned, hash-pinned bundle of tool definitions plus the credentials they need. **Not installable on its own** — vendored inside agent packs | `<data>/integration_store/<sha256>/` |
| **conversation** | one thread with an instance | `<data>/chats/<id>.json`, carrying `instance_id` |

An **agent pack** is the only thing that installs. An **integration** is a dependency
it carries — it has no independent lifecycle, no enable/disable, and no install of its
own.

It was called an "integration pack" until spec 2. The name outlived the meaning: "pack"
promised installability the system had stopped offering, and it collided with "agent
pack", which really is installable. Renaming it was cheaper than continuing to explain
the difference — this section used to have to say *"never write bare 'pack'"*, which is
a sign the vocabulary was doing damage rather than work.

---

## 2. Archive layout

```
<id>-<version>.agentpack               (zip, DEFLATE)
  agent_pack.json                      the manifest (§3)
  agent_presets/<slug>.json            exactly one (§4)
  agent_presets/<slug>/memories.jsonl  optional seed memories (§6)
  agent_presets/<slug>/vectors.bin     optional precomputed embeddings (§7)
  agent_presets/<slug>/avatar.png      optional
  personas/<slug>.json                 every persona the preset names
  skills/<slug>.md                     every skill the preset or its personas load
  integrations/<id>/integration.json   every integration the preset declares
  integrations/<id>/api_tools/*.json
  integrations/<id>/README.md          optional
  flows/<id>.json                      optional (§8)
  README.md                            optional
  SIGNATURE                            optional, detached, over agent_pack.json (§9)
```

**Self-contained by construction.** Installing MUST succeed with no network access. A
pack that does not carry a dependency it declares is invalid — there is no thin variant.

### 2.1 Path rules

An implementor MUST reject an archive if any entry:

- contains a `..` segment, after normalising `\` to `/`;
- is absolute (leading `/`) or carries a drive letter;
- is a symlink;
- decodes to a path outside the archive root by any other means.

Directory entries are ignored, not rejected.

### 2.2 Size

`MAX_BUNDLE_BYTES` is **64 MiB** of *decompressed* bytes, summed across all entries.

The budget MUST be enforced against bytes **actually read**, never against the zip
header's declared uncompressed size — that field is attacker-controlled and independent
of the compressed stream. Read one byte past the remaining budget so an over-long entry
is detectable rather than silently truncated.

---

## 3. `agent_pack.json`

```jsonc
{
  "manifest_version": 2,                  // this document's spec version (§3.4)
  "id": "amy-kitchen-agent",              // required, §3.1
  "handle": "amy_kitchen",                // optional, registry handle
  "name": "Amy's Kitchen Agent",          // required
  "description": "…",
  "version": "1.4.0",                     // required, semver
  "license": "MIT",
  "author": { "handle": "…", "display_name": "…", "sub": "…" },
  "category": "food",
  "tags": ["cooking"],

  "presets": ["amy-kitchen"],             // required, exactly one (§3.2)
  "provides": {
    "personas": ["amy", "amy-shopper", "amy-critic"],
    "skills":   ["knife-skills", "menu-planning"],
    "integrations": [
      { "id": "metalcraft-calendar", "version": "1.7.1",
        "content_sha256": "ab12…", "source": "https://packs.metalcraftai.com" }
    ]
  },

  "requires_env": [                       // DERIVED, never author-supplied (§5)
    { "name": "METALCRAFT_TOKEN", "needed_by": ["metalcraft-calendar"], "required": true }
  ],
  "domains": ["calendar.metalcraftai.com"], // DERIVED (§5)

  "content_sha256": "…",                  // §3.3
  "parent": { "id": "generic-chef-agent", "version": "0.9.2", "content_sha256": "…" }
}
```

Unknown top-level fields MUST be preserved on round-trip and MUST NOT cause rejection.
`manifest_version` other than `2` MUST be rejected.

**Spec 1 → 2 is a real break, not a rename.** Vendored dependencies moved from
`integration_packs/<id>/pack.json` to `integrations/<id>/integration.json`, and
`provides.integration_packs` became `provides.integrations`. Because the archive path
is part of what `content_sha256` covers, a v1 archive's hash cannot be reproduced under
v2 layout — so there is no dual-read path and none is wanted. Nothing had been published
under v1, which is the only reason this was affordable; a v1 archive is refused with its
version in the message.

The two places a v1 name survives are **`Persona.integrations`** and
**`AgentPreset.integrations`**, which accept `packs` and `integration_packs` as serde
aliases. Those documents live on people's pods rather than inside archives, so breaking
them would break working installs for a word.

### 3.4 Two documents, two version numbers

`agent_pack.json` and `agent_presets/<slug>.json` each carry a `manifest_version`, and
they are **independent**. The archive manifest is at **2**; the preset document is at
**1** and did not change when the archive layout did, because nothing about a preset
changed — spec 1 → 2 moved vendored dependencies and renamed a field on the *manifest*.

So a valid spec-2 pack contains an `agent_pack.json` with `"manifest_version": 2` and an
`agent_presets/<slug>.json` with `"manifest_version": 1`. That looks like a mistake and
is not one; implementors have shipped it that way on both sides (`metalcraft-agent`'s
`MANIFEST_VERSION = 2` for archives against `AgentPreset`'s `default = 1`, and
`axoniac-prime`'s seeder writing each). Bumping the preset to 2 would assert a format
change that never happened, and would invalidate every preset already on disk.

### 3.1 Identifiers

`id`, every preset/persona/skill/integration slug, and `handle` all satisfy:

```
^[a-z0-9][a-z0-9_-]{0,63}$
```

Implementors MUST use `metalcraft_packs::is_valid_integration_id` where available rather than
recompiling the regex.

### 3.2 Exactly one preset

`presets` MUST contain exactly one entry. Zero and two are both rejected, with the count
in the error.

`presets` stays an **array** in the format so multi-preset "crews" remain an additive
change. Implementors MUST NOT collapse it to a scalar.

### 3.3 `content_sha256`

The hash of every file in the archive **except `agent_pack.json`**, so the manifest can
pin what it describes without hashing itself.

Computed by `metalcraft_packs::canonical_sha256` over `(path, bytes)` pairs sorted by
path. Implementors MUST call that function. Reimplementing it is the one thing in this
spec that cannot be caught by a validation test on the other side.

If present, it MUST be verified before anything is written to disk or to a database. If
absent, the archive is unpinned: an implementor MAY accept it (local development,
`agentpack_export` output) but a registry MUST record the hash it computed.

---

## 4. `agent_presets/<slug>.json`

```jsonc
{
  "manifest_version": 1,                  // the preset document's own version — not the
                                          // archive spec version. See §3.4.
  "slug": "amy-kitchen",
  "name": "Amy's Kitchen Agent",
  "tagline": "…",
  "description": "…",
  "avatar": "agent_presets/amy-kitchen/avatar.png",

  "default_persona": "amy",               // required
  "personas": [
    { "slug": "amy",         "role": "default" },
    { "slug": "amy-shopper", "role": "subagent", "description": "Places grocery orders" },
    { "slug": "amy-critic",  "role": "internal" }
  ],

  "skills": ["knife-skills"],
  "integrations": ["metalcraft-calendar"],
  "flows": ["sunday-meal-prep"],          // optional (§8)

  "memories": { "file": "agent_presets/amy-kitchen/memories.jsonl", "count": 214,
                "embed_model": "text-embedding-3-small", "dims": 384 },

  "model": { "tier": "standard", "needs": ["tool_calling"],
             "min_context": 128000, "prefer": "gpt-5.4" },
  "requires_env": ["METALCRAFT_TOKEN"],
  "version": "1.4.0"
}
```

### 4.1 Roles

| `role` | Meaning |
|---|---|
| `default` | what a new instance starts as. **At most one** per preset. |
| `subagent` | offered to `sub_agent`'s persona mode. The default when `role` is absent. |
| `internal` | reachable only from inside this preset; never listed in a picker. |

**Callable roster** = `default_persona` ∪ { `personas[].slug` where `role != "internal"` }.
This is the set `sub_agent` may delegate to and the set a flow may name (§8).

### 4.2 Structural rules

An implementor MUST reject a preset if:

- `default_persona` is empty or whitespace;
- more than one entry has `role: "default"`;
- `personas` is non-empty and does not contain `default_persona`.

An empty `personas` array with a non-empty `default_persona` is **valid** — it is the
minimal preset, and `general-agent` predates rosters.

### 4.3 `model` is a capability floor, not a model name

A hard `"prefer": "gpt-5.4"` MUST NOT be treated as a requirement. The pod maps
`tier` / `needs` / `min_context` onto what it has. A pod that cannot meet the floor
**warns at install and installs anyway** — refusing would make a pack undistributable
rather than degraded.

---

## 5. Derived fields: consent is computed, never declared

`requires_env` and `domains` on the manifest, and everything shown to a human before
they approve an install, MUST be **derived from the archive's own bytes**. An author
writes what their agent *is*; what it can reach is computed from the integrations
actually inside.

An implementor MUST re-derive rather than trust, and a manifest disagreeing with its own
contents is a rejection, not a warning.

### 5.1 Derivation

For each `integrations/<id>/`:

- **`integration.json`** → each `requires_env` name, attributed to `<id>`; each `native_tools`
  name into the tool list.
- **each `api_tools/<file>.json`**:
  - **tool name** = the document's own `name` field, falling back to the filename. A
    tool that lies about its name still cannot hide.
  - **domain** = host of `url`, ignoring `{placeholder}` path segments.
  - **mutating** if `method` is anything other than `GET` (absent means `GET`).
  - **credentials** = every `$NAME` / `${NAME}` reference in `headers` values. These are
    routinely omitted from `requires_env` and are the ones that matter.

Domains and tools are sorted and deduplicated.

### 5.2 The consent summary

```jsonc
{
  "domains": ["api.instacart.com", "calendar.metalcraftai.com"],
  "requires_env": [{ "name": "…", "needed_by": ["…"], "required": true }],
  "tools": ["ic_order", "mcal_create", "mcal_list"],
  "mutating_tools": ["ic_order"]
}
```

`mutating_tools` is required. A read-only agent is a materially smaller commitment than
one that can write, and a surface that cannot say which it is has not obtained consent.

---

## 6. Seed memories

`agent_presets/<slug>/memories.jsonl` — one JSON object per line, blank lines and lines
failing to parse skipped with a count (a corrupt line costs one memory, never the pack).

```jsonc
{ "kind": "semantic", "summary": "Amy braises with stock, never water",
  "content": "…", "entity": "amy", "importance": 0.7, "tags": ["technique"],
  "source_entry": "braising-basics" }
```

`summary` is what recall shows when filling a token budget, so a paragraph-length
`content` costs one line until it is the relevant one.

At most **5,000** memories per preset. A registry MUST enforce this at publish; a pod
MAY warn and truncate rather than fail an install.

Seed memories become the preset's **base layer**, built once per `preset@version` and
shared by every instance of it. They are `Source::Seeded` and are never written by a
running agent.

---

## 7. `vectors.bin` — optional precomputed embeddings

A registry that embeds seed memories for its own search SHOULD write those vectors into
the archive. On a 5,000-memory pack this is the difference between an instant install
and a visible one.

Format: the same fixed-shape record file the pod's `memory::vectors` module writes.

A pod MUST use it only when **both** `embed_model` and `dims` in the preset's `memories`
block match what it would produce itself. A mismatch is ignored silently and the base is
rebuilt. If no embedder is available at all, the base is built BM25-only and
`backfill_embeddings` fills in later — **recall degrades, never fails.**

---

## 8. Flows in a pack

A pack MAY ship `flows/<id>.json`. Two rules, both absolute:

1. **Install never arms a schedule.** Flows land with every schedule disabled, and the
   install report says how many. Installing an identity must not silently start
   background work.
2. **A flow may only name personas from its preset's callable roster** (§4.1) — the
   flow-level default, any node's `data.persona`, and any schedule's `persona`. A flow
   naming a persona outside the roster fails the install, naming both.

`FlowScheduleSpec.instance` is **pod-local and MUST be stripped** on export and on
publish. A downloaded flow arriving with somebody else's instance id is a bug of the
same class as shipping a key.

---

## 9. `SIGNATURE`

Detached, over the exact bytes of `agent_pack.json`, base64 in a single line.

Each host publishes its key at `/.well-known/agent-pack-signing.json`:

```jsonc
{ "keys": [{ "kid": "axoniac-2026-08", "alg": "ed25519", "public_key": "base64…" }] }
```

**Ship the field in v1, enforce in v2.** Adding a signature slot later is a breaking
change; ignoring an unverifiable one now costs nothing. A pack signed by one host does
not validate when served from another — that is what makes "same id on two hosts" safe
rather than a substitution attack (§11.3).

---

## 10. Validation — the conformance checklist

Every implementor that **accepts** an agent pack (a pod installing, a registry
publishing) MUST perform all of these, and MUST report *all* failures at once rather
than stopping at the first — an author fixing one error at a time per round-trip is the
experience this rule exists to prevent.

| # | Check | Failure |
|---|---|---|
| V1 | Every archive path is safe (§2.1) | reject |
| V2 | Decompressed total ≤ 64 MiB, measured on bytes read (§2.2) | reject |
| V3 | `agent_pack.json` present at the archive root and parses | reject |
| V4 | `manifest_version == 2` | reject |
| V5 | `id` matches §3.1 | reject |
| V6 | `version` parses as semver | reject |
| V7 | `content_sha256`, if present, matches the recomputed hash (§3.3) | reject **before any write** |
| V8 | `presets.len() == 1` (§3.2) | reject |
| V9 | The declared preset file exists in the archive and parses | reject |
| V10 | Preset passes §4.2 | reject |
| V11 | Every persona in the callable roster exists at `personas/<slug>.json` | reject, naming it |
| V12 | Every persona's `integrations[]` ⊆ the preset's `integrations` | reject, naming both |
| V13 | Every skill the preset declares **and every skill its roster personas load** exists at `skills/<slug>.md` | reject, naming it |
| V14 | Every integration the preset declares is vendored in the archive | reject, naming it |
| V15 | Every `provides.integrations[].content_sha256`, where present, matches the vendored bytes | reject, showing both hashes |
| V16 | Every shipped flow names only roster personas (§8) | reject, naming both |
| V17 | ≤ 5,000 seed memories per preset (§6) | registry: reject · pod: warn |

V12 is the containment rule. It is cheap, it is what makes the consent summary complete,
and it is why a pack cannot reach anywhere it did not disclose.

V15 is the check whose absence on one side is exactly the divergence this document
exists to prevent: a registry that skips it serves packs every pod refuses, and the
author finds out from a download.

### 10.1 Conformance tests

| Check | `metalcraft-agent` | `axoniac-prime` |
|---|---|---|
| V1 | `bundle::tests::path_traversal_is_refused` | `bundle::tests::path_traversal_is_refused` |
| V2 | *(cap enforced in `Bundle::read`)* | `bomb_tests::a_zip_bomb_is_refused_rather_than_inflated` |
| V5 | `bundle::tests::ids_are_constrained` | `bundle::tests::ids_are_constrained` |
| V7 | `agent_pack_install_test` (“tampering”) | `bundle::tests::a_tampered_archive_is_refused` |
| V8 | `agent_pack_install_test` (“two presets”) | `bundle::tests::a_pack_with_two_presets_is_refused` |
| V12 | `agent_pack_install_test` (“containment”) | `bundle::tests::a_persona_reaching_outside_its_presets_packs_is_rejected` |
| V15 | `agent_pack_install_test::…a_vendored_pack_that_does_not_match_its_pin` | `bundle::tests::a_vendored_pack_that_does_not_match_its_pin_is_refused` |
| V16 | `agent_pack_install_test` (flow containment) | `bundle::tests::a_flow_naming_a_persona_outside_the_roster_is_refused` |
| V17 | *(warn — `Bundle::validate`)* | `bundle::tests::too_many_seed_memories_are_refused` |

Matching test names across repos is deliberate: a check added on one side has an
obvious place to land on the other, and `grep` finds the pair.

Two rules are asymmetric by design, and the table shows it. **V15** was the one the
registry did not enforce, so it published archives every pod refused; it is now checked
on both sides. **V17** is a hard reject at publish and a warning at install, because a
pod already has the bytes and a noisier agent beats a failed install.

---

## 11. The registry protocol

**Registries are a protocol, not a host.** axoniac.com is the social discovery host and
the only one configured by default; a company may self-host, and a pod treats every host
as interchangeable — the crates.io alternative-registries model.

`packs.metalcraftai.com` is **not** one of these. It serves *integration* packs at
`/api/v1/packs/*` — a different unit, reached by a different client — and answers 404 to
every path below. Configuring a host that cannot answer is worse than configuring none:
it offers a browse tab that can only ever be empty, and it makes §11.3's ambiguity check
consult a host with no opinion.

### 11.1 The four endpoints

```
GET /api/v1/agent-packs/{id}/version    → { id, handle?, version, content_sha256 }
GET /api/v1/agent-packs/{id}/manifest   → the raw agent_pack.json
GET /api/v1/agent-packs/{id}/download   → the .agentpack bytes
GET /api/v1/agent-packs/search?q=&limit= → { results: [ … ] }        (optional)
```

`{id}` is the pack's registry handle where it has one, falling back to its manifest `id`.
A host MAY accept both.

- **`/version`** is the update check. It MUST be cheap enough to poll and MUST NOT
  require auth for a public pack.
- **`/manifest`** lets a client show what a pack contains without downloading it.
- **`/download`** MUST serve `application/zip`. Bundles are immutable and content
  addressed, so it SHOULD send `Cache-Control: public, max-age=31536000, immutable` and
  an `ETag` equal to the content hash.
- **`/search`** is optional; a host may be fetch-only. A search result carries at
  minimum `{ handle, name, version, tagline?, category?, tags[] }`.
- **`POST /{id}/installed`** is optional, and a pod calls it after an install it
  completed *from that host* — never after an inspect, an upload, or a local path. It
  is a **soft signal**: unauthenticated (installing a public pack is unauthenticated,
  and counting only linked accounts would be a worse number), and a pod sends it fire
  and forget. A host that does not implement it answers 404, which is not an error.
  The alternative is the number this replaced: a count nothing ever reported, ordering
  listings by zero while looking like a signal.

A private or unlisted pack MUST 404 rather than 403 for a viewer who cannot see it —
never leak that it exists.

### 11.2 `<data>/registries.json` (pod side)

```jsonc
{
  "default": "axoniac",
  "registries": {
    "axoniac": { "url": "https://axoniac.com",          "trust": "verified-only" },
    "acme":    { "url": "https://agents.acme.internal", "trust": "explicit",
                 "token_key": "ACME_TOKEN" }
  }
}
```

| `trust` | Meaning |
|---|---|
| `first-party` | installs with the ordinary approval prompt |
| `verified-only` | refuses a pack the host has not marked verified, unless overridden |
| `explicit` | the user added this host by hand; prompt on every install |

### 11.3a Connecting a pod to a host (optional)

The four endpoints are anonymous for a public pack, so nothing above requires a
credential. A host that also serves **private** packs, or that wants to say which
account a pod belongs to, MAY answer:

```
GET /api/v1/whoami   (Bearer)  → 200 { linked, email? }        this pod's account
                               → 403 { error, link_url }       no account claims this token
                               → 404                            the host has no identity layer
```

A pod connects by pointing a registry entry's `token_key` at a credential it already
holds — on Metalcraft that is `METALCRAFT_TOKEN`, injected by the control plane. **No
credential is minted for a registry**, and the pod sends one only to hosts explicitly
configured with a `token_key`.

The `403` is the load-bearing case, and it is why this is a distinct status rather than
a bare refusal: it means *the token is good and no account has claimed it*, which is the
one state a human can fix in one click. `link_url` is where that click goes. A pod MUST
show the host's own `link_url` rather than construct one.

### 11.3 Reference resolution

- `@amy_kitchen` resolves against `default`.
- `axoniac:@amy_kitchen` is explicit.
- A bare `https://…` URL is allowed only if its origin belongs to a configured registry.

**An id present in more than one configured registry is an error unless qualified.**
Never a silent first match — that is the supply-chain substitution attack, and it is the
reason this rule is normative rather than advisory.

Redirects MUST NOT be followed: a redirect is how an allowed origin gets used to reach
one that isn't. Userinfo (`https://evil@allowed.example/`) MUST be stripped before the
origin is compared.

---

## 12. Install semantics

### 12.1 Order

1. Read and verify the archive (§10) **entirely in memory**. Nothing is written until
   every check has passed.
2. Vendor integrations into `<data>/integration_store/<sha256>/`, refcounted. Ten packs
   vendoring the same integration means one stored copy, and two versions coexist
   without conflict.
3. Write the pack's own files to `<data>/agent_packs/<id>/`.
4. Build the preset's memory base at `<data>/memory/presets/<slug>@<version>/`.
5. Record in `<data>/agent_packs.json` and the lockfile as `(id, version, sha, source)`.
6. Report: presets, personas, skills, packs stored/deduplicated, missing credentials,
   flows installed-unscheduled, memories indexed, vectors reused or built, slug
   collisions, unmet model floor.

### 12.2 Upgrading in place

New files are written **before** withdrawn ones are removed. A reader is not
synchronised with the installer, so clearing the directory first would make an in-flight
turn lose its agent's personas mid-turn. Every path that resolved before still resolves
throughout; only genuinely withdrawn ones stop.

Store garbage collection runs **after** the new refs are recorded — "garbage" means "not
referenced by any installed pack", and that question has the wrong answer until the new
refs are visible. GC MUST be skipped entirely when any installed pack's manifest is
unreadable, or one corrupt pack makes installing an unrelated one delete the corrupt
pack's content for good.

### 12.3 Updates: live instances follow the pack

Updating is **explicit and approval-gated**. There is no auto-update; nothing changes
underneath a running agent because someone published.

Once updated:

| Element | Behaviour |
|---|---|
| Persona prompts, tools, skills | **Follow.** The instance resolves against the installed version. |
| Preset roster, declared packs | **Follow.** |
| Seed memories | **Follow, additively.** The base pointer moves to `<slug>@<new>`; new records appear, tombstoned ones stay forgotten, the delta is untouched. |
| Learned memories, conversations, name, `persistent` | **Never touched.** |

Two edge rules, both reported:

- **A persona an instance is currently using was withdrawn** → the instance falls back
  to the preset's new `default_persona`, and that is recorded on the instance record.
- **A preset an instance uses was withdrawn** → the instance is **orphaned**: it keeps
  its delta and its conversations, resolves against a frozen copy of the preset written
  at orphan time, and is flagged. Never silently deleted; somebody's agent is in there.

### 12.4 Uninstall

Refuses while any **persistent** instance references one of the pack's presets, listing
them. `force` orphans them deliberately (§12.3).

---

## 13. Memory layering (informative)

Included because §7 and §12.3 are only coherent alongside it.

```
<data>/memory/
  presets/<slug>@<version>/    BASE  — built once at install, immutable, shared, refcounted
  instances/<instance-id>/     DELTA — this agent's own; wal, snapshot, tombstones
```

Recall queries both and fuses. A write goes to the delta. Editing a base memory
materialises that one record into the delta, shadowing it by id. Forgetting a base
memory writes a **tombstone**, which lives in the delta and therefore survives the base
being repointed at a new pack version.

Instance creation is O(1): a pointer and an empty delta. Twenty instances of one preset
hold one copy of its vectors, on disk and in RAM.

---

## 14. Changing this document

1. Update this spec first, including §10's table and §10.1's test names.
2. Land the check in every implementor, using the named test.
3. A rule that only one side enforces is a bug in this process, not a difference of
   opinion — §10 V15 is what that looks like when it happens.

Version this document by its `Spec version` header. Adding an optional field or a new
`trust` level is a minor change. Adding a required field, tightening a rule, or changing
`canonical_sha256` is a new spec version and needs a `manifest_version` bump.
