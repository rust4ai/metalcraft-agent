# Upgrade Plan — metalcraft-agent

> Evolving from a simple weather-tool agent to a capable coding agent,
> informed by Cline and Pi's architectures.

## Current State

```
metalcraft-agent (today)
├── main.rs          — entry point, hardcoded task
├── tools/
│   ├── get_weather  — stub weather tool
│   └── report_result — termination signal
└── Uses: metalcraft create_react_agent + rig
```

What we have: a working ReAct loop via metalcraft, LLM calls via rig, tool registry. What we lack: everything that makes Cline/Pi actually useful.

---

## Phase 1: Core Coding Tools

**Goal**: Agent can read, write, search, and run commands.

### Tools to Implement

| Tool | Priority | Description |
|------|----------|-------------|
| `read_file` | P0 | Read file contents (with line range support) |
| `write_file` | P0 | Write/create files |
| `edit_file` | P0 | Search-and-replace edits (like Pi's edit tool) |
| `bash` | P0 | Execute shell commands with timeout |
| `list_files` | P1 | List directory contents (respects .gitignore) |
| `grep` | P1 | Search file contents with regex |
| `find_files` | P1 | Find files by glob pattern |

### Implementation Pattern

Each tool implements `metalcraft::Tool`:

```rust
struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str { "read_file" }
    fn description(&self) -> &str { "Read file contents" }
    fn parameters_schema(&self) -> Value { ... }
    async fn call(&self, args: Value) -> Result<Value> {
        // Read file, return contents
    }
}
```

### Bash Executor

Model after Pi's `bash-executor.ts`:
- Spawn process with timeout
- Capture stdout + stderr
- Kill on timeout
- Truncate long output (prevent context blowup)

### Changes

- New `src/tools/` files: `read_file.rs`, `write_file.rs`, `edit_file.rs`, `bash.rs`, `list_files.rs`, `grep.rs`, `find_files.rs`
- Update `tools/mod.rs` to register all tools
- Add `output_guard.rs` — truncate tool output exceeding a max length
- Remove `get_weather.rs` and `report_result.rs`

---

## Phase 2: Interactive CLI Loop

**Goal**: User can chat with the agent in a terminal, not just run a hardcoded task.

### Components

| Component | Description |
|-----------|-------------|
| REPL loop | Read user input → agent turn → display result → repeat |
| Streaming | Stream LLM tokens to terminal as they arrive |
| Multi-provider | Support OpenAI + Anthropic via rig providers |
| `.env` config | Model, provider, API key from env |

### Architecture

```
main.rs
├── loop {
│   ├── read user input
│   ├── create AgentState::new(input)
│   ├── executor.run(state)  // metalcraft ReAct loop
│   ├── display final answer
│   └── carry over conversation history
│ }
```

### Key Challenge: Conversation Continuity

Current `create_react_agent` creates fresh state each turn. Need to either:
- Extend `AgentState` to carry history across turns, or
- Build custom graph (like Pi's agent-loop approach) that maintains message history

### Changes

- Rewrite `main.rs` as interactive REPL
- Add `rustyline` or `reedline` for readline support
- Add `--provider` / `--model` CLI flags
- Support both OpenAI and Anthropic via rig

---

## Phase 3: Human-in-the-Loop Approval

**Goal**: User approves dangerous actions before execution.

### Design (Inspired by Both Cline and Pi)

```
Tool call from LLM
    │
    ▼
┌──────────────────┐
│ Is tool read-only?│
│ (read, grep, ls) │
└────────┬─────────┘
    yes  │  no
    ┌────┘  └────┐
    ▼            ▼
  Auto-      Prompt user:
  approve    "[tool] args — approve? (y/n/e)"
                 │
            ┌────┼────┐
            y    n    e(dit)
            │    │    │
          exec  skip  edit args
```

### Implementation

Use metalcraft's `beforeToolCall` hook pattern (from Pi's design):

```rust
// In a custom agent node, before dispatching:
match tool_approval_policy(&tool_name) {
    Policy::AutoApprove => { /* proceed */ }
    Policy::RequireApproval => {
        print!("[{}] {} — approve? (y/n): ", tool_name, args);
        if !user_confirms() { return skip_result; }
    }
}
```

### Tool Categories

| Category | Policy | Tools |
|----------|--------|-------|
| Read-only | Auto-approve | read_file, list_files, grep, find_files |
| Mutating | Require approval | write_file, edit_file |
| Dangerous | Always confirm | bash |

### Changes

- Add `src/approval.rs` — approval policies and prompts
- Move from `create_react_agent` to custom graph with approval node
- Add `--auto-approve` flag to bypass for scripted use

---

## Phase 4: Context Management

**Goal**: Agent can work on real codebases without blowing the context window.

### Context Compaction (From Pi)

When token usage approaches limit:

1. Estimate tokens in message history
2. Find cut point (user/assistant boundary, never mid-tool-call)
3. Summarize old messages via LLM call
4. Replace old messages with summary
5. Keep recent messages intact

```rust
struct CompactionConfig {
    context_window: usize,      // e.g., 128_000
    reserve_tokens: usize,      // e.g., 16_384
    keep_recent_tokens: usize,  // e.g., 20_000
}
```

### Output Truncation

Large tool results (e.g., reading a 10K-line file) must be truncated:

```rust
fn truncate_output(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars { return output.to_string(); }
    let half = max_chars / 2;
    format!("{}...\n[truncated {} chars]\n...{}",
        &output[..half], output.len() - max_chars, &output[output.len()-half..])
}
```

### System Prompt Engineering

Build dynamic system prompt (like Pi):
- Working directory
- Available tools with descriptions
- Project context (README, key files)
- Custom instructions (.agentrc or similar)

### Changes

- Add `src/context/compaction.rs` — LLM-powered summarization
- Add `src/context/token_counter.rs` — estimate token counts
- Add output truncation to all tool results
- Build dynamic system prompt in `src/system_prompt.rs`

---

## Phase 5: Parallel Tool Execution

**Goal**: Execute independent tool calls concurrently.

### Design (From Pi)

```rust
// Check if any tool requires sequential execution
let needs_sequential = tool_calls.iter().any(|tc|
    tool_registry.get(tc.name).execution_mode == Sequential
);

if needs_sequential {
    execute_sequential(&tool_calls).await
} else {
    execute_parallel(&tool_calls).await  // tokio::join!
}
```

### Tool Execution Modes

| Mode | Tools | Why |
|------|-------|-----|
| Parallel | read, grep, find, ls | No side effects, safe to run concurrently |
| Sequential | bash, write, edit | Side effects, order matters |

### Changes

- Add `execution_mode` to tool trait or registry metadata
- Replace metalcraft's `ToolNode` with custom parallel-aware executor
- Use `tokio::JoinSet` for concurrent execution

---

## Phase 6: Error Recovery & Loop Detection

**Goal**: Agent handles failures gracefully and doesn't get stuck.

### Errors as Data (From Pi)

Tool errors become tool results, not panics:

```rust
// Instead of propagating errors:
let result = match tool.call(args).await {
    Ok(v) => v,
    Err(e) => json!({"error": e.to_string()}),
};
// LLM sees the error and can reason about it
```

### Consecutive Error Tracking

```rust
if all_tools_errored {
    consecutive_errors += 1;
    if consecutive_errors >= 3 {
        // Force stop — agent is stuck
    }
} else {
    consecutive_errors = 0;
}
```

### Loop Detection (From Cline)

Detect when agent repeats the same action:

```rust
fn detect_loop(recent_actions: &[(String, Value)]) -> bool {
    // Check last N tool calls for repetition
    if recent_actions.len() >= 4 {
        let last = &recent_actions[recent_actions.len()-1];
        let prev = &recent_actions[recent_actions.len()-3];
        last.0 == prev.0 && last.1 == prev.1
    } else {
        false
    }
}
```

### Changes

- Add `src/error_recovery.rs`
- Add `src/loop_detection.rs`
- Wire into agent graph as conditional checks

---

## Phase 7: Sub-Agents / Parallel Agents

**Goal**: Agent can spawn child agents for subtasks (like Claude Code's parallel agents).

### Architecture

```
Main Agent
├── "Research X" → spawn sub-agent (read-only tools)
├── "Research Y" → spawn sub-agent (read-only tools)
│   (run concurrently)
├── collect results
└── synthesize answer
```

### Implementation

```rust
struct SubAgentTool {
    model: M,
    registry: Arc<ToolRegistry>,
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str { "spawn_agent" }

    async fn call(&self, args: Value) -> Result<Value> {
        let task = args["task"].as_str().unwrap();
        let tools = args["tools"].as_array(); // subset of tools

        let sub_graph = create_react_agent(
            self.model.clone(), sub_registry, system_prompt
        )?;
        let executor = Executor::new(sub_graph).max_steps(10);
        let outcome = executor.run(AgentState::new(task), "sub").await?;

        Ok(json!({"result": outcome.final_answer()}))
    }
}
```

### Parallel Execution

```rust
// When LLM requests multiple sub-agents:
let handles: Vec<_> = sub_tasks.iter()
    .map(|task| tokio::spawn(run_sub_agent(task)))
    .collect();

let results = futures::future::join_all(handles).await;
```

### Changes

- Add `src/tools/sub_agent.rs`
- Add `src/parallel.rs` — parallel agent orchestration
- Sub-agents get restricted tool sets (read-only by default)

---

## Phase 8: Extension System

**Goal**: Users can add custom tools and behaviors without modifying core code.

### Design (Inspired by Pi)

```rust
trait Extension: Send + Sync {
    fn name(&self) -> &str;
    fn tools(&self) -> Vec<Box<dyn Tool>>;
    fn on_before_tool_call(&self, name: &str, args: &Value) -> Option<Value> { None }
    fn on_after_tool_call(&self, name: &str, result: &Value) -> Option<Value> { None }
}
```

### Loading

- Scan `~/.metalcraft/extensions/` for WASM plugins or dynamic libraries
- Or simpler: TOML-defined tools that map to shell commands

### Changes

- Add `src/extensions/` module
- Extension loader + runner
- Hook into tool dispatch pipeline

---

## Implementation Order

```
Phase 1: Core Tools          ← Start here. Makes agent actually useful.
Phase 2: Interactive CLI      ← Makes it usable by humans.
Phase 3: Approval System      ← Makes it safe.
Phase 4: Context Management   ← Makes it work on real projects.
Phase 5: Parallel Tools       ← Performance win.
Phase 6: Error Recovery       ← Robustness.
Phase 7: Sub-Agents           ← Power feature.
Phase 8: Extensions           ← Ecosystem play.
```

### Dependency Graph

```
Phase 1 ──→ Phase 2 ──→ Phase 3
                │
                ▼
            Phase 4 ──→ Phase 6
                │
                ▼
            Phase 5 ──→ Phase 7
                            │
                            ▼
                        Phase 8
```

---

## What We Keep

- **metalcraft** — graph orchestration, state machine, compiled graphs
- **rig** — LLM provider abstraction, multi-provider support
- **Rust** — performance, safety, single binary deployment

## What We Build

Everything above the orchestration layer: tools, CLI, approval, context management, sub-agents. The metalcraft + rig foundation is solid — we build the coding agent on top.

---

## Comparison: Where We'd Land

| Capability | Cline | Pi | metalcraft-agent (after) |
|------------|-------|-----|--------------------------|
| Core tools (read/write/bash) | ✅ | ✅ | ✅ (Phase 1) |
| Interactive CLI | ❌ (VS Code) | ✅ | ✅ (Phase 2) |
| Human approval | ✅ | ✅ | ✅ (Phase 3) |
| Context compaction | ✅ | ✅ | ✅ (Phase 4) |
| Parallel tools | ✅ | ✅ | ✅ (Phase 5) |
| Error recovery | ✅ | ✅ | ✅ (Phase 6) |
| Sub-agents | ✅ | ❌ | ✅ (Phase 7) |
| Extensions/MCP | ✅ (MCP) | ✅ | ✅ (Phase 8) |
| Browser automation | ✅ | ❌ | Future |
| IDE integration | ✅ (VS Code) | ❌ | Future |
| Web UI | ❌ | ✅ | Future |
| 43+ providers | ✅ | ❌ | Via rig |
| Rust / single binary | ❌ (TS) | ❌ (TS) | ✅ (advantage) |
