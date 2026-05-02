# Pi — Architecture Analysis

> **Source**: [github.com/badlogic/pi-mono](https://github.com/badlogic/pi-mono)
> **Language**: TypeScript (97%) | **Platform**: CLI + Web | **License**: MIT

## Overview

Pi is an AI agent toolkit built as a monorepo. The core is a coding agent CLI, backed by a provider-agnostic LLM layer, a generic agent runtime, and both terminal and web UIs. Created by Mario Zechner (badlogic).

---

## 1. Monorepo Structure

```
packages/
├── ai/           — @mariozechner/pi-ai         (LLM abstraction)
├── agent/        — @mariozechner/pi-agent-core  (agent runtime)
├── coding-agent/ — @mariozechner/pi-coding-agent (CLI tool)
├── tui/          — @mariozechner/pi-tui         (terminal UI)
└── web-ui/       — @mariozechner/pi-web-ui      (web UI)
```

### Layering

```
coding-agent
    ├── agent-core (generic loop, tool dispatch)
    │     └── pi-ai (LLM calls, streaming, providers)
    └── tui / web-ui (rendering)
```

Key insight: **agent-core is provider-agnostic and tool-agnostic**. The coding-agent registers specific tools and UI. This separation makes the agent runtime reusable.

---

## 2. Agent Loop (`packages/agent/`)

5 files total — minimal, focused:

| File | Purpose |
|------|---------|
| `agent.ts` | Agent class, state management, lifecycle |
| `agent-loop.ts` | Core loop: stream → tools → loop |
| `proxy.ts` | Proxy/middleware layer |
| `types.ts` | Event types, tool types, config |
| `index.ts` | Exports |

### Loop Architecture (Nested State Machine)

```typescript
// Outer loop: follow-up messages after agent stops
while (true) {
  // Inner loop: tool calls + steering within a turn
  while (hasMoreToolCalls || pendingMessages.length > 0) {
    // 1. Transform context (compaction, etc.)
    // 2. Convert messages to LLM format
    // 3. Stream LLM response
    // 4. Execute tool calls (parallel or sequential)
    // 5. Check for steering messages (injected mid-turn)
    if (shouldStopAfterTurn) break;
  }
  // Check for follow-up messages
  const followUps = await config.getFollowUpMessages?.();
  if (followUps.length > 0) continue;
  break;
}
```

### Three Message Queues

1. **Steering queue** — messages injected mid-turn (e.g., user correction while agent runs)
2. **Follow-up queue** — messages queued after agent completes a turn
3. **Prompt messages** — initial user input

### Tool Execution Modes

```typescript
// Dynamic detection: if any tool is marked sequential, run all sequentially
const hasSequentialToolCall = toolCalls.some(
  tc => tools.find(t => t.name === tc.name)?.executionMode === "sequential"
);
if (config.toolExecution === "sequential" || hasSequentialToolCall) {
  executeToolCallsSequential(...);
} else {
  executeToolCallsParallel(...);
}
```

### Tool Call Lifecycle

1. **beforeToolCall** hook — validation, modification, or cancellation
2. **Execute** — with abort signal support
3. **afterToolCall** hook — transform results, trigger side effects
4. Errors become tool results (not exceptions) — LLM can reason about failures

### Termination

- `shouldStopAfterTurn` returns true
- All tool results have `terminate: true`
- No follow-up messages remain
- Response has `stopReason: "error"` or `"aborted"`

### Event System

Granular events emitted throughout:
- `message_start`, `text_delta`, `toolcall_delta`, `message_end`
- `tool_execution_start`, `tool_execution_end`
- `agent_end`

Listeners receive the abort signal, enabling cooperative cancellation.

---

## 3. Agent State (`agent.ts`)

```typescript
class Agent {
  private _state: MutableAgentState;
  private readonly listeners: Set<(event, signal) => Promise<void>>;
  private readonly steeringQueue: PendingMessageQueue;
  private readonly followUpQueue: PendingMessageQueue;
  private activeRun?: ActiveRun;
}
```

### State Isolation

- Internal state is `MutableAgentState` (read-write)
- External API exposes immutable `AgentState` snapshots
- Array properties (tools, messages) are **copied on assignment** to prevent mutation
- Context snapshot created before each loop run

---

## 4. Coding Agent (`packages/coding-agent/`)

### Tools (14 files)

| Tool | Purpose |
|------|---------|
| `bash.ts` | Shell command execution |
| `read.ts` | Read file contents |
| `write.ts` | Write files |
| `edit.ts` | Structured file edits |
| `edit-diff.ts` | Diff-based edits |
| `find.ts` | Find files by pattern |
| `grep.ts` | Search file contents |
| `ls.ts` | List directory |
| `truncate.ts` | Truncate long output |
| `file-mutation-queue.ts` | Queue file changes |
| `tool-definition-wrapper.ts` | Tool wrapper for extensions |
| `path-utils.ts` | Path helpers |
| `render-utils.ts` | Output formatting |

### Tool Registration (Multi-Layered)

1. **Base tools** — built-in: read, bash, edit, write
2. **Custom tools** — SDK-registered via config
3. **Extension tools** — wrapped through extension system
4. **Allowlist filtering** — selective activation per session
5. **`baseToolsOverride`** — complete replacement for custom runtimes

### Agent Session Architecture

`AgentSession` is the central orchestrator:

```
AgentSession
├── Agent (from agent-core)
├── SessionManager (persistence)
├── BashExecutor (shell)
├── ExtensionRunner (plugins)
├── ModelRegistry (LLM discovery)
├── EventBus (pub/sub)
└── CompactionSystem (context compression)
```

### Operational Modes

- **Interactive** — terminal UI with real-time streaming
- **RPC** — remote procedure call (headless, API-driven)
- **Print** — formatted output mode

### Extension System

```
extensions/
├── types.ts    — interface contracts
├── loader.ts   — discover & load extensions
├── runner.ts   — execute extension logic
└── wrapper.ts  — wrap tools with extension hooks
```

Extensions can intercept tool calls, add custom tools, and react to session events.

---

## 5. LLM Layer (`packages/ai/`)

### Providers

- OpenAI (completions + responses variants)
- Anthropic
- Google (standard + Vertex AI)
- Amazon Bedrock
- Azure OpenAI
- Mistral
- OpenAI Codex

### Unification Strategy

- **Registry pattern** — providers self-register via `register-builtins.js`
- **Shared types** — unified request/response interfaces
- **Streaming** — event-based streaming across all providers
- **Thinking levels** — configurable: off, minimal, low, medium, high
- **OAuth** — provider authentication integration

---

## 6. Context Compaction

### Trigger

```
contextTokens > contextWindow - reserveTokens
```
- Default `reserveTokens`: 16,384
- Default `keepRecentTokens`: 20,000

### Algorithm

1. **Walk backward** through history, accumulate token estimates
2. Find valid **cut point** at user/assistant message boundary (never mid-tool-call)
3. If cut lands mid-turn, capture **turn prefix** separately
4. **Dual summarization** via LLM:
   - History summary: goals, constraints, progress, decisions, critical context
   - Turn prefix summary: focused context for continuing current work
5. **Iterative**: new summaries incorporate previous ones
6. **File tracking**: extract and preserve read/modified file lists

### Branch Summarization

Separate `branch-summarization.ts` — likely summarizes git branch context for long-running tasks.

---

## 7. Session Management

- **Persistence**: messages auto-saved on agent events
- **Session branching**: fork sessions for exploration
- **Session recording**: capture and publish sessions (HuggingFace datasets)
- **Dynamic model switching**: change models mid-session with Ctrl+P
- **Thinking level cycling**: adjust reasoning depth on the fly

---

## 8. Key Design Patterns

1. **Layered architecture**: ai → agent-core → coding-agent. Each layer reusable independently
2. **Errors as data**: tool errors become tool results, not exceptions. LLM reasons about failures
3. **Cooperative cancellation**: abort signals threaded through entire stack
4. **Hook-based extensibility**: beforeToolCall/afterToolCall enable middleware patterns
5. **Parallel-by-default**: tools run concurrently unless marked sequential
6. **Immutable external state**: mutable internals, immutable snapshots exposed
7. **Queue-based message injection**: steering + follow-up queues enable mid-turn intervention
8. **Context compaction**: LLM-powered summarization with iterative updates
9. **Extension system**: full plugin architecture for custom tools and behaviors
10. **Session recording**: publish sessions as training data
