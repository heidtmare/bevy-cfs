# 0002 — cFS bring-up on macOS/Docker

**Status:** closed. Phase 0 gate met — real telemetry captured, decoder verified
against it.

**Build:** nasa/cFS **v7.0.1** (Draco, 2026-05-13), EDS disabled, `native_std`
config, built and run for **linux/arm64** in Docker Desktop 4.82 on Apple
Silicon. Fixture: `fixtures/cfs-v7.0.1-hk.cfspkt`.

## Result

cFE reaches OPERATIONAL, and 42 packets across 20 message IDs were captured and
parsed with zero errors. Four things had to be fixed to get there; each is worth
knowing before anyone repeats this.

## 1. The bundle's build system changed

The widely documented flow — copy `cfe/cmake/Makefile.sample` to the root, copy
`sample_defs`, `make SIMULATION=native prep` — no longer applies. The bundle now
ships its own top-level `Makefile` plus `target-configs.mk` defining named
configurations.

It also ships **two** `*_defs` directories (`sample_defs` and a `simple_defs`
symlink), so auto-detection is ambiguous and fails with:

```
Unable to automatically determine the mission config directory.
Specify it with the MISSIONCONFIG variable.
```

**Correct invocation:**

```sh
make native_std.prep && make native_std.install   # -> build-native_std/exe/cpu1
```

`native_std` passes `-DMISSIONCONFIG=sample` itself. There is also a
**`native_eds`** config — the bundle's reference build for EDS, with Python
bindings. That is the concrete starting point for the Phase 1 question of
generating Rust types from EDS XML, and it is a build flag rather than an
integration project.

## 2. Container message-queue limits

cFE creates one POSIX message queue per software-bus pipe. Container defaults
(`msg_max=10`, `msgsize_max=8192`) are far below what it asks for, so
`OS_QueueCreate` fails with EINVAL, ES and EVS cannot create their pipes, and
cFE processor-resets in a loop.

Both are IPC-namespaced, so Docker sets them per container:

```yaml
sysctls:
  fs.mqueue.msg_max: "1024"
  fs.mqueue.msgsize_max: "65536"
```

Running as root does **not** help — exceeding `msg_max` needs `CAP_SYS_RESOURCE`,
which is not in Docker's default capability set.

## 3. The telemetry port is 2234, not 1235

This is the one that cost real time. `to_lab` computes its destination as
`TO_LAB_MISSION_TLM_PORT + CFE_PSP_GetProcessorId() - 1`, and the mission default
is now **2234**; cpu1 emits on 2234. Command ingest is still 1234.

The failure mode is maximally misleading: the enable-output command *worked*,
`to_lab` logged `TO telemetry output enabled for IP ...` on every keepalive, and
the link showed zero received, zero errors. The command path and the telemetry
path fail independently, and only the telemetry side was wrong.

`tcpdump` inside the container's network namespace found the traffic in seconds
after a long detour. **Sniff before theorising** is the lesson; a link with no
packets and no errors is a routing question, not a decoding one.

## 4. Docker Desktop does not forward UDP to the macOS host

Container → host UDP does not traverse the Docker Desktop gateway. A plain
`nc -u` to `host.docker.internal` from the container's network namespace never
arrives, with the macOS firewall disabled. TCP to the host works.

Two smaller traps on the way: `host.docker.internal` resolves to **IPv6** here
(`fdc4:f303:9324::254`), and `to_lab`'s `dest_IP` field is 16 octets — IPv4 only.
Use `getent ahostsv4`.

**Consequence for Phase 2.** A Bevy app running natively on macOS cannot receive
telemetry from cFS in Docker Desktop. Options, in the order they should be tried:

1. **Develop against `fake-cfs` and fixture replay.** Works today, on the host,
   with no container. This is the normal development loop regardless.
2. **Relay UDP over TCP** for live demos on macOS: `socat` in a sidecar,
   forwarding to a TCP listener on the host. CCSDS packets are self-delimiting,
   so a TCP stream of concatenated packets reframes exactly — `ccsds::PacketIter`
   with its `remainder()` already handles the partial-packet case. Cost is a TCP
   transport mode in `cfs-link`.
3. **A Linux VM with bridged networking** (Lima/UTM), where the container gets a
   real reachable address.
4. On a **Linux host**, the whole problem disappears.

## Resolved from the verification backlog (0001)

- **Message IDs (item 1):** with EDS disabled, v7.0.1 still uses the historical
  v1 stream IDs. `TO_LAB_CMD 0x1880`, `TO_LAB_HK 0x0880`, `CI_LAB_HK 0x0884`,
  `SAMPLE_APP_HK 0x0883` all confirmed against the capture. `MsgIds::LAB_DEFAULTS`
  is correct *for this build*; `native_eds` would likely differ.
- **Function code and payload (items 2, 3):** `OUTPUT_ENABLE_CC = 6` with a
  16-octet NUL-padded `dest_IP` is correct — `to_lab` acted on the command.
- **Timestamp layout (item 4):** confirmed 32-bit seconds + 16-bit subseconds,
  **big-endian**. Decoded span was 11.556 s across a 12 s capture, monotonic
  non-decreasing.
- **Epoch (item 5):** 1980-01-01. The decoded time (1980-01-12 14:07:27) matches
  cFE's own console timestamps (`1980-012-14:03:20`) exactly.

## Still open

- **Checksum (item 6):** untested. `ci_lab` accepted our commands, but it does not
  validate checksums, so this proves nothing. Needs checking against
  `CFE_MSG_ComputeCheckSum`.
- **Payload endianness (item 7):** not yet exercised — no payload field has been
  decoded from a real packet, only headers. The first real payload decode settles it.
- **`native_eds` message IDs:** unknown, and the more interesting configuration.
