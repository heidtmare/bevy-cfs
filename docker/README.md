# Running cFS

**Verified** against nasa/cFS v7.0.1 (Draco) on Docker Desktop 4.82, Apple
Silicon, linux/arm64. See [../docs/findings/0002-cfs-bring-up.md](../docs/findings/0002-cfs-bring-up.md)
for what it took.

```sh
docker compose -f docker/compose.yaml up -d --build
docker compose -f docker/compose.yaml logs -f
```

Expect `CFE_ES_Main: CFE_ES_Main entering OPERATIONAL state`, then
`CI_LAB listening on UDP port: 1234` and `TO Lab Initialized ... Awaiting enable
command`.

## Ports

| Port | Direction | What |
|---|---|---|
| 1234/udp | into cFS | `ci_lab` command ingest (published to the host) |
| **2234**/udp | out of cFS | `to_lab` telemetry — **not** the 1235 in older docs |

`to_lab` computes its port as `TO_LAB_MISSION_TLM_PORT + processor_id - 1`, and
the mission default is now 2234, so cpu1 emits on 2234.

## Capturing telemetry

Docker Desktop **does not forward UDP from a container to the macOS host**. TCP
works; UDP silently vanishes, firewall or no firewall. So the capture runs inside
the cFS container's network namespace:

```sh
docker run --rm --network=container:docker-cfs-1 \
  -v "$PWD":/w -w /w -e CARGO_TARGET_DIR=/tmp/t rust:1-slim \
  cargo run -q -p tlm-capture -- \
    --cfs-host 127.0.0.1 --dest-ip 127.0.0.1 --tlm-port 2234 \
    --seconds 12 --out fixtures/cfs-v7.0.1-hk.cfspkt
```

`CARGO_TARGET_DIR` is redirected so the Linux build does not collide with the
host's `target/`.

A Bevy app on macOS therefore cannot receive live telemetry from this container
directly — `docs/findings/0002` §4 lists the options. Development against
`fake-cfs` and fixture replay needs none of them.

## Debugging a silent link

A link with zero packets *and* zero errors is a routing problem, not a decoding
one. Check that cFS is actually transmitting before touching the decoder:

```sh
docker run --rm --network=container:docker-cfs-1 nicolaka/netshoot \
  timeout 8 tcpdump -n -i any udp
```

If packets are flowing to a port you are not listening on, that is the answer.
This is how the 2234 discovery was made, after a long detour through the message
IDs — which were fine.

## Gotchas already handled in compose.yaml

- **`fs.mqueue` sysctls.** cFE needs deeper and larger POSIX message queues than
  a container's defaults. Without them `OS_QueueCreate` fails with EINVAL and cFE
  processor-resets in a loop. Running as root does not help — that needs
  `CAP_SYS_RESOURCE`, which Docker drops.
- **Native architecture.** The image builds for the host arch (arm64 on Apple
  Silicon) rather than emulating x86_64. Uncomment the `platform:` pin if
  something turns out to be architecture-specific.
- **`host.docker.internal` resolves to IPv6** here, and `to_lab`'s `dest_IP`
  field is 16 octets — IPv4 only. Use `getent ahostsv4` if you need that address.

## Changing the pinned version

`CFS_REF` is pinned to `v7.0.1` in `compose.yaml`. Changing it invalidates the
committed fixtures and possibly every message ID, so it is a deliberate act:
re-capture, re-run the golden tests, and write a new finding.

Switching to the **`native_eds`** config (EDS enabled, the interesting Phase 1
question) is a one-word change in the Dockerfile's `make` lines — but expect
different message IDs.
