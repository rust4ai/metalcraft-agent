# buildr.space spice harness

`tests/buildr_space_spice_test.rs` answers one end-to-end question:

> Can an orchestrator preset agent, handed an agent pack it has never heard of,
> provision a remote coding workspace and clone a repo into it?

The request under test is the user's own phrasing:

> create a buildrspace workspace and clone in https://github.com/ethereumdegen/octaweave

It is the first harness that exercises the whole chain — **agent pack install →
preset delegation roster → `sub_agent` → a vendored integration's HTTP tools → a
real external service** — rather than one link of it.

## Layers

| Tier | Test | Needs | Proves |
|---|---|---|---|
| 1 | `buildr_pack_installs_into_a_clean_pod` | nothing | a clean pod gains a whole agent from one archive, and the orchestrator can reach it |
| 2 | `live_buildr_tools_provision_and_clone` | `BUILDR_API_KEY` | the pack's tools really provision and clone, with no LLM in the loop |
| 3 | `live_orchestrator_delegates_clone_to_buildr` | tier 2 + inference | the agent does it from the natural-language request |
| — | `greeting-no-tools` (inside tier 3's suite) | tier 3 | a plain greeting delegates nothing |

Tier 1 runs everywhere and gates merges. Tiers 2–3 provision a real sprites.dev
workspace and spend inference, so they are opt-in and skip loudly otherwise.

Tier 2 exists to be the diagnostic when tier 3 fails: it separates *"the model
never called the tools"* from *"buildr.space could not do it"*.

## Running it

```bash
cargo test --test buildr_space_spice_test -- --nocapture
```

Live tiers need a crate-root `.env`:

```text
BUILDR_SPICE_LIVE=1        # opt in; without it tiers 2-3 skip
BUILDR_API_KEY=bsk_...     # a WRITE-scoped PAT from buildr.space -> account menu -> API keys
OPENAI_API_KEY=sk-...      # or METALCRAFT_TOKEN + OPENAI_BASE_URL for the gateway
# BUILDR_TEST_REPO=ethereumdegen/octaweave     # the default
# BUILDR_AGENTPACK=/path/to/other.agentpack    # override the vendored fixture
# SUB_AGENT_TIMEOUT_SECS=900                   # operator override; the persona already asks for 900
```

`ethereumdegen/octaweave` is **private**. The buildr.space GitHub App must be
installed on the `ethereumdegen` account, under the same buildr.space account
that owns `BUILDR_API_KEY`. buildr resolves the installation from the repo owner,
so there is no id to pass — but a missing grant fails as git's own 404, four
minutes into a paid workspace, naming neither cause nor cure. So the live tiers
**preflight** before provisioning anything:

1. `buildr_whoami` — is the token real, and does it carry the `write` scope?
2. `buildr_list_installations` — is there an installation for the repo's owner?
3. `buildr_list_repos` — does that installation's grant actually cover this repo?

Any of those missing is a `SKIP` with the fix spelled out, not a failure.

## The pack is a fixture, not a fetch

`tests/fixtures/buildr-space-0.2.0.agentpack` is the byte-for-byte archive
`packctl` builds from `axoniac-seeded-agent-packs/packs/buildr-space/` — what the
axoniac registry serves and what a pod installs in production. A harness that
reached out to a registry to fetch the thing it is testing would fail for reasons
that have nothing to do with this agent.

The test asserts the installed version equals the `PACK_VERSION` constant, so a
stale fixture fails loudly rather than quietly testing an old pack. Refresh
instructions are in `tests/fixtures/README.md`.

## What tier 1 actually pins down

Beyond "the archive installs":

- the pack vendors exactly the 26 `buildr_*` tools, named individually — a pack
  that silently drops one fails on the name, not on an arithmetic mismatch;
- `consent.requires_env` declares `BUILDR_API_KEY` and `consent.domains` is
  `buildr.space` — computed from the archive's bytes, so it holds regardless of
  what the developer's environment happens to contain;
- **a `sub_agent` delegation can reach the tools.** The test reassembles exactly
  what `sub_agent` builds for `persona: "buildr-space-agent"` and asserts the
  registry contains `buildr_create_workspace` / `buildr_get_workspace` /
  `buildr_clone`. A persona can list an integration whose tools never register,
  and the delegation then fails at call time with the model reaching for a tool
  that does not exist;
- **the orchestrator, which shipped long before this pack, can now delegate to
  it.** `general-agent` sets `delegates_to_any_persona`, so `buildr-space-agent`
  joins its `delegation_roster` on install, and the *assembled* prompt — what the
  model actually sees — names it via `{{available_personas}}`;
- the orchestrator holds none of the `buildr_*` tools itself. It routes.

## Ground truth, not transcripts

Tier 3's spice assertions read the trace: it delegated to `buildr-space-agent`,
the delegated task still mentions the repo, and the delegate called create/poll/
clone. Those can all pass while nothing happened.

So after the suite, the test reads the answer off the sprite: it lists the
account's workspaces and runs `git -C /workspace/app config --get
remote.origin.url` on the candidates, asserting one really holds an octaweave
checkout. (The clone script authenticates through a credential helper, so no
token can be sitting in the URL this prints.)

It prefers the workspaces this run created, falling back to any workspace on the
account — because the persona is told to reuse a `ready` workspace rather than
provision a second sprite, and doing so is correct behaviour, not a failure.

## Cleanup

A workspace is a running sprite, which is a recurring bill. Each live tier
snapshots the account's workspace ids first and deletes whatever is new
afterwards, through `catch_unwind`, so a failed assertion still cleans up.

It deletes **by difference, never by name**. The account running this may hold
real workspaces, and a sweep that guessed from names would eventually eat one.

## Two product changes this required

Both are cases of a bound that was a constant needing to be a property of the
work, and both surfaced the same way: the harness failed in a manner that named
neither cause nor cure.

**1. The delegation timeout.** `sub_agent` cut every delegated run off at a
hard-coded 120 seconds. A buildr.space workspace reaches `ready` in one to two
minutes, so a delegate that had to create → poll → clone could never finish, and
the timeout was indistinguishable from an agent that simply failed the task.

The bound still exists — a runaway sub-agent burning tokens unattended is what it
guards — but it is now layered: a persona declares `max_run_secs` (the pack author
knows the work; `buildr-space-agent` asks for 900), `SUB_AGENT_TIMEOUT_SECS`
overrides that (the operator paying for the tokens gets the last word), and
everything is clamped to 30 minutes so a persona cannot declare its way out of
being bounded at all. Anything unparseable or zero falls back rather than
disabling the guard, because a typo must not turn it off.

**2. The api-tool timeout.** `HttpApiTool` gave every HTTP api-tool a fixed
30-second client timeout, while buildr.space allows itself 300s for a clone, 120s
for an exec and 600s for a build. The tool gave up first, so the agent saw a
failure for work the server was still doing — and for a build, the `runs` row was
written only *after* the command finished, so hanging up meant the result was
recorded nowhere and could not even be polled for.

`HttpApiToolConfig` now takes a per-tool `timeout_secs` (default 30, clamped to
600), and the pack declares one on every op whose server-side bound exceeds it.
buildr.space changed to match: `build`/`test` open the `runs` row *before* the
command starts and follow it in a spawned task, so the result survives a caller
that hangs up, and `wait_secs` decides how long the request holds open before
answering `running` with an id to poll.

Tier 1 asserts all of it — the declared timeouts, `wait_secs` and its default, the
`poll` flags, and the persona's `max_run_secs` — so the pack cannot quietly drift
back to the defaults that made this fail.
