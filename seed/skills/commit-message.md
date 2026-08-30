---
description: Write clear, conventional commit messages
version: 1.0.0
---

# Commit Message

When asked to write a commit message:

1. Run `git diff --staged` to see staged changes
2. If nothing staged, run `git diff` to see unstaged changes
3. Write a commit message following Conventional Commits format:
   - `feat:` for new features
   - `fix:` for bug fixes
   - `refactor:` for code restructuring
   - `docs:` for documentation
   - `chore:` for maintenance
4. Subject line ≤50 characters
5. Add body only when the "why" isn't obvious from the subject
6. Focus on intent, not mechanics
