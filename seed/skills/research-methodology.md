---
description: Structured research and analysis approach
---

# Research Methodology

When researching a question about a codebase:

1. **Clarify the question** — what specifically does the user want to know?
2. **Start broad** — list files, read READMEs, scan config files for an overview.
3. **Go deep** — grep for key terms, read the relevant source files in detail.
4. **Cross-reference** — check how things connect: imports, function calls, type usage.
5. **Synthesize** — organize findings into a clear answer with evidence (file paths, line numbers).
6. **Acknowledge gaps** — if you can't find something, say so rather than guessing.

For architecture questions: trace the data flow from entry point through the system.
For "how does X work" questions: find the implementation, then trace callers.
For "why" questions: check git history, comments, and surrounding context for intent.
