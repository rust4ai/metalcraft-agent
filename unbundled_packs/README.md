# Unbundled agent packs

Agent packs that live in this repo but are **deliberately not embedded in the
binary**. `seed/` is what `include_dir!` compiles in and what
`seed::install_seed_agent_packs` installs on every boot; anything here is
outside that tree, so a fresh pod comes up without it.

| Pack | Why it is here |
| --- | --- |
| `email` | Read-only IMAP mailbox access. Its `email_*` tools are native Rust (`src/tools/email_imap.rs`) and stay compiled into the agent — only the pack that declares and describes them is unbundled, so installing this pack later still works on any build that has those tools. |
| `metalcraft-email` | The `memail_*` HTTP tools for the hosted email.metalcraftai.com cache. Purely declarative; nothing of it remains in the binary. |

Reading someone's mailbox is not a capability every pod should arrive holding.
These are opt-in: distribute them through a registry (axoniac) and let a pod
install one the same way it installs anyone else's pack.

They are still validated by the test suite — `tests/http_api_tool_test.rs` and
the `native_tools` drift guard in `src/tools/mod.rs` scan this directory
alongside `seed/agent_packs` — so an unbundled pack cannot rot into something
that would fail at install.

## Building an archive

To publish one, build the `.agentpack` with the same code the pod uses to read
it (this is what computes `content_sha256` and the consent summary — a hand-made
zip is rejected at install):

```sh
cargo run --example pack_dir -- unbundled_packs/email /tmp/email.agentpack
```

Then upload that archive to the registry. A pod installs it by reference
(`axoniac:@handle`) or from a link, and the `email` pack's native tools light up
because the binary still carries them.
