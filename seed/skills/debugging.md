---
description: Reproduce, locate, hypothesize, fix, test
---

# Debugging

When investigating a bug or error:

1. **Reproduce** — understand the exact error message or unexpected behavior. Read it carefully.
2. **Locate** — use grep to find where the error originates. Trace the call stack if available.
3. **Read context** — read the surrounding code to understand the intended logic.
4. **Hypothesize** — form a theory about what's wrong before changing anything.
5. **Verify** — test your hypothesis by reading related code, checking types, or running commands.
6. **Fix** — make the minimal change that fixes the root cause, not just the symptom.
7. **Test** — run the relevant tests. If no tests cover this case, suggest adding one.

Common patterns to check:
- Off-by-one errors in loops and slicing
- Null/None/unwrap on unexpected empty values
- Type mismatches or wrong variable in scope
- Async/await missing, causing futures that never execute
- Error variants not handled (match arms, Result propagation)
