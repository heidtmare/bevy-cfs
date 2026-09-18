# 0003 — Driving animation from telemetry: the rate-mismatch layer

**Status:** closed. Phase 2 gate met — the plugin produces smooth motion from a
live telemetry source, and degrades gracefully under packet loss, reordering and
signal loss.

**Pinned:** Bevy **0.19.1**.

## The problem

Telemetry arrives at 1-10 Hz, jittered. Rendering wants a value 60-120 times a
second. Snapping to the newest sample each frame produces visible stepping; at
low rates the vehicle appears to teleport.

## The approach, and what it costs

Play back on a deliberate delay of **two telemetry periods**
(`BufferConfig::for_rate`). Holding that much history means there is almost
always a sample on *both* sides of the playback instant, so every frame is an
interpolation between two real measurements rather than a guess past the end.

The cost is latency, bought knowingly: a viewer cannot perceive 200 ms of delay
in an attitude display, but they can certainly perceive stepping. For an ops
display where absolute latency matters more than smoothness, the delay is one
config field.

## The rule that shaped everything: never extrapolate

Past the newest sample the buffer **holds** the last known value and reports
increasing staleness — it does not continue the motion.

This matters more than it sounds. A display that keeps smoothly rotating a
vehicle after telemetry stopped is inventing data, and it invents it most
convincingly at exactly the moment something has gone wrong. `Freshness` is
therefore part of the public API (`NoData`, `Warming`, `Live`, `Holding { age }`,
`Stale { age }`) and is meant to be surfaced in the UI, not just logged. A test
asserts the held value does not drift.

## Where the logic lives, and why not in Bevy

The jitter buffer, interpolation and clock resync are in **`telemetry-model`**,
which has no Bevy dependency. `bevy_cfs` is a thin adapter: two systems, four
resources, no logic worth testing.

The reason is testability. The hard parts — bracketing, reordering, clock jumps —
are far easier to test as plain functions than inside a running `App`, and 13 of
the buffer's tests need no Bevy at all. It also keeps the door open for
Architecture B, where this code could run on the flight side.

`bevy_cfs` takes `bevy` with **`default-features = false`**: it needs ECS and
time, not a renderer. It compiles and tests headlessly in CI with no GPU, and the
rendering choices stay in `apps/viz`.

## Two real bugs the tests caught

Both were found by tests written from the *failure* direction — what should
happen when the link misbehaves — rather than from the happy path.

1. **Resync keyed on the wrong sample.** The clock-jump detector compared against
   the newest *held* sample via `back()`. After a backwards clock jump the new
   timeline lands at the *front* of the deque, so `back()` was still the stale
   timeline and the resync never fired. Now keyed on the arriving sample.

   Not hypothetical: cFE restarts its clock on a processor reset, which we
   watched happen during bring-up (finding 0002).

2. **Pruning discarded the left-hand bracket.** History older than a fixed age
   was trimmed, including the newest sample *before* the playback instant — the
   one interpolation needs. Across a telemetry gap, playback snapped to the far
   side instead of sweeping across it. Now the rule is structural: keep at least
   one sample at or before the playback instant, and let `capacity` bound memory.

## Testing approach

`crates/bevy_cfs/tests/headless.rs` runs the real plugin against a real UDP
socket, with a stand-in `to_lab` that injects impairments — dropping one packet
in 7 and delivering one in 5 out of order. The gate assertion is that the
per-frame step in a steadily-sweeping value stays below 2 deg where nominal is
0.96 deg; stepping or snapping shows up as a spike.

Two details that make it meaningful:

- **`Time` is advanced by hand**, with no `TimePlugin`. Otherwise the test
  measures how fast the machine ran, not how well the buffer interpolates.
- **The impairment test asserts it actually impaired something**
  (`buffer.reordered > 0`). The first version silently restored packet order —
  it sent the held packet *before* its successor — and passed anyway. A
  fault-injection test that cannot detect its own no-op is decoration.

## Ports: a local collision

The cFS container publishes UDP 1234, so `fake-cfs serve` cannot bind the default
command port while the container is up — it exits with "Address already in use".
Run it on other ports (`--cmd-port 11234 --tlm-port 11235`) or stop the container.

## Open for Phase 3

The buffer emits a `SpacecraftState` per frame. How that drives *animation* is
the next question, and the three candidate mappings (direct transform drive,
clip-as-lookup-table, graph blending) all consume this same struct — which is
what keeps the comparison fair.

One thing already visible: `Mode` snaps at the interpolation midpoint rather than
blending, because interpolating an enum would invent states that never existed.
Smoothing that transition is an *animation* concern (a blend between two poses),
not a telemetry one — which is exactly the Phase 3 boundary.
