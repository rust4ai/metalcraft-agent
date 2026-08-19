# Workshop UIs — the agent-presets update (AP7)

> Two clients, one API. This is the missing scoping pass for **AP7** in
> `metalcraft-agent/docs/AGENT_PRESETS_PLAN.md` §9 — the phase that lands in neither
> of the two plan docs because it lives in neither repo.
>
> | | Repo | Shape | Reaches the pod by |
> |---|---|---|---|
> | **Desktop** | `metalcraft-workshop` | Tauri (`crates/workshop-tauri`) + React/Vite frontend | a local project dir, **or** a minted `pod:{slug}` connection token |
> | **Web** | `metalcraft-workshop-web` | Rust/Axum stateless auth-aware proxy serving an embedded React SPA | `mc_session` cookie → proxy → per-pod `workshop_api_key`, held server-side |
>
> Both frontends generate their client types the same way:
> `npm run gen:types` → `openapi-typescript openapi.json`. **That is the seam this
> whole plan hangs on** — the agent's `/api/v1/openapi.json` is the contract, and every
> new surface below is already annotated with `#[utoipa::path]`, so step one is
> mechanical for both clients.

---

## 1. What changed underneath them

Shipped on `metalcraft-agent`'s `prime` branch (AP2–AP4):

| New | Endpoint | Notes |
|---|---|---|
| Agent presets | `GET /api/v1/agent-presets`, `GET/PUT/DELETE …/{slug}` | `GET …/{slug}` returns the preset **plus its resolved roster**, each persona flagged `installed: true/false` |
| Agent instances | `GET/POST /api/v1/agents/instances`, `GET/DELETE …/{id}` | `GET …/{id}` returns the instance and its conversations; list carries `conversation_count` |
| Chat creation | `POST /api/v1/chats` gains `agent_preset`, `instance_id`, `name`; `persona_slug` is now **optional** | an explicit persona is validated against the preset roster |
| Chat records | every chat carries `instance_id` | backfilled at startup for legacy chats |

Plus the four API gaps this plan originally listed as blockers — all now shipped and in
the spec; see §6. Still pod-side and not exposed: agent-pack install (AP5/AP6) and
flow↔preset binding.

---

## 2. The one screen that matters

**Starting a chat should ask which agent, not which persona.** Today both clients
show a persona picker; the persona is an implementation detail of a preset, and every
other change here is secondary to this one.

```
  ┌─ New chat ─────────────────────────────────┐
  │  Agent                                      │
  │  ┌───────────────┐  ┌───────────────┐       │
  │  │ General Agent │  │ Amy's Kitchen │  …    │
  │  │ orchestrator  │  │ 6 personas    │       │
  │  └───────────────┘  └───────────────┘       │
  │                                             │
  │  ▸ Advanced — start as a specific persona   │
  │      (only this agent's roster)             │
  └─────────────────────────────────────────────┘
```

- Default selection: `general-agent`, which behaves exactly as today.
- The persona picker survives, demoted behind **Advanced**, and its options come from
  `GET /api/v1/agent-presets/{slug}` → `personas[]` rather than the pod-wide persona
  list. A persona with `installed: false` renders disabled with its error — that's the
  UI surfacing "this preset names a persona your pod doesn't have", which the API
  already answers and neither client can currently show.

---

## 3. Per-client work

### 3.1 Shared first step (both repos, ~30 minutes)

```bash
curl -s https://<pod>/api/v1/openapi.json > frontend/openapi.json   # or from a local run
npm run gen:types
```

Every endpoint below is already `#[utoipa::path]`-annotated on `prime`, so the types
appear without hand-writing a line. Do this before anything else — it turns the rest
into type errors that point at exactly what needs changing.

### 3.2 Desktop — `metalcraft-workshop`

`crates/workshop-tauri/frontend/src/components/`:

| Component | Change |
|---|---|
| `ChatsView.tsx` | The agent picker (§2). Chat list groups by instance; a chat shows which agent it belongs to. |
| `Sidebar.tsx` | New **Agents** entry, above Personas. Personas stay — authoring them is still a real task, it's just no longer the entry point. |
| **`AgentsView.tsx`** *(new)* | Two panes: **Presets** (what this pod can be) and **Instances** (agents that actually exist, with conversation counts and a persistent/ephemeral marker). Create, rename, delete. |
| `PersonasView.tsx` | Show which presets reference each persona — the reverse lookup, so deleting one isn't a guess. Mark pack-provided personas read-only (`read_only` already in the summary). |
| `PacksView.tsx` | Unchanged for now; becomes the agent-pack browser at AP6. |

The desktop app also opens a **local project directory** — so it can read
`<data>/agent_presets/*.json` and `<data>/agent_instances/*/instance.json` straight
off disk, exactly as it already does for `personas/*.json`. That path needs the same
two views without any API at all, which is worth remembering: the desktop client has
*two* data sources and both must learn presets.

### 3.3 Web — `metalcraft-workshop-web`

`frontend/src/views/`:

| View | Change |
|---|---|
| `Chat.tsx` | The agent picker (§2). |
| **`Agents.tsx`** *(new)* | Same two panes as desktop, read-mostly first: list presets, list instances, start a conversation with one. Create/delete can follow. |
| `Workshop.tsx` | Route + nav entry. |
| `Sessions.tsx` | Show the instance a session belongs to. |

**The proxy needs nothing new.** It forwards `/api/v1/*` to the pod with the per-pod
key; the new routes are under that prefix and pass through unchanged. Worth verifying
rather than assuming — if there's an allowlist of proxied paths, `/api/v1/agents/*`
and `/api/v1/agent-presets/*` have to be on it.

### 3.4 Mobile — `metalcraft-mobile`

Out of scope here, but it talks to the same API and will show a persona picker that no
longer matches the model. Flag it; don't let it silently drift.

---

## 4. Sequencing

| Step | Both clients | Depends on |
|---|---|---|
| **W1** | Regenerate types | agent `prime` deployed to a reachable pod |
| **W2** | Agent picker on new-chat, preset-scoped persona list behind Advanced | W1 |
| **W3** | Agents view — presets and instances, create/rename/delete | W1 |
| **W4** | Chats grouped by instance; instance shown on a session | W3 |
| **W5** | Agent-pack browser + install dialog with the permission summary | agent **AP6** |
| **W6** | Per-instance memory view — what this agent knows, base vs learned | `…/instances/{id}/memory` (shipped, §6) |

W1–W4 **and W6** are shippable now against `prime`. Only W5 waits on the agent.

Do **desktop first**: it has both data sources (local dir + pod), so it flushes out
model problems the web client would hit later. Then port to web, where the views are
simpler because there's no local-directory path.

---

## 5. Design notes worth getting right

**Don't call an instance an "instance" in the UI.** It's an *agent*. "Amy — Sunday
prep" is an agent; the fact that it's an instance of a preset is our vocabulary, not
the user's. Reserve "preset" for the authoring surface, where it genuinely is the
template being edited.

**Ephemeral agents should be invisible until they aren't.** Every new chat mints an
instance, so an unfiltered list is one row per chat ever started — noise. Show
persistent agents by default; "show all" reveals the rest. Naming an agent is the
action that promotes it, and that should be one click from the chat header.

**A channel-bound agent needs a badge.** `origin: {kind: "gateway", channel}` means
messages arrive without anyone watching. That agent is doing things on its own and the
list should say so.

**Delete needs to say what it keeps.** `DELETE /api/v1/agents/instances/{id}` returns
`conversations_kept` — deliberately, because losing an agent must not lose transcripts.
Say that in the dialog rather than making the user guess.

---

## 6. What the agent owed the UIs — now shipped

All four are on `prime` and **in the generated spec** (50 paths total; verified by
`cargo run --example dump_openapi`). W3 is no longer blocked, and W6 is unblocked too:

| Was missing | Now |
|---|---|
| No per-instance memory read | `GET /api/v1/agents/instances/{id}/memory?limit=` → `{ base, shipped, learned, forgotten, sample[] }`, each sample tagged `origin: "shipped" \| "learned"`. Read-only: it does **not** touch access counts, so looking at what an agent knows can't skew its decay curve. |
| No rename | `PATCH /api/v1/agents/instances/{id}` — `name`, `persistent`, `persona`. Setting a name also sets `persistent`, which is the one-click promotion §5 asks for. A `persona` outside the preset roster is rejected. |
| Snapshot lacked agents | `GET /api/v1/snapshot` now carries `agent_presets`, `agent_instances` (persistent only — the rest is noise), `default_agent_preset`, and both new dirs in `layout`. One round-trip paints the picker. |
| Conversation-create undiscoverable | `POST /api/v1/agents/instances/{id}/conversations` — delegates to the chat-create path, so it's the same operation, just discoverable from the agent you're looking at. |

Also confirmed in the spec: **`CreateChatRequest.persona_slug` is no longer required**
(`required: None`), and `agent_preset` / `instance_id` / `name` are present — which is
precisely the contract W2's picker needs.

### Still genuinely outstanding

- **Agent-pack install** (agent AP5/AP6) — blocks W5.
- **Flow ↔ preset binding** (`docs/FLOWS_AND_AGENT_PRESETS_PLAN.md`) — until it lands,
  a flow's persona list is still pod-wide, so any flow editor UI shows a roster that
  the preset model says shouldn't exist.
- **`metalcraft-mobile`** will show a persona picker that no longer matches the model.
