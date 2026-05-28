---
description: Structured code review methodology
---

# Code Review

When asked to review code:

1. Read the files or diff to understand the changes
2. Check for:
   - Bugs and logic errors
   - Security vulnerabilities (injection, XSS, etc.)
   - Performance issues
   - Missing error handling
   - Code style and naming consistency
3. Format each comment as: `file:line — problem — suggested fix`
4. Prioritize: bugs > security > correctness > style
5. Be specific — quote the problematic code
6. If the code looks good, say so briefly
