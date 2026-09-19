//! Spacecraft attitude dynamics, reaction-wheel control and a mission
//! sequencer — the vehicle software a stock cFS bundle does not ship.
//!
//! # Why this crate exists
//!
//! Finding 0005 recorded the most awkward fact in the investigation: **stock
//! cFS publishes no vehicle dynamics.** No attitude, no body rates, no wheel
//! speeds. Those come from a mission's own applications, and the bundle ships
//! none, so `apps/viz` had to show `-- no source --` for most of what it can
//! animate. That gap was the honest answer to "what can a ground visualizer
//! read off a stock cFS?", and it was also a dead end for the visualizer.
//!
//! This crate closes it from the other side. It is the vehicle software — and
//! it is what `spikes/rust-cfs-app` runs *inside cFE*, on the flight side of
//! the socket, publishing what it computes as real telemetry on the real
//! software bus.
//!
//! # What it is not
//!
//! Not a simulator of any particular spacecraft, and not a validated one of
//! anything. The inertia tensor, wheel sizes and gains are plausible
//! small-satellite numbers chosen to produce motion legible at a glance —
//! slews that take a few seconds and wheels that saturate occasionally, rather
//! than either. What *is* load-bearing is that the physics is closed: momentum
//! is conserved between the wheels and the body, a saturated wheel genuinely
//! stops producing torque, and nothing anywhere writes an attitude directly.
//! The numbers on the downlink are the output of an integration, so they
//! behave like telemetry — they lag, they overshoot, they settle.
//!
//! # No `std`, no Bevy, no sockets
//!
//! PLAN.md's rule for the `no_std` crates is that they are what gets reused on
//! the flight side if Architecture B goes ahead. This is the first crate where
//! that stopped being a hypothesis: the same object code runs in the
//! visualizer, in `tools/fake-cfs` and inside cFE. The crate therefore has no
//! I/O of any kind — it is a state machine you step.

#![no_std]
#![forbid(unsafe_code)]

mod math;
pub mod vec3;
pub mod wheels;

pub use vec3::{Quat, Vec3};
pub use wheels::{Applied, WHEEL_COUNT, Wheels, saturation_rpm};

use telemetry_model::{Mode, SpacecraftState};

// ------------------------------------------------------------- constants ---

/// Diagonal inertia tensor, kg·m². A roughly box-shaped small satellite, of
/// the size where a 90° slew takes seconds rather than the many minutes a
/// large bus would need.
///
/// That sizing is a deliberate choice and it is the only place where "make it
/// watchable" outranked "pick a typical number" — and it does not compromise
/// the physics, because every other quantity here is then sized *consistently*
/// with it. A vehicle of this inertia really would slew this fast with wheels
/// of [`wheels::WHEEL_MOMENTUM_LIMIT`].
///
/// Deliberately *not* symmetric: equal principal moments would make every axis
/// behave identically and hide the gyroscopic coupling entirely, which is one
/// of the few things in here that a viewer can actually see happening.
pub const INERTIA: Vec3 = Vec3::new(0.12, 0.15, 0.10);

/// Attitude-error gain, N·m per radian.
///
/// [`KP`] and [`KD`] together put the closed loop at roughly 0.45 rad/s with a
/// damping ratio near 0.9 against [`INERTIA`] — fast enough to be watchable,
/// damped enough that a slew settles instead of ringing.
const KP: f32 = 0.025;

/// Rate-damping gain, N·m per rad/s.
const KD: f32 = 0.10;

/// Largest torque the controller will ask for, N·m — a small reaction wheel's
/// real capability. Every slew of any size is torque-limited at both ends,
/// which is what gives the motion its accelerate-coast-decelerate shape rather
/// than the exponential a pure PD loop would produce.
const MAX_TORQUE: f32 = 0.030;

/// Body rate below which `Safe` mode's detumbler has finished, rad/s.
const DETUMBLE_SETTLED: f32 = 0.005;

/// Rate-damping gain used in `Safe`, where there is no attitude target — only
/// the requirement to stop moving.
const SAFE_KD: f32 = 0.05;

/// Seconds a deployment takes, matching the authored `Deploy` clip's duration.
///
/// The ground's clip is 2 s (`telemetry_anim::rig::DEPLOY_DURATION_S`) but that
/// is a *playback* duration for an authored animation; this is how long the
/// mechanism takes on the vehicle, and the two are independent by design. The
/// viz seeks the clip by progress fraction, not by time, so they cannot drift.
pub const DEPLOY_DURATION_S: f32 = 6.0;

/// Sun direction in inertial coordinates. Unit, and constant: over the minutes
/// this runs for, the sun does not move enough to matter.
pub const SUN_INERTIAL: Vec3 = Vec3::new(1.0, 0.0, 0.0);

/// Attitude error below which a slew counts as complete, radians (≈0.6°).
const SLEW_COMPLETE_RAD: f32 = 0.01;

/// Seconds to hold a pointing target before the sequencer picks the next one.
const SURVEY_DWELL_S: f32 = 6.0;

// -------------------------------------------------------------- commands ---

/// What the ground can ask the vehicle to do.
///
/// Each maps to a function code on `RUST_APP`'s command message; see
/// `cfs_msg::rust_app`. Kept as an enum here so the dynamics can be driven
/// identically from a socket, from `tools/fake-cfs`, and from a test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Drop to `Safe`: no pointing, damp the rates, stop the survey.
    Safe,
    /// Resume attitude control and the survey sequence.
    Nominal,
    /// Slew immediately to the next survey target, whatever the dwell timer
    /// says, and resume automatic sequencing if [`Command::Hold`] stopped it.
    NextTarget,
    /// Inertial hold: freeze on the current attitude and stop the sequencer.
    ///
    /// Distinct from [`Command::Safe`], and the difference is the point —
    /// `Hold` keeps the attitude controller running and stays exactly where it
    /// is, while `Safe` gives up pointing entirely and only damps the rates. On
    /// screen one is motionless and one drifts.
    Hold,
    /// Run the deployment. Ignored if it has already run.
    Deploy,
    /// Return the arrays to stowed, so the sequence can be watched again.
    Stow,
    /// Dump wheel momentum to zero — what a magnetorquer would do over minutes,
    /// compressed into an instant because the alternative is a viz that has to
    /// be left running for an hour to show recovery.
    DumpMomentum,
}

// ----------------------------------------------------------------- state ---

/// The vehicle. Step it; read [`Vehicle::state`] for what to publish.
#[derive(Clone, Copy, Debug)]
pub struct Vehicle {
    /// Body-to-inertial attitude.
    pub attitude: Quat,
    /// Body rate, rad/s.
    pub rate: Vec3,
    pub wheels: Wheels,
    /// Commanded attitude the controller is driving toward.
    pub target: Quat,
    pub mode: Mode,
    /// 0 stowed, 1 deployed.
    pub deploy_progress: f32,
    /// Solar-array yoke angle, degrees, from sun tracking.
    pub array_deg: f32,
    /// True while any wheel is at its momentum limit.
    pub saturated: bool,
    /// Whether the sequencer picks new targets on its own. Cleared by
    /// [`Command::Hold`].
    survey_active: bool,
    /// Seconds since the current survey target was commanded.
    dwell_s: f32,
    /// Index into [`SURVEY`].
    survey_index: usize,
    /// Pointing targets commanded since startup.
    ///
    /// Monotonic, unlike [`Vehicle::survey_index`], which wraps — and the
    /// difference is not academic: a test that compared indices across a long
    /// run once passed a stuck sequencer because six advances had brought the
    /// index back to where it started. Counters that only go up are the ones
    /// worth putting on a downlink.
    targets_commanded: u32,
    /// Direction the deployment is running, `+1`, `-1` or `0`.
    deploy_dir: f32,
    /// Whether the post-detumble automatic deployment has already run.
    auto_deploy_done: bool,
    /// Seconds since [`Vehicle::new`], for the sequencer only.
    elapsed_s: f32,
}

/// Pointing targets the sequencer walks through, as (axis, degrees).
///
/// A tour rather than a random walk: reproducible, so a screenshot taken at
/// `t = 12 s` is the same frame every time, which is what makes the captures in
/// `docs/findings/images/` regenerable.
const SURVEY: [(Vec3, f32); 6] = [
    (Vec3::new(0.0, 0.0, 1.0), 0.0),
    (Vec3::new(0.0, 1.0, 0.0), 55.0),
    (Vec3::new(1.0, 0.2, 0.0), -70.0),
    (Vec3::new(0.0, 0.0, 1.0), 120.0),
    (Vec3::new(0.3, -1.0, 0.4), 95.0),
    (Vec3::new(-1.0, 0.0, 0.5), 40.0),
];

impl Default for Vehicle {
    fn default() -> Self {
        Self::new()
    }
}

impl Vehicle {
    pub fn new() -> Self {
        Self {
            attitude: Quat::IDENTITY,
            // An initial tumble, so `Safe` mode has something to damp and the
            // first thing a viewer sees is the controller doing work. Sized to
            // carry less momentum than the wheels can absorb — a tumble the
            // wheels cannot catch is a real failure mode, but not the one this
            // should open with every time.
            rate: Vec3::new(0.15, -0.22, 0.12),
            wheels: Wheels::default(),
            target: Quat::IDENTITY,
            mode: Mode::Safe,
            deploy_progress: 0.0,
            array_deg: 0.0,
            saturated: false,
            survey_active: true,
            dwell_s: 0.0,
            survey_index: 0,
            targets_commanded: 0,
            deploy_dir: 0.0,
            auto_deploy_done: false,
            elapsed_s: 0.0,
        }
    }

    /// Apply a ground command.
    pub fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Safe => {
                self.mode = Mode::Safe;
                self.deploy_dir = 0.0;
            }
            Command::Nominal => {
                if self.mode == Mode::Safe {
                    self.mode = Mode::Nominal;
                    // Hold where we are rather than snapping back to the survey:
                    // commanding a mode should not also command a slew.
                    self.target = self.attitude;
                    self.dwell_s = SURVEY_DWELL_S;
                }
            }
            Command::NextTarget => {
                if self.mode != Mode::Safe {
                    self.survey_active = true;
                    self.advance_survey();
                }
            }
            Command::Hold => {
                if self.mode != Mode::Safe {
                    self.survey_active = false;
                    self.target = self.attitude;
                    self.dwell_s = 0.0;
                }
            }
            Command::Deploy => {
                if self.deploy_progress < 1.0 && self.mode != Mode::Safe {
                    self.deploy_dir = 1.0;
                    self.mode = Mode::Deploying;
                }
            }
            Command::Stow => {
                if self.deploy_progress > 0.0 && self.mode != Mode::Safe {
                    self.deploy_dir = -1.0;
                    self.mode = Mode::Deploying;
                }
            }
            Command::DumpMomentum => self.wheels = Wheels::default(),
        }
    }

    fn advance_survey(&mut self) {
        self.survey_index = (self.survey_index + 1) % SURVEY.len();
        self.targets_commanded = self.targets_commanded.wrapping_add(1);
        let (axis, deg) = SURVEY[self.survey_index];
        self.target = Quat::from_axis_angle(axis, deg.to_radians());
        self.dwell_s = 0.0;
    }

    /// Attitude error as a body-frame rotation vector, radians.
    ///
    /// `q_err = target⁻¹ ∘ attitude`, canonicalized so the controller always
    /// takes the short way round. The vector part of a unit quaternion is
    /// `sin(θ/2)·axis`, so doubling it is the small-angle rotation vector and
    /// stays well-behaved out to large errors — it just softens, which is a
    /// desirable gain schedule for a slew rather than a defect.
    fn attitude_error(&self) -> Vec3 {
        self.target.conjugate().mul(self.attitude).canonical().vector() * 2.0
    }

    /// Angular momentum of the whole vehicle in body coordinates — the body
    /// plus what the wheels are storing.
    fn total_momentum(&self) -> Vec3 {
        INERTIA.mul_elem(self.rate) + self.wheels.total_momentum()
    }

    /// Torque the controller wants this step, before the wheels get a say.
    ///
    /// The `+ gyroscopic` term is feed-forward cancellation of the `ω × h`
    /// coupling that [`Vehicle::step`] then applies, and it is not optional
    /// polish: with wheels this size, `ω × h` during a fast slew is comparable
    /// to [`MAX_TORQUE`] itself, so a controller that ignores it spends most of
    /// its authority fighting its own wheels and never converges. Cancelling a
    /// known nonlinearity before closing a linear loop around what is left is
    /// how real attitude controllers are built, and it is why the PD gains
    /// above can be chosen from `INERTIA` alone.
    ///
    /// The cancellation is inside the clamp, deliberately. Putting it outside
    /// would let the commanded torque exceed what the wheels can deliver and
    /// quietly turn the actuator limit into a fiction.
    fn control_torque(&self) -> Vec3 {
        let feedback = match self.mode {
            // No attitude reference in Safe — just take the energy out.
            Mode::Safe => self.rate * -SAFE_KD,
            _ => self.attitude_error() * -KP - self.rate * KD,
        };
        let gyroscopic = self.rate.cross(self.total_momentum());
        clamp_torque(feedback + gyroscopic, MAX_TORQUE)
    }

    /// Advance the vehicle by `dt` seconds.
    ///
    /// One fixed step, called at whatever rate the host runs. The caller is
    /// responsible for `dt` being small and roughly constant; the cFE app calls
    /// this on its own timed loop and `fake-cfs` on its generator tick.
    pub fn step(&mut self, dt: f32) {
        if dt <= 0.0 {
            return;
        }
        self.elapsed_s += dt;

        let request = self.control_torque();
        let applied = self.wheels.apply(request, dt);
        self.saturated = applied.saturated;

        // Euler's equation with the wheel momentum included. Without the wheel
        // term the body would gain angular momentum out of nothing every time a
        // wheel spun up, which is precisely the physics this crate exists to
        // not fake.
        let gyroscopic = self.rate.cross(self.total_momentum());
        let accel = (applied.body_torque - gyroscopic).div_elem(INERTIA);

        self.rate = self.rate + accel * dt;
        self.attitude = self.attitude.integrate(self.rate, dt);

        self.step_modes(dt);
        self.array_deg = self.sun_tracking_deg();
    }

    /// Mode transitions and the survey sequencer.
    fn step_modes(&mut self, dt: f32) {
        // Deployment runs to completion in either direction and owns the mode
        // while it does.
        if self.deploy_dir != 0.0 {
            self.deploy_progress =
                (self.deploy_progress + self.deploy_dir * dt / DEPLOY_DURATION_S).clamp(0.0, 1.0);
            if self.deploy_progress >= 1.0 {
                self.deploy_dir = 0.0;
                self.mode = Mode::Deployed;
            } else if self.deploy_progress <= 0.0 {
                self.deploy_dir = 0.0;
                self.mode = Mode::Nominal;
            }
            return;
        }

        match self.mode {
            Mode::Safe => {
                // Leave Safe on its own once the tumble is damped — a vehicle
                // that needs a ground command to start working would leave the
                // viz showing nothing until someone pressed a key.
                if self.rate.length() < DETUMBLE_SETTLED {
                    self.mode = Mode::Nominal;
                    self.target = self.attitude;
                    self.survey_active = true;
                    // Start the first slew immediately rather than dwelling on
                    // the arbitrary attitude the tumble happened to end at.
                    self.dwell_s = SURVEY_DWELL_S;

                    // Deploy the arrays once the tumble is under control, which
                    // is the order a real vehicle does it in — deploying while
                    // still tumbling would fling the panels around on their
                    // hinges. Doing it automatically also means a visualizer
                    // that connects to a long-running cFS finds a deployed
                    // spacecraft rather than a folded one waiting to be asked.
                    // `Stow` can still be commanded afterwards, and this does
                    // not fire again.
                    if !self.auto_deploy_done {
                        self.auto_deploy_done = true;
                        self.deploy_dir = 1.0;
                        self.mode = Mode::Deploying;
                    }
                }
            }
            Mode::Nominal | Mode::Deployed => {
                self.dwell_s += dt;
                let settled = self.attitude_error().length() < SLEW_COMPLETE_RAD;
                if self.survey_active && settled && self.dwell_s >= SURVEY_DWELL_S {
                    self.advance_survey();
                }
            }
            // Only reachable while `deploy_dir` is non-zero, handled above.
            Mode::Deploying => self.mode = Mode::Nominal,
        }
    }

    /// Solar-array yoke angle that points the panel at the sun, degrees.
    ///
    /// The yoke is a single hinge about body X (see `apps/viz`'s `solar_array`
    /// driver), so it has one degree of freedom and cannot generally point the
    /// panel exactly at the sun — it can only do the best available rotation.
    /// That is true of real single-axis drives too, and it is why the array
    /// angle moves whenever the *attitude* moves: the tracking is a consequence
    /// of the vehicle's pointing, not an independent signal.
    fn sun_tracking_deg(&self) -> f32 {
        let sun_body = self.attitude.inverse_rotate(SUN_INERTIAL);
        // Panel normal at yoke angle θ is (0, -sin θ, cos θ); the θ maximizing
        // the dot product with the sun is atan2(-y, z).
        let deg = math::atan2(-sun_body.y, sun_body.z).to_degrees();
        if deg < 0.0 { deg + 360.0 } else { deg }
    }

    /// Pointing targets commanded since startup. Monotonic.
    pub fn targets_commanded(&self) -> u32 {
        self.targets_commanded
    }

    /// Whether the sequencer is picking its own targets.
    pub fn survey_active(&self) -> bool {
        self.survey_active
    }

    /// Attitude error the controller is currently working against, radians.
    /// Published so the ground can see a slew in progress as a number, not just
    /// as motion.
    pub fn pointing_error_rad(&self) -> f32 {
        self.target.conjugate().mul(self.attitude).angle()
    }

    /// Seconds since construction. Only the sequencer and tests use it.
    pub fn elapsed_s(&self) -> f32 {
        self.elapsed_s
    }

    /// Body rate in rad/s as an array, for the wire format.
    pub fn rate_array(&self) -> [f32; 3] {
        self.rate.to_array()
    }

    /// The vehicle as the shared ground/flight state type.
    ///
    /// This is the conversion that makes the whole arrangement work: the flight
    /// app fills a [`SpacecraftState`], encodes it with
    /// `telemetry_model::encode_vehicle_state`, and the visualizer decodes the
    /// same struct with the same code. There is no second definition of the
    /// layout to drift out of sync with this one.
    pub fn state(&self) -> SpacecraftState {
        SpacecraftState {
            attitude: self.attitude.into(),
            solar_array_deg: self.array_deg,
            deploy_progress: self.deploy_progress,
            wheel_rpm: self.wheels.rpm(),
            mode: self.mode,
        }
    }
}

/// Scale a torque vector down to `limit` without changing its direction.
///
/// Clamping each axis separately would be simpler and wrong: it changes the
/// torque's *direction*, so a saturated three-axis slew would curve away from
/// the shortest path for reasons no telemetry would explain.
fn clamp_torque(t: Vec3, limit: f32) -> Vec3 {
    let n = t.length();
    if n > limit && n > 0.0 { t * (limit / n) } else { t }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `seconds` of simulated time at a realistic step.
    fn run(v: &mut Vehicle, seconds: f32) {
        let dt = 0.02;
        for _ in 0..((seconds / dt) as usize) {
            v.step(dt);
        }
    }

    /// Command a slew of `angle` radians about a body axis, *relative to where
    /// the vehicle is now*.
    ///
    /// Tests want "slew by this much"; an absolute target makes the size of the
    /// slew depend on wherever the sequencer happened to leave the vehicle,
    /// which is how two of these tests started failing for reasons that had
    /// nothing to do with what they were testing.
    fn slew_by(v: &mut Vehicle, axis: Vec3, angle: f32) {
        v.target = v.attitude.mul(Quat::from_axis_angle(axis, angle));
    }

    /// Step until the vehicle leaves `Safe`, then hold it there. Most tests
    /// want a settled, non-sequencing vehicle as their starting point.
    fn settled() -> Vehicle {
        let mut v = Vehicle::new();
        run(&mut v, 60.0);
        assert_eq!(v.mode, Mode::Deployed, "detumble and deployment did not finish in 60 s");
        v.command(Command::Hold);
        run(&mut v, 20.0);
        assert!(v.pointing_error_rad() < 0.01, "hold never settled");
        v
    }

    /// The opening sequence a viewer actually sees: tumble, detumble, deploy.
    /// Asserted because it is what makes the default view worth looking at, and
    /// because "the arrays never came out" is easy to miss in a test suite that
    /// only ever commands deployment explicitly.
    #[test]
    fn the_arrays_deploy_themselves_once_the_tumble_is_damped() {
        let mut v = Vehicle::new();
        assert_eq!(v.deploy_progress, 0.0);
        run(&mut v, 60.0);
        assert_eq!(v.mode, Mode::Deployed);
        assert!((v.deploy_progress - 1.0).abs() < 1e-6);
    }

    /// ...and only once, or a commanded stow would be undone by the next pass
    /// through the mode machine.
    #[test]
    fn the_automatic_deployment_does_not_fire_twice() {
        let mut v = Vehicle::new();
        run(&mut v, 60.0);
        v.command(Command::Stow);
        run(&mut v, DEPLOY_DURATION_S + 2.0);
        assert_eq!(v.deploy_progress, 0.0);

        v.command(Command::Safe);
        run(&mut v, 60.0);
        assert_eq!(v.deploy_progress, 0.0, "safe/nominal cycle redeployed the arrays");
    }

    #[test]
    fn safe_mode_damps_the_initial_tumble_and_hands_over() {
        let mut v = Vehicle::new();
        assert_eq!(v.mode, Mode::Safe);
        let initial = v.rate.length();
        assert!(initial > 0.1, "nothing to damp");

        // The rate at the moment Safe hands over, not at the end of the run:
        // once the sequencer takes charge it starts slewing, and the body rate
        // goes back up for entirely correct reasons.
        let mut handover = None;
        for _ in 0..4_000 {
            v.step(0.02);
            if v.mode != Mode::Safe {
                handover = Some((v.elapsed_s(), v.rate.length()));
                break;
            }
        }
        let (t, rate) = handover.expect("never left Safe");
        assert!(rate < DETUMBLE_SETTLED, "handed over still tumbling at {rate} rad/s");
        assert!(t < 30.0, "detumble took {t} s");
        // Straight into the automatic deployment.
        assert_eq!(v.mode, Mode::Deploying);
    }

    #[test]
    fn a_commanded_slew_converges_and_stays() {
        let mut v = settled();
        slew_by(&mut v, Vec3::new(0.0, 0.0, 1.0), 1.0);

        run(&mut v, 25.0);
        assert!(v.pointing_error_rad() < 0.02, "slew left {} rad", v.pointing_error_rad());

        // And it holds there rather than drifting off once the error is gone.
        run(&mut v, 10.0);
        assert!(v.pointing_error_rad() < 0.02, "drifted to {} rad", v.pointing_error_rad());
    }

    /// A slew should look like a slew: accelerate, coast, decelerate. A pure PD
    /// loop would instead peak instantly and decay, which is both wrong for a
    /// torque-limited vehicle and far less legible on screen.
    #[test]
    fn a_large_slew_is_torque_limited_at_both_ends() {
        let mut v = settled();
        slew_by(&mut v, Vec3::new(0.0, 0.0, 1.0), 2.0);

        let mut peak_rate = 0.0f32;
        let mut samples = 0;
        for _ in 0..1500 {
            v.step(0.02);
            let r = v.rate.length();
            peak_rate = peak_rate.max(r);
            if r > 0.05 {
                samples += 1;
            }
        }
        assert!(peak_rate > 0.1, "slew never got moving: peak {peak_rate} rad/s");
        assert!(samples > 100, "slew was over too fast to be torque-limited");
        assert!(v.pointing_error_rad() < 0.02, "left {} rad", v.pointing_error_rad());
    }

    /// The property that separates this from writing a quaternion directly:
    /// angular momentum only moves between the body and the wheels, it is never
    /// created. Checked in the inertial frame, where it is conserved.
    #[test]
    fn total_angular_momentum_is_conserved() {
        let mut v = Vehicle::new();
        let momentum = |v: &Vehicle| {
            v.attitude.rotate(INERTIA.mul_elem(v.rate) + v.wheels.total_momentum())
        };
        let before = momentum(&v);
        run(&mut v, 30.0);
        let after = momentum(&v);
        let drift = (after - before).length();
        assert!(
            drift < 0.02 * before.length().max(1e-3),
            "momentum drifted by {drift} from {:?}",
            before
        );
    }

    #[test]
    fn deploy_runs_to_completion_and_stows_again() {
        let mut v = settled();
        // `settled()` already deployed; stow first so there is something to do.
        v.command(Command::Stow);
        run(&mut v, DEPLOY_DURATION_S + 1.0);
        assert_eq!(v.mode, Mode::Nominal);
        assert!(v.deploy_progress.abs() < 1e-6);

        v.command(Command::Deploy);
        assert_eq!(v.mode, Mode::Deploying);
        run(&mut v, DEPLOY_DURATION_S + 1.0);
        assert_eq!(v.mode, Mode::Deployed);
        assert!((v.deploy_progress - 1.0).abs() < 1e-6);
    }

    /// Deploy is a mechanism, not a pointing mode: it must not be startable
    /// while the vehicle is still tumbling in Safe.
    #[test]
    fn deploy_is_refused_in_safe() {
        let mut v = Vehicle::new();
        v.command(Command::Deploy);
        assert_eq!(v.mode, Mode::Safe);
        assert_eq!(v.deploy_progress, 0.0);
    }

    /// The array angle must be a consequence of attitude, not a free-running
    /// ramp — otherwise it would keep sweeping with the vehicle held still,
    /// which is exactly the "plausible-looking lie" the viz must not tell.
    #[test]
    fn array_tracking_follows_attitude_and_holds_when_attitude_does() {
        let mut v = settled();
        let held = v.array_deg;
        run(&mut v, 4.0);
        assert!(
            vec3::close(v.array_deg, held, 0.5),
            "array moved {} deg with the vehicle on inertial hold",
            v.array_deg - held
        );

        // A roll about body X is exactly what a single-axis yoke has to answer.
        slew_by(&mut v, Vec3::new(1.0, 0.0, 0.0), 1.2);
        run(&mut v, 25.0);
        assert!(
            (v.array_deg - held).abs() > 30.0,
            "array did not track a 69-degree roll: {held} -> {}",
            v.array_deg
        );
    }

    /// The single-axis yoke cannot point at the sun from every attitude, and
    /// claiming otherwise would be the interesting kind of lie. This pins the
    /// achievable geometry instead: the yoke always finds the *best* angle
    /// available about body X.
    #[test]
    fn array_finds_the_best_angle_its_one_hinge_allows() {
        let mut v = settled();
        for (axis, deg) in [
            (Vec3::new(1.0, 0.0, 0.0), 40.0f32),
            (Vec3::new(0.0, 1.0, 0.0), -35.0),
            (Vec3::new(0.2, 0.7, 0.3), 80.0),
        ] {
            slew_by(&mut v, axis, deg.to_radians());
            run(&mut v, 30.0);

            let sun_body = v.attitude.inverse_rotate(SUN_INERTIAL);
            let theta = v.array_deg.to_radians();
            let normal = Vec3::new(0.0, -math::sin(theta), math::cos(theta));
            let best = normal.dot(sun_body);
            // No other yoke angle does better.
            for probe_deg in (0..360).step_by(3) {
                let p = (probe_deg as f32).to_radians();
                let n = Vec3::new(0.0, -math::sin(p), math::cos(p));
                assert!(
                    n.dot(sun_body) <= best + 1e-3,
                    "yoke at {} deg beaten by {probe_deg} deg",
                    v.array_deg
                );
            }
        }
    }

    /// Determinism is what makes the committed screenshots regenerable.
    #[test]
    fn stepping_is_deterministic() {
        let (mut a, mut b) = (Vehicle::new(), Vehicle::new());
        run(&mut a, 60.0);
        run(&mut b, 60.0);
        assert_eq!(a.state(), b.state());
    }

    /// Without this the vehicle would sit still until someone sent a command,
    /// and the whole point is that the flight side is doing something on its own.
    #[test]
    fn the_survey_advances_on_its_own() {
        let mut v = Vehicle::new();
        run(&mut v, 60.0);
        let first = v.targets_commanded();
        run(&mut v, 90.0);
        assert!(v.targets_commanded() > first, "sequencer never advanced");
        assert!(v.survey_active());
    }

    #[test]
    fn hold_stops_the_sequencer_and_next_target_restarts_it() {
        let mut v = settled();
        assert!(!v.survey_active());
        let commanded = v.targets_commanded();
        run(&mut v, 60.0);
        assert_eq!(v.targets_commanded(), commanded, "held vehicle picked a new target");

        v.command(Command::NextTarget);
        assert!(v.survey_active());
        assert_eq!(v.targets_commanded(), commanded + 1);
    }

    /// `Hold` and `Safe` must not be the same thing: one points, one does not.
    /// On screen that is the difference between a motionless vehicle and one
    /// that coasts to a stop wherever the disturbance left it.
    #[test]
    fn safe_gives_up_pointing_where_hold_keeps_it() {
        let mut held = settled();
        let mut safed = settled();
        // Safe mode ends by handing back to the sequencer; the automatic
        // deployment must not be part of what it hands back (covered by
        // `the_automatic_deployment_does_not_fire_twice`).
        // The same disturbance to each.
        held.rate = Vec3::new(0.0, 0.0, 0.08);
        safed.rate = Vec3::new(0.0, 0.0, 0.08);
        let start = safed.attitude;
        safed.command(Command::Safe);

        run(&mut held, 30.0);
        // Only until Safe hands back over — after that the sequencer is flying.
        for _ in 0..1_500 {
            safed.step(0.02);
            if safed.mode != Mode::Safe {
                break;
            }
        }

        assert!(held.pointing_error_rad() < 0.02, "hold drifted {}", held.pointing_error_rad());
        let moved = start.conjugate().mul(safed.attitude).angle();
        assert!(moved > 0.05, "safe mode restored the attitude it was told to abandon");
        assert!(safed.rate.length() < DETUMBLE_SETTLED, "safe mode did not damp");
    }

    /// A long survey builds momentum the wheels cannot hold forever; the
    /// vehicle must still be recoverable when it does.
    #[test]
    fn momentum_can_be_dumped() {
        let mut v = settled();
        v.wheels = Wheels { momentum: [wheels::WHEEL_MOMENTUM_LIMIT; WHEEL_COUNT] };
        assert!((v.wheels.saturation() - 1.0).abs() < 1e-6);
        v.command(Command::DumpMomentum);
        assert_eq!(v.wheels.saturation(), 0.0);
    }

    #[test]
    fn torque_clamping_preserves_direction() {
        let t = Vec3::new(3.0, -4.0, 12.0);
        let c = clamp_torque(t, MAX_TORQUE);
        assert!(vec3::close(c.length(), MAX_TORQUE, 1e-6));
        let cos = c.normalize_or_zero().dot(t.normalize_or_zero());
        assert!(vec3::close(cos, 1.0, 1e-6), "direction changed, cos = {cos}");
    }
}
