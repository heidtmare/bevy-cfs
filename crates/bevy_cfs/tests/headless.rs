//! Phase 2 gate: the plugin, driven over a real UDP socket by a stand-in
//! `to_lab`, must produce smooth motion and degrade gracefully under packet loss
//! and reordering.
//!
//! Headless and deterministic. `Time` is advanced by hand rather than by
//! `TimePlugin`, so the test measures interpolation behavior rather than how
//! fast the machine happened to run.

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use bevy::prelude::*;

use ccsds::{PacketType, PrimaryHeader, TlmSecondaryHeader};
use cfs_link::LinkConfig;
use cfs_msg::MsgIds;
use telemetry_model::{
    DEMO_PAYLOAD_LEN, DEMO_PAYLOAD_OFFSET, Freshness, Mode, Quat, SpacecraftState,
    encode_demo_payload,
};
use bevy_cfs::{CfsPlugin, LinkHealth, Telemetry};

const FRAME: Duration = Duration::from_millis(16);
const TLM_PERIOD_S: f64 = 0.1; // 10 Hz, typical of cFS housekeeping
const SOLAR_RATE_DEG_S: f32 = 60.0;

/// Build one demo telemetry packet stamped at `sim_time`.
fn packet(seq: u16, sim_time: f64) -> Vec<u8> {
    let total = DEMO_PAYLOAD_OFFSET + DEMO_PAYLOAD_LEN;
    let mut pkt = vec![0u8; total];

    PrimaryHeader::for_total_len(
        MsgIds::LAB_DEFAULTS.sample_app_hk_tlm.apid(),
        PacketType::Telemetry,
        true,
        seq,
        total,
    )
    .unwrap()
    .write(&mut pkt[..6])
    .unwrap();

    let seconds = sim_time.trunc() as u32;
    let subseconds = (sim_time.fract() * 65536.0) as u16;
    TlmSecondaryHeader { seconds, subseconds }
        .write(&mut pkt[6..6 + TlmSecondaryHeader::LEN])
        .unwrap();
    // pkt[12..16]: the cFE telemetry-header spare, which cFE writes and the
    // decoder must skip. See ccsds::CFE_TLM_SPARE_LEN.

    // Steady sweep: any stepping or snapping in playback shows up as a spike in
    // the per-frame delta.
    let state = SpacecraftState {
        attitude: Quat::IDENTITY,
        solar_array_deg: sim_time as f32 * SOLAR_RATE_DEG_S,
        deploy_progress: (sim_time / 10.0).clamp(0.0, 1.0) as f32,
        wheel_rpm: [1000.0; 4],
        mode: Mode::Nominal,
    };
    let mut payload = [0u8; DEMO_PAYLOAD_LEN];
    encode_demo_payload(&state, &mut payload);
    pkt[DEMO_PAYLOAD_OFFSET..].copy_from_slice(&payload);
    pkt
}

/// Bind a port, then release it, so the app can bind it itself.
fn free_port() -> SocketAddr {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = s.local_addr().unwrap();
    drop(s);
    addr
}

struct Harness {
    app: App,
    to_lab: UdpSocket,
    tlm_addr: SocketAddr,
    sim_time: f64,
    seq: u16,
    next_tlm: f64,
    /// Held back to be sent after the following packet, to force reordering.
    delayed: Option<Vec<u8>>,
}

impl Harness {
    fn new() -> Self {
        let to_lab = UdpSocket::bind("127.0.0.1:0").unwrap();
        to_lab.set_nonblocking(true).unwrap();
        let tlm_addr = free_port();

        let mut app = App::new();
        // No MinimalPlugins: TimePlugin would overwrite delta from the real
        // clock, and this test needs a fixed timestep.
        app.init_resource::<Time>();
        app.add_plugins(CfsPlugin {
            link: LinkConfig {
                cmd_addr: to_lab.local_addr().unwrap(),
                tlm_bind: tlm_addr,
                dest_ip: "127.0.0.1".into(),
                ..Default::default()
            },
            buffer: telemetry_model::BufferConfig::for_rate(TLM_PERIOD_S),
            tlm_msg_id: MsgIds::LAB_DEFAULTS.sample_app_hk_tlm,
            connect: true,
        });

        Self { app, to_lab, tlm_addr, sim_time: 1000.0, seq: 0, next_tlm: 1000.0, delayed: None }
    }

    fn send(&self, pkt: &[u8]) {
        self.to_lab.send_to(pkt, self.tlm_addr).unwrap();
    }

    /// Run one frame, emitting telemetry when the simulated schedule says to.
    ///
    /// `drop_every` and `reorder_every` are in packets; 0 disables.
    fn frame(&mut self, drop_every: u16, reorder_every: u16) {
        // Discard the enable-output commands the link keeps sending.
        let mut sink = [0u8; 512];
        while self.to_lab.recv_from(&mut sink).is_ok() {}

        if self.sim_time >= self.next_tlm {
            self.next_tlm += TLM_PERIOD_S;
            let pkt = packet(self.seq, self.sim_time);
            let n = self.seq;
            self.seq = self.seq.wrapping_add(1) & 0x3FFF;

            let dropped = drop_every > 0 && n % drop_every == drop_every - 1;
            let reorder = reorder_every > 0 && n % reorder_every == reorder_every - 1;

            // Take first, send last: the held packet must arrive *after* its
            // successor, otherwise the delay restores the original order and no
            // reordering is exercised at all.
            let held = self.delayed.take();
            if !dropped {
                if reorder {
                    self.delayed = Some(pkt);
                } else {
                    self.send(&pkt);
                }
            }
            if let Some(held) = held {
                self.send(&held);
            }
        }

        // Let the receive thread actually deliver the datagram. Real time, not
        // simulated: without it the socket thread may not have run yet.
        std::thread::sleep(Duration::from_millis(1));

        self.app.world_mut().resource_mut::<Time>().advance_by(FRAME);
        self.sim_time += FRAME.as_secs_f64();
        self.app.update();
    }

    fn telemetry(&self) -> Telemetry {
        *self.app.world().resource::<Telemetry>()
    }

    fn health(&self) -> LinkHealth {
        *self.app.world().resource::<LinkHealth>()
    }
}

/// Largest single-frame jump in the swept value, over frames where playback was
/// interpolating on both sides.
fn max_live_step(samples: &[(f32, Freshness)]) -> f32 {
    samples
        .windows(2)
        .filter(|w| w[0].1.is_live() && w[1].1.is_live())
        .map(|w| (w[1].0 - w[0].0).abs())
        .fold(0.0f32, f32::max)
}

/// Nominal motion, no impairments.
#[test]
fn clean_link_produces_smooth_motion() {
    let mut h = Harness::new();
    let mut seen = Vec::new();
    for _ in 0..200 {
        h.frame(0, 0);
        let t = h.telemetry();
        seen.push((t.state.solar_array_deg, t.freshness));
    }

    assert!(h.health().packets_received > 0, "no telemetry reached the plugin");
    let live = seen.iter().filter(|(_, f)| f.is_live()).count();
    assert!(live > 100, "expected sustained Live playback, got {live} frames");

    // One frame of the sweep is 60 deg/s * 16 ms ~= 0.96 deg. Anything much
    // larger means playback snapped between samples instead of interpolating.
    let step = max_live_step(&seen);
    assert!(step < 2.0, "max per-frame step {step} deg suggests stepping, not interpolation");
    assert!(seen.iter().all(|(v, _)| v.is_finite()), "non-finite value in playback");

    let first_live = seen.iter().find(|(_, f)| f.is_live()).unwrap().0;
    let last_live = seen.iter().rev().find(|(_, f)| f.is_live()).unwrap().0;
    assert!(last_live > first_live + 50.0, "the array should have swept appreciably");
}

/// The gate: loss and reordering together must not produce visible jumps.
#[test]
fn lossy_reordered_link_still_animates_smoothly() {
    let mut h = Harness::new();
    let mut seen = Vec::new();
    // Drop one in 7, and deliver one in 5 out of order.
    for _ in 0..250 {
        h.frame(7, 5);
        let t = h.telemetry();
        seen.push((t.state.solar_array_deg, t.freshness));
    }

    let health = h.health();
    assert!(health.packets_received > 0, "no telemetry reached the plugin");
    assert!(health.buffer.reordered > 0, "test did not actually exercise reordering");

    let live = seen.iter().filter(|(_, f)| f.is_live()).count();
    assert!(live > 100, "expected sustained Live playback, got {live} frames");

    // A dropped packet doubles the interpolation span but does not change the
    // rate, so the per-frame step should stay near nominal.
    let step = max_live_step(&seen);
    assert!(step < 2.0, "max per-frame step {step} deg under loss/reorder");
    assert!(seen.iter().all(|(v, _)| v.is_finite()));
}

/// Telemetry stops: playback must hold, then admit it is stale — never keep
/// smoothly animating a vehicle that stopped reporting.
#[test]
fn signal_loss_holds_then_reports_stale() {
    let mut h = Harness::new();
    for _ in 0..80 {
        h.frame(0, 0);
    }
    let before = h.telemetry();
    assert!(before.freshness.is_live());

    // Stop transmitting by pushing the schedule out of reach.
    h.next_tlm = f64::INFINITY;
    for _ in 0..120 {
        h.frame(0, 0);
    }

    let after = h.telemetry();
    assert!(matches!(after.freshness, Freshness::Stale { .. }), "got {:?}", after.freshness);
    assert!(
        (after.state.solar_array_deg - before.state.solar_array_deg).abs() < 30.0,
        "held value drifted: playback extrapolated instead of holding"
    );
}

/// No cFS at all: the app must still run and report NoData rather than panic.
#[test]
fn absent_link_does_not_break_the_app() {
    let mut app = App::new();
    app.init_resource::<Time>();
    app.add_plugins(CfsPlugin {
        link: LinkConfig { tlm_bind: free_port(), ..Default::default() },
        connect: false,
        ..Default::default()
    });

    for _ in 0..10 {
        app.world_mut().resource_mut::<Time>().advance_by(FRAME);
        app.update();
    }
    assert_eq!(app.world().resource::<Telemetry>().freshness, Freshness::NoData);
}
