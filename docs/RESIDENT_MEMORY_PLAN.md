# Resident memory

What this process holds for the life of a pod, why each part of it grew without
a ceiling, and the order the ceilings go in.

The agent is a daemon. A CLI run that leaks is a CLI run that exits; a pod that
leaks is a pod that gets OOM-killed at 3am, restarts, and tells nobody which of
its three growing things did it. Everything below is in service of two
questions an operator could not previously answer: **which one grew**, and
**since when**.

## What was unbounded

| Where | What grows | Bound by |
|---|---|---|
| `workshop_api::chat_store` | Every chat ever loaded stays resident; startup loads all of them | nothing |
| `ChatSession::archived` | Every reset moves the closed context here; nothing ever moves it out | nothing |
| `ChatSession::running` | Rebuilt as a full wire copy of the transcript on **every executor step** | transcript length |
| turn execution | `state_before_turn = agent_state.clone()` — a full transcript clone per turn | transcript length |
| `diagnostics::log_turn` | Wrote the **whole message list** after every step → quadratic in session length | nothing |
| `diagnostics::log_llm_request` | Full context snapshot per LLM call, built as a `Value` and a pretty `String` | nothing |
| `chat_broadcasters` | Append-only: a 64-slot `ChatEvent` ring per chat id ever touched | nothing |
| `memory::capture` | `read_all` loads the entire queue, then `pending` builds a second `Vec` | nothing |
| memory indexes | Budgeted by instance count, not bytes | nothing |
| HTTP bodies | No `DefaultBodyLimit` | axum's 2 MB default only |
| tool results | Per-tool truncation where the author remembered; nothing global | nothing |

Measured on the synthetic harness (`tests/memory_bounds_test.rs`), a 1 000-step
session wrote **~1 030 MB** of diagnostics under the old full-history writer.
The same session now writes **~2.4 MB**.

## PR 1 — stop the bleeding (done)

Deliberately the smallest change that removes the unbounded growth, with no
redesign of turn ownership or of any persistence format. Everything here is
either a ceiling or a measurement.

- **Diagnostics are a delta.** `log_turn` records only what a step *added*, with
  `first_index` placing it; concatenating `turn_NNN.json` in order rebuilds the
  full history exactly as before. Compaction rewrites the list rather than
  appending, so a shorter list resets the cursor and the file is marked
  `rewritten` — a reader discards what it had rather than appending to it.
- **Diagnostics are bounded.** Every file is streamed through a limiting writer
  under `MAX_DIAGNOSTIC_FILE_BYTES` (512 KiB). Streaming is the point:
  serializing first and measuring afterwards would bound the disk and leave the
  memory spike where it was.
- **Session directories no longer collide.** They are named for the second the
  session started, and `create_dir_all` succeeded on an existing one — so two
  chats opening in the same second shared a directory and overwrote each other's
  turn files. Now the second one takes a `-2` suffix.
- **Tool results have a ceiling.** `CappedTool` wraps every registration
  (`register_capped`), so a result over `MAX_TOOL_RESULT_BYTES` (256 KiB) becomes
  a middle-elided preview that says so. This matters more than its size suggests:
  a tool result is appended to `AgentState`, persisted with the chat, and
  replayed into every later request in the turn.
- **Request bodies have a ceiling.** `MAX_REQUEST_BODY_BYTES` (16 MiB), applied
  to the whole router so it covers the unauthenticated `/webhook/gateway` too.
- **Event buses are reclaimed.** Dropped when the last SSE subscriber leaves,
  dropped outright when a chat is deleted, and swept every 10 minutes for
  whatever those two miss.
- **All of it is visible.** `GET /api/v1/metrics` reports resident chat count,
  context/archived/in-flight bytes, turns in flight, bus count vs. subscribed
  count, diagnostics bytes written, tool truncations, and RSS.
- **Toolchain pinned.** `rust-toolchain.toml` at 1.91, matching the floor
  `metalcraft-flows` 0.6 and `metalcraft-packs` 0.2 already declare.

Every limit reads an env override once, so a pod can be given more room without
a rebuild. A `0` means unbounded, not zero.

### Not done in PR 1, on purpose

`ChatSession::running` is still rebuilt in full on every step
(`workshop_api::snapshotting`), and a turn still clones the whole state for
rollback. Both are per-turn transient rather than resident, and fixing them is
PR 4 — it changes who owns the context during a turn, which is the one part of
this that can break cancellation and reconnect.

## PR 2 — bound chat residency

- Replace load-everything-at-startup with metadata discovery plus lazy load.
- Count/byte/idle-LRU limits on `ChatStore`; never evict a session that is busy
  or has a subscriber.
- Bound `archived` in RAM while keeping the full transcript on disk.

## PR 3 — bound the memory subsystem

- Process captures in bounded batches instead of whole-file load plus a second
  `Vec`.
- Budget the indexes by bytes rather than by instance count, with disk
  rehydration.

## PR 4 — remove full-turn duplication

Revision-based state and turn-local deltas in place of the full clone and the
full per-step snapshot. Highest value, highest risk: it touches concurrency,
cancellation, persistence, and reconnect together.

## Open questions, and what the code already answers

Four of the six questions the investigation raised are settled by reading the
code; they are recorded here so PR 2 does not re-ask them.

- **Is persisted chat authoritative and safe for lazy reload?** Largely yes, and
  by existing design. `list_chats` already reads the catalog from
  `<data>/chats/*.json` rather than the store, with a comment saying the
  in-memory map is the authority only for *live per-turn* state. `instance_of_chat`
  already falls back to the persisted file for a chat that was never resident.
- **Can multiple turns run concurrently in one chat?** No. `ChatSession::busy`
  refuses a second turn, and a message that arrives mid-turn joins the running
  one through `pending` (`POST /chats/{id}/turn` answers 202). Ordering is FIFO
  within a chat.
- **Which diagnostics consumers need full per-step history?** None require the
  full list *per file*. `diagnostics_browse` renders each file as an opaque JSON
  timeline event, and the `diagnostics_*` meta tools read through it.
- **Can streaming clients resume from deltas?** Not today — `GET
  /chats/{id}/events` is a live broadcast with no sequence number, and a lagged
  subscriber silently skips. Per-chat sequencing is the subject of
  `ONE_CHAT_STREAM_PLAN.md`, and PR 4 should be sequenced after it rather than
  inventing a second scheme.

Still genuinely open, and both needed before PR 2 is designed:

- **Must the API return the entire transcript synchronously?** `GET /chats/{id}`
  returns `archived + context` in one body today. Paginating it is a
  client-visible contract change across the Workshop, the Tauri front, and iOS.
- **What are the deployment RSS limits, and what disk-read latency is
  acceptable** when a lazily-evicted chat is reopened?
