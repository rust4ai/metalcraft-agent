# Chat Error-Response System

Status: **approved, implementing** · Owner: agent chat sessions · Date: 2026-08-04

## Problem

When an agent chat turn fails — most importantly when the inference gateway
(`inference.metalcraftai.com`) rejects the request for **insufficient credits**
or a **lapsed premium subscription** — the failure is not surfaced to the end
user. A structured error exists at the source and is then progressively
discarded:

1. **Inference** returns a clean `402 {"error":"insufficient credits — top up or upgrade"}`
   — but with **no machine-readable code**, only a human string.
   (`metalcraft-inference/src/services/credits.rs:28`, `middleware/auth.rs:59`)
2. **rig** (the agent's OpenAI client, v0.38.1) throws away the HTTP status and
   keeps only the response body text → `CompletionError::ProviderError(String)`.
3. The **metalcraft core** flattens that into `GraphError::Node { message: String }`
   (`metalcraft/src/prebuilt.rs:485`) and surfaces it as
   `RunOutcome::Failed { error: String }` (`metalcraft/src/executor.rs:59`).
4. At the surfaces the string is then:
   - **workshop-web** → **ignored** (`Chat.tsx` `done` handler drops `reason`) → *silent failure*.
   - **Tauri workshop** → shown **raw** (`failed: ProviderError { ... }`).
   - **WhatsApp/SMS gateway** → the turn **never calls the reply sink**, so
     *nothing is sent back to the user* (`run_one_gateway_turn` failure arms only log).

A friendly translator already exists — `out_of_credits_message`
(`metalcraft-agent/src/runtime.rs:338`) — but it is wired into exactly **one**
path (the one-shot `run_task`, `runtime.rs:328`), not chat and not the gateway,
and it collapses the two distinct 402s into one message.

**Goal:** stop discarding the error, give it a stable identity, classify it in
**one** place, and render it consistently on all three surfaces — including an
outbound WhatsApp/SMS reply.

## Locked decisions

| Decision | Choice |
|---|---|
| Error source | **Add a structured `code`** to inference `ApiError` (self-describing wire format); agent parses it, substring-match kept as fallback. |
| SSE shape | **New `ChatEvent::Error { code, message, retryable }`** frame; `Done` keeps meaning "completed/interrupted". |
| Gateway notify policy | **Terminal errors only** — reply to the user for non-retryable errors (insufficient_credits, not_premium); stay silent + log for transient/internal. |
| Taxonomy placement | **`metalcraft-agent`** — no `metalcraft` core change needed. The error string carrying the JSON body flows intact through `RunOutcome::Failed.error` / `GraphError::Node.message`, so the classifier operates on that string. |

### Why no `metalcraft` core change

`metalcraft` is a **published crates.io dependency** (`metalcraft = "0.8.2"` in
`metalcraft-agent/Cargo.toml:11`, no path/patch override). Changing it means
cutting a release. We avoid that entirely: rig keeps the JSON **body** inside
`ProviderError(String)`, the body survives `error_chain` concatenation into
`GraphError::Node.message`, and the agent receives that full string at the
`RunOutcome::Failed` / `Err(e)` boundary. The classifier reads the `code` out of
that string. If a future need arises to preserve the HTTP *status* structurally,
that would require a core change (custom HTTP layer before rig) — explicitly out
of scope here.

## Architecture

```
inference (structured code) ──▶ agent classify_turn_error() ──▶ ChatError ──┬──▶ SSE ChatEvent::Error ──▶ workshop-web / Tauri
   {"error","code"}  402         (parses body string)                        └──▶ gateway reply sink   ──▶ WhatsApp/SMS
```

One classifier, one taxonomy, consumed by both the workshop SSE path and the
gateway path — "the same fundamentals".

### Taxonomy (`metalcraft-agent`)

```rust
pub enum ErrorCode {
    InsufficientCredits, // 402, terminal
    NotPremium,          // 402, terminal
    UpstreamUnavailable, // provider 5xx / network, retryable
    Internal,            // anything unrecognized, retryable, raw text -> diagnostics only
}

pub struct ChatError {
    pub code: ErrorCode,
    pub user_message: String, // safe to show an end user
    pub retryable: bool,
}

/// Classify the flattened error string the agent sees at the turn boundary.
/// 1. Try to parse a JSON object with a `code` field out of the string
///    (the inference body survives inside rig's ProviderError text).
/// 2. Fallback: substring-match known phrases (subsumes out_of_credits_message).
/// 3. Default: Internal — generic user message; raw text goes to diagnostics only.
pub fn classify_turn_error(raw: &str) -> ChatError
```

`user_message` per code:
- `InsufficientCredits` → "You're out of Metalcraft inference credits. Top up or check your plan at https://id.metalcraftai.com/account."
- `NotPremium` → "Metalcraft inference needs an active premium subscription. Check your plan at https://id.metalcraftai.com/account."
- `UpstreamUnavailable` → "The AI service is temporarily unavailable — please try again in a moment."
- `Internal` → "Something went wrong handling that message. Please try again."

`out_of_credits_message` is **removed** and its callsite (`runtime.rs:328`,
one-shot path) is migrated to `classify_turn_error`, so all three run paths
(one-shot, workshop chat, gateway) share one classifier.

## Component changes

### 1. `metalcraft-inference` — emit a code (additive, backward-compatible)

`src/error.rs`:
- Add `pub code: &'static str` to `ApiError`.
- `into_response` emits `{"error": message, "code": code}`.
- Add `payment_required_code(code, msg)` (or dedicated `insufficient_credits` /
  `not_premium` constructors). Existing constructors default `code` to a sane
  value (`bad_request`→`"bad_request"`, `bad_gateway`→`"upstream_unavailable"`,
  `not_found`→`"not_found"`, sqlx→`"internal"`, `payment_required`→`"payment_required"`).
- `services/credits.rs:28` → code `"insufficient_credits"`.
- `middleware/auth.rs:59` → code `"not_premium"`.

Old clients ignore the extra field. Also covers the `/v1/responses` path
(same `ApiError` type; `controllers/responses.rs:122` calls `credits::authorize`).

### 2. `metalcraft-agent` — taxonomy + classifier (`src/runtime.rs`)

- Add `ErrorCode`, `ChatError`, `classify_turn_error`.
- Remove `out_of_credits_message`; migrate `runtime.rs:328` one-shot path.
- Port/extend the existing unit tests (`inference_tests`) to the new classifier,
  covering: JSON body with `code`, legacy substring phrases, non-premium vs
  insufficient-credits disambiguation, and unrelated-error → `Internal`.

### 3. `metalcraft-agent` — workshop SSE path (`src/workshop_api.rs`)

- Add `ChatEvent::Error { code: String, message: String, retryable: bool }`
  to the enum (~`:1587`), serializing as `{"kind":"error", ...}`.
- In `run_chat_turn`'s failure arms:
  - `RunOutcome::Failed` (~`:1998`): keep logging the raw `reason` to
    diagnostics/trace and keep the partial state, but emit `ChatEvent::Error`
    from `classify_turn_error(&error)` instead of `Done{status:"failed"}`.
  - `Err(e)` (~`:2018`): same — classify `error_chain(e)`, roll back state as
    today, emit `ChatEvent::Error`.
- The SSE HTTP response stays 200; the error rides as a frame (unchanged transport).

### 4. `metalcraft-agent` — gateway/WhatsApp path (`src/workshop_api.rs`)

In `run_one_gateway_turn` failure arms (~`:3058` `Failed`, ~`:3064` `Err`):
```rust
let ce = classify_turn_error(&raw);          // raw = "{node}: {error}" or error_chain(e)
if let Some(logger) = &s.diagnostics { logger.log_error(&raw); }  // always log raw
// Terminal-only: don't spam "try again" on transient upstream blips.
if !ce.retryable {
    let _ = sink(ce.user_message.clone()).await;   // the WhatsApp/SMS reply missing today
}
s.state = Some(/* partial on Failed, state_before_turn on Err — unchanged */);
```
Optional de-dupe: remember the last delivered `ErrorCode` per `gateway_chat_id`
so a persistently-out-of-credits sender isn't messaged on every inbound. (Phase
2 — start without it; the sink already records outbound to the activity feed.)

The sink is the same `ReplySink`
(`Arc<dyn Fn(String) -> BoxFuture<'static, Result<(), String>>>`,
`src/tools/mod.rs:36`) that `say_to_user` uses, built by `gateway_reply_sink`
(`:2862`) — so the error reply goes out the exact channel the inbound arrived on.

### 5. Frontends

**workshop-web** (`metalcraft-workshop-web/frontend`):
- `src/lib/pod.ts` `consumeSse` (~`:88`): handle `kind === "error"`.
- `src/types.ts`: add the `error` event type.
- `src/views/Chat.tsx` (~`:127`): on `error`, render an error bubble/banner with
  `message` (today `reason` is dropped → silent failure). Clear the spinner.

**Tauri workshop** (`metalcraft-workshop/crates/workshop-tauri/frontend`):
- `src/components/ChatsView.tsx` (~`:392`): consume the `error` frame → show the
  friendly `message` instead of the raw `reason`.

Un-updated clients receiving an unknown `error` frame must degrade gracefully
(ignore, don't crash) — verify the SSE parsers `default`/no-op on unknown kinds.

## Rollout ordering (each step backward-compatible)

1. **inference `code`** — additive; old agents ignore it.
2. **agent classifier + `ChatEvent::Error` + gateway reply** — fallback
   substring-matching means it works even before inference ships the code, and
   even against un-updated inference.
3. **frontends** — old frontends keep working (they just ignore the new frame,
   as they ignore `reason` today); new frontends render it.

## Testing

- **Unit** (agent): `classify_turn_error` table — JSON-with-code, legacy phrases,
  premium vs credits, unknown→Internal, retryable flags.
- **Unit** (inference): `into_response` includes `code`; the two 402 sites carry
  the right codes.
- **Manual/integration**: force a 402 (zero-credit account) and verify:
  - workshop-web shows an error bubble (not silence);
  - Tauri shows the friendly message;
  - a WhatsApp inbound gets an outbound "out of credits" reply;
  - transient/internal failures do **not** spam the WhatsApp user.

## Out of scope

- Preserving the HTTP *status* structurally through rig (needs a core change).
- Retry/backoff orchestration for `retryable` errors (the flag is defined and
  logged; auto-retry is a later enhancement).
- Per-sender rate-limit/de-dupe of gateway error replies (Phase 2 optional).
