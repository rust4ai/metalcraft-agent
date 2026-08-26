# Test fixtures

## `buildr-space-0.2.0.agentpack`

The real [buildr.space](https://buildr.space) agent pack, vendored as a binary
fixture so `tests/buildr_space_spice_test.rs` can install it into a throwaway pod
data dir with no registry, no network and no credential. A harness that reached
out to a registry to fetch the thing it is testing would fail for reasons that
have nothing to do with this agent.

It is the byte-for-byte archive `packctl` builds from
[`axoniac-seeded-agent-packs`](https://github.com/rust4ai/axoniac-seeded-agent-packs)
`packs/buildr-space/`, which is what the axoniac registry serves and what a pod
installs in production. 26 `buildr_*` HTTP api-tools, one persona
(`buildr-space-agent`), one skill, one preset.

### Refreshing it

```bash
cd ~/ai/axoniac-seeded-agent-packs
cargo run -- build buildr-space
cp dist/buildr-space-<version>.agentpack ~/ai/metalcraft-agent/tests/fixtures/
rm tests/fixtures/buildr-space-<old version>.agentpack
```

Then update `FIXTURE` / `PACK_VERSION` in `tests/buildr_space_spice_test.rs`. The
test asserts the installed version equals `PACK_VERSION`, so a fixture that drifts
from the constant fails loudly instead of quietly testing an old pack.
