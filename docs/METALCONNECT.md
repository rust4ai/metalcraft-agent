# MetalConnect

**The pattern: a Metalcraft subapp is connected to a pod in one click, and what
lands on the pod is a narrow app key the user never sees or types.**

Connecting an agent to a service has a traditional shape: open the service, find
the API keys page, create a key, choose scopes, copy it, come back, paste it,
hope it was copied whole. Every one of those steps still happens under
MetalConnect. None of them is the user's to do.

This is possible only inside the Metalcraft ecosystem, and for one reason: the
pod, the desktop app, and every subapp all authenticate the *same* Metalcraft ID
credential. A shared identity turns "the user must carry a secret between two
services" into "one service asks the other for a secret on the user's behalf."

The reference implementation is the Octaweave connection in **metalcraft-front**
(`crates/front-tauri/src/rpc/octaweave.rs`, `crates/front-cloud/src/octaweave.rs`).
This document is the generalisation.

---

## The two properties that define it

Everything else here is mechanics. These two are the pattern.

### 1. The pod holds an app key, never the PAT

An `mck_…` PAT names a **person**. It reaches every workspace they belong to and
every other Metalcraft subapp besides. A minted app key (`owk_…` in Octaweave's
case) names **one workspace in one subapp**, carries a fixed scope list, and can
never reach sideways — the subapp's authorizer checks its pinned workspace before
any lookup.

Both would work as a bearer token. The pack sends whatever is in its env var and
the subapp accepts either. That is exactly why the choice has to be deliberate:
the lazy path and the correct path are indistinguishable at runtime and differ
enormously in blast radius.

So the PAT is **bootstrap authority only**. It authenticates the mint call and is
then done. It is never written to the pod's key store, never handed to a pack,
never in scope for a tool call.

> **Why the PAT cannot be eliminated entirely.** A key may not mint another key —
> Octaweave's `keys::create` refuses `principal.is_api_key()`. That refusal is
> what makes an escaped app key non-escalating, and it is also why bootstrapping
> requires a person-scoped credential. The PAT is the smallest thing that can
> start the chain; the point is to end the chain somewhere smaller.

### 2. The app installs the key into the pod's key store

The minted key travels **service → connecting process → pod key store**. It does
not pass through a UI, a clipboard, or a webview. In the desktop reference
implementation the Rust side does the mint and the `PUT /api/v1/keys/{NAME}`; the
React side receives a workspace name and a scope list and no credential at all.

The user's entire contribution is a click. Not "a click instead of typing a key" —
a click instead of the *whole errand*.

The key name is not a preference. It is whatever the integration pack declares in
`requires_env` (`OCTAWEAVE_API_KEY`), because the pack will not resolve it under
any other name. Connect finishes by installing the pack itself
(`POST /api/v1/integrations/install`), so one action produces a working
capability rather than a credential and a to-do.

---

## The flow

```
                 mck_ PAT (keychain, or the pod's injected METALCRAFT_TOKEN)
                        │
   [1] check the pod ───┤   nothing is minted if the result has nowhere to land
                        │
   [2] GET  /api/v1/workspaces          401 here == "not linked yet"
                        │
   [3] pick a workspace                 admin-only; 1 → auto, N → ask
                        │
   [4] revoke ours                      best-effort, BEFORE the mint
                        │
   [5] POST /api/v1/w/{ws}/keys         → owk_ key, named scopes
                        │
   [6] GET  /api/v1/whoami  (as owk_)   prove it authenticates
                        │
   [7] PUT  {pod}/api/v1/keys/{NAME}    the app key lands on the pod
                        │
   [8] POST {pod}/api/v1/integrations/install {slug}
```

### The ordering is load-bearing

Each of these is a real failure someone would otherwise hit in production.

- **[1] before [5]** — a key minted and then not storable is a live credential
  nobody holds, and the subapp has no idea it was born orphaned.
- **[4] before [5]** — reconnecting must *replace* its predecessor, not pile a
  second live key beside it. This works because the key label is fixed
  (`"Metalcraft agent"`), not timestamped: anything the sweep misses is still
  visibly ours in the subapp's own keys page.
- **[6] between [5] and [7]** — minting succeeding is not the same claim as the
  key authenticating. Close the gap for the price of one request, here, rather
  than discovering it mid-conversation three days later.
- **[6] fails → revoke immediately.** The key exists on the service and is about
  to be unreachable from here. Take it back out.
- **[8] failing does not fail the connect.** The key is stored and that is worth
  keeping. Report the halfway state honestly ("key only") instead of a failure
  that invites the user to redo a step that already succeeded.

### Scopes are named modules, never the coarse grant

`notes:write board:write drive:write calendar:write blog:write blog:publish
studio:write search:read` — not `write`, which by the subapp's own definition
covers actions invented after the key was minted.

The cost is honest: a module added later fails with a scope error until the list
grows. That is a better failure than a credential that silently widens. Worth a
unit test asserting the coarse grant is absent, since the diff that adds it looks
like a simplification.

Note what *is* in the list: `blog:publish`. Putting something on the open internet
is a different act from editing a draft, and it belongs behind arming and
approval in the conversation — not behind a withheld scope that surfaces as a
403 halfway through a sentence.

---

## Resumable, not interactive

Two things can interrupt a connect, and a blocking implementation would have to
own a browser and a modal to handle them. Instead, connect **returns what is
missing** and is called again:

```rust
enum ConnectOutcome {
    NeedsLink      { url: String },              // no identity link row yet
    ChooseWorkspace{ workspaces: Vec<Workspace> },// several, and it is not our call
    Connected      { connection: Box<Connection> },
}
```

This is what makes it **pollable**. The first-time case opens the browser at
`{subapp}/link/metalcraft`, then re-calls connect every 2.5s for three minutes
while the user is away. Opening the browser is therefore a *separate* command
from connect — a connect that opened a tab would spray one tab per poll.

Three minutes is where "they got distracted" becomes likelier than "it is about
to work". Giving up beats an endless spinner, and it costs nothing: the link
survives on the service side, and the next Connect picks it up.

Cancel stops the asking, not the linking.

The workspace picker exists because which workspace an agent lives in is a
judgement about someone's life, not a default to compute. Non-admin workspaces
are filtered out at the *listing*, not at the mint, so the picker never offers a
row that would fail on the next click.

---

## What a subapp must provide

Six endpoints, all authenticating `mck_` PATs:

| Endpoint | Auth | Purpose |
|---|---|---|
| `GET /link/metalcraft` | session | Write the identity link row. Path fixed by `SUBAPP_STANDARD.md` §2 — the subapp's own account page derives its Connect button from it. Signed out, it bounces through sign-in and resumes. |
| `GET /api/v1/workspaces` | `mck_` | List reachable workspaces with the caller's role. **401 is the unlinked signal** — the cheapest possible link check. |
| `POST /api/v1/w/{ws}/keys` | `mck_` | Mint. Must refuse when the principal is itself an API key. |
| `GET /api/v1/whoami` | any | Identify a token: actor kind, pinned workspace, scope count. |
| `GET /api/v1/w/{ws}/keys` | `mck_` | List, so reconnect can find its own. |
| `DELETE /api/v1/w/{ws}/keys/{id}` | `mck_` | Revoke. Treat already-gone as success — that is the state we wanted. |

Two shape notes that have bitten before: `whoami.scopes` is a **count**, and it
is null for anything unrestricted; `actor.workspace_id` is null for anything that
is not a pinned API key, because a person's token has no single workspace to
report. Model both as nullable or the hub-token case panics on deserialize.

**Unlinking is instant and total.** However valid a `mck_` token is, it resolves
only while the identity row exists. Deleting the row is a complete revocation of
the bootstrap path without touching any credential.

## What the pod must provide

Already true today:

- `PUT /api/v1/keys/{name}` — upsert a secret (`key_store.rs`).
- `POST /api/v1/integrations/install {slug}` — the pod fetches the pack from
  `packs.metalcraftai.com` itself; the connecting app never downloads a pack, and
  enabling is part of installing (`workshop_api.rs:774`).
- `GET /api/v1/integrations` — so the UI can distinguish *installed*, *installed
  but disabled* (tools exist and will never fire — indistinguishable from absent
  inside a conversation), and *absent*.

---

## Who drives, and the two variants

The pattern does not care which process holds the bootstrap PAT. Two variants
exist in the ecosystem today:

**App-driven** (Octaweave, in metalcraft-front). The desktop holds the PAT in the
OS keychain, mints, and writes the result to whichever pod is connected. Best
when the user is present and a workspace choice may be needed.

**Pod-driven** (Metalcraft Gateway, `src/metalcraft_gateway.rs`). The pod holds
its own `METALCRAFT_TOKEN` injected by the k3s control plane and connects itself
with no client involved. Best for something the pod needs whether or not anyone
is looking at a UI.

The gateway connect is the older sibling and only **partially** MetalConnect: it
gets property 2 right (nothing is pasted — it fetches base URL, integration id
and webhook secret itself) but historically wrote the pod's broad
`METALCRAFT_TOKEN` as the channel's `API_KEY`. It now prefers an adopted
connection token and falls back to the PAT only when none is present. Closing
that fallback is what would make it fully MetalConnect, and it is the same fix in
kind as `SCOPED_KEYS_PLAN.md`: connect-managed credentials should be narrow,
scoped to the thing that owns them, and rendered in the Keys UI as managed rather
than as loose globals the user might hand-edit and silently break.

---

## Disconnect

Deleting the key from the pod alone leaves a working credential on the service
that nothing holds and nothing displays. That is not what the button means to
anyone pressing it.

1. `DELETE {pod}/api/v1/keys/{NAME}` — first, because a pod that dropped the key
   is disconnected whether or not the service was reachable to hear about it.
2. Revoke at the service — best-effort, second.

The pack stays installed. Its tools are inert without a key, and reinstalling one
to reconnect would be a surprising amount of work for what was asked.

---

## Checklist for the next subapp

- [ ] Accepts `mck_` PATs as a first-class credential.
- [ ] `GET /link/metalcraft` exists and resumes through sign-in.
- [ ] An API key cannot mint an API key.
- [ ] Scopes are named modules; the coarse grant is not requested.
- [ ] The key label is fixed, so reconnect replaces rather than accumulates.
- [ ] Connect returns `needs_link` / `choose_workspace` / `connected` — it never
      blocks on a human.
- [ ] Opening the browser is a separate call from connect.
- [ ] Pod reachability is checked before anything is minted.
- [ ] The minted key is verified before it is stored, and revoked if it fails.
- [ ] The key name matches the pack's `requires_env` exactly.
- [ ] No credential crosses into any UI layer.
- [ ] Disconnect revokes as well as deletes.
