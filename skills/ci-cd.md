---
description: CI/CD pipeline best practices
---

# CI/CD

When working with CI/CD pipelines:

1. Read existing pipeline config first (.github/workflows/, .gitlab-ci.yml, Jenkinsfile, etc.)
2. Understand the current stages: build, test, lint, deploy
3. Test pipeline changes locally when possible (act for GitHub Actions, docker for Dockerfiles)
4. Keep pipelines fast — cache dependencies, parallelize independent steps
5. Pin action/image versions to avoid surprise breakages
6. Separate concerns: don't mix build logic with deployment logic
7. Use environment variables for secrets — never hardcode them
8. Add status checks that block merges on failure
9. Keep deployment steps idempotent — safe to re-run
