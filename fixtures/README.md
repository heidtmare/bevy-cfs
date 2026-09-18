# Packet fixtures

Raw telemetry captured from a real cFS instance, used as golden tests for the
decoder. They are what lets Phase 1 onward proceed with cFS switched off.

## Format

Length-prefixed frames, repeated to end of file:

```
u32 little-endian length | that many octets of one space packet
```

Written by `tlm-capture`, read by `fake-cfs replay`.

## Recording

Running *inside* the cFS container's network namespace, where `127.0.0.1`
reaches both apps, needs no gateway address — which is why the committed capture
was taken this way:

```sh
docker run --rm --network=container:docker-cfs-1 \
  -v "$PWD":/w -w /w -e CARGO_TARGET_DIR=/tmp/t rust:1-slim \
  cargo run -q -p tlm-capture -- \
    --cfs-host 127.0.0.1 --dest-ip 127.0.0.1 --tlm-port 2234 \
    --seconds 12 --out fixtures/cfs-v7.0.1-hk.cfspkt
```

From the host it works too, as long as `--dest-ip` is this machine's address *as
cFS sees it* — the IPv4 host gateway under Docker Desktop, not `127.0.0.1`:

```sh
cargo run -p tlm-capture -- --dest-ip 192.168.65.254 --seconds 10 --out fixtures/hk.cfspkt
```

## Replaying

```sh
cargo run -p fake-cfs -- replay fixtures/hk.cfspkt --rate 10 --loop
```

## Committed captures

- `cfs-v7.0.1-hk.cfspkt` — 42 packets, 20 message IDs, from nasa/cFS v7.0.1
  (Draco, EDS disabled, linux/arm64). Backs the golden tests in
  `crates/ccsds/tests/golden.rs` (headers) and
  `crates/cfs-msg/tests/real_payloads.rs` (payloads, and the endianness proof).

`demo.cfspkt` is *not* committed — it is synthetic, proves nothing, and is
covered by the `.gitignore` rule below. Regenerate it whenever you want one:

```sh
cargo run -p fake-cfs -- serve --cmd-port 11234 --tlm-port 11235 --rate 20 &
cargo run -p tlm-capture -- --cmd-port 11234 --tlm-port 11235 --seconds 5 --out fixtures/demo.cfspkt
```

## What to commit

Commit captures from a real cFS run — they are the evidence that the decoder
matches a real build, and they are small. `.gitignore` excludes `*.cfspkt` by
default so that scratch captures do not pile up; force-add the ones worth
keeping:

```sh
git add -f fixtures/hk.cfspkt
```

Alongside each capture, note in `docs/findings/` which cFS ref produced it. A
fixture whose build is unknown cannot be used to settle a layout question later.
