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

```sh
cargo run -p tlm-capture -- --dest-ip <host-as-cfs-sees-it> --seconds 10 --out fixtures/hk.cfspkt
```

## Replaying

```sh
cargo run -p fake-cfs -- replay fixtures/hk.cfspkt --rate 10 --loop
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
