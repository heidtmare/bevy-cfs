//! UDP transport to a running cFS instance via `ci_lab` (commands in) and
//! `to_lab` (telemetry out).
//!
//! # Why a thread and not async
//!
//! The consumer is a Bevy app with a fixed frame budget. The one hard rule is
//! that no system may ever block on the socket, and a dedicated thread feeding a
//! bounded channel achieves that without pulling an async runtime into the
//! render loop. If the consumer stalls, the channel fills and the *oldest*
//! packets are dropped — losing stale telemetry is the correct failure mode for
//! a live display.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ccsds::SpacePacket;
use cfs_msg::{MsgIds, to_lab};

/// Default `ci_lab` command port.
pub const DEFAULT_CMD_PORT: u16 = 1234;

/// Default `to_lab` telemetry port.
///
/// 2234, not the 1235 that older cFS documentation and ground tools use.
/// `to_lab` computes its destination port as
/// `TO_LAB_MISSION_TLM_PORT + CFE_PSP_GetProcessorId() - 1`, and the mission
/// default moved to 2234; cpu1 therefore emits on 2234.
///
/// This cost an hour of debugging a link that looked completely dead while
/// `to_lab` was cheerfully logging "telemetry output enabled" — the enable
/// command had worked all along. Verified by packet capture against v7.0.1;
/// see docs/findings/0002.
pub const DEFAULT_TLM_PORT: u16 = 2234;

/// Largest datagram accepted. cFE's own limit is a mission config; this is
/// generous enough to catch oversize packets as an error rather than truncating
/// them silently.
const MAX_DATAGRAM: usize = 16 * 1024;

/// How often to re-send the enable-output command and the subscriptions.
///
/// `to_lab` forgets its destination *and* its runtime subscriptions when cFS
/// restarts, so a viz that sent them once would go quiet forever after a
/// restart it never noticed. Re-sending is harmless: enable-output is
/// idempotent, and an add-packet for a stream `to_lab` already carries is
/// answered with an error event and no change.
const ENABLE_INTERVAL: Duration = Duration::from_secs(5);

/// Streams to ask `to_lab` for at connect time, with their queue depths.
///
/// The stock subscription table is compiled into the mission build and names
/// only the bundle's own applications, so anything published by
/// `spikes/rust-cfs-app` has to be requested at runtime — see
/// [`cfs_msg::to_lab::add_packet`]. Vehicle state gets a queue of 4 because it
/// is published at 10 Hz against `to_lab`'s own slower service loop; one would
/// drop samples the jitter buffer then has to paper over.
fn runtime_subscriptions(ids: &MsgIds) -> [(cfs_msg::MsgId, u8); 2] {
    [(ids.rust_app_vehicle_tlm, 4), (ids.rust_app_hk_tlm, 1)]
}

/// How long the runtime-subscribed streams may be silent before the
/// subscriptions are sent again.
///
/// Re-subscribing unconditionally on every keepalive would be simpler, and was
/// the first version. It is wrong in a way worth naming: `to_lab` answers an
/// add-packet for a stream it already carries by incrementing its *command
/// error counter*, which this project puts on screen. Within a minute the
/// panel showed a dozen errors caused entirely by the ground software asking
/// for something it already had — noise in exactly the indicator an operator
/// uses to notice real problems.
///
/// Silence is the signal that actually means "the subscription is gone": cFS
/// restarted, `to_lab` forgot, and nothing is arriving. Four seconds is several
/// times the 10 Hz publish rate and comfortably longer than a slow scheduler
/// tick, so a healthy link never trips it.
const SUBSCRIPTION_SILENCE: Duration = Duration::from_secs(4);

#[derive(Clone, Debug)]
pub struct LinkConfig {
    /// Where `ci_lab` listens for commands.
    pub cmd_addr: SocketAddr,
    /// Local address to bind for telemetry.
    pub tlm_bind: SocketAddr,
    /// Address `to_lab` should send telemetry to, *as seen from the cFS host*.
    ///
    /// With cFS in a container this is the host gateway (`host.docker.internal`
    /// resolves to it from inside; from outside, pass the gateway IP), never
    /// `127.0.0.1` — that would point cFS at itself.
    pub dest_ip: String,
    pub msg_ids: MsgIds,
    /// Packets buffered before the oldest are dropped.
    pub queue_depth: usize,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            cmd_addr: SocketAddr::from(([127, 0, 0, 1], DEFAULT_CMD_PORT)),
            tlm_bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_TLM_PORT)),
            dest_ip: "127.0.0.1".to_string(),
            msg_ids: MsgIds::LAB_DEFAULTS,
            queue_depth: 256,
        }
    }
}

/// Counters for the link-health indicator. All relaxed: they are for display,
/// never for control flow.
#[derive(Debug, Default)]
pub struct LinkStats {
    pub packets_received: AtomicU64,
    /// Milliseconds since process start at the last packet on a
    /// runtime-subscribed stream. Zero until one arrives.
    pub last_subscribed_ms: AtomicU64,
    pub bytes_received: AtomicU64,
    pub packets_dropped: AtomicU64,
    pub parse_errors: AtomicU64,
    pub sequence_gaps: AtomicU64,
    pub commands_sent: AtomicU64,
    /// Milliseconds since the process started, at the last packet.
    pub last_packet_ms: AtomicU64,
}

impl LinkStats {
    fn snapshot_field(v: &AtomicU64) -> u64 {
        v.load(Ordering::Relaxed)
    }

    /// Time since the last packet, or `None` if none has arrived.
    pub fn staleness(&self, started: Instant) -> Option<Duration> {
        match Self::snapshot_field(&self.last_packet_ms) {
            0 => None,
            ms => Some(started.elapsed().saturating_sub(Duration::from_millis(ms))),
        }
    }
}

/// Whether nothing has arrived on a runtime-subscribed stream recently.
///
/// "Never" counts as silent, so the first keepalive after a connect that found
/// no flight application still re-asks — which is what makes the viz recover
/// when cFS is restarted with the application loaded, without a reconnect.
fn subscriptions_are_silent(stats: &LinkStats, started: Instant) -> bool {
    match stats.last_subscribed_ms.load(Ordering::Relaxed) {
        0 => true,
        ms => started.elapsed().saturating_sub(Duration::from_millis(ms)) > SUBSCRIPTION_SILENCE,
    }
}

/// A received datagram, kept as raw bytes so decoding happens on the consumer's
/// thread and the socket thread stays free.
#[derive(Clone, Debug)]
pub struct RawPacket {
    pub bytes: Vec<u8>,
    pub received: Instant,
}

impl RawPacket {
    pub fn parse(&self) -> Result<SpacePacket<'_>, ccsds::Error> {
        SpacePacket::parse(&self.bytes)
    }
}

/// A live connection to cFS.
pub struct CfsLink {
    // Mutex because `Receiver` is `Send` but not `Sync`, and a Bevy `Resource`
    // must be both. Draining is the only use, it happens once per frame from one
    // system, and the lock is never held across a blocking call — so the cost is
    // an uncontended lock per frame.
    rx: Mutex<Receiver<RawPacket>>,
    cmd_socket: UdpSocket,
    cmd_addr: SocketAddr,
    seq_count: Mutex<u16>,
    stats: Arc<LinkStats>,
    shutdown: Arc<AtomicBool>,
    started: Instant,
    msg_ids: MsgIds,
}

impl CfsLink {
    /// Bind the telemetry socket, start the receive thread, and ask `to_lab` to
    /// start sending.
    pub fn connect(config: LinkConfig) -> io::Result<Self> {
        let tlm_socket = UdpSocket::bind(config.tlm_bind)?;
        // A read timeout is what lets the thread notice the shutdown flag.
        tlm_socket.set_read_timeout(Some(Duration::from_millis(200)))?;

        let cmd_socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))?;

        let (tx, rx) = sync_channel::<RawPacket>(config.queue_depth);
        let stats = Arc::new(LinkStats::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let started = Instant::now();

        {
            let stats = Arc::clone(&stats);
            let shutdown = Arc::clone(&shutdown);
            let subscribed: Vec<u16> =
                runtime_subscriptions(&config.msg_ids).iter().map(|(id, _)| id.0).collect();
            thread::Builder::new().name("cfs-tlm-rx".into()).spawn(move || {
                let mut buf = vec![0u8; MAX_DATAGRAM];
                let mut last_seq: HashMap<u16, u16> = HashMap::new();

                while !shutdown.load(Ordering::Relaxed) {
                    let n = match tlm_socket.recv_from(&mut buf) {
                        Ok((n, _from)) => n,
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue;
                        }
                        Err(_) => break,
                    };

                    let datagram = &buf[..n];
                    // Validate before queueing: a malformed datagram should raise
                    // the error counter here, not surface as a decode failure in
                    // a render system where it is much harder to attribute.
                    let Ok(pkt) = SpacePacket::parse(datagram) else {
                        stats.parse_errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };

                    let sid = pkt.primary().stream_id();
                    let seq = pkt.primary().seq_count;
                    if let Some(prev) = last_seq.insert(sid, seq) {
                        // 14-bit counter, wraps at 0x3FFF.
                        if seq != (prev + 1) & 0x3FFF {
                            stats.sequence_gaps.fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    stats.packets_received.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_received.fetch_add(n as u64, Ordering::Relaxed);
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    stats.last_packet_ms.store(elapsed_ms, Ordering::Relaxed);
                    if subscribed.contains(&sid) {
                        stats.last_subscribed_ms.store(elapsed_ms, Ordering::Relaxed);
                    }

                    let raw = RawPacket { bytes: datagram.to_vec(), received: Instant::now() };
                    match tx.try_send(raw) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
            })?;
        }

        let link = Self {
            rx: Mutex::new(rx),
            cmd_socket,
            cmd_addr: config.cmd_addr,
            seq_count: Mutex::new(0),
            stats,
            shutdown: Arc::clone(&shutdown),
            started,
            msg_ids: config.msg_ids,
        };

        link.enable_output(&config.dest_ip)?;
        // Order matters only in that both must happen; a subscription added
        // before output is enabled is still remembered.
        link.subscribe_runtime_packets();
        link.spawn_keepalive(config.dest_ip);
        Ok(link)
    }

    fn spawn_keepalive(&self, dest_ip: String) {
        let socket = match self.cmd_socket.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        let addr = self.cmd_addr;
        let shutdown = Arc::clone(&self.shutdown);
        let stats = Arc::clone(&self.stats);
        let msg_ids = self.msg_ids;
        let started = self.started;
        let _ = thread::Builder::new().name("cfs-keepalive".into()).spawn(move || {
            let mut seq: u16 = 1;
            while !shutdown.load(Ordering::Relaxed) {
                thread::sleep(ENABLE_INTERVAL);
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let mut buf = [0u8; 64];
                if let Ok(pkt) = to_lab::enable_output(&mut buf, msg_ids.to_lab_cmd, seq, &dest_ip)
                    && socket.send_to(pkt, addr).is_ok()
                {
                    stats.commands_sent.fetch_add(1, Ordering::Relaxed);
                }
                seq = seq.wrapping_add(1) & 0x3FFF;

                // Only when the subscribed streams have gone quiet. See
                // SUBSCRIPTION_SILENCE for why this is not unconditional.
                if subscriptions_are_silent(&stats, started) {
                    for (stream, depth) in runtime_subscriptions(&msg_ids) {
                        if let Ok(pkt) =
                            to_lab::add_packet(&mut buf, msg_ids.to_lab_cmd, seq, stream, depth)
                            && socket.send_to(pkt, addr).is_ok()
                        {
                            stats.commands_sent.fetch_add(1, Ordering::Relaxed);
                        }
                        seq = seq.wrapping_add(1) & 0x3FFF;
                    }
                }
            }
        });
    }

    /// Tell `to_lab` to send telemetry to `dest_ip`.
    pub fn enable_output(&self, dest_ip: &str) -> io::Result<()> {
        let mut buf = [0u8; 64];
        let seq = self.next_seq();
        let pkt = to_lab::enable_output(&mut buf, self.msg_ids.to_lab_cmd, seq, dest_ip)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        self.send_raw(pkt)
    }

    /// Whether the runtime-subscribed streams have been silent long enough to
    /// be worth asking for again.
    pub fn subscriptions_silent(&self) -> bool {
        subscriptions_are_silent(&self.stats, self.started)
    }

    /// Ask `to_lab` for the packets that are not in its compiled-in table.
    ///
    /// Failures are logged, not returned: a build without the Rust application
    /// loaded will never publish these, and refusing to connect in that case
    /// would break every other use of this link.
    pub fn subscribe_runtime_packets(&self) {
        for (stream, depth) in runtime_subscriptions(&self.msg_ids) {
            let mut buf = [0u8; 64];
            let seq = self.next_seq();
            match to_lab::add_packet(&mut buf, self.msg_ids.to_lab_cmd, seq, stream, depth)
                .map_err(|e| e.to_string())
                .and_then(|pkt| self.send_raw(pkt).map_err(|e| e.to_string()))
            {
                Ok(()) => {}
                Err(e) => eprintln!("cfs-link: subscribing {stream} failed: {e}"),
            }
        }
    }

    /// Send an already-built command packet to `ci_lab`.
    pub fn send_raw(&self, packet: &[u8]) -> io::Result<()> {
        self.cmd_socket.send_to(packet, self.cmd_addr)?;
        self.stats.commands_sent.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Next command sequence count.
    pub fn next_seq(&self) -> u16 {
        let mut guard = self.seq_count.lock().unwrap_or_else(|e| e.into_inner());
        *guard = guard.wrapping_add(1) & 0x3FFF;
        *guard
    }

    /// Drain everything received since the last call into `out`, returning how
    /// many were appended. Never blocks.
    ///
    /// Takes a buffer rather than returning one so a caller on a frame budget can
    /// reuse an allocation (in Bevy, a `Local<Vec<RawPacket>>`).
    pub fn drain_into(&self, out: &mut Vec<RawPacket>) -> usize {
        let rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        let before = out.len();
        out.extend(rx.try_iter());
        out.len() - before
    }

    /// Drain everything received since the last call. Never blocks.
    ///
    /// Allocates; prefer [`CfsLink::drain_into`] on a per-frame path.
    pub fn drain(&self) -> Vec<RawPacket> {
        let mut out = Vec::new();
        self.drain_into(&mut out);
        out
    }

    pub fn stats(&self) -> &Arc<LinkStats> {
        &self.stats
    }

    pub fn started(&self) -> Instant {
        self.started
    }
}

impl Drop for CfsLink {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfs_msg::MsgId;

    /// `CfsLink` must stay `Send + Sync` to be usable as a Bevy resource. This
    /// fails at compile time if a future field breaks that.
    const _: fn() = || {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CfsLink>();
    };

    /// End-to-end over the loopback: a fake `ci_lab` receives the enable-output
    /// command the link sends on connect, then answers with telemetry that the
    /// link must deliver.
    #[test]
    fn connect_enables_output_and_receives_telemetry() {
        let fake_ci = UdpSocket::bind("127.0.0.1:0").unwrap();
        fake_ci.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let cmd_addr = fake_ci.local_addr().unwrap();

        let tlm = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tlm_addr = tlm.local_addr().unwrap();
        drop(tlm); // release the port for the link to bind

        let link = CfsLink::connect(LinkConfig {
            cmd_addr,
            tlm_bind: tlm_addr,
            dest_ip: "127.0.0.1".into(),
            ..Default::default()
        })
        .unwrap();

        let mut buf = [0u8; 256];
        let (n, _) = fake_ci.recv_from(&mut buf).unwrap();
        let cmd = SpacePacket::parse(&buf[..n]).unwrap();
        assert_eq!(cmd.primary().stream_id(), MsgIds::LAB_DEFAULTS.to_lab_cmd.0);
        assert_eq!(cmd.cmd_secondary().unwrap().function_code, to_lab::OUTPUT_ENABLE_CC);

        // Now play `to_lab`: send a telemetry packet back.
        let mut tlm_pkt = [0u8; 20];
        let hdr =
            ccsds::PrimaryHeader::for_total_len(0x080, ccsds::PacketType::Telemetry, true, 7, 20)
                .unwrap();
        hdr.write(&mut tlm_pkt).unwrap();
        fake_ci.send_to(&tlm_pkt, tlm_addr).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let got = loop {
            if let Some(p) = link.drain().into_iter().next() {
                break p;
            }
            assert!(Instant::now() < deadline, "no telemetry delivered");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(got.parse().unwrap().primary().seq_count, 7);
        assert_eq!(link.stats().packets_received.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn malformed_datagrams_are_counted_not_delivered() {
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tlm = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tlm_addr = tlm.local_addr().unwrap();
        drop(tlm);

        let link = CfsLink::connect(LinkConfig {
            cmd_addr: sink.local_addr().unwrap(),
            tlm_bind: tlm_addr,
            msg_ids: MsgIds { to_lab_cmd: MsgId(0x1880), ..MsgIds::LAB_DEFAULTS },
            ..Default::default()
        })
        .unwrap();

        sink.send_to(&[0xFF; 8], tlm_addr).unwrap(); // bogus version field

        let deadline = Instant::now() + Duration::from_secs(5);
        while link.stats().parse_errors.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "parse error never counted");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(link.drain().len(), 0);
    }
}
