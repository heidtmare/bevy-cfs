# cFS ↔ Bevy investigation

Can a Bevy (Rust) animation layer be driven by NASA core Flight System
telemetry — and can Rust live inside cFS itself? See [PLAN.md](PLAN.md) for the
phases, gates and risks; [docs/findings/](docs/findings/) for answers as they
land.

## State

**Phase 0 and Phase 2 gates met.** cFS v7.0.1 builds and runs in Docker, and 42 real packets
across 20 message IDs have been captured and verified against the decoder with
zero parse errors. The capture is committed as a fixture and backs the golden
tests, so the codec is checked against real cFE bytes on every `cargo test`.

Confirmed against the real build: message IDs, `to_lab` command code and payload,
telemetry timestamp layout and epoch. Still open: the command checksum, and
payload endianness — no payload field has been decoded from a real packet yet.
See [docs/findings/](docs/findings/).

Phase 2 added the rate-mismatch layer: a jitter buffer that plays back two
telemetry periods behind, interpolating between real samples and **never
extrapolating** — past the newest sample it holds and reports staleness, because
a display that keeps animating after telemetry stops is inventing data. Verified
against induced packet loss, reordering and signal loss in
[headless tests](crates/bevy_cfs/tests/headless.rs).

One constraint worth knowing up front: **Docker Desktop does not forward UDP from
a container to the macOS host**, so a native Bevy app cannot take live telemetry
from the container without a relay. Development against `fake-cfs` and fixture
replay is unaffected.

| Crate | Purpose | std |
|---|---|---|
| [crates/ccsds](crates/ccsds) | CCSDS space packet codec, zero-copy | no_std |
| [crates/cfs-msg](crates/cfs-msg) | cFE message IDs, `to_lab` commands | no_std |
| [crates/telemetry-model](crates/telemetry-model) | Decoded telemetry as domain state + interpolation | no_std |
| [crates/cfs-link](crates/cfs-link) | UDP transport, handshake, link health | std |
| [crates/bevy_cfs](crates/bevy_cfs) | Bevy plugin: resources, systems, playback | std |
| [tools/fake-cfs](tools/fake-cfs) | Synthetic cFS: telemetry generator and fixture replayer | std |
| [tools/tlm-capture](tools/tlm-capture) | Record real telemetry to a fixture | std |

The three `no_std` crates must never gain a Bevy dependency: they are what gets
reused on the flight side if Architecture B goes ahead.

`bevy_cfs` takes `bevy` with `default-features = false` — ECS and time, no
renderer — so the whole workspace tests headlessly without a GPU. `apps/viz`
joins in Phase 4 and brings the rendering.

## Watch it animate, without cFS

```sh
cargo run -p fake-cfs -- serve --cmd-port 11234 --tlm-port 11235 --rate 10
cargo run -p bevy_cfs --example headless_viz -- --cmd-port 11234 --tlm-port 11235
```

Prints the interpolated state each frame, with link health and freshness. Uses
non-default ports because the cFS container publishes 1234.

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

```sh
docker compose -f docker/compose.yaml up -d --build
```

See [docker/README.md](docker/README.md) for ports (telemetry is on **2234**, not
the 1235 in older docs), capturing fixtures, and the gotchas already encoded in
the compose file.

## Tests

```sh
cargo test --workspace
```

No network, no cFS, no container required.

## Next

1. **Phase 3, the actual research question:** how telemetry should map onto
   Bevy's animation system — direct `Transform` drive, clip-as-lookup-table, or
   `AnimationGraph` blending. All three consume the same `SpacecraftState`, which
   is what keeps the comparison fair.
2. Decode a real payload — settles the last substantive item in the
   [verification backlog](docs/findings/0001-verification-backlog.md).
3. Try the `native_eds` build configuration, which is how the "generate Rust
   types from EDS rather than hand-writing them" question gets answered.
