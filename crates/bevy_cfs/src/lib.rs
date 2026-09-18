//! Bevy plugin that turns live cFS telemetry into smooth, sampleable scene state.
//!
//! # Where the work happens
//!
//! Almost none of it is here. Packet decoding lives in `ccsds`/`cfs-msg`, the
//! transport in `cfs-link`, and the jitter buffer and interpolation in
//! `telemetry-model` — all Bevy-free and unit-tested on their own. This crate is
//! the adapter: it owns resources, runs two systems per frame, and nothing else.
//!
//! That split is deliberate. The hard logic (interpolation, reordering, clock
//! resync) is the part most likely to be wrong, and it is far easier to test as
//! plain functions than inside a running `App`.
//!
//! # Frame budget
//!
//! Neither system can block. The socket is drained through a bounded channel
//! filled by a background thread in `cfs-link`, so a stalled renderer drops old
//! packets instead of stalling the socket, and a dead link costs a single empty
//! channel poll per frame.
//!
//! # Usage
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_cfs::{CfsPlugin, Telemetry};
//!
//! fn main() {
//!     App::new()
//!         .add_plugins(MinimalPlugins)
//!         .add_plugins(CfsPlugin::default())
//!         .add_systems(Update, |tlm: Res<Telemetry>| {
//!             // tlm.state is interpolated for this instant; tlm.freshness says
//!             // whether to believe it.
//!             let _ = tlm.state.attitude;
//!         })
//!         .run();
//! }
//! ```

use bevy::prelude::*;

use cfs_link::{CfsLink, LinkConfig, RawPacket};
use cfs_msg::hk::{CiLabHk, SampleAppHk, ToLabHk};
use cfs_msg::{MsgId, MsgIds, sample_app};
use telemetry_model::{BufferConfig, BufferStats, Freshness, JitterBuffer, SpacecraftState};

/// System sets, exposed so a consumer can order its own work against telemetry.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CfsSet {
    /// Drain the socket and decode into the buffer.
    Ingest,
    /// Advance playback and publish [`Telemetry`].
    Sample,
    /// Send queued commands. Runs last so a command written this frame goes out
    /// in the same frame it was requested.
    Command,
}

/// The live connection. Absent when running offline against replayed data.
#[derive(Resource)]
pub struct CfsLinkRes(pub CfsLink);

/// The jitter buffer. Exposed for tests and for offline sources to push into.
#[derive(Resource)]
pub struct TelemetryBuffer(pub JitterBuffer);

/// Which telemetry message to decode.
#[derive(Resource, Debug, Clone, Copy)]
pub struct DecodeConfig {
    /// Which message carries the vehicle-state payload.
    pub tlm_msg_id: MsgId,
    /// Everything else this build publishes that the plugin understands.
    pub msg_ids: MsgIds,
}

/// Interpolated vehicle state for the current frame — what a renderer reads.
///
/// Always populated: before any telemetry arrives it holds
/// [`SpacecraftState::default`] with [`Freshness::NoData`], so consumers never
/// deal with an `Option` and never get a silently missing resource.
#[derive(Resource, Debug, Clone, Copy)]
pub struct Telemetry {
    pub state: SpacecraftState,
    /// Whether to believe [`Telemetry::state`]. Surface this in the UI.
    pub freshness: Freshness,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self { state: SpacecraftState::default(), freshness: Freshness::NoData }
    }
}

/// Real cFE housekeeping, decoded from the live bus.
///
/// Distinct from [`Telemetry`] on purpose. `Telemetry` is *vehicle* state,
/// which on a real mission arrives in a mission-specific packet; this is the
/// *flight software's* own state, which every cFS build publishes whether or
/// not anyone has written a spacecraft yet. A ground display needs both, and
/// against a stock `cFS` bundle this is the only one that exists.
///
/// Each field is `None` until the corresponding message has been seen, so a
/// panel can distinguish "zero" from "never heard from".
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct Housekeeping {
    pub sample_app: Option<SampleAppHk>,
    pub to_lab: Option<ToLabHk>,
    pub ci_lab: Option<CiLabHk>,
    /// Mission-epoch timestamp of the most recent housekeeping packet.
    pub last_time: Option<f64>,
}

/// A command to send to `ci_lab` on this frame.
///
/// A message rather than a direct call so that the UI layer never touches the
/// socket: the button writes, one system sends, and the send has exactly one
/// place to fail, count and log.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfsCommand {
    /// `SAMPLE_APP` no-op. Increments its `CommandCounter`, which comes back on
    /// the next housekeeping cycle — the cheapest observable round trip.
    SampleAppNoop,
    /// `SAMPLE_APP` reset counters. Drives both counters to zero.
    SampleAppResetCounters,
}

/// Link and buffer counters, for a health panel.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct LinkHealth {
    pub packets_received: u64,
    pub packets_dropped: u64,
    pub parse_errors: u64,
    pub sequence_gaps: u64,
    /// Packets that parsed but were not the message we decode. Expected: a live
    /// software bus carries plenty of traffic we do not model.
    pub packets_ignored: u64,
    /// Packets whose payload did not decode. Unexpected — a layout mismatch.
    pub decode_failures: u64,
    /// Commands this process has put on the wire, enable-output keepalives
    /// included.
    pub commands_sent: u64,
    pub buffer: BufferStats,
    pub buffered_samples: usize,
    pub estimated_rate_hz: Option<f64>,
}

/// Drives [`Telemetry`] from a cFS link.
pub struct CfsPlugin {
    pub link: LinkConfig,
    pub buffer: BufferConfig,
    pub tlm_msg_id: MsgId,
    /// Whether to open the socket on startup.
    ///
    /// `false` leaves the plugin running with no source, which is how tests and
    /// fixture replay drive it — the buffer is populated directly instead.
    pub connect: bool,
}

impl Default for CfsPlugin {
    fn default() -> Self {
        Self {
            link: LinkConfig::default(),
            buffer: BufferConfig::default(),
            tlm_msg_id: MsgIds::LAB_DEFAULTS.sample_app_hk_tlm,
            connect: true,
        }
    }
}

impl Plugin for CfsPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(TelemetryBuffer(JitterBuffer::new(self.buffer)))
            .insert_resource(DecodeConfig {
                tlm_msg_id: self.tlm_msg_id,
                msg_ids: self.link.msg_ids,
            })
            .init_resource::<Telemetry>()
            .init_resource::<Housekeeping>()
            .init_resource::<LinkHealth>()
            .add_message::<CfsCommand>()
            .add_systems(
                Update,
                (
                    ingest_packets.in_set(CfsSet::Ingest),
                    advance_playback.in_set(CfsSet::Sample),
                )
                    .chain(),
            )
            .add_systems(Update, send_commands.in_set(CfsSet::Command).after(CfsSet::Sample));

        if self.connect {
            match CfsLink::connect(self.link.clone()) {
                Ok(link) => {
                    app.insert_resource(CfsLinkRes(link));
                }
                // A viz that cannot reach cFS should still start and show a dead
                // link, rather than refusing to launch.
                //
                // eprintln rather than bevy's error!: that macro lives in
                // bevy_log, which is a default feature this crate deliberately
                // does not take. apps/viz brings its own logging.
                Err(e) => eprintln!("bevy_cfs: link unavailable, no telemetry: {e}"),
            }
        }
    }
}

/// Drain the socket, decode, and insert into the jitter buffer.
#[allow(clippy::too_many_arguments)]
fn ingest_packets(
    link: Option<Res<CfsLinkRes>>,
    mut buffer: ResMut<TelemetryBuffer>,
    config: Res<DecodeConfig>,
    mut hk: ResMut<Housekeeping>,
    mut health: ResMut<LinkHealth>,
    // Reused across frames so a busy link does not allocate every frame.
    mut scratch: Local<Vec<RawPacket>>,
) {
    let Some(link) = link else { return };

    scratch.clear();
    link.0.drain_into(&mut scratch);

    for raw in scratch.iter() {
        let Ok(packet) = raw.parse() else {
            // Counted by cfs-link's receive thread, which rejects malformed
            // datagrams before they are ever queued.
            continue;
        };
        // Housekeeping first, and unconditionally: it is real cFE telemetry and
        // arrives whether or not this build publishes anything the vehicle
        // model understands.
        let stream_id = packet.primary().stream_id();
        let ids = config.msg_ids;
        let mut recognized = false;
        if stream_id == ids.sample_app_hk_tlm.0
            && let Some(v) = SampleAppHk::from_packet(&packet)
            // A demo packet shares this message ID and is far longer; length is
            // what tells the two apart without a mode flag.
            && packet.cfe_tlm_payload().map(|p| p.len()) == Ok(SampleAppHk::LEN)
        {
            hk.sample_app = Some(v);
            recognized = true;
        }
        if stream_id == ids.to_lab_hk_tlm.0
            && let Some(v) = ToLabHk::from_packet(&packet)
        {
            hk.to_lab = Some(v);
            recognized = true;
        }
        if stream_id == ids.ci_lab_hk_tlm.0
            && let Some(v) = CiLabHk::from_packet(&packet)
        {
            hk.ci_lab = Some(v);
            recognized = true;
        }
        if recognized && let Ok(t) = packet.tlm_secondary() {
            hk.last_time = Some(t.as_secs_f64());
        }

        match telemetry_model::decode_demo(&packet, config.tlm_msg_id) {
            Some(sample) => buffer.0.insert(sample),
            None if recognized => {}
            None if stream_id == config.tlm_msg_id.0 => {
                // Right message, unreadable payload: a real layout mismatch.
                health.decode_failures += 1;
            }
            None => health.packets_ignored += 1,
        }
    }

    use std::sync::atomic::Ordering::Relaxed;
    let stats = link.0.stats();
    health.packets_received = stats.packets_received.load(Relaxed);
    health.packets_dropped = stats.packets_dropped.load(Relaxed);
    health.parse_errors = stats.parse_errors.load(Relaxed);
    health.sequence_gaps = stats.sequence_gaps.load(Relaxed);
    health.commands_sent = stats.commands_sent.load(Relaxed);
}

/// Put queued commands on the wire.
///
/// Silently doing nothing with no link would be the wrong failure: a button
/// that appears to work is worse than one that reports it cannot. The counter
/// the UI displays comes from the link's own stats, so it only moves when a
/// datagram was actually sent.
fn send_commands(
    link: Option<Res<CfsLinkRes>>,
    config: Res<DecodeConfig>,
    mut commands: MessageReader<CfsCommand>,
) {
    let Some(link) = link else {
        for cmd in commands.read() {
            eprintln!("bevy_cfs: no link, dropping {cmd:?}");
        }
        return;
    };

    for cmd in commands.read() {
        let mut buf = [0u8; 64];
        let seq = link.0.next_seq();
        let id = config.msg_ids.sample_app_cmd;
        let built = match cmd {
            CfsCommand::SampleAppNoop => sample_app::noop(&mut buf, id, seq),
            CfsCommand::SampleAppResetCounters => sample_app::reset_counters(&mut buf, id, seq),
        };
        match built.map_err(|e| e.to_string()).and_then(|pkt| {
            link.0.send_raw(pkt).map_err(|e| e.to_string())
        }) {
            Ok(()) => {}
            Err(e) => eprintln!("bevy_cfs: sending {cmd:?} failed: {e}"),
        }
    }
}

/// Advance the playback clock and publish the interpolated state.
fn advance_playback(
    time: Res<Time>,
    mut buffer: ResMut<TelemetryBuffer>,
    mut telemetry: ResMut<Telemetry>,
    mut health: ResMut<LinkHealth>,
) {
    buffer.0.advance(time.delta_secs_f64());
    let (state, freshness) = buffer.0.playback();
    telemetry.state = state;
    telemetry.freshness = freshness;

    health.buffer = buffer.0.stats();
    health.buffered_samples = buffer.0.len();
    health.estimated_rate_hz = buffer.0.estimated_rate_hz();
}
