---
description: Read-first, minimal edits, verify after changing code
version: 1.0.0
---

# Edit Workflow

When modifying code:

1. **Read first** — always read the file before editing. Understand the context around the change.
2. **Plan the change** — identify exactly what lines need to change and why. State the plan before editing.
3. **Make minimal edits** — change only what's necessary. Don't refactor surrounding code unless asked.
4. **Verify after** — read the file again after editing to confirm the change looks correct.
5. **Run tests** — if tests exist, run them after the change. Fix any failures before moving on.
6. **One thing at a time** — make one logical change per edit. Don't batch unrelated changes.

When creating new files:
- Check if a similar file exists first — prefer editing over creating
- Follow existing naming conventions in the project
- Add the new file to any module declarations (mod.rs, lib.rs, etc.)

When deleting code:
- Grep for usages before removing anything
- Remove imports and module declarations that become unused
