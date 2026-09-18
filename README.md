# cFS ↔ Bevy investigation

Can a Bevy (Rust) animation layer be driven by NASA core Flight System
telemetry — and can Rust live inside cFS itself? See [PLAN.md](PLAN.md) for the
phases, gates and risks; [docs/findings/](docs/findings/) for answers as they
land.

## State

Phase 0/1 scaffolding is in place and tested. No cFS instance has been built
yet — Docker was not running on this machine — so everything so far is exercised
against `fake-cfs` rather than the real thing.

| Crate | Purpose | std |
|---|---|---|
| [crates/ccsds](crates/ccsds) | CCSDS space packet codec, zero-copy | no_std |
| [crates/cfs-msg](crates/cfs-msg) | cFE message IDs, `to_lab` commands | no_std |
| [crates/telemetry-model](crates/telemetry-model) | Decoded telemetry as domain state + interpolation | no_std |
| [crates/cfs-link](crates/cfs-link) | UDP transport, handshake, link health | std |
| [tools/fake-cfs](tools/fake-cfs) | Synthetic cFS: telemetry generator and fixture replayer | std |
| [tools/tlm-capture](tools/tlm-capture) | Record real telemetry to a fixture | std |

The three `no_std` crates must never gain a Bevy dependency: they are what gets
reused on the flight side if Architecture B goes ahead.

`crates/bevy_cfs` and `apps/viz` join the workspace in Phase 2, once a Bevy
release is pinned. Keeping them out until then means `cargo test` on the codec
never waits on a Bevy build.

## Try it without cFS

```sh
# terminal 1 — pretends to be ci_lab + to_lab
cargo run -p fake-cfs -- serve --rate 20

# terminal 2 — sends the enable-output command, records what comes back
cargo run -p tlm-capture -- --seconds 5 --out fixtures/demo.cfspkt
```

Expected: `tlm-capture` reports captured packets and lists the message IDs it
saw. `fake-cfs` withholds telemetry until the enable-output command arrives,
exactly as `to_lab` does, so this exercises the real handshake.

## With cFS

See [docker/README.md](docker/README.md). The container setup is written but
**unverified** — Docker was unavailable.

## Tests

```sh
cargo test --workspace
```

No network, no cFS, no container required.

## Next

1. Build the cFS container and capture real packets (Phase 0 gate).
2. Work through [docs/findings/0001-verification-backlog.md](docs/findings/0001-verification-backlog.md) —
   every assumption currently baked into the decoder, in the order it will bite.
3. Pin Bevy, add `crates/bevy_cfs`, build the jitter buffer and interpolation
   against `fake-cfs` (Phase 2).
