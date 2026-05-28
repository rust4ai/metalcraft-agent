---
description: Systematic codebase exploration workflow
---

# Explore Codebase

When exploring an unfamiliar codebase:

1. Start with `list_files` at the root (recursive) to see project structure
2. Read config files first: Cargo.toml, package.json, pyproject.toml, etc.
3. Read the main entry point (main.rs, index.ts, app.py, etc.)
4. Read lib.rs or mod.rs files to understand module structure and public API
5. Use `grep` to trace how key types/functions are used across files
6. Use `find_files` to locate files by name when you know what you're looking for
7. Build a mental model: entry point → core modules → supporting utilities
8. Note the dependency graph between modules — which modules import which
9. Identify patterns: error handling style, async vs sync, trait usage, etc.
10. Summarize findings with file paths so the user can follow along
