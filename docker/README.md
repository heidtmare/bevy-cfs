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

## Getting telemetry out to the host

`to_lab` sends telemetry to whatever address the enable-output command names, so
that address has to be **this machine as cFS sees it** — the Docker Desktop host
gateway, and it must be **IPv4**, because `dest_IP` is a 16-octet string field:

```sh
docker exec docker-cfs-1 getent ahostsv4 host.docker.internal | head -1
# 192.168.65.254  host.docker.internal
```

```sh
cargo run -p viz -- --dest-ip 192.168.65.254
```

That works — telemetry arrives on the host's UDP 2234 with no relay and no VM.
An earlier version of this file said Docker Desktop does not forward UDP to the
macOS host. **That was wrong**: the probe used `host.docker.internal`, which
resolves to IPv6 here, so it was testing an unreachable destination rather than
a blocked transport. See [../docs/findings/0005-vertical-slice.md](../docs/findings/0005-vertical-slice.md).

## Capturing telemetry

Capturing from inside the container's network namespace still works and needs no
gateway address at all, which is why the committed fixture was taken this way:

```sh
docker run --rm --network=container:docker-cfs-1 \
  -v "$PWD":/w -w /w -e CARGO_TARGET_DIR=/tmp/t rust:1-slim \
  cargo run -q -p tlm-capture -- \
    --cfs-host 127.0.0.1 --dest-ip 127.0.0.1 --tlm-port 2234 \
    --seconds 12 --out fixtures/cfs-v7.0.1-hk.cfspkt
```

`CARGO_TARGET_DIR` is redirected so the Linux build does not collide with the
host's `target/`.

From the host, pass the gateway instead:

```sh
cargo run -p tlm-capture -- --dest-ip 192.168.65.254 --seconds 12 --out fixtures/hk.cfspkt
```

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
  field is 16 octets — IPv4 only. Use `getent ahostsv4`. This one trap is
  responsible for an entire wrong finding; see 0005.

## Changing the pinned version

`CFS_REF` is pinned to `v7.0.1` in `compose.yaml`. Changing it invalidates the
committed fixtures and possibly every message ID, so it is a deliberate act:
re-capture, re-run the golden tests, and write a new finding.

Switching to the **`native_eds`** config (EDS enabled, the interesting Phase 1
question) is a one-word change in the Dockerfile's `make` lines — but expect
different message IDs.
