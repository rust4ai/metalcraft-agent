# Cline — Architecture Analysis

> **Source**: [github.com/cline/cline](https://github.com/cline/cline)
> **Language**: TypeScript | **Platform**: VS Code Extension | **License**: Apache 2.0

## Overview

Cline is an autonomous AI coding agent that lives inside VS Code. It can read/write files, run terminal commands, automate browsers, and delegate to sub-agents — all through a human-in-the-loop approval model. The name stands for **CLI** + a**N**d + **E**ditor.

---

## 1. Architecture

```
extension.ts → webview → controller → task
```

### Key Modules

| Module | Purpose |
|--------|---------|
| `core/task/` | Agent loop, tool execution, streaming |
| `core/api/` | LLM provider abstraction + retry logic |
| `core/context/` | Context window management, tracking, instructions |
| `core/prompts/` | System prompt construction, commands |
| `core/permissions/` | Human approval / auto-approve logic |
| `core/controller/` | Webview ↔ task bridge |
| `services/browser/` | Headless browser automation |
| `services/tree-sitter/` | AST-level code analysis |
| `services/mcp/` | Model Context Protocol integration |
| `services/ripgrep/` | Fast code search |

### Data Flow

1. User types task in webview
2. Controller creates Task
3. Task builds system prompt + context
4. Task enters agent loop (stream LLM → parse → execute tools → loop)
5. Each action surfaces in webview for approval
6. Checkpoint captured at each step

---

## 2. Agent Loop

The loop lives in `core/task/` with these key components:

- **StreamResponseHandler** — processes streaming LLM output
- **StreamChunkCoordinator** — coordinates chunks into coherent blocks
- **ToolExecutor** — dispatches tool calls
- **ToolExecutorCoordinator** — manages parallel/sequential execution
- **loop-detection.ts** — detects infinite loops (agent repeating itself)

### Flow

```
┌─────────────────────────────────┐
│         Build Messages          │
│  (system prompt + history +     │
│   context mentions)             │
└──────────────┬──────────────────┘
               ▼
┌─────────────────────────────────┐
│      Stream LLM Response        │
│  (chunks → text + tool_use)     │
└──────────────┬──────────────────┘
               ▼
┌─────────────────────────────────┐
│    Parse Assistant Message      │
│  (text blocks, tool use blocks) │
└──────────────┬──────────────────┘
               ▼
         ┌─────┴─────┐
         │ Tool call? │
         └─────┬─────┘
          yes  │  no
         ┌─────┘  └──────┐
         ▼               ▼
  ┌──────────────┐  ┌─────────┐
  │ Approve tool │  │  Done   │
  │  (human/auto)│  └─────────┘
  └──────┬───────┘
         ▼
  ┌──────────────┐
  │ Execute tool │
  │ Add result   │
  └──────┬───────┘
         ▼
      Loop back
```

### Loop Detection

Cline detects when the agent gets stuck repeating actions and can intervene — a critical safety feature most agents lack.

---

## 3. Tool System

### 25 Tool Handlers

| Category | Tools |
|----------|-------|
| **File I/O** | `ReadFileToolHandler`, `WriteToFileToolHandler`, `ListFilesToolHandler`, `ApplyPatchHandler` |
| **Search** | `SearchFilesToolHandler` (ripgrep), `ListCodeDefinitionNamesToolHandler` (tree-sitter) |
| **Terminal** | `ExecuteCommandToolHandler` |
| **Browser** | `BrowserToolHandler` (headless, screenshots, clicks, typing) |
| **Web** | `WebFetchToolHandler`, `WebSearchToolHandler` |
| **Context** | `CondenseHandler` (compress context), `SummarizeTaskHandler` |
| **Planning** | `PlanModeRespondHandler`, deep planning commands |
| **Delegation** | `SubagentToolHandler` (spawn sub-agents) |
| **MCP** | `UseMcpToolHandler`, `AccessMcpResourceHandler`, `LoadMcpDocumentationHandler` |
| **Skills** | `UseSkillToolHandler` |
| **Meta** | `AskFollowupQuestionToolHandler`, `AttemptCompletionHandler`, `NewTaskHandler` |
| **Other** | `GenerateExplanationToolHandler`, `ReportBugHandler` |

### Key Design Choices

- **ToolValidator** validates tool calls before execution
- **autoApprove.ts** implements configurable auto-approval rules
- Tools return structured results that feed back into the conversation
- **Sub-agent delegation** allows spawning child agents for subtasks

---

## 4. LLM Integration

### 43+ Provider Implementations

Anthropic, OpenAI, Gemini, Bedrock, Vertex, Azure, Groq, Cerebras, DeepSeek, Mistral, Ollama, LM Studio, OpenRouter, Together, Fireworks, HuggingFace, xAI, plus many more niche providers.

### Architecture

- `core/api/providers/` — one file per provider
- `core/api/adapters/` — protocol adapters (OpenAI-compatible, Anthropic, etc.)
- `core/api/transform/` — request/response transformations
- `core/api/retry.ts` — retry logic with backoff

### Approach

- Uses **native tool calling** (not text-based parsing) where providers support it
- **Streaming** throughout — chunks processed incrementally
- Token and cost tracking per request
- Model-specific capability detection

---

## 5. Context Management

### Three Subsystems

1. **context-management/** — sliding window, compression, token budgeting
2. **context-tracking/** — monitors usage patterns
3. **instructions/** — manages custom instructions (.clinerules, etc.)

### Strategies

- **Mentions system**: `@url`, `@file`, `@folder`, `@problems` for precise context injection
- **CondenseHandler** — LLM-powered summarization of older conversation turns
- **Checkpoint system** — workspace snapshots at each step, restorable
- **Tree-sitter** — extracts code definitions for compact representation

---

## 6. Human-in-the-Loop

### Permission Model

- Every tool call requires approval by default
- **CommandPermissionController** manages per-command policies
- **Auto-approve** configurable per tool type (e.g., auto-approve reads, require approval for writes)
- Diff view for file changes before approval
- Workspace timeline for tracking all modifications

### Modes

- **Act Mode** — agent executes actions (with approval)
- **Plan Mode** — agent plans without executing, user reviews

---

## 7. Key Design Patterns

1. **Streaming-first**: All LLM interactions stream, enabling real-time UI feedback
2. **Tool validation**: Pre-execution validation prevents malformed tool calls
3. **Loop detection**: Catches infinite loops before they waste tokens
4. **Checkpoint/restore**: Full workspace snapshots at each step
5. **Provider agnostic**: 43+ providers behind unified interface
6. **MCP extensibility**: External tools via Model Context Protocol
7. **Sub-agents**: Delegate subtasks to child agents
8. **Focus chain**: Guides agent attention to relevant code sections
9. **Cost tracking**: Real-time token/cost display per task
