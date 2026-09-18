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
use cfs_msg::{MsgId, MsgIds};
use telemetry_model::{BufferConfig, BufferStats, Freshness, JitterBuffer, SpacecraftState};

/// System sets, exposed so a consumer can order its own work against telemetry.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CfsSet {
    /// Drain the socket and decode into the buffer.
    Ingest,
    /// Advance playback and publish [`Telemetry`].
    Sample,
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
    pub tlm_msg_id: MsgId,
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
            .insert_resource(DecodeConfig { tlm_msg_id: self.tlm_msg_id })
            .init_resource::<Telemetry>()
            .init_resource::<LinkHealth>()
            .add_systems(
                Update,
                (ingest_packets.in_set(CfsSet::Ingest), advance_playback.in_set(CfsSet::Sample))
                    .chain(),
            );

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
fn ingest_packets(
    link: Option<Res<CfsLinkRes>>,
    mut buffer: ResMut<TelemetryBuffer>,
    config: Res<DecodeConfig>,
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
        match telemetry_model::decode_demo(&packet, config.tlm_msg_id) {
            Some(sample) => buffer.0.insert(sample),
            None if packet.primary().stream_id() == config.tlm_msg_id.0 => {
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
