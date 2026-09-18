# 0001 — Verification backlog

**Status:** open. Everything here is an assumption currently baked into the Rust
code, made on the basis of how cFS has historically worked rather than on the
build this project will actually use. Each one needs to be checked against the
pinned cFS source or against a real capture, and this file updated with the
answer.

They are listed in the order they will bite.

## 1. Message IDs — `crates/cfs-msg/src/lib.rs`

`MsgIds::LAB_DEFAULTS` uses the historical v1 stream IDs (`0x1880` for
`TO_LAB_CMD` and so on). Caelum-era and later builds derive message IDs from
topic IDs, and any mission tree can renumber them.

**Check:** the generated `cfs_msgids.h` in the build tree. Write the real values
into a config file and load them with `MsgIds::parse`.

**Symptom if wrong:** commands are silently ignored and no telemetry ever
arrives. There is no error — `ci_lab` forwards the packet to the software bus and
nobody is subscribed. This is the single most likely cause of a dead first
bring-up.

## 2. Command function codes — `crates/cfs-msg/src/to_lab.rs`

`OUTPUT_ENABLE_CC = 6` and the rest are from `to_lab_msg.h`.

**Check:** `to_lab_msg.h` in the pinned bundle.

**Symptom if wrong:** an event message on the cFS console about an invalid
command code — this one at least fails loudly.

## 3. `TO_LAB_EnableOutput_Payload_t` — `crates/cfs-msg/src/to_lab.rs`

Assumed to be exactly `char dest_IP[16]`, NUL-padded.

**Check:** the payload struct in `to_lab_msg.h`. Some versions carry additional
fields.

## 4. Telemetry timestamp layout — `crates/ccsds/src/secondary.rs`

Assumed `CFE_SB_TIME_32_16_SUBS`: 32 bits of seconds, 16 bits of subseconds at
2^-16 s, big-endian, immediately after the primary header.

**Check:** `CFE_MISSION_SB_PACKET_TIME_FORMAT` in the mission config, and the
byte order the msg module actually writes.

**Symptom if wrong:** times that look plausible but advance at the wrong rate, or
jump. Worth cross-checking a capture against wall-clock arrival times — a real
capture settles this in minutes.

## 5. Time epoch — same file

`as_secs_f64` returns seconds since the *mission* epoch (`CFE_MISSION_TIME_EPOCH_*`,
TAI-based by default), not the Unix epoch.

**Check:** the mission config. Until confirmed, only use these values as
*differences* — which is all the interpolation in `telemetry-model` needs, so
this does not block Phase 2.

## 6. Command checksum — `crates/ccsds/src/secondary.rs`

Assumed XOR of all octets seeded with `0xFF`, computed with the checksum octet
zeroed.

**Check:** `CFE_MSG_ComputeCheckSum`.

**Symptom if wrong:** likely none. `ci_lab` has historically not validated
checksums, so a wrong value will be accepted — which is exactly why this must be
checked against source rather than against observed behavior, and why it is
listed last despite being easy to get wrong.

## 7. Payload endianness — `crates/telemetry-model/src/lib.rs`

The demo decoder reads little-endian payload fields, matching a native x86/ARM
build. CCSDS *headers* are always big-endian, but payloads follow the build.

**Check:** a real capture with a known-value field in it.

**Symptom if wrong:** wildly wrong magnitudes — this one is obvious the moment a
real packet is decoded, which is why capturing fixtures comes before writing
real decoders.
