# Running cFS

```sh
docker compose -f docker/compose.yaml up --build
```

The first build clones the cFS bundle with submodules and compiles it; expect it
to be slow under emulation on Apple Silicon.

## Pointing telemetry at the host

`to_lab` sends nothing until it is told where to send. The destination must be
the host **as the container sees it**, not `127.0.0.1` — that would point cFS at
itself.

```sh
# Find the gateway address the container sees, then capture:
GW=$(docker compose -f docker/compose.yaml exec cfs sh -c "ip route | awk '/default/ {print \$3}'")
cargo run -p tlm-capture -- --dest-ip "$GW" --seconds 10 --out fixtures/hk.cfspkt
```

`tlm-capture` sends the enable-output command itself and re-sends it every five
seconds, so it recovers if cFS restarts mid-capture.

## Status

**Unverified.** Docker was not running when this was written, so the image has
never been built. Treat the Dockerfile as a starting point, not a known-good
recipe; the likely friction points are:

- the bundle's `Makefile.sample` / `sample_defs` layout, which has moved between
  releases — check the bundle's own README for the pinned `CFS_REF`;
- `core-cpu1` expecting to write to its `cf` ramdisk directory;
- message-queue limits, which some Linux hosts set too low for cFE's defaults
  and which surface as app-startup failures in the console log.

Record whatever it actually took in `docs/findings/`.
