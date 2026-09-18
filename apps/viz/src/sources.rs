//! Where each animated signal's value comes from, and what to do when the
//! answer is "nowhere".
//!
//! This module exists because of the single most awkward fact in the whole
//! investigation: **a stock cFS bundle publishes no vehicle dynamics.** There
//! is no attitude quaternion, no joint angle and no wheel speed anywhere on the
//! software bus, because those come from a mission's own applications and the
//! bundle ships none. What it does publish is the flight software's own
//! housekeeping — command counters, uplink statistics, the mission clock.
//!
//! There are two honest responses to that and one dishonest one. The dishonest
//! one is to quietly run the demo generator behind a window labelled "live".
//! The honest ones are to show nothing where there is no signal, and to derive
//! what genuinely can be derived from the packets that do arrive — clearly
//! labelled as derived. This module does both, and the panel prints the source
//! of every signal so the distinction is on screen rather than in a comment.
//!
//! The derived mappings are demo mappings and are not pretending otherwise.
//! What is *not* a demo is the path: every byte behind them was decoded from a
//! real cFE packet that crossed a real socket.

use bevy::prelude::Resource;
use bevy_cfs::{Housekeeping, Telemetry};
use telemetry_model::{Freshness, Mode, Quat, SpacecraftState};

/// How many uplinked datagrams correspond to a full array deployment.
///
/// Arbitrary, and deliberately small enough that a few keypresses sweep the
/// mechanism end to end.
const PACKETS_PER_DEPLOY: u32 = 10;

/// Degrees of solar-array rotation per second of mission time.
const ARRAY_DEG_PER_S: f64 = 6.0;

/// Where a signal's value came from this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Decoded from the demo vehicle-state payload (`fake-cfs` or a replayed
    /// fixture). Every field is present because the generator invented it.
    Demo,
    /// Derived from a real cFE housekeeping field, named here.
    Derived(&'static str),
    /// Generated locally by `--offline`. Named so a screenshot can never be
    /// mistaken for one taken against a real link.
    Offline,
    /// No packet on this bus carries this quantity. The signal holds its
    /// default and the panel says so.
    None,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Demo => "demo vehicle-state payload",
            Source::Offline => "synthetic (--offline)",
            Source::Derived(field) => field,
            Source::None => "-- no source --",
        }
    }
}

/// One `Source` per animated signal, in panel order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sources {
    pub attitude: Source,
    pub solar_array: Source,
    pub deploy: Source,
    pub mode: Source,
    pub wheels: Source,
}

impl Sources {
    /// Every signal from the demo vehicle-state payload.
    pub fn demo() -> Self {
        Self::ALL_DEMO
    }

    const ALL_DEMO: Self = Self {
        attitude: Source::Demo,
        solar_array: Source::Demo,
        deploy: Source::Demo,
        mode: Source::Demo,
        wheels: Source::Demo,
    };

    /// Every signal generated locally.
    pub fn offline() -> Self {
        Self {
            attitude: Source::Offline,
            solar_array: Source::Offline,
            deploy: Source::Offline,
            mode: Source::Offline,
            wheels: Source::Offline,
        }
    }

    const NONE: Self = Self {
        attitude: Source::None,
        solar_array: Source::None,
        deploy: Source::None,
        mode: Source::None,
        wheels: Source::None,
    };

    /// Rows for the panel: (signal, source, Phase 3 mapping).
    ///
    /// The third column is the Phase 3 recommendation table applied. It is
    /// printed rather than merely implemented so the slice can be read as the
    /// answer to Phase 3's question, not just as a demo.
    pub fn rows(&self) -> [(&'static str, Source, &'static str); 5] {
        [
            ("attitude", self.attitude, "direct transform"),
            ("solar array", self.solar_array, "direct transform"),
            ("deploy", self.deploy, "clip seek"),
            ("mode", self.mode, "graph blend"),
            ("wheels", self.wheels, "material emissive"),
        ]
    }
}

/// Which pipeline produced the state the scene is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pipeline {
    /// Nothing has arrived yet.
    Waiting,
    /// A vehicle-state payload is arriving and driving the rig directly.
    Demo,
    /// Only flight-software housekeeping is arriving; the rig is driven by
    /// signals derived from it.
    Derived,
    /// No socket: the app is generating its own state for a screenshot.
    Offline,
}

impl Pipeline {
    pub fn label(self) -> &'static str {
        match self {
            Pipeline::Waiting => "WAITING - no telemetry yet",
            Pipeline::Demo => "LIVE - vehicle-state payload",
            Pipeline::Derived => "LIVE - derived from cFE housekeeping",
            Pipeline::Offline => "OFFLINE - no socket, synthetic state",
        }
    }
}

/// Baselines captured the first time housekeeping was seen.
///
/// Counters in cFE are absolute since the application started, and cFS has
/// usually been running for a while before a viz connects. Subtracting a
/// baseline is what stops the array appearing fully deployed the moment the
/// window opens.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct Baseline {
    pub ingest_packets: Option<u32>,
    pub mission_time_s: Option<f64>,
}

/// Assemble vehicle state from whatever the bus actually carries.
///
/// Returns the state to animate, where each signal came from, and which
/// pipeline was used.
pub fn resolve(
    telemetry: &Telemetry,
    hk: &Housekeeping,
    baseline: &mut Baseline,
) -> (SpacecraftState, Sources, Pipeline) {
    // A demo payload beats derivation: if something is publishing real vehicle
    // state, that is the thing to show.
    if telemetry.freshness != Freshness::NoData {
        return (telemetry.state, Sources::ALL_DEMO, Pipeline::Demo);
    }

    let Some(sample_app) = hk.sample_app else {
        return (SpacecraftState::default(), Sources::NONE, Pipeline::Waiting);
    };

    let mut state = SpacecraftState::default();
    let mut sources = Sources::NONE;

    // Mode from a counter only *our* commands move. Pressing the no-op key
    // steps the vehicle through its modes, which is the whole point: the pose
    // change on screen is caused by a command that went to real flight
    // software and came back as telemetry.
    state.mode = match sample_app.command_counter % 4 {
        0 => Mode::Safe,
        1 => Mode::Nominal,
        2 => Mode::Deploying,
        _ => Mode::Deployed,
    };
    sources.mode = Source::Derived("SAMPLE_APP.CommandCounter % 4");

    if let Some(ci_lab) = hk.ci_lab {
        let base = *baseline.ingest_packets.get_or_insert(ci_lab.ingest_packets);
        let since = ci_lab.ingest_packets.saturating_sub(base);
        state.deploy_progress =
            (since as f32 / PACKETS_PER_DEPLOY as f32).clamp(0.0, 1.0);
        sources.deploy = Source::Derived("CI_LAB.IngestPackets since connect");
    }

    if let Some(now) = hk.last_time {
        let base = *baseline.mission_time_s.get_or_insert(now);
        // Modulo before the cast: mission time is ~1.4e9 seconds and f32 has
        // 24 bits of mantissa, so converting first would quantize the angle
        // into steps of tens of degrees.
        let deg = ((now - base) * ARRAY_DEG_PER_S).rem_euclid(360.0);
        state.solar_array_deg = deg as f32;
        sources.solar_array = Source::Derived("CFE mission time");
    }

    // attitude and wheel_rpm keep their defaults, and `sources` still says
    // `None` for both. That gap is a result, not an omission: it is the
    // measurement of how much of a spacecraft a stock cFS bundle describes.
    (state, sources, Pipeline::Derived)
}

/// Deterministic state for `--offline`, used by screenshots and by anyone
/// without a container.
///
/// Shaped like the Phase 3 spike's synthetic source so captures from the two
/// are comparable: hold, deploy, hold.
pub fn offline_state(t: f32) -> SpacecraftState {
    let progress = ((t - 2.0) / 8.0).clamp(0.0, 1.0);
    let mode = if t < 1.5 {
        Mode::Safe
    } else if t < 2.0 {
        Mode::Nominal
    } else if progress < 1.0 {
        Mode::Deploying
    } else {
        Mode::Deployed
    };
    let yaw = t * 0.18;
    SpacecraftState {
        attitude: Quat([0.0, (yaw * 0.5).sin(), 0.0, (yaw * 0.5).cos()]),
        solar_array_deg: t * 12.0,
        deploy_progress: progress,
        wheel_rpm: [t * 40.0, -t * 25.0, t * 10.0, 0.0],
        mode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfs_msg::hk::{CiLabHk, SampleAppHk};

    fn hk_with(command_counter: u8, ingest: u32, time: f64) -> Housekeeping {
        Housekeeping {
            sample_app: Some(SampleAppHk { command_counter, command_error_counter: 0 }),
            to_lab: None,
            ci_lab: Some(CiLabHk { ingest_packets: ingest, ..Default::default() }),
            last_time: Some(time),
        }
    }

    fn no_demo() -> Telemetry {
        Telemetry { state: SpacecraftState::default(), freshness: Freshness::NoData }
    }

    #[test]
    fn nothing_on_the_bus_means_nothing_claimed() {
        let (state, sources, pipeline) =
            resolve(&no_demo(), &Housekeeping::default(), &mut Baseline::default());
        assert_eq!(pipeline, Pipeline::Waiting);
        assert_eq!(state, SpacecraftState::default());
        assert!(sources.rows().iter().all(|(_, s, _)| *s == Source::None));
    }

    /// The gap is the finding, so it is asserted rather than left to inspection.
    #[test]
    fn stock_cfs_supplies_no_attitude_and_no_wheels() {
        let (state, sources, pipeline) =
            resolve(&no_demo(), &hk_with(1, 50, 1000.0), &mut Baseline::default());
        assert_eq!(pipeline, Pipeline::Derived);
        assert_eq!(sources.attitude, Source::None);
        assert_eq!(sources.wheels, Source::None);
        assert_eq!(state.attitude, Quat::IDENTITY);
        assert_eq!(state.wheel_rpm, [0.0; 4]);
    }

    /// Connecting to a cFS that has been up for hours must not show a vehicle
    /// mid-deployment.
    #[test]
    fn counters_are_relative_to_the_moment_we_connected() {
        let mut baseline = Baseline::default();
        let (first, _, _) = resolve(&no_demo(), &hk_with(0, 9_000, 1000.0), &mut baseline);
        assert_eq!(first.deploy_progress, 0.0, "a long-running cFS started us mid-deploy");
        assert_eq!(first.solar_array_deg, 0.0);

        let (later, _, _) = resolve(&no_demo(), &hk_with(0, 9_005, 1010.0), &mut baseline);
        assert!((later.deploy_progress - 0.5).abs() < 1e-6);
        assert!((later.solar_array_deg - 60.0).abs() < 1e-3);
    }

    /// Mission time is a large number and the angle is an f32; converting in the
    /// wrong order throws away every degree of resolution.
    #[test]
    fn array_angle_survives_a_realistic_mission_time() {
        let mut baseline = Baseline::default();
        let epoch = 1_456_000_000.0f64;
        resolve(&no_demo(), &hk_with(0, 0, epoch), &mut baseline);
        let (state, _, _) = resolve(&no_demo(), &hk_with(0, 0, epoch + 1.5), &mut baseline);
        assert!((state.solar_array_deg - 9.0).abs() < 1e-3, "got {}", state.solar_array_deg);
    }

    #[test]
    fn each_no_op_steps_the_mode_and_wraps() {
        let seen: Vec<Mode> = (0u8..5)
            .map(|c| resolve(&no_demo(), &hk_with(c, 0, 0.0), &mut Baseline::default()).0.mode)
            .collect();
        assert_eq!(seen, [Mode::Safe, Mode::Nominal, Mode::Deploying, Mode::Deployed, Mode::Safe]);
    }

    /// A real vehicle-state payload must win, or connecting to `fake-cfs` would
    /// show derived counters instead of the vehicle it is describing.
    #[test]
    fn a_vehicle_payload_outranks_derivation() {
        let telemetry = Telemetry {
            state: SpacecraftState { solar_array_deg: 42.0, ..Default::default() },
            freshness: Freshness::Live,
        };
        let (state, sources, pipeline) =
            resolve(&telemetry, &hk_with(3, 99, 1000.0), &mut Baseline::default());
        assert_eq!(pipeline, Pipeline::Demo);
        assert_eq!(state.solar_array_deg, 42.0);
        assert_eq!(sources.attitude, Source::Demo);
    }
}
