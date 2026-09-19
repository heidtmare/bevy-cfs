# 0007 — Closing the vehicle-dynamics gap from the flight side

**Status:** closed. See [`crates/vehicle-dyn`](../../crates/vehicle-dyn),
[`spikes/rust-cfs-app`](../../spikes/rust-cfs-app).

**Verdict: the gap closes, and closing it exercised the reuse claim the whole
of Architecture B rests on.** Finding 0005 recorded that stock cFS publishes no
vehicle dynamics, so `apps/viz` showed `-- no source --` for attitude and wheel
speeds and derived the rest from command counters. A `no_std` crate
(`crates/vehicle-dyn`) now integrates rigid-body attitude dynamics and
reaction-wheel control, and finding 0006's Rust cFE application runs it *inside
cFE* and publishes the result on the software bus. Every row of the visualizer's
panel now names a real cFE field, and the same crate, compiled from the same
source files, also runs in `tools/fake-cfs` and in the visualizer's `--offline`
mode.

Nothing about the dynamics was hard. What cost the time — and what this finding
is actually about — was **allocating three message IDs**, which failed twice,
silently, in two different ways.

## 1. What runs where

```
crates/vehicle-dyn  ──compiled into──>  spikes/rust-cfs-app   (inside cFE, in the container)
                    ──compiled into──>  tools/fake-cfs        (a host process)
                    ──compiled into──>  apps/viz --offline    (the renderer itself)

crates/telemetry-model::encode_vehicle_state
                    ──called by──>  all three producers
                    ──inverse called by──>  apps/viz, for all three
```

One definition of the vehicle model and one definition of the wire format, with
no second copy anywhere. `docker/Dockerfile` copies the four `no_std` crates
into the image and builds them as path dependencies of the application's own
workspace — not a vendored snapshot, the same files `cargo test --workspace`
compiles on the host.

This is the concrete version of PLAN.md's rule that the `no_std` crates "are
what gets reused on the flight side if Architecture B goes ahead". Until now
that was an assertion about a hypothetical. The cost of honouring it turned out
to be two things, both small: the crates had to actually build `no_std` (they
did, because `cargo check --no-default-features` had been run on them all
along), and the container build needed a four-line workspace stub
(`docker/crates-workspace.toml`) because `version.workspace = true` needs a
workspace root and the real one lists members a flight image should not contain.

## 2. The message-ID allocation, wrong twice

Three IDs were needed: housekeeping, vehicle state, and commands.

**First attempt — a telemetry ID that worked by accident.** Finding 0006 reports
the Rust application's housekeeping being observed on the wire at `0x0890`, and
treats that as confirmation the packet was correctly formed. It was correctly
formed. It also reached the ground for a reason that had nothing to do with the
application: **`0x0890` is `MD_HK_TLM_MID`**, the Memory Dwell application's
housekeeping, and it is already in `to_lab`'s compiled-in subscription table.
The `md` application is not in this build's startup script, so nothing collided
and nothing complained.

Had `md` been loaded, two applications would have published different payloads
under one message ID and the ground would have decoded whichever arrived last —
a bug with no error anywhere in it, on either side.

**Second attempt — checking only half the tables.** The replacement IDs were
chosen by dumping `to_lab_sub.tbl` and picking values that were not in it:

```sh
docker run --rm cfs-build od -A d -t x4 \
    /src/build-native_std/exe/cpu1/cf/to_lab_sub.tbl
```

`0x0891` and `0x0892` passed. `0x1891` passed too, and is nevertheless claimed:
it is in **`sch_lab`'s schedule table**, so the scheduler sends a message to it
every few seconds. The symptom was the new application's command counter
incrementing on its own, and a `RUST_APP: NOOP` event in the container log that
no ground software had commanded:

```
EVS Port1 ... 66/1/RUST_APP 4: RUST_APP: NOOP
```

Which is a genuinely confusing thing to meet, because the obvious suspect is the
ground software that had just learned to send commands. The second table:

```sh
docker run --rm cfs-build od -A d -t x4 \
    /src/build-native_std/exe/cpu1/cf/sch_lab_table.tbl
```

The command MID moved to `0x1892` and the phantom commands stopped.

**What this actually means.** The lesson is not "check two tables". Both tables
are generated downstream of the topic-ID allocation in the bundle's
`*_topicid_values.h` headers, and the correct way to add an application is to
allocate a topic ID there and regenerate — at which point both tables, and the
EDS definitions, and anything else derived from them, are consistent by
construction. The constants in `crates/cfs-msg/src/rust_app.rs` are explicitly
*not* that: they are hand-picked values verified empirically against one pinned
build, and they are the single biggest thing standing between this spike and
anything flyable. This is also a concrete argument for the `native_eds`
configuration that the plan already lists as the highest-leverage open question.

## 3. Getting the packet to the ground

Because the new IDs are deliberately in no subscription table, `to_lab` does not
forward them, and the honest default is that a packet nobody asked for should
not arrive. `cfs-link` therefore commands `to_lab` at runtime with
`TO_LAB_ADD_PKT` (function code 2) for each stream it wants.

Two details cost a few minutes each and are pinned by tests:

- `TO_LAB_AddPacket_Payload_t` is `{ CFE_SB_MsgId_t Stream; CFE_SB_Qos_t Flags;
  uint8 BufLimit; }` — 7 significant octets, padded to **8**. `to_lab` reads the
  struct, so a 7-octet command is the wrong length and is rejected.
- The `CFE_SB_MsgId_t` in that payload is **little-endian**, four octets after
  the CCSDS primary header whose stream ID is **big-endian** by standard. The
  same 16-bit number, spelled two different ways, in one datagram.

**Re-subscribing needs a trigger, not a timer.** The first version re-sent the
subscriptions on the same 5-second keepalive as the enable-output command, on
the reasoning that `to_lab` forgets everything when cFS restarts. It does. But
`to_lab` answers an add-packet for a stream it already carries by incrementing
its **command error counter**, which this project puts on the screen — so within
a minute the panel showed a dozen errors caused entirely by the ground asking
for something it already had, in exactly the indicator an operator uses to spot
real problems. The link now re-subscribes only when nothing has arrived on those
streams for four seconds, which is the condition that actually means the
subscription is gone.

## 4. What the dynamics are, and what they are not

Rigid-body attitude propagation with a four-wheel pyramid reaction-wheel array,
a PD attitude controller with gyroscopic feed-forward, wheel momentum
saturation, single-axis solar-array sun tracking, a deployment sequence and a
pointing survey. Roughly 300 lines, `no_std`, 26 tests.

It is not a simulator of any particular spacecraft and not a validated one of
anything. The inertia tensor and wheel sizes are plausible small-satellite
numbers **chosen so that a 90° slew takes seconds rather than the many minutes a
large bus would need** — the one place where "make it watchable" outranked "pick
a typical number". That choice does not compromise the physics, because every
other quantity is then sized consistently with it: a vehicle of that inertia
really would slew that fast with wheels of that momentum capacity.

What *is* load-bearing, and is asserted rather than claimed:

- **Momentum is conserved.** `total_angular_momentum_is_conserved` checks the
  inertial-frame total across a 30-second run. Nothing writes an attitude
  directly; every number on the downlink is the output of an integration, so it
  lags, overshoots and settles the way telemetry does.
- **The torque allocation is exact and minimal.** The pyramid geometry is chosen
  at the tetrahedral angle, which makes the array's Gram matrix exactly
  `(4/3)·I` and collapses the minimum-norm pseudo-inverse to a scalar multiple
  of the transpose — the whole allocation is four dot products with no matrix
  inverse. `allocation_is_the_smallest_that_works` checks it against the
  null-space family of alternatives.
- **Saturation is real and recoverable.** A wheel at its momentum limit stops
  producing torque in the direction that would load it further, and still
  accepts torque that unloads it.
- **The array angle is a consequence of attitude**, not a free-running ramp.
  `array_finds_the_best_angle_its_one_hinge_allows` checks the yoke angle
  against every other angle at 3° resolution: a single-axis drive generally
  *cannot* point the panel at the sun, and the test pins that it finds the best
  available angle rather than pretending otherwise.

### One thing that was genuinely a bug, not a tuning choice

The vehicle would not detumble. With wheels this size the gyroscopic coupling
`ω × h` during a fast slew is *comparable to the maximum wheel torque itself*,
so a controller that ignores it spends most of its authority fighting its own
wheels; the body rate fell from 0.30 to 0.17 rad/s over 40 seconds and stopped
there. Cancelling the known nonlinearity as feed-forward before closing a linear
loop around what remains — standard practice, and the reason the PD gains can be
chosen from the inertia tensor alone — fixed it. The cancellation goes *inside*
the torque clamp; putting it outside would let the commanded torque exceed what
the wheels can deliver and quietly turn the actuator limit into a fiction.

## 5. What the visualizer shows now

Against a cFS with the application loaded, every signal names a real flight
field and the pipeline line reads `LIVE - vehicle state from RUST_APP, inside
cFE`. Against `fake-cfs` the panel reads `LIVE - vehicle state from fake-cfs`
and the flight-software block says `RUST_APP -- (not loaded: no vehicle dynamics
on this bus)`.

**How the visualizer tells those apart is itself the interesting part.** Both
producers emit byte-identical packets on the same message ID — that is the point
of sharing the encoder — so the packet cannot identify its own author, and a
flag inside it would be a claim the ground could not check. The discriminator is
the presence of `RUST_APP`'s *own housekeeping* on the bus, which is evidence of
a kind the vehicle packet cannot manufacture.

Six keys now command the vehicle rather than a counter: slew to next target,
inertial hold, deploy, stow, safe mode, dump momentum. The round trip is visible
as motion — press `T` and the model slews, because flight software integrated a
new attitude and published it.

`Hold` and `Safe` are deliberately distinct and look different on screen: one
keeps the attitude controller running and stays exactly where it is, the other
gives up pointing entirely and only damps the rates, so the vehicle coasts to a
stop wherever the disturbance left it.

Two smaller things this change broke and fixed:

- **The camera was framed for a stowed vehicle.** The arrays now deploy
  automatically once the tumble is damped (the order a real vehicle does it in —
  deploying while tumbling would fling the panels around on their hinges), and
  the first capture afterwards had both wings running off the top and bottom of
  the image. The camera distance is now derived from the rig's swept radius and
  the field of view rather than written down.
- **The `--offline` panel claimed to be "interpolating between real samples".**
  There is no jitter buffer offline — the state is computed for the frame's
  timeline position, so there is nothing to interpolate between. That is exactly
  the kind of lie the panel exists to catch, and it survived until a capture was
  read carefully.

`--offline` now runs the real model forward rather than evaluating a closed-form
curve, so an offline screenshot is a pose the vehicle could actually reach and is
comparable with a live one instead of merely resembling it.

## 6. What this does not answer

- **Table services and `CFE_TBL_*` are still not exercised**, unchanged from
  0006 §3. The gains and inertia here are compile-time constants; a real
  application would carry them in a table, which involves a memory-sharing and
  CRC-validation model a Rust struct would have to match byte for byte.
- **The `CFE_ES_ExitApp`-after-`catch_unwind` crash from 0006 §4 is still
  open**, and this application still ships the same workaround.
- **Message IDs are still hand-allocated.** See §2; this is now the most
  concrete argument in the repository for trying `native_eds`.
- **The control loop's timing is `CFE_SB_ReceiveBuffer`'s timeout**, not a
  scheduler subscription. It works, and `dt` is measured from `CFE_TIME_GetTime`
  rather than assumed so a late loop integrates the time it actually lost — but
  a real attitude control loop would be driven by `sch_lab` at a guaranteed
  cadence, which is the other thing a correctly allocated topic ID would buy.
