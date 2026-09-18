# 0005 — The vertical slice: live cFS to Bevy, and a command back

**Status:** closed. Phase 4 gate met — `apps/viz` runs against the containerized
cFS v7.0.1, decodes real telemetry, animates it, and closes the command loop
with a measured round trip.

**Build:** nasa/cFS v7.0.1 (Draco, EDS disabled, `native_std`, linux/arm64) in
Docker Desktop 4.82 on macOS 26.6.2. **Bevy 0.19.1.**

![The slice against live cFS](images/viz-live-cfs.png)

That screenshot is the finding in one image. Every counter in it came off a
socket; `confirmed in 1.30s` is the time between a keystroke becoming a
`SAMPLE_APP` no-op on the software bus and the resulting counter arriving back
on the downlink; and the two `-- no source --` rows are the honest answer to
what a stock cFS bundle can tell a visualizer about a spacecraft.

## What the slice does

| Step | Where |
|---|---|
| `to_lab` enable-output handshake, keepalive, link counters | `crates/cfs-link` |
| CCSDS + cFE header decode | `crates/ccsds` |
| Real housekeeping payload decode | `crates/cfs-msg/src/hk.rs` |
| Jitter buffer, interpolation, staleness | `crates/telemetry-model` |
| Telemetry → animation maths | `crates/telemetry-anim` |
| Bevy adapter, command send | `crates/bevy_cfs` |
| Scene, panel, command loop | `apps/viz` |

The animation layer is a direct transcription of finding 0004's table and took
an afternoon. That is the return on Phase 3: the interesting decisions had
already been made and measured, so Phase 4 spent its time on the parts that
were still unknown.

## The bug that mattered: cFE telemetry payloads start at octet 16

`CFE_MSG_TelemetryHeader_t` is not `{ primary, timestamp }`. It is:

```c
struct CFE_MSG_TelemetryHeader {
    CFE_MSG_Message_t                  Msg;      /* 6 octets */
    CFE_MSG_TelemetrySecondaryHeader_t Sec;      /* 6 octets */
    uint8                              Spare[4]; /* alignment padding */
};
```

The four-octet `Spare` exists so a payload needing 64-bit alignment does not
make the compiler insert padding of its own. It is a cFE convention, not part of
CCSDS, and it is **not** present on command packets.

`ccsds::SpacePacket::payload()` was CCSDS-correct — data field minus secondary
header — and therefore cFE-wrong by exactly four octets. Decoding `CI_LAB_HkTlm`
through it reads:

| Field | Correct | Four octets early |
|---|---|---|
| `SocketConnected` | 1 | 0 |
| `IngestPackets` | 8 | 16,777,216 |
| `IngestErrors` | 0 | 8 |

Nothing rejects the packet. No length check trips. The panel shows a wildly
busy, faulty uplink on an idle, healthy one. This is the failure mode item 7 of
the verification backlog was opened to catch, and it survived three phases
undetected for one reason: **no real payload had ever been decoded.** Headers
had been verified exhaustively against a real capture; the bytes after them
never had.

The fix is `SpacePacket::cfe_tlm_payload()`, with `payload()` left alone and
documented as the CCSDS meaning. `fake-cfs` now emits the spare too, so the
offline development path exercises the same offsets as the live one — a
stand-in whose framing differs from the real thing trains the decoder on the
wrong layout, which is how this went unnoticed in the first place.

`crates/cfs-msg/tests/real_payloads.rs` pins both readings against the committed
capture, so the shifted decode is now a test failure rather than a discovery.

## Backlog item 7 — payload endianness: **little-endian** (RESOLVED)

The first real payload decode settles it, and not by the magnitude of one field,
which could be argued either way. The capture holds two `CI_LAB` housekeeping
packets five seconds apart, taken while `tlm-capture` was re-sending its
enable-output command on a five-second keepalive:

- Little-endian: `IngestPackets` reads **7** then **8** — one datagram in five
  seconds, exactly the keepalive.
- Big-endian: the same octets read 117,440,512 then 134,217,728 — seventeen
  million datagrams in five seconds.

Cross-checked against a second application in the same two packets: `TO_LAB`'s
`CommandCounter` reads 7 then 8, counting the same commands.

This is a property of the **target**, not of cFS: payloads are raw C structs in
native byte order, and a big-endian flight target (SPARC/LEON, PowerPC) flips
it. Nothing in the packet says which it was, so byte order belongs in the same
config that carries the message IDs.

Two field-order details worth recording, both read out of the build's own
headers rather than guessed:

- `SAMPLE_APP_HkTlm_Payload_t` and `TO_LAB_HkTlm_Payload_t` are
  `{ CommandCounter, CommandErrorCounter }` — **counter first**. Several other
  cFS applications order the pair the other way, and with both usually zero the
  mistake is invisible until something goes wrong.
- `CI_LAB_HkTlm_Payload_t` is
  `{ CommandCounter, CommandErrorCounter, EnableChecksums, SocketConnected, IngestPackets: u32, IngestErrors: u32 }`.

## Backlog item 6 — command checksum: **as assumed** (RESOLVED)

From `cfe/modules/msg/fsw/src/cfe_msg_sechdr_checksum.c` in the pinned build:

```c
CFE_MSG_Checksum_t CFE_MSG_ComputeCheckSum(const CFE_MSG_Message_t *MsgPtr)
{
    CFE_MSG_Checksum_t chksum = 0xFF;
    CFE_MSG_GetSize(MsgPtr, &PktLen);
    while (PktLen--) { chksum ^= *(BytePtr++); }
    return chksum;
}
```

Seed `0xFF`, XOR every octet of the whole packet, with the checksum octet zeroed
first (`CFE_MSG_GenerateChecksum` zeroes it before computing). `ccsds::compute_checksum`
skips the checksum octet instead of XORing a zero, which is the same thing.
Verification is `ComputeCheckSum(...) == 0` over the finished packet.

And the other half of item 6, which the source cannot answer: `ci_lab` reports
**`EnableChecksums = 0`** in its own housekeeping on this build. So our commands
being accepted proved nothing about the checksum, exactly as 0001 predicted —
the answer had to come from reading the algorithm, and it did.

The eight octets this repository puts on the wire for a `SAMPLE_APP` no-op are
now frozen in a test:

```
18 82 C0 01 00 01 00 A5
```

These are the bytes that produced `SAMPLE_APP 3: SAMPLE: NOOP command v7.0.0+dev0`
on a real cFS console.

## Correction to finding 0002 §4: container → host UDP **does** work

0002 concluded that Docker Desktop does not forward UDP from a container to the
macOS host. **That conclusion was wrong**, and it is worth being precise about
why, because it cost Phase 2 a documented workaround it never needed.

Measured now, same Docker Desktop 4.82, same container: sending `to_lab`'s
enable-output with `dest_IP = 192.168.65.254` — the IPv4 host-gateway address —
delivers telemetry to the host's UDP 2234 immediately and continuously. 31
packets arrived in the first 8 s; `apps/viz` has since run for minutes at a
time with `dropped 0  parse err 0  seq gaps 0`.

The most likely cause of the original mistake is named in 0002 itself, one
paragraph above the wrong conclusion: `host.docker.internal` resolves to **IPv6**
here, and `to_lab`'s `dest_IP` field is 16 octets — IPv4 only. The original
`nc -u` probe used the name. An unreachable destination and a blocked transport
look identical from the sending end, and the difference was never isolated.

Consequences:

- The UDP→TCP relay sketched as option 2 in 0002 is **not needed**. The TCP
  transport mode it would have required in `cfs-link` was never built, which in
  hindsight is the only piece of luck in this paragraph.
- The rule that survives is narrower and more useful: **resolve the gateway with
  `getent ahostsv4` and pass it explicitly.** `apps/viz` takes `--dest-ip` and
  the README gives the value.

The general lesson is the one this repository keeps re-learning: a negative
result from a test that was never shown to be capable of a positive one is not a
result. 0002 §4 has been amended in place rather than deleted, so the wrong turn
stays visible.

## Closing the loop, and what the number means

Phase 4's gate asks for "a button that sends a command and shows the resulting
state change". Sending a datagram is easy; showing that it *did* something means
watching a value in the downlink change and being able to say why.

`SAMPLE_APP_HkTlm_Payload_t::CommandCounter` is that value. cFE increments it
inside `sample_app`'s command handler, on the flight side. Nothing on the ground
can move it.

Two details that make the measurement mean something:

- **Confirmation is "the counter is no longer what it was", not "the counter
  equals what I predicted".** Predicting `before + 1` assumes nobody else is
  commanding `sample_app` — which a ground station may not assume — and breaks
  at the `u8` wrap. Comparing against the value at press time is wrap-safe,
  survives a reset command driving the counter *down*, and stays true with other
  operators on the bus.
- **One measurement in flight at a time.** With several outstanding, a counter
  change cannot be attributed to a particular command. A second press still
  sends — refusing to command would be worse — it just does not start a second
  measurement.

Measured over twelve round trips against the container: **0.90 s to 5.52 s, mean
2.94 s.** That is not network latency, and calling it latency would be wrong by
two orders of magnitude. `sample_app` housekeeping is published on a scheduler
tick, so the number is dominated by how long until the next cycle, and a
uniformly timed command should average half of it. The panel therefore says
"confirmed in", never "latency".

## Telemetry cadence is per-message, not per-link

Measured over 30 s against the pinned build, 20 message IDs, 108 packets:

| Message | Period |
|---|---|
| `CFE_ES_HK` (0x0800) | 4.000 s |
| `CI_LAB_HK` (0x0884) | 5.000 s |
| `TO_LAB_HK` (0x0880) | 5.200 s |
| `SAMPLE_APP_HK` (0x0883) | 5.401 s |

There is no single "telemetry rate" to size a jitter buffer against. Each
message has its own period, and two of these are not even integer multiples of
`sch_lab`'s tick. `BufferConfig::for_rate` takes a period for exactly this
reason; a real mission would want one buffer per animated message, not one per
link.

## Stock cFS publishes no vehicle dynamics

The most awkward fact in the whole investigation, and better stated than worked
around: there is no attitude quaternion, no joint angle and no wheel speed
anywhere on a stock cFS software bus. Those come from a mission's own
applications, and the bundle ships none. What it publishes is the flight
software's own housekeeping — command counters, uplink statistics, the mission
clock.

There are two honest responses and one dishonest one. The dishonest one is to
run the demo generator quietly behind a window labelled "live". `apps/viz` does
both honest ones:

- **Show nothing where there is no signal.** `attitude` and `wheels` read
  `-- no source --` and hold their defaults. The gap is asserted by a test, not
  left to inspection.
- **Derive what can genuinely be derived, and name the field it came from.**
  Every row of the panel prints its source and the Phase 3 mechanism driving it.

| Signal | Source against live cFS | Mechanism |
|---|---|---|
| attitude | none | direct transform |
| solar array | `CFE` mission time | direct transform |
| deploy | `CI_LAB.IngestPackets` since connect | clip seek |
| mode | `SAMPLE_APP.CommandCounter % 4` | graph blend |
| wheels | none | material emissive |

The derived mappings are demo mappings and say so. What is not a demo is the
path: every byte behind them was decoded from a real cFE packet that crossed a
real socket, and the `mode` row in particular is driven by a counter that only
moves when the operator commands the vehicle.

Two consequences worth carrying forward:

- Counters are absolute since application start, and cFS has usually been up for
  a while before a viz connects. Everything derived is **relative to a baseline
  taken at connect**, or the array appears fully deployed the moment the window
  opens. There is a test for it.
- Mission time is ~1e9 seconds and the array angle is an `f32`. Converting
  before taking the modulo quantizes the angle into steps of tens of degrees.
  There is a test for that too.

## Freshness is not playback state

`Freshness` describes the *vehicle-state* stream. When the rig is driven by
signals derived from housekeeping there is no such stream, and printing
"interpolating between real samples" would have been a straightforward lie:
derived signals step once per scheduler tick and nothing interpolates them.

The panel therefore separates the two, and says
`LIVE (derived) - housekeeping steps at the scheduler rate, not interpolated`.
The distinction is between smooth motion that is real and smooth motion that is
invented, which is the one thing this display exists to keep straight.

## Two Bevy 0.19 traps

Both cost real time and neither produces an error.

1. **`Screenshot::primary_window()` captures the compositor.** A macOS window
   that is not frontmost is not composited, so the capture *succeeds* and writes
   a solid black PNG. Nothing warns. Rendering the camera into an `Image` via
   `RenderTarget::Image` and capturing that with `Screenshot::image(handle)`
   bypasses the compositor entirely and works with the window buried, on another
   desktop, or from a script. Phase 3's documented capture command had silently
   started producing black images; the spike is fixed too.
2. **UI needs an explicit camera when the camera is offscreen.** Bevy's default
   UI target is the camera rendering to the primary window. With the only camera
   pointed at an image there is no such camera, and the first offscreen capture
   came out with a spacecraft and no panel. `UiTargetCamera(entity)` fixes it.

Also, for the record: `RenderTarget` is its own component in 0.19, not a field
of `Camera`.

## A stand-in's sloppiness looked like a buffer bug

`fake-cfs serve --rate 8` was delivering **6.24 Hz**, because the command
socket's 50 ms read timeout was also the resolution of the send schedule, and
because `next_send` was reset from "now" rather than advanced by a period, so
every late wake-up pushed all later packets late.

The symptom was not "the rate is wrong". The symptom was the viz reporting
`HOLDING 0.1s` and a nonsense rate estimate — playback running off the end of
the buffer — which reads exactly like a jitter-buffer defect. The jitter buffer
was fine: fed a clean 8 Hz it reports 8.0 Hz and stays `Live`, which is now a
thing this repository has checked rather than assumed. With the timeout at 2 ms
and the schedule accumulated rather than reset, `fake-cfs` delivers 8.00 Hz
measured, and the viz reports `rate 8.0 Hz` and `LIVE`.

The general form: **a stand-in that is approximately right produces failures
that are attributed to whatever it is standing in for.** If offline development
is the normal loop — and here it is — the stand-in's fidelity is part of the
test infrastructure, not a convenience.

## Still open

- **`native_eds`.** Untouched, and the highest-leverage remaining question:
  whether flight and ground types can share one source of truth. Expect
  different message IDs; the golden tests are the tripwire.
- **Command authentication.** The gate predicted this would surface, and it did:
  `ci_lab` accepts any well-formed datagram on UDP 1234 with checksum validation
  switched off. That is fine for a lab configuration and is worth stating plainly
  before anyone points this at something that matters.
- **Phase 5**, Architecture B: Rust inside cFS.
