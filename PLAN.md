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

> **Done, with one amendment** — see `docs/findings/0002`. cFS v7.0.1 builds and runs, but
> "host-reachable" turned out to be unachievable on Docker Desktop for macOS: it does not forward
> UDP from a container to the host. Telemetry is reachable from *inside* the container network,
> which is enough for Phase 0/1 (capturing fixtures) but changes Phase 2 and Phase 4: a native Bevy
> app on macOS needs a UDP→TCP relay, a bridged Linux VM, or a Linux host for live telemetry.
> Development against `fake-cfs` and fixture replay is unaffected, which is precisely why that
> stand-in was built first.

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

---

## 6. Phase 3 — The animation question itself (1 week)

The interesting part is *how* telemetry should map onto Bevy's animation system. Evaluate three
mappings on the same glTF model and write up which fits which kind of signal:

1. **Direct transform drive** — telemetry writes `Transform` each frame; `AnimationPlayer` unused.
   Right for continuous physical state (attitude, gimbal angle, solar array rotation). Simple and
   exact.
2. **Clip-as-lookup-table** — author a clip in Blender for a mechanism's full travel, then *seek*
   the active animation to `normalized_telemetry * clip_duration` instead of letting it play.
   Right for rigged multi-part mechanisms (deployment arms, latches, docking hardware) where an
   artist owns the motion and telemetry owns only the parameter.
3. **Graph blending** — `AnimationGraph` weights driven by telemetry/mode. Right for discrete modes
   with transitions (stowed → deploying → deployed, safe-mode poses).

Also test: animating non-`Transform` properties (material emissive for heaters/thruster plumes,
visibility for fault indicators) via custom animatable properties, since spacecraft telemetry is
mostly *not* rigid-body motion.

**Gate:** a written recommendation table — signal type → mapping — backed by the running demo.

---

## 7. Phase 4 — Vertical slice demo (1 week)

`apps/viz`: glTF spacecraft, a 3D view driven live by the containerized cFS, a telemetry side panel,
link/staleness indicator, and one **command path** (a button that sends a command to `ci_lab` and
shows the resulting state change). The command path matters: it proves the loop closes, and it
forces you to confront command authentication/validation questions early.

Record a screen capture against live cFS and against a replayed fixture.

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

---

## 9. Repository layout

```
Cargo.toml                 # workspace
crates/
  ccsds/                   # space packet codec, no_std-friendly
  cfs-msg/                 # cFE/lab message types (+ EDS codegen evaluation)
  cfs-link/                # UDP transport, handshake, reconnect, link metrics
  telemetry-model/         # decoded packets -> domain state
  bevy_cfs/                # Bevy plugin: resources, events, time sync, interpolation
apps/
  viz/                     # the Bevy application
tools/
  fake-cfs/                # synthetic generator + fixture replayer
spikes/
  rust-cfs-app/            # Phase 5 FFI spike
docker/                    # cFS build + runtime images, compose file
fixtures/                  # captured packets (golden tests)
docs/
  findings/                # one markdown file per decision gate
assets/                    # glTF model + authored clips
```

**Discipline:** `crates/ccsds`, `crates/cfs-msg`, and `crates/telemetry-model` must not depend on
Bevy. If Architecture B or C goes ahead, those three crates are what gets reused on the flight side,
and a Bevy dependency there would kill that option.

---

## 10. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| ~~macOS/cFS mismatch eats the schedule~~ | High | **Realized and handled.** Container works; the residue is the UDP limitation above |
| Silent message-layout mismatch (endianness, padding, msgid v1 vs v2) | High | Hand-annotate one packet before writing a decoder; golden fixtures |
| Bevy animation API churn between releases | Medium | Pin the version; isolate animation calls in `bevy_cfs` |
| Telemetry too slow/jittery for convincing animation | Medium | Jitter buffer + interpolation designed in Phase 2, not bolted on |
| Phase 5 FFI rabbit hole | Medium | Hard time-box; a negative verdict is an acceptable output |
| EDS turns out to be impractical to consume | Low | Hand-written types already work; EDS is upside, not a dependency |

---

## 11. Success criteria

1. One command brings up cFS and the Bevy viz, and the viz animates from live telemetry.
2. The decoder is verified against captured real packets, not only synthetic ones.
3. A written recommendation mapping telemetry signal types to Bevy animation mechanisms.
4. A yes/no/constrained verdict on Rust code running inside cFS, with the blocker named.
5. Everything reproducible offline via fixtures and `fake-cfs`.

---

## 12. Open questions to settle before Phase 2

- Is the end goal an **ops/monitoring display**, a **training/demo visual**, or a **simulation**
  whose state cFS consumes? A simulation inverts the data flow and changes Phase 2 substantially.
- Which spacecraft/mechanism is being animated, and does a glTF model with a usable rig exist?
- Is there a specific cFS mission tree in play, or the stock `nasa/cFS` lab bundle?
- Must this run on flight-representative hardware, or is a desktop/container target sufficient?
