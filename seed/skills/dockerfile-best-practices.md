---
description: Docker image best practices and patterns
version: 1.0.0
---

# Dockerfile Best Practices

When writing or reviewing Dockerfiles:

1. Use multi-stage builds to minimize image size
2. Pin base image versions (no `latest` tag)
3. Order layers from least to most frequently changing
4. Combine RUN commands to reduce layers
5. Use .dockerignore to exclude unnecessary files
6. Run as non-root user in production
7. Use COPY instead of ADD unless extracting archives
8. Set HEALTHCHECK for production containers
9. Don't store secrets in the image — use build args or runtime env
