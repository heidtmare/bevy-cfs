# Bevy ↔ NASA cFS Integration Investigation

**Goal:** determine how cleanly a Bevy (Rust) animation/visualization layer can be driven by, and
eventually embedded alongside, NASA core Flight System telemetry — and produce a working vertical
slice plus a written recommendation.

**Nature:** a time-boxed technical spike (~4–6 weeks part-time), not a product. Every phase ends in a
decision gate with a written answer, and each phase is independently useful if the next is cancelled.

---

## 1. The question, stated precisely

"Integrating Bevy animations with cFS" resolves into three distinct architectures. They are not
alternatives to pick blindly — they stack in increasing risk, and the investigation should walk them
in order.

| # | Architecture | Coupling | Risk | Payoff |
|---|---|---|---|---|
| A | **Ground-side viz.** Bevy app is a telemetry consumer over UDP (`to_lab` out, `ci_lab` in). No FFI, no Rust inside the flight build. | Socket | Low | Real animated 3D view of a live cFS instance; ops/console/training value |
| B | **Rust cFS app.** A cFS application written in Rust, loaded by cFE ES, publishing on the Software Bus. Bevy still runs on the ground, but the flight side is now partly Rust. | FFI/bindgen | Medium | Proves Rust is viable *inside* cFS; opens shared message types between flight and viz |
| C | **Bevy ECS inside/next to the flight process.** Headless Bevy (ECS + animation, no renderer) as a simulation/kinematics component in a cFS app, or in a hardware-in-the-loop sim process. | Deep | High | Shared scene/kinematics model between sim and viz; likely the real research finding, positive or negative |

**Recommendation:** commit to A, spike B, only scope C after B's findings. A is the deliverable; B
and C are the investigation.

---

## 2. Environment prerequisites (blocking, do first)

- **cFS does not target macOS.** OSAL's POSIX port assumes Linux (dynamic module loading via
  `dlopen` of `.so`, `elf2cfetbl`, permissions/scheduling assumptions). Do not burn days on a native
  macOS build.
  → **Run cFS in Linux containers** (`--platform linux/amd64` on Apple Silicon, or a UTM/Lima VM).
    Bevy runs natively on macOS and talks to the container over UDP with a published port.
- **`cmake` is missing** — `brew install cmake` (needed only if building cFS outside the container;
  keep the container image self-contained anyway).
- **Pin versions on day one and record them in the repo**: cFS bundle tag, cFE version, and the Bevy
  release. Bevy's animation API has changed shape nearly every release
  (`AnimationGraph`, `AnimationTarget`, curve-based animation) — the plan below assumes a
  graph-based `AnimationPlayer`; verify against the version you pin before writing systems.

**Exit criterion:** `docker compose up cfs` produces a running `core-cpu1` that emits telemetry on a
host-reachable UDP port, reproducible from a clean clone.

> **Done** — see `docs/findings/0002`. cFS v7.0.1 builds and runs on a host-reachable UDP port,
> reproducible from a clean clone.
>
> This amendment previously said Docker Desktop for macOS does not forward UDP from a container to
> the host, and that Phase 4 would therefore need a UDP→TCP relay or a Linux VM. **That was wrong**,
> and Phase 4 found out: telemetry reaches the host over plain UDP as long as `to_lab` is pointed at
> the **IPv4** gateway (`getent ahostsv4 host.docker.internal`, `192.168.65.254` here). The original
> probe used the `host.docker.internal` name, which resolves to IPv6 — an unreachable destination,
> not a blocked transport. See `docs/findings/0005`. No relay was ever built, so the cost of the
> mistake was one paragraph of wrong documentation.

---

## 3. Phase 0 — Baseline cFS (2–3 days)

1. Clone the `nasa/cFS` bundle with submodules; build the lab configuration
   (`cfe`, `osal`, `psp`, `sample_app`, `ci_lab`, `to_lab`, `sch_lab`).
2. Dockerfile: builder stage (cmake/gcc/python) → runtime stage running `core-cpu1`.
3. Confirm the loop by hand before writing any Rust:
   - send `TO_LAB` *Enable Output* command to `ci_lab` (UDP **1234**) naming the host as the
     telemetry destination;
   - observe telemetry arriving on UDP **1235**; capture it with `tcpdump`/`nc` to a `.pcap`/raw file.
4. **Commit those captures into `fixtures/`.** They become the decoder's golden tests and let all
   Bevy work proceed with the container stopped.

**Gate:** raw bytes captured and byte-offsets of one housekeeping packet annotated by hand.

---

## 4. Phase 1 — Message layer in Rust (3–5 days)

Separate crates so the flight-agnostic parts stay testable and `no_std`-friendly for Phase B.

- `crates/ccsds` — CCSDS Space Packet primary header (6 bytes), command secondary header
  (function code + checksum), telemetry secondary header (time). Zero-copy borrowed views over
  `&[u8]`, plus builders. Property tests for round-trip; golden tests against `fixtures/`.
- `crates/cfs-msg` — cFE/lab message definitions and message IDs.
  - Start hand-written for the four or five packets you actually need.
  - **EDS is a build configuration, not a project.** The bundle ships a `native_eds` config
    alongside `native_std`. Switching to it is a one-word change in the Dockerfile; the open
    question is only what it does to the generated message IDs and whether the XML is pleasant to
    consume from Rust.
  - **Then evaluate EDS** (cFS's Electronic Data Sheets, XML message definitions, CMake-gated):
    if enabled, generate Rust structs from the same XML the C build consumes. This is the single
    highest-leverage finding in the whole investigation — it decides whether flight and ground
    types can share one source of truth instead of drifting.
  - Record the answer on endianness (CCSDS headers are big-endian; payloads follow the build's
    native/`_MSG_` configuration) and on struct padding/alignment. Getting this wrong is the most
    likely source of silent garbage.

**Gate:** `cargo test` decodes every captured fixture packet into typed values, no cFS running.

---

## 5. Phase 2 — Transport + Bevy plugin (1–1.5 weeks)

- `crates/cfs-link` — UDP transport. Owns the `to_lab` enable-output handshake, re-enables on
  restart, exposes decoded packets on a bounded channel, and counts drops/gaps/sequence-count
  discontinuities. Runs on its own thread; **never block a Bevy system on the socket.**
- `tools/fake-cfs` — synthetic telemetry generator (sweeping attitude, joint angles, mode changes)
  and a fixture replayer. Keeps the whole Bevy side developable offline and makes animation behavior
  reproducible in tests and demos.
- `crates/bevy_cfs` — the Bevy plugin. This is where the actual research question lives:
  - **Resources:** `CfsLink` (connection state), `TelemetryClock`.
  - **Events/components:** decoded packets → domain state (`Attitude(Quat)`, `JointAngles`,
    `WheelRpm`, `Mode`) as components on entities, not as raw packets.
  - **Rate mismatch is the core problem.** Telemetry arrives at 1–10 Hz; rendering is 60–120 Hz.
    Use a small jitter buffer with a deliberate playback delay (~2 telemetry periods), interpolate
    between the two bracketing samples, and extrapolate only with an explicit staleness cutoff —
    then *show* staleness in the UI rather than animating a lie.
  - **Time:** map cFE TIME (MET/STCF, TAI vs UTC) to a monotonic playback timeline. Do not drive
    animation from wall clock; telemetry timestamps are authoritative and can jump.

**Gate:** plugin driven purely by `fake-cfs` produces smooth motion; induced packet loss and
out-of-order delivery degrade gracefully.

> **Done** — see `docs/findings/0003`. Bevy pinned at 0.19.1. The buffer, interpolation and clock
> resync live in `telemetry-model` (Bevy-free, 13 unit tests); `bevy_cfs` is a thin adapter taking
> `bevy` with `default-features = false`, so the workspace tests headlessly. Two real bugs surfaced,
> both about misbehaving links rather than the happy path.

---

## 6. Phase 3 — The animation question itself (1 week)

The interesting part is *how* telemetry should map onto Bevy's animation system. Evaluate three
mappings on the same glTF model and write up which fits which kind of signal:

1. **Direct transform drive** — telemetry writes `Transform` each frame; `AnimationPlayer` unused.
   Right for continuous physical state (attitude, gimbal angle, solar array rotation). Simple and
   exact.
2. **Clip-as-lookup-table** — author a clip for a mechanism's full travel (Blender in a real
   pipeline; here `tools/gltf-gen` emits an equivalent staged, eased clip), then *seek*
   the active animation to `normalized_telemetry * clip_duration` instead of letting it play.
   Right for rigged multi-part mechanisms (deployment arms, latches, docking hardware) where an
   artist owns the motion and telemetry owns only the parameter.
3. **Graph blending** — `AnimationGraph` weights driven by telemetry/mode. Right for discrete modes
   with transitions (stowed → deploying → deployed, safe-mode poses).

Also test: animating non-`Transform` properties (material emissive for heaters/thruster plumes,
visibility for fault indicators) via custom animatable properties, since spacecraft telemetry is
mostly *not* rigid-body motion.

**Gate:** a written recommendation table — signal type → mapping — backed by the running demo.

> **Done.** See [docs/findings/0004-animation-mappings.md](docs/findings/0004-animation-mappings.md).
> The answer is that there is no single answer: a real rig mixes all three, and the useful output is
> the signal-type table rather than a winner. Direct drive and clip seek were given every constant in
> common — `telemetry-anim` owns the rig and the easing curve, and `gltf-gen` bakes the keyframes from
> it — and still diverge by **5.29°** mid-travel, because a sparse clip is a sampling of a curve and
> not the curve. Confirmed twice, in angle space and from the `Transform` Bevy wrote, agreeing to
> 0.01°. Also surfaced: `telemetry-model` claimed `no_std` but had never been built without `std`.
> The model is generated (`cargo run -p gltf-gen`), not an opaque `.glb`, so the rig is reviewable.

---

## 7. Phase 4 — Vertical slice demo (1 week)

`apps/viz`: glTF spacecraft, a 3D view driven live by the containerized cFS, a telemetry side panel,
link/staleness indicator, and one **command path** (a button that sends a command to `ci_lab` and
shows the resulting state change). The command path matters: it proves the loop closes, and it
forces you to confront command authentication/validation questions early.

Record a screen capture against live cFS and against a replayed fixture.

**Gate:** the loop closes — a command sent from the viz produces an observable change in telemetry
that the viz displays, against real cFS.

> **Done.** See [docs/findings/0005-vertical-slice.md](docs/findings/0005-vertical-slice.md).
> `apps/viz` runs against the container over plain UDP; **N** sends a `SAMPLE_APP` no-op and the
> `CommandCounter` it increments comes back on the downlink 0.90-5.52 s later (mean 2.94 s over
> twelve round trips — a scheduler cadence, not network latency). Screenshots against live cFS and
> against `fake-cfs` are committed under `docs/findings/images/`.
>
> Two things the gate did not anticipate. First, decoding the first real *payload* exposed a
> four-octet offset bug — `CFE_MSG_TelemetryHeader_t` has an alignment spare, so cFE payloads start
> at octet 16 — which had survived three phases because only headers had ever been verified. That
> closed verification-backlog items 6 and 7 as well. Second, stock cFS publishes **no vehicle
> dynamics** at all, so the viz names the real cFE field behind each animated signal and shows
> `-- no source --` for attitude and wheel speeds rather than inventing them.
>
> On command authentication, which the gate predicted would surface: it did. `ci_lab` accepts any
> well-formed datagram on UDP 1234, and reports `EnableChecksums = 0` in its own housekeeping —
> it does not validate. Fine for a lab build; worth stating before anyone points it at a vehicle.

---

## 8. Phase 5 — Rust-inside-cFS spike (Architecture B) (1 week, time-boxed hard)

`spikes/rust-cfs-app`: a `cdylib` implementing a cFS app entry point, loaded via
`cfe_es_startup.scr`, registering with ES/EVS, subscribing to the Software Bus and publishing a
telemetry packet.

Enumerate and answer explicitly, because these decide feasibility:
- `bindgen` over cFE headers; which cFE symbols resolve at `dlopen` time from the core executable.
- **Panics must not unwind across FFI** — `panic = "abort"` vs a `catch_unwind` boundary at every
  entry point, and what aborting means for a cFE task.
- Allocation: Rust's global allocator vs cFE's memory pools; whether flight rules permit heap use.
- The cFE task model (OSAL tasks are OS threads) vs Rust's threading/TLS assumptions.
- Table services, event filters, and `perf` instrumentation from Rust.

**Gate:** honest verdict — *viable / viable with constraints / not worth it* — with the specific
blocker named. A negative result here is a legitimate deliverable.

> **Done.** See [docs/findings/0006-rust-cfs-app.md](docs/findings/0006-rust-cfs-app.md). Verdict:
> **viable with constraints**. A pure-Rust `cdylib` loads into cFE ES exactly like a C app — no CMake
> integration needed, since `add_cfe_app` links C apps against a headers-only interface target and
> resolves every `CFE_*` symbol at `dlopen` time anyway. It registers with EVS, creates an SB pipe,
> and publishes correctly-formed telemetry, confirmed live against v7.0.1 and independently verified
> by capturing the packet off the wire with `tlm-capture` (MID `0x0890`, ~1 Hz, zero drops). bindgen
> against the real headers worked, with three named, worked-around gaps (macros invisible to it
> entirely, one cast-typed `#define` it silently drops rather than errors on, and a double-prefixing
> default on cFE's own enum-naming convention).
>
> The constraint: a Rust app that has caught a panic via `catch_unwind` must never call
> `CFE_ES_ExitApp` again on that thread afterward — doing so crashes the whole `core-cpu1` process
> (`SIGTRAP`), taking every other app down with it, not just the one that panicked. Confirmed by
> deliberately crashing the container three different ways to isolate the trigger: it is specifically
> the combination of a prior `catch_unwind` and a later `CFE_ES_ExitApp` call (itself a forced unwind,
> via OSAL's `OS_TaskExit` → glibc's `pthread_exit`) on the same thread — either alone is fine. The
> workaround (skip `CFE_ES_ExitApp` after a caught panic, just return) avoids the crash but leaves
> cFE ES never informed that the app's task exited, which is the open cost the "with constraints"
> qualifier is carrying.

---

## 8b. Phase 5b — closing the vehicle-dynamics gap from the flight side

Not in the original plan. It was added because Phase 4 ended with the visualizer showing
`-- no source --` for most of the vehicle, and Phase 5 had just demonstrated that the missing
publisher could be *written* — which turns a documented limitation into a task.

The question: can the `no_std` crates this plan has protected since Phase 1 actually be reused on
the flight side, in the way §9's discipline note asserts they could?

> **Done.** See [docs/findings/0007-vehicle-dynamics-in-cfe.md](docs/findings/0007-vehicle-dynamics-in-cfe.md).
> Verdict: **yes, and the reuse cost almost nothing.** `crates/vehicle-dyn` — rigid-body attitude
> dynamics, a four-wheel reaction-wheel array with real momentum saturation, a PD controller with
> gyroscopic feed-forward, sun tracking and a mission sequencer — is compiled into
> `spikes/rust-cfs-app` and runs inside cFE, into `tools/fake-cfs`, and into `apps/viz --offline`.
> The wire format has one definition (`telemetry_model::encode_vehicle_state`) called by all three
> producers and inverted by the one consumer. The container builds against the same source files
> the host workspace tests; the only new machinery is a four-line workspace stub, because
> `version.workspace = true` needs a root and the real one lists members a flight image should not
> contain.
>
> **The obstacle was not the FFI, the dynamics or the `no_std` discipline — it was allocating three
> message IDs**, which failed twice and produced no error either time. `0x0890` reached the ground
> and looked like success; it is `MD_HK_TLM_MID`, already in `to_lab`'s subscription table, and
> would have collided silently had the `md` app been loaded. The replacement command ID `0x1891`
> passed a check against *that* table and is in `sch_lab`'s **schedule** table, so the scheduler
> sent the new app phantom commands it appeared to be receiving from the ground.
>
> Both tables are generated from the bundle's topic-ID allocation, which is the thing that should
> have been edited. That makes §4's `native_eds` question considerably less optional than it looked:
> it is no longer "upside", it is the mechanism by which this class of silent error stops being
> possible.

## 9. Repository layout

```
Cargo.toml                 # workspace
crates/
  ccsds/                   # space packet codec, no_std-friendly
  cfs-msg/                 # cFE/lab message types (+ EDS codegen evaluation)
  cfs-link/                # UDP transport, handshake, reconnect, link metrics
  telemetry-model/         # decoded packets -> domain state, and the wire format
  telemetry-anim/          # domain state -> animation maths, no Bevy
  vehicle-dyn/             # attitude dynamics + wheels; compiled into the cFE app
  bevy_cfs/                # Bevy plugin: resources, events, time sync, interpolation
apps/
  viz/                     # the Bevy application
tools/
  fake-cfs/                # synthetic generator + fixture replayer
  tlm-capture/             # record real telemetry to a fixture
  gltf-gen/                # generates assets/spacecraft.gltf
spikes/
  anim-mappings/           # Phase 3 mapping comparison
  rust-cfs-app/            # Phase 5 cFE application: FFI spike, now flying the vehicle
docker/                    # cFS build + runtime images, compose file
fixtures/                  # captured packets (golden tests)
docs/
  findings/                # one markdown file per decision gate
assets/                    # glTF model + authored clips
```

**Discipline:** `crates/ccsds`, `crates/cfs-msg`, `crates/telemetry-model`, `crates/telemetry-anim`
and `crates/vehicle-dyn` must not depend on Bevy, and are built `no_std` in CI so the claim is
checked rather than asserted.

This began as a bet about a hypothetical flight side. It is no longer hypothetical: four of the five
(`ccsds`, `cfs-msg`, `telemetry-model`, `vehicle-dyn`) are compiled into `spikes/rust-cfs-app` and
loaded by cFE ES, so a Bevy dependency added to any of them now breaks the container build rather
than merely forfeiting a future option. `telemetry-anim` is the exception and stays ground-side by
nature — it is about presentation, which flight software has no opinion about.

---

## 10. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| ~~macOS/cFS mismatch eats the schedule~~ | High | **Realized and handled.** Container works; the residue is the UDP limitation above |
| Silent message-layout mismatch (endianness, padding, msgid v1 vs v2) | High | Hand-annotate one packet before writing a decoder; golden fixtures |
| Bevy animation API churn between releases | Medium | Pin the version; isolate animation calls in `bevy_cfs` |
| Telemetry too slow/jittery for convincing animation | Medium | Jitter buffer + interpolation designed in Phase 2, not bolted on |
| ~~Phase 5 FFI rabbit hole~~ | Medium | **Did not materialize.** bindgen and the FFI were the easy part; see 0006 and 0007 |
| Hand-allocated message IDs collide with the mission's own | High | **Realized twice, silently.** See 0007 §2. Mitigated for now by dumping `to_lab_sub.tbl` *and* `sch_lab_table.tbl` and asserting the chosen values in a test; properly fixed only by EDS |
| EDS turns out to be impractical to consume | Low | Hand-written types already work — but it is no longer only upside, it is the fix for the row above |

---

## 11. Success criteria

1. One command brings up cFS and the Bevy viz, and the viz animates from live telemetry.
2. The decoder is verified against captured real packets, not only synthetic ones.
3. A written recommendation mapping telemetry signal types to Bevy animation mechanisms.
4. A yes/no/constrained verdict on Rust code running inside cFS, with the blocker named.
5. Everything reproducible offline via fixtures and `fake-cfs`.
6. *(Added with Phase 5b)* The `no_std` crates demonstrably reused on the flight side rather than
   merely kept eligible for it.

---

## 12. Open questions to settle before Phase 2

- Is the end goal an **ops/monitoring display**, a **training/demo visual**, or a **simulation**
  whose state cFS consumes? A simulation inverts the data flow and changes Phase 2 substantially.
- Which spacecraft/mechanism is being animated, and does a glTF model with a usable rig exist?
- Is there a specific cFS mission tree in play, or the stock `nasa/cFS` lab bundle?
- Must this run on flight-representative hardware, or is a desktop/container target sufficient?
