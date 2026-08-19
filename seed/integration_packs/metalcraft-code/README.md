# metalcraft-code pack

Agent pack for **Metalcraft Code** (`code.metalcraftai.com`) — a remote coding
environment on sprites.dev. Gives the agent tools to connect GitHub, provision an
ephemeral workspace, clone a repo, read/edit code, run/build/test, configure
GitHub Actions, and commit & push.

- **Auth:** single `METALCRAFT_TOKEN` (`Authorization: Bearer`), ecosystem-wide.
  Reads work with any token; create/edit/exec/commit/push need the `write` scope.
- **Base URL:** fixed to `https://code.metalcraftai.com`.
- **Tool prefix:** `mcode_`.

## Tools
Orientation: `mcode_whoami`, `mcode_list_installations`, `mcode_list_repos`,
`mcode_list_workspaces`, `mcode_get_workspace`, `mcode_list_runs`, `mcode_get_run`.

Lifecycle: `mcode_create_workspace`, `mcode_wake_workspace`,
`mcode_hibernate_workspace`, `mcode_delete_workspace`.

Code ops: `mcode_clone`, `mcode_read_file`, `mcode_list_dir`, `mcode_write_file`,
`mcode_delete_path`, `mcode_exec`, `mcode_build`, `mcode_test`, `mcode_git`,
`mcode_configure_actions`, `mcode_expose`.

## Persona / skill
- `personas/metalcraft-code-agent.json` — the remote-coding persona.
- `skills/metalcraft-code.md` — the connect → provision → clone → edit → test →
  commit/push workflow.

Requires the user to have connected a GitHub App installation in the Metalcraft
Code web app first.
