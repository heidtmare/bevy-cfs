//! Where each animated signal's value comes from, and what to do when the
//! answer is "nowhere".
//!
//! This module exists because of what used to be the most awkward fact in the
//! whole investigation: **a stock cFS bundle publishes no vehicle dynamics.**
//! There is no attitude quaternion, no joint angle and no wheel speed anywhere
//! on the software bus, because those come from a mission's own applications
//! and the bundle ships none. What it does publish is the flight software's own
//! housekeeping — command counters, uplink statistics, the mission clock.
//!
//! There are two honest responses to that and one dishonest one. The dishonest
//! one is to quietly run a generator behind a window labelled "live". The
//! honest ones are to show nothing where there is no signal, and to derive what
//! genuinely can be derived from the packets that do arrive — clearly labelled
//! as derived. This module does both, and the panel prints the source of every
//! signal so the distinction is on screen rather than in a comment.
//!
//! # The third response: publish the missing packet
//!
//! There is now a fourth source, and it is the interesting one.
//! `spikes/rust-cfs-app` is a cFE application, written in Rust, loaded into the
//! same cFS container — and it publishes real vehicle dynamics on the software
//! bus, computed by `crates/vehicle-dyn` running inside cFE. Against a build
//! with it loaded, every row in the panel names a real flight field and nothing
//! is derived or missing.
//!
//! All four sources still coexist, and the ranking in [`resolve`] is the whole
//! contract: a flight vehicle-state packet beats a stand-in, a stand-in beats
//! derivation from housekeeping, and derivation beats inventing a number. The
//! panel says which one won.

use bevy::prelude::Resource;
use bevy_cfs::{Housekeeping, Telemetry};
use telemetry_model::{Freshness, Mode, SpacecraftState};


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
    /// Computed by `vehicle-dyn` inside the cFE application and decoded from
    /// the vehicle-state packet it published. The named field is the one in
    /// `vehicle_dyn::Vehicle` that produced it.
    Flight(&'static str),
    /// The same payload from `tools/fake-cfs` — the same model, stepped in a
    /// host process instead of inside cFE. Distinguished from `Flight` because
    /// "a real cFS computed this" is a different claim from "the same code
    /// computed this somewhere else", and the panel should not conflate them.
    Standin,
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
            Source::Flight(field) => field,
            Source::Standin => "fake-cfs vehicle-state payload",
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
    /// Every signal from the stand-in's vehicle-state payload.
    pub fn standin() -> Self {
        Self::ALL_STANDIN
    }

    const ALL_STANDIN: Self = Self {
        attitude: Source::Standin,
        solar_array: Source::Standin,
        deploy: Source::Standin,
        mode: Source::Standin,
        wheels: Source::Standin,
    };

    /// Every signal from the Rust cFE application's vehicle-state packet.
    ///
    /// The names are the fields of `vehicle_dyn::Vehicle` that produced each
    /// value, not the packet offsets — what an operator wants to know is which
    /// piece of flight software is responsible, and the offsets are in
    /// `telemetry_model::encode_vehicle_state`.
    const ALL_FLIGHT: Self = Self {
        attitude: Source::Flight("RUST_APP.attitude (integrated)"),
        solar_array: Source::Flight("RUST_APP.array_deg (sun tracking)"),
        deploy: Source::Flight("RUST_APP.deploy_progress"),
        mode: Source::Flight("RUST_APP.mode"),
        wheels: Source::Flight("RUST_APP.wheel_rpm x4"),
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
    /// A vehicle-state payload from the Rust cFE application is driving the rig.
    Flight,
    /// A vehicle-state payload from `fake-cfs` is driving the rig.
    Standin,
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
            Pipeline::Flight => "LIVE - vehicle state from RUST_APP, inside cFE",
            Pipeline::Standin => "LIVE - vehicle state from fake-cfs",
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
    // A vehicle-state payload beats derivation: if something is publishing real
    // vehicle state, that is the thing to show.
    //
    // Which producer it came from is decided by whether `RUST_APP` housekeeping
    // is also on the bus, not by anything in the vehicle packet itself. Both
    // producers emit byte-identical packets on the same message ID — that is
    // the point of sharing the encoder — so the packet cannot identify its own
    // author, and a flag inside it would be a claim the ground could not check.
    // The presence of the application's own housekeeping is evidence of a kind
    // the packet cannot fake.
    if telemetry.freshness != Freshness::NoData {
        let (sources, pipeline) = if hk.rust_app.is_some() {
            (Sources::ALL_FLIGHT, Pipeline::Flight)
        } else {
            (Sources::ALL_STANDIN, Pipeline::Standin)
        };
        return (telemetry.state, sources, pipeline);
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
/// Runs the real vehicle model forward from `t = 0` to `t`, rather than
/// evaluating a closed-form curve. That costs a few thousand integration steps
/// per frame at 60 Hz, which is nothing, and buys two things worth more: the
/// offline pose is a pose the vehicle can actually reach, and an offline
/// screenshot is comparable with a live one instead of merely resembling it.
///
/// Deterministic because [`vehicle_dyn::Vehicle`] is: same `t`, same step size,
/// same state, which is what makes `--screenshot --at` reproducible.
pub fn offline_state(t: f32) -> SpacecraftState {
    /// Integration step. Fixed, and not the frame time: a screenshot at `--at
    /// 12.0` must be the same frame whatever rate the renderer happened to hit.
    const STEP: f32 = 1.0 / 120.0;

    let mut vehicle = vehicle_dyn::Vehicle::new();
    for _ in 0..((t.max(0.0) / STEP) as u32) {
        vehicle.step(STEP);
    }
    vehicle.state()
}

/// Seconds into the offline timeline at which the arrays are mid-travel.
///
/// The vehicle deploys itself once it has detumbled (see
/// `vehicle_dyn::Vehicle::step_modes`), so this is an *observation* of the
/// sequence rather than a choice about it — which is why it is pinned by
/// `offline_deploys_partway_through` rather than just written in a comment.
/// It is the `--at` value the committed deployment screenshot uses.
pub const OFFLINE_MID_DEPLOY_S: f32 = 14.0;

#[cfg(test)]
mod tests {
    use super::*;
    use cfs_msg::hk::{CiLabHk, SampleAppHk};

    fn hk_with(command_counter: u8, ingest: u32, time: f64) -> Housekeeping {
        Housekeeping {
            sample_app: Some(SampleAppHk { command_counter, command_error_counter: 0 }),
            to_lab: None,
            ci_lab: Some(CiLabHk { ingest_packets: ingest, ..Default::default() }),
            rust_app: None,
            last_time: Some(time),
        }
    }

    fn no_vehicle_packet() -> Telemetry {
        Telemetry { state: SpacecraftState::default(), freshness: Freshness::NoData }
    }

    fn live_vehicle_packet() -> Telemetry {
        Telemetry {
            state: SpacecraftState { solar_array_deg: 42.0, ..Default::default() },
            freshness: Freshness::Live,
        }
    }

    #[test]
    fn nothing_on_the_bus_means_nothing_claimed() {
        let (state, sources, pipeline) =
            resolve(&no_vehicle_packet(), &Housekeeping::default(), &mut Baseline::default());
        assert_eq!(pipeline, Pipeline::Waiting);
        assert_eq!(state, SpacecraftState::default());
        assert!(sources.rows().iter().all(|(_, s, _)| *s == Source::None));
    }

    /// The gap is the finding, so it is asserted rather than left to inspection.
    ///
    /// Still asserted now that `spikes/rust-cfs-app` fills it: this is the
    /// behaviour against a cFS *without* that application loaded, which is what
    /// a stock bundle is, and the claim in the README rests on it.
    #[test]
    fn stock_cfs_supplies_no_attitude_and_no_wheels() {
        let (state, sources, pipeline) =
            resolve(&no_vehicle_packet(), &hk_with(1, 50, 1000.0), &mut Baseline::default());
        assert_eq!(pipeline, Pipeline::Derived);
        assert_eq!(sources.attitude, Source::None);
        assert_eq!(sources.wheels, Source::None);
        assert_eq!(state.attitude, telemetry_model::Quat::IDENTITY);
        assert_eq!(state.wheel_rpm, [0.0; 4]);
    }

    /// Connecting to a cFS that has been up for hours must not show a vehicle
    /// mid-deployment.
    #[test]
    fn counters_are_relative_to_the_moment_we_connected() {
        let mut baseline = Baseline::default();
        let (first, _, _) =
            resolve(&no_vehicle_packet(), &hk_with(0, 9_000, 1000.0), &mut baseline);
        assert_eq!(first.deploy_progress, 0.0, "a long-running cFS started us mid-deploy");
        assert_eq!(first.solar_array_deg, 0.0);

        let (later, _, _) = resolve(&no_vehicle_packet(), &hk_with(0, 9_005, 1010.0), &mut baseline);
        assert!((later.deploy_progress - 0.5).abs() < 1e-6);
        assert!((later.solar_array_deg - 60.0).abs() < 1e-3);
    }

    /// Mission time is a large number and the angle is an f32; converting in the
    /// wrong order throws away every degree of resolution.
    #[test]
    fn array_angle_survives_a_realistic_mission_time() {
        let mut baseline = Baseline::default();
        let epoch = 1_456_000_000.0f64;
        resolve(&no_vehicle_packet(), &hk_with(0, 0, epoch), &mut baseline);
        let (state, _, _) =
            resolve(&no_vehicle_packet(), &hk_with(0, 0, epoch + 1.5), &mut baseline);
        assert!((state.solar_array_deg - 9.0).abs() < 1e-3, "got {}", state.solar_array_deg);
    }

    #[test]
    fn each_no_op_steps_the_mode_and_wraps() {
        let seen: Vec<Mode> = (0u8..5)
            .map(|c| {
                resolve(&no_vehicle_packet(), &hk_with(c, 0, 0.0), &mut Baseline::default()).0.mode
            })
            .collect();
        assert_eq!(seen, [Mode::Safe, Mode::Nominal, Mode::Deploying, Mode::Deployed, Mode::Safe]);
    }

    /// A real vehicle-state payload must win, or connecting to something
    /// publishing vehicle state would show derived counters instead of the
    /// vehicle it is describing.
    #[test]
    fn a_vehicle_payload_outranks_derivation() {
        let (state, sources, pipeline) =
            resolve(&live_vehicle_packet(), &hk_with(3, 99, 1000.0), &mut Baseline::default());
        assert_eq!(pipeline, Pipeline::Standin);
        assert_eq!(state.solar_array_deg, 42.0);
        assert_eq!(sources.attitude, Source::Standin);
    }

    /// The claim the panel makes — "this came from software running inside
    /// cFE" — must rest on evidence the vehicle packet cannot manufacture.
    /// `RUST_APP` housekeeping is that evidence.
    #[test]
    fn the_flight_claim_needs_the_flight_apps_own_housekeeping() {
        let mut hk = hk_with(0, 0, 1000.0);
        let (_, standin, pipeline) =
            resolve(&live_vehicle_packet(), &hk, &mut Baseline::default());
        assert_eq!(pipeline, Pipeline::Standin, "an identical packet claimed to be from flight");
        assert_eq!(standin.attitude, Source::Standin);

        hk.rust_app = Some(cfs_msg::rust_app::RustAppHk::default());
        let (_, flight, pipeline) = resolve(&live_vehicle_packet(), &hk, &mut Baseline::default());
        assert_eq!(pipeline, Pipeline::Flight);
        assert!(matches!(flight.attitude, Source::Flight(_)));
        assert!(
            flight.rows().iter().all(|(_, s, _)| matches!(s, Source::Flight(_))),
            "a signal was left unsourced with the flight app publishing"
        );
    }

    /// `--offline` must produce a vehicle the model could actually have
    /// reached, and produce the same one every time, or the committed
    /// screenshots would drift.
    #[test]
    fn offline_state_is_deterministic_and_physical() {
        let a = offline_state(20.0);
        let b = offline_state(20.0);
        assert_eq!(a, b, "offline state is not reproducible");

        let n: f32 = a.attitude.0.iter().map(|v| v * v).sum();
        assert!((n - 1.0).abs() < 1e-4, "attitude is not a unit quaternion: {n}");
        assert!((0.0..=1.0).contains(&a.deploy_progress));

        // Far enough along to have left Safe and be flying the survey.
        assert_ne!(a.mode, Mode::Safe);
    }

    /// The offline timeline must reach the deployment on its own, and
    /// [`OFFLINE_MID_DEPLOY_S`] must actually land inside it — otherwise the
    /// committed screenshot of the mechanism would quietly become a screenshot
    /// of a stowed or fully deployed vehicle after any tuning change.
    #[test]
    fn offline_deploys_partway_through() {
        assert_eq!(offline_state(5.0).deploy_progress, 0.0, "deployed while still tumbling");

        let mid = offline_state(OFFLINE_MID_DEPLOY_S).deploy_progress;
        assert!(
            mid > 0.05 && mid < 0.95,
            "{OFFLINE_MID_DEPLOY_S}s is not mid-deploy any more: progress {mid}"
        );
        assert_eq!(offline_state(OFFLINE_MID_DEPLOY_S).mode, Mode::Deploying);

        assert_eq!(offline_state(30.0).deploy_progress, 1.0, "never finished deploying");
        assert_eq!(offline_state(30.0).mode, Mode::Deployed);
    }
}
