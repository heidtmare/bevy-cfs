# 0001 — Verification backlog

**Status:** mostly resolved by [0002](0002-cfs-bring-up.md), which captured real
telemetry from cFS v7.0.1 and verified the decoder against it. Items 1-5 are
confirmed **for that build**; items 6 and 7 remain open.

Everything here started as an assumption baked into the Rust code on the basis of
how cFS has historically worked. Each entry now records how it turned out. They
are listed in the order they will bite.

> A resolved item is resolved for **v7.0.1 with EDS disabled**, the build pinned
> in `docker/`. The `native_eds` configuration is expected to differ, most of all
> on message IDs.

## 1. Message IDs — RESOLVED (v7.0.1, EDS off) — `crates/cfs-msg/src/lib.rs`

`MsgIds::LAB_DEFAULTS` uses the historical v1 stream IDs (`0x1880` for
`TO_LAB_CMD` and so on). Caelum-era and later builds derive message IDs from
topic IDs, and any mission tree can renumber them.

**Check:** the generated `cfs_msgids.h` in the build tree. Write the real values
into a config file and load them with `MsgIds::parse`.

**Symptom if wrong:** commands are silently ignored and no telemetry ever
arrives. There is no error — `ci_lab` forwards the packet to the software bus and
nobody is subscribed.

**Outcome:** correct as guessed. v7.0.1 with EDS disabled still uses the v1
stream IDs, and `0x1880`/`0x0880`/`0x0883`/`0x0884` all appear in the capture.
The tripwire is `crates/ccsds/tests/golden.rs`, which fails if a future build
renumbers them.

A dead bring-up did happen — but the cause was the *telemetry port*, not the
message IDs. See 0002 §3.

## 2. Command function codes — RESOLVED — `crates/cfs-msg/src/to_lab.rs`

`OUTPUT_ENABLE_CC = 6` and the rest are from `to_lab_msg.h`.

**Check:** `to_lab_msg.h` in the pinned bundle.

**Symptom if wrong:** an event message on the cFS console about an invalid
command code — this one at least fails loudly.

**Outcome:** correct. `to_lab` logged `TO telemetry output enabled for IP ...` in
response to the command.

## 3. `TO_LAB_EnableOutput_Payload_t` — RESOLVED — `crates/cfs-msg/src/to_lab.rs`

Assumed to be exactly `char dest_IP[16]`, NUL-padded.

**Check:** the payload struct in `to_lab_msg.h`. Some versions carry additional
fields.

**Outcome:** correct — `to_lab` parsed the address out of our packet and echoed
it back in the event message. Note the field is 16 octets and therefore **IPv4
only**; an IPv6 address does not fit, which matters because
`host.docker.internal` resolves to IPv6 on Docker Desktop.

## 4. Telemetry timestamp layout — RESOLVED — `crates/ccsds/src/secondary.rs`

Assumed `CFE_SB_TIME_32_16_SUBS`: 32 bits of seconds, 16 bits of subseconds at
2^-16 s, big-endian, immediately after the primary header.

**Check:** `CFE_MISSION_SB_PACKET_TIME_FORMAT` in the mission config, and the
byte order the msg module actually writes.

**Symptom if wrong:** times that look plausible but advance at the wrong rate, or
jump.

**Outcome:** correct as assumed — 32-bit seconds, 16-bit subseconds, big-endian.
Decoded span was 11.556 s across a 12 s capture, monotonic non-decreasing.
Asserted in `crates/ccsds/tests/golden.rs`.

## 5. Time epoch — RESOLVED — same file

`as_secs_f64` returns seconds since the *mission* epoch (`CFE_MISSION_TIME_EPOCH_*`,
TAI-based by default), not the Unix epoch.

**Check:** the mission config. Until confirmed, only use these values as
*differences* — which is all the interpolation in `telemetry-model` needs.

**Outcome:** the epoch is **1980-01-01**. Decoded 1980-01-12 14:07:27, matching
cFE's own console timestamps (`1980-012-14:03:20`) exactly. Absolute times are
now meaningful, not just differences.

## 6. Command checksum — STILL OPEN — `crates/ccsds/src/secondary.rs`

Assumed XOR of all octets seeded with `0xFF`, computed with the checksum octet
zeroed.

**Check:** `CFE_MSG_ComputeCheckSum`.

**Symptom if wrong:** likely none. `ci_lab` has historically not validated
checksums, so a wrong value will be accepted — which is exactly why this must be
checked against source rather than against observed behavior.

**Status:** still open, and the bring-up did **not** settle it. Our commands were
accepted, but that is equally consistent with a wrong checksum being ignored.

## 7. Payload endianness — STILL OPEN — `crates/telemetry-model/src/lib.rs`

The demo decoder reads little-endian payload fields, matching a native x86/ARM
build. CCSDS *headers* are always big-endian, but payloads follow the build.

**Check:** a real capture with a known-value field in it.

**Symptom if wrong:** wildly wrong magnitudes — obvious the moment a real packet
is decoded.

**Status:** still open. The capture verified *headers* only; no payload field has
been decoded from a real packet yet. The first real payload decode settles it.
