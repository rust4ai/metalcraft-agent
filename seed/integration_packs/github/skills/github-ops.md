---
description: How to work with GitHub over the REST API — reading private repos, pushing commits, and the branch -> commit -> PR workflow
---

# GitHub Operations

These tools call the GitHub REST API (`https://api.github.com`) authenticated by a
single `GITHUB_TOKEN` (a classic or fine-grained personal access token). The token
decides what you can see and do: reading private repos needs `repo` (classic) or the
fine-grained "Contents: read" permission; pushing needs write/Contents:write; PRs
and issues need the matching permissions. You never pass the token yourself — every
tool already carries it.

## Orient yourself first

- **`github_get_authenticated_user`** — who the token belongs to. The cheapest way
  to confirm the token works.
- **`github_list_repos`** — repos you can access, **including private ones** (pass
  `visibility: "all"`). Use it to find the exact `full_name` (owner/repo).
- **`github_get_repo`** — metadata for one repo: `default_branch`, visibility,
  permissions.

## Reading files (works on private repos)

`github_get_file_contents` returns the file metadata plus base64-encoded `content`
and a `sha`:

1. Decode `content` from base64 to read the text.
2. **Keep the `sha`** — you must supply it to update that same file later.

Pass `ref` to read from a specific branch/tag/commit; omit it for the default branch.

## Pushing a commit (create or update a file)

`github_create_or_update_file` writes one file and produces a commit — the simplest
push. Rules:

- `content` MUST be **base64-encoded** (encode the full new file body).
- Creating a new file: omit `sha`.
- Updating an existing file: include its current `sha` (from
  `github_get_file_contents`) or GitHub rejects the write with a 409/422.
- Pass `branch` to commit somewhere other than the default branch. Prefer a feature
  branch for anything non-trivial.

For multi-file commits, write the files one at a time onto the same branch (each call
is its own commit), then open one PR.

## Branch -> commit -> PR workflow

1. **Base SHA** — `github_get_ref` with the base `branch` (e.g. `main`) → read
   `object.sha`.
2. **Create branch** — `github_create_branch` with `ref: "refs/heads/<new-branch>"`
   and `sha` = that base SHA.
3. **Commit** — `github_create_or_update_file` with `branch: "<new-branch>"`.
4. **Open PR** — `github_create_pull_request` with `head: "<new-branch>"`,
   `base: "main"`, a clear `title` and `body`.

## Pull requests and issues

- **`github_list_pull_requests`** / **`github_list_issues`** — browse (filter by
  `state`). Note: the issues endpoint also returns PRs (they carry a `pull_request`
  field) — skip those when you only want issues.
- **`github_create_pull_request`** — `head` is your branch, `base` is the target.
- **`github_create_issue`** — open an issue (optionally with `labels` / `assignees`).
- **`github_create_issue_comment`** — comment on an issue **or a PR**. A PR is an
  issue, so use the PR `number` as `issue_number` to comment on a pull request.

## Safety

- Every write hits a real repository. Double-check `owner`/`repo`/`branch` before
  committing, and avoid committing straight to a default/shared branch unless the
  user asked for it — prefer a branch + PR.
- Confirm ambiguous targets with the user first.
- Never expose the token or raw tool URLs.
