# One chat stream

> **The change in one line:** every frame of every turn goes on the chat's bus, so
> "who is watching this conversation" stops being "whoever started the turn".
>
> **Client half:** `~/ai/metalcraft-mobile` (§5.1), `~/ai/metalcraft-workshop-web`
> (§5.2), `~/ai/metalcraft-front` (§5.3). The pod half can land alone — every
> client keeps working untouched, which is the point of §3.2.

---

## 1. Where this picks up

Two streams carry a conversation today, and they were built for different jobs:

| | Written by | Read by |
|---|---|---|
| `POST /chats/{id}/turn` → SSE response | `post_chat_turn`, into a private `mpsc` nobody else can subscribe to (`src/workshop_api.rs:5672`) | exactly one reader: whoever sent the turn |
| `GET /chats/{id}/events` | a per-chat `broadcast` channel (`src/workshop_api.rs:93`) | any number of subscribers |

The bus is written by the things nobody typed — a flow firing (`record_flow_turn`),
a fired follow-up (`deliver_followup_to_chat`), the queue drain, a gateway turn.
An **interactive** turn is written only to the private channel. That asymmetry is
the whole of this document.

What already landed, and is assumed here:

- A turn no longer empties the conversation while it runs: `ChatSession.running`
  holds the in-flight messages and `transcript_of` prefers them
  (`src/workshop_api.rs:4556`). `ChatDetail.busy` says whether a turn is going.
- iOS reattaches on reopen (`SessionStore.attach`) and, when a chat opens **busy**
  with no stream of its own, re-reads it every 2s until the turn ends
  (`SessionStore.observeIfRunning`).
- The watch reconnects with backoff and re-reads the chat when it comes back
  (`SessionStore.startWatching` / `resync`).

That closes the case that was reported — leave a chat mid-turn, come back, find it
blank. It closes it with a **poll**, which is the tell that the frames are still
going somewhere only one reader can see.

---

## 2. What still fails

Three scenarios, all the same missing edge:

1. **Two devices, one conversation.** Phone and desktop both have the chat open.
   Type on the desktop: the phone shows nothing until it is closed and reopened.
   Its watch is attached and healthy — the frames simply are not on it.
2. **A turn that starts while you are looking.** The poll in `observeIfRunning`
   only starts if the chat was *already* busy when it opened. A turn that begins
   a second later is invisible until the next open.
3. **A fleet view that is honest.** `front-core`'s `subscribe()` says it is what
   "makes a live fleet view possible at all… N open sessions (and a phone) can
   watch one turn" (`crates/front-core/src/pod.rs:1230`). For interactive turns
   that is not true today, and no client has been able to use it for that.

There is also a smaller, sharper bug that the same work fixes. Opening a chat is
*read the transcript, then subscribe*. A frame that lands between those two steps
is in neither — the read was too early for it, the subscription too late. Rare,
silent, and unfixable without §3.3.

---

## 3. Decisions

### 3.1 One stream, not two

The turn's frames go on the bus. A client that wants to follow a conversation
subscribes to the bus and **ignores the body of its own `POST /turn`** — the POST
becomes "start this turn", answered by its status code, not by its stream.

This is the smallest change that closes all three scenarios, because it removes
the concept the bug lives in: there is no longer a turn that "belongs" to one
connection. It also deletes the dedup problem instead of solving it — one
delivery path cannot double-deliver.

The private channel stays for now, fed in parallel, so today's clients keep
working (§3.2). §6 retires it.

### 3.2 Opt-in, so nothing regresses

`GET /chats/{id}/events` keeps its current meaning — **agent-initiated frames
only** — and gains a query parameter:

```
GET /chats/{id}/events?include=all
```

`include=all` adds interactive turns. Default is today's behaviour.

This matters because two shipped clients subscribe to the bus *while* reading
their own turn stream: iOS (`SessionStore.startWatching` + `send`) and
workshop-web (`subscribeEvents` + `postTurn`, `frontend/src/lib/pod.ts:109`).
Publishing turn frames unconditionally would draw every turn twice in both, on
pods they did not ask to be upgraded to. An opt-in cannot do that to anyone.

So the bus frame grows an origin — `interactive` vs `agent` — and the route
filters on it. The origin is a server-side detail: it is not on the wire.

### 3.3 A sequence number, so "catch up" is exact

Every frame published to a chat gets a monotonic per-chat `seq`, and
`GET /chats/{id}` reports the `through_seq` its transcript already reflects.

That makes the open sequence exact rather than best-effort:

```
subscribe (buffer frames)  →  GET /chats/{id}  →  apply buffered frames with seq > through_seq
```

It is what lets a client attach *before* reading without double-applying, and it
is the only thing that closes the race in §2. It pays for two more things on the
way past:

- **A lagged subscriber knows it lagged.** The broadcast drops frames for a slow
  reader; today `get_chat_events` maps `Lagged` to `None` and says nothing
  (`src/workshop_api.rs:6229`). A jump in `seq` is a client-visible "you missed
  some — re-read the chat".
- **Idempotence stops being a reducer property.** The iOS reducer keys tool cards
  by `tool_call_id` so a repeated frame lands in place, but user and reply items
  are keyed by list position and would duplicate. With `seq` the store drops the
  frame before the reducer sees it, which is where that decision belongs.

`seq` rides in the JSON payload (`{"kind":"reply","seq":41,…}`), not the SSE `id:`
field. Every client here hand-parses `data:` lines and ignores everything else, so
the payload is the only place all of them can already see. SSE `id:` +
`Last-Event-ID` is the better long-run answer and is worth revisiting when a
client uses a real `EventSource`; it is not worth a parser change in three
languages today.

### 3.4 The pod owns the whole account of a turn

A client should never need frames it missed. It re-reads the chat and gets
everything — that is already true, because §0's `running` snapshot means a
transcript read mid-turn is complete as of that read. `seq` just tells the client
where the read stopped. Nothing in this plan requires the bus to replay history,
and it must not grow into a log.

---

## 4. The pod half

### 4.1 Publish through one place

`chat_event_sender` (`src/workshop_api.rs:93`) becomes `chat_publisher`, handing
back a small type instead of a bare `broadcast::Sender`:

```rust
struct ChatPublisher {
    chat_id: String,
    bus: broadcast::Sender<BusFrame>,
    seq: Arc<AtomicU64>,
}

struct BusFrame { seq: u64, origin: Origin, event: ChatEvent }
enum Origin { Interactive, Agent }
```

`publish(origin, event)` stamps the next `seq` and sends. It is sync — the
broadcast send is — which is a simplification for the several call sites that
currently `.await` an `mpsc::Sender`.

Then in `post_chat_turn`, the ~12 `tx.try_send(…)` / `tx.send(…).await` sites in
the hooks, step guard, reply sink, phase sink, plan sink and mailbox emit through
the publisher instead. The private `mpsc` is fed from the same call, so the POST
response is byte-identical to today's.

The four headless paths already hold a bus sender and only need `Origin::Agent`
threaded through.

### 4.2 Filter at the route

```rust
#[derive(Deserialize)]
struct EventsQuery {
    /// `all` adds interactive turns. Absent or anything else = agent-initiated
    /// only, which is what every client built before this shipped expects.
    include: Option<String>,
}
```

`get_chat_events` keeps a frame when `include=all` or `frame.origin == Agent`,
and serializes `frame.event` with `seq` merged in (`serde_json::to_value` + insert
— the enum is `#[serde(tag = "kind")]` and adding a field to every variant would
be twelve copies of the same line).

Capacity goes from 64 to 256 while it is being touched: a tool-heavy turn emits
several frames per step, and a subscriber that is one paint behind should not lose
any.

### 4.3 `through_seq` on the read

`ChatDetail` gains `through_seq: u64` next to `busy`, read under the same lock as
the transcript in `get_chat` (`src/workshop_api.rs:5141`) so the pairing is
atomic. `ChatSummary` does not need it.

### 4.4 Tests

- `chat_publisher` stamps `seq` monotonically per chat and starts at 1 for a chat
  that has never published.
- Origin filter: two subscribers, one with `include=all`; an interactive frame
  reaches one and an agent frame reaches both.
- `get_chat` returns a `through_seq` that matches the frames published so far —
  the pairing this whole design rests on.
- Existing `chat_interrupt_test` / `flow_conversation_test` must not move: they
  are the proof the default behaviour did not change.

### 4.5 Version

Agent `0.39.0` (`Cargo.toml:3`). Additive on the wire — a new optional query
parameter, two new fields — so `PodVersionFloor.minimum` in the app does **not**
move; §5 says what each client does when the pod is older.

---

## 5. The client half

### 5.1 iOS — `metalcraft-mobile`

`SessionStore` becomes a single-stream client:

- `startWatching` subscribes with `include=all`.
- `attach` subscribes **first**, buffering, then reads the chat, then applies the
  buffer from `through_seq`. Today it reads then subscribes (§2's race).
- `send` stops consuming the POST response — it still awaits the request to catch
  a 402/409/503 and the 202 queued answer, but frames arrive on the watch.
- `apply` drops any frame whose `seq` is not greater than the last applied.
- `observeIfRunning` — the 2s poll — is kept **only** as the pre-0.39 fallback: no
  `through_seq` in the detail means an old pod, and the poll is how those keep
  working. Gate it on that, do not delete it.
- `resync` should go through `adopt` rather than seeding directly, so a
  reconnect restores `busy` and `plan` like every other read does. It is the last
  path that seeds a transcript its own way.

Live-pod test: two `PodClient`s on one chat; one posts a turn, the other must see
`turn_started` … `done` without touching the POST response.

### 5.2 workshop-web

Same move, smaller: `subscribeEvents` passes `?include=all`, `postTurn` stops
feeding the reducer, the reducer drops frames at or below the last `seq`. Until
then it is unaffected — it does not send the parameter and sees exactly what it
sees today.

### 5.3 metalcraft-front

`pod.subscribe()` exists and has no callers; the session store drives turns off
`pod.turn()`. This is the client the change was written for — a fleet view where
every open session shows its agent working, whoever started it. Same three edits,
plus finally using `subscribe`.

### 5.4 metalcraft-ai-web hero

Turn stream only, no bus. Nothing to do — and worth leaving alone: the hero is one
person, one browser tab, one turn.

---

## 6. After

Once all three clients are on the bus, `post_chat_turn` can stop maintaining a
private channel and answer `202 Accepted` for every turn rather than an SSE stream
it no longer needs to fill. That is a breaking change for any client not listed in
§5, so it is a separate decision made later, not a phase of this plan.

---

## 7. Risks

- **Double-render on a client that opts in without dropping its POST stream.**
  The whole failure mode §3.2 protects strangers from, reintroduced by an
  incomplete client change. The `seq` guard in `apply` is what makes it safe, so
  land that edit before the `include=all` edit in each client.
- **Broadcast lag under a tool-heavy turn.** Mitigated by capacity 256 and made
  *visible* by `seq`; a client that sees a gap re-reads. Silent loss is the thing
  being fixed, not introduced.
- **Ordering.** One publisher per chat and a `busy` flag that forbids concurrent
  turns, so `seq` order is send order. Worth an assertion in the publisher test
  rather than a comment.

---

## 8. Open questions

1. Should `include=all` instead be the default on a **new route**
   (`GET /chats/{id}/stream`), leaving `/events` frozen? Cleaner semantics, one
   more route to keep alive. Current answer: no — the parameter is honest and the
   old meaning is a real one clients still want.
2. Does the gateway path want `Origin::Interactive`? A WhatsApp turn is somebody
   typing, just not here. Leaning yes, since the phone's fleet view should show
   the agent working; it changes nothing for a client that does not opt in.
