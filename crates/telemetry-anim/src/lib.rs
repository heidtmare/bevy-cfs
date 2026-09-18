//! Telemetry-to-animation mapping maths — the part of Phase 3 that is arithmetic
//! rather than Bevy.
//!
//! Phase 3 asks which of three mechanisms should drive an animated scene from
//! telemetry: writing transforms directly, seeking an authored clip, or blending
//! an animation graph. Everything here is the half of that question that can be
//! answered without a GPU, a window, or an `App`, and it is kept in its own
//! crate for the same reason the jitter buffer is: logic that can be tested as
//! plain functions should be.
//!
//! The split against [`telemetry_model`] is that it describes *what the vehicle
//! is doing* and this crate describes *how to present it*. Nothing here decides
//! what a value means; it only decides how that value reaches the screen.

#![no_std]
#![forbid(unsafe_code)]

// The crate is no_std; tests still want `println!` to report measurements.
#[cfg(test)]
extern crate std;

pub use telemetry_model::{Freshness, Mode, SpacecraftState};

/// Constants describing the rig, shared with `tools/gltf-gen`.
///
/// The asset generator imports these rather than repeating them, so the model
/// and the direct-drive kinematics cannot disagree about where the hinges are
/// or how far they travel. That sharing is deliberate and is itself part of the
/// finding: see [`deploy_angles_at`].
pub mod rig {
    /// Inner panel hinge angle when stowed, degrees about the joint's local Y.
    pub const PANEL1_STOWED_DEG: f32 = -150.0;
    /// Outer panel hinge angle when stowed (folded back onto the inner panel).
    pub const PANEL2_STOWED_DEG: f32 = 170.0;

    /// Length of the authored `Deploy` clip, seconds.
    pub const DEPLOY_DURATION_S: f32 = 2.0;

    /// When each hinge moves. The windows overlap: the outer panel starts
    /// unfolding at 0.8 s while the inner one is still swinging out, and that
    /// staging is the artistic content a mapping either inherits or reinvents.
    pub const PANEL1_WINDOW_S: (f32, f32) = (0.0, 1.2);
    pub const PANEL2_WINDOW_S: (f32, f32) = (0.8, 2.0);

    /// Keyframe knots per hinge in the generated clip.
    ///
    /// Five is few on purpose: a sparse clip sampled with glTF `LINEAR`
    /// interpolation is what a real exported asset looks like, and the gap
    /// between it and a continuously evaluated curve is exactly what
    /// `clip_vs_direct_divergence` measures.
    pub const KNOTS: usize = 5;
}

/// Hermite smoothstep on `0..=1`. Clamped, so callers need not pre-clamp.
pub fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Fraction of a window `(start, end)` elapsed at time `t`, clamped to `0..=1`.
fn window_fraction(t: f32, (start, end): (f32, f32)) -> f32 {
    if end <= start {
        return if t >= end { 1.0 } else { 0.0 };
    }
    ((t - start) / (end - start)).clamp(0.0, 1.0)
}

/// Hinge angles (inner, outer) in degrees at time `t` along the deploy timeline.
///
/// This is the *continuous* definition of the motion. `tools/gltf-gen` samples
/// it at [`rig::KNOTS`] points per hinge to bake the `Deploy` clip, so the
/// asset and this function share one source of truth.
///
/// They still do not agree everywhere. The clip is interpolated linearly
/// between knots while this evaluates the real curve, so the two mappings
/// disagree between keyframes by a bounded amount — quantified by the
/// `clip_vs_direct_divergence` test. That residue is the honest version of the
/// duplication cost: sharing constants removes the *coarse* drift, and what
/// survives is the curve shape itself, which no shared constant can capture.
pub fn deploy_angles_at(t: f32) -> (f32, f32) {
    let inner = rig::PANEL1_STOWED_DEG * (1.0 - smoothstep(window_fraction(t, rig::PANEL1_WINDOW_S)));
    let outer = rig::PANEL2_STOWED_DEG * (1.0 - smoothstep(window_fraction(t, rig::PANEL2_WINDOW_S)));
    (inner, outer)
}

/// Hinge angles for a normalized deployment parameter, `0..=1`.
///
/// What the direct-drive mapping calls. Note what it has to know to do so: the
/// clip duration, both hinge windows, the easing function, and the stowed
/// angles. A telemetry consumer has no business knowing any of that, and that
/// is the argument against direct drive for authored mechanisms.
pub fn deploy_angles(progress: f32) -> (f32, f32) {
    deploy_angles_at(progress.clamp(0.0, 1.0) * rig::DEPLOY_DURATION_S)
}

/// Where to seek an authored clip for a normalized parameter.
///
/// Clamped, never wrapped and never run past the end. The jitter buffer already
/// refuses to extrapolate telemetry; seeking past a clip's last keyframe would
/// reintroduce invented motion one layer higher up, where it would look like
/// the mechanism kept moving after the data stopped.
pub fn clip_seek_time(progress: f32, duration_s: f32) -> f32 {
    progress.clamp(0.0, 1.0) * duration_s.max(0.0)
}

// ------------------------------------------------------------------ modes ---

/// Modes in graph order. The animation graph's clip nodes are built in this
/// order, so the index of a mode is also the index of its weight and its clip.
pub const MODE_COUNT: usize = 4;

/// Clip names in the generated asset, indexed by [`mode_index`].
pub const MODE_CLIPS: [&str; MODE_COUNT] =
    ["ModeSafe", "ModeNominal", "ModeDeploying", "ModeDeployed"];

pub fn mode_index(mode: Mode) -> usize {
    match mode {
        Mode::Safe => 0,
        Mode::Nominal => 1,
        Mode::Deploying => 2,
        Mode::Deployed => 3,
    }
}

/// The pose each mode corresponds to: antenna elevation in degrees, and a scale
/// multiplier on the fault lamp.
///
/// Shared with `tools/gltf-gen`, which bakes these same values into the
/// `ModeSafe`/`ModeNominal`/... clips. The columns that snap compute the pose
/// here; the column that blends reads it out of the asset. Both therefore agree
/// at rest and differ only during a transition, which is precisely the property
/// under test.
pub fn mode_pose(mode: Mode) -> (f32, f32) {
    match mode {
        Mode::Safe => (-70.0, 1.8),
        Mode::Nominal => (0.0, 1.0),
        Mode::Deploying => (-20.0, 1.4),
        Mode::Deployed => (15.0, 1.0),
    }
}

/// Cross-fading weights over the discrete modes, for `AnimationGraph` blending.
///
/// `SpacecraftState::lerp` snaps `mode` at the midpoint rather than
/// interpolating it, because a blended enum would name a mode the vehicle was
/// never in. That is correct for *state* and wrong for *presentation*: an
/// instant pose change reads as a glitch. So the smoothing lives here, one
/// layer up, where it is honest — the reported mode still steps, and only the
/// pose eases across.
///
/// Weights are kept normalized so the blended pose is always a true convex
/// combination of authored poses. If they summed to more than one the result
/// would overshoot every pose the artist approved.
#[derive(Debug, Clone, Copy)]
pub struct ModeBlend {
    weights: [f32; MODE_COUNT],
    target: Mode,
    /// Seconds for a fade to complete, if uninterrupted.
    transition_s: f32,
}

impl ModeBlend {
    pub fn new(initial: Mode, transition_s: f32) -> Self {
        let mut weights = [0.0; MODE_COUNT];
        weights[mode_index(initial)] = 1.0;
        Self { weights, target: initial, transition_s: transition_s.max(f32::EPSILON) }
    }

    pub fn set_target(&mut self, mode: Mode) {
        self.target = mode;
    }

    pub fn target(&self) -> Mode {
        self.target
    }

    pub fn weights(&self) -> &[f32; MODE_COUNT] {
        &self.weights
    }

    /// True once the target holds effectively all the weight.
    pub fn is_settled(&self) -> bool {
        self.weights[mode_index(self.target)] > 0.999
    }

    /// Advance the fade by `dt` seconds.
    ///
    /// The target gains weight at a fixed rate and the others are scaled down
    /// to absorb exactly that much. Scaling rather than subtracting a fixed
    /// step is what makes an interrupted fade behave: if the mode changes twice
    /// in quick succession, the outgoing weights shrink proportionally instead
    /// of one of them going negative and having to be clamped.
    pub fn advance(&mut self, dt: f32) {
        if dt <= 0.0 {
            return;
        }
        let ti = mode_index(self.target);
        let step = dt / self.transition_s;
        let gained = (self.weights[ti] + step).min(1.0);
        self.weights[ti] = gained;

        let rest: f32 = self
            .weights
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != ti)
            .map(|(_, w)| *w)
            .sum();
        if rest > 0.0 {
            let scale = (1.0 - gained) / rest;
            for (i, w) in self.weights.iter_mut().enumerate() {
                if i != ti {
                    *w *= scale;
                }
            }
        }
    }
}

// ---------------------------------------------------- non-transform output ---

/// How to present the fault lamp this frame.
///
/// Neither field is a `Transform`, which is the point: most spacecraft
/// telemetry is not rigid-body motion, and a mapping that only knows how to
/// move things cannot show a heater duty cycle or a caution annunciator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lamp {
    pub visible: bool,
    /// Multiplier on the material's authored `emissiveFactor`, `0..=1`.
    pub emissive_gain: f32,
}

/// Blink phase as a square wave: `true` for the first half of each period.
fn blink(phase_s: f32, hz: f32) -> bool {
    let cycles = phase_s * hz;
    (cycles - libm_floorf(cycles)) < 0.5
}

/// `floorf` without pulling in a math crate. Inputs here are small and
/// non-negative, so the truncate-and-adjust form is exact.
fn libm_floorf(x: f32) -> f32 {
    let t = x as i32 as f32;
    if t > x { t - 1.0 } else { t }
}

/// Lamp state for a mode and link condition.
///
/// Link health outranks vehicle mode. If the data is stale the lamp reports
/// *that*, because a caution light driven by a frozen value is worse than no
/// caution light: it asserts a condition the ground no longer knows.
pub fn lamp(mode: Mode, freshness: Freshness, phase_s: f32) -> Lamp {
    match freshness {
        Freshness::NoData | Freshness::Stale { .. } => {
            Lamp { visible: blink(phase_s, 1.0), emissive_gain: 0.35 }
        }
        _ => match mode {
            Mode::Safe => Lamp { visible: blink(phase_s, 2.0), emissive_gain: 1.0 },
            Mode::Deploying => Lamp { visible: true, emissive_gain: 0.7 },
            _ => Lamp { visible: true, emissive_gain: 0.12 },
        },
    }
}

/// Clamped normalization, for driving a material channel from an engineering
/// unit (wheel RPM, heater current, plume thrust).
pub fn normalized(value: f32, lo: f32, hi: f32) -> f32 {
    // No `f32::abs` here: it is std-only, and this crate must build no_std.
    let span = hi - lo;
    if span > -f32::EPSILON && span < f32::EPSILON {
        return 0.0;
    }
    ((value - lo) / span).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linear interpolation between the baked keyframes, i.e. what the glTF
    /// runtime actually evaluates. Mirrors `gltf-gen`'s knot placement.
    pub(super) fn clip_sample_pub(t: f32, w: (f32, f32), s: f32) -> f32 {
        clip_sample(t, w, s)
    }

    fn clip_sample(t: f32, window: (f32, f32), stowed_deg: f32) -> f32 {
        let (start, end) = window;
        let n = rig::KNOTS;
        let times: [f32; 5] =
            core::array::from_fn(|i| start + (end - start) * i as f32 / (n - 1) as f32);
        let vals: [f32; 5] = core::array::from_fn(|i| {
            stowed_deg * (1.0 - smoothstep(i as f32 / (n - 1) as f32))
        });
        if t <= times[0] {
            return vals[0];
        }
        if t >= times[n - 1] {
            return vals[n - 1];
        }
        for i in 0..n - 1 {
            if t <= times[i + 1] {
                let f = (t - times[i]) / (times[i + 1] - times[i]);
                return vals[i] + (vals[i + 1] - vals[i]) * f;
            }
        }
        vals[n - 1]
    }

    #[test]
    fn deploy_endpoints_are_exactly_stowed_and_deployed() {
        let (i0, o0) = deploy_angles(0.0);
        assert_eq!(i0, rig::PANEL1_STOWED_DEG);
        assert_eq!(o0, rig::PANEL2_STOWED_DEG);
        let (i1, o1) = deploy_angles(1.0);
        assert_eq!(i1, 0.0);
        assert_eq!(o1, 0.0);
    }

    #[test]
    fn deploy_is_staged_not_simultaneous() {
        // At the moment the outer hinge is released, the inner one is already
        // well along. If this ever reads 0 the panels sweep through each other.
        let (inner, outer) = deploy_angles_at(rig::PANEL2_WINDOW_S.0);
        let inner_travelled = 1.0 - inner / rig::PANEL1_STOWED_DEG;
        assert!(inner_travelled > 0.6, "inner only {inner_travelled} through when outer starts");
        assert_eq!(outer, rig::PANEL2_STOWED_DEG, "outer moved before its window");
    }

    #[test]
    fn deploy_is_monotonic_in_progress() {
        let (mut prev_i, mut prev_o) = (f32::NEG_INFINITY, f32::INFINITY);
        for step in 0..=200 {
            let (i, o) = deploy_angles(step as f32 / 200.0);
            assert!(i >= prev_i - 1e-4, "inner reversed at {step}");
            assert!(o <= prev_o + 1e-4, "outer reversed at {step}");
            prev_i = i;
            prev_o = o;
        }
    }

    /// The headline number for the findings write-up.
    ///
    /// Direct drive and clip-seek share every constant and still disagree,
    /// because one evaluates the curve and the other interpolates a sparse
    /// sampling of it. This pins the magnitude so the write-up quotes a
    /// measurement rather than an intuition.
    #[test]
    fn clip_vs_direct_divergence() {
        let mut worst_inner: f32 = 0.0;
        let mut worst_outer: f32 = 0.0;
        for step in 0..=2000 {
            let t = step as f32 / 2000.0 * rig::DEPLOY_DURATION_S;
            let (di, do_) = deploy_angles_at(t);
            let ci = clip_sample(t, rig::PANEL1_WINDOW_S, rig::PANEL1_STOWED_DEG);
            let co = clip_sample(t, rig::PANEL2_WINDOW_S, rig::PANEL2_STOWED_DEG);
            worst_inner = worst_inner.max((di - ci).abs());
            worst_outer = worst_outer.max((do_ - co).abs());
        }
        // Five knots over a smoothstep leaves a few degrees of chord error.
        // Non-zero is the finding; the bound stops it growing unnoticed.
        assert!(worst_inner > 0.5, "expected measurable divergence, got {worst_inner}");
        assert!(worst_inner < 8.0, "inner divergence grew to {worst_inner} deg");
        assert!(worst_outer < 9.0, "outer divergence grew to {worst_outer} deg");
    }

    #[test]
    fn seek_never_leaves_the_clip() {
        assert_eq!(clip_seek_time(-5.0, 2.0), 0.0);
        assert_eq!(clip_seek_time(0.5, 2.0), 1.0);
        assert_eq!(clip_seek_time(7.0, 2.0), 2.0, "seek ran past the last keyframe");
    }

    #[test]
    fn blend_weights_stay_a_convex_combination() {
        let mut b = ModeBlend::new(Mode::Safe, 0.4);
        b.set_target(Mode::Deploying);
        for _ in 0..200 {
            b.advance(1.0 / 60.0);
            let sum: f32 = b.weights().iter().sum();
            assert!((sum - 1.0).abs() < 1e-3, "weights sum to {sum}");
            assert!(b.weights().iter().all(|w| *w >= 0.0), "negative weight: {:?}", b.weights());
        }
    }

    #[test]
    fn blend_reaches_the_target_within_the_transition() {
        let mut b = ModeBlend::new(Mode::Safe, 0.4);
        b.set_target(Mode::Nominal);
        for _ in 0..24 {
            b.advance(1.0 / 60.0);
        }
        assert!(b.is_settled(), "not settled after 0.4s: {:?}", b.weights());
    }

    /// Mode flapping is a real telemetry condition near a threshold, and it is
    /// exactly where a naive subtract-a-step blender produces negative weights.
    #[test]
    fn rapid_mode_flapping_stays_valid() {
        let mut b = ModeBlend::new(Mode::Safe, 0.5);
        for i in 0..600 {
            b.set_target(match i % 4 {
                0 => Mode::Safe,
                1 => Mode::Nominal,
                2 => Mode::Deploying,
                _ => Mode::Deployed,
            });
            b.advance(1.0 / 60.0);
            let sum: f32 = b.weights().iter().sum();
            assert!(sum.is_finite() && (sum - 1.0).abs() < 1e-3, "sum {sum} at {i}");
            assert!(b.weights().iter().all(|w| (0.0..=1.0).contains(w)), "at {i}: {:?}", b.weights());
        }
    }

    #[test]
    fn zero_and_huge_steps_are_both_safe() {
        let mut b = ModeBlend::new(Mode::Safe, 0.5);
        b.set_target(Mode::Deployed);
        b.advance(0.0);
        assert_eq!(b.weights()[mode_index(Mode::Safe)], 1.0, "dt=0 moved the blend");
        b.advance(1e6);
        assert!(b.is_settled());
        assert!((b.weights().iter().sum::<f32>() - 1.0).abs() < 1e-3);
    }

    /// The snapping columns and the blended column must agree at rest, or the
    /// side-by-side comparison is measuring an asset bug rather than a mapping.
    #[test]
    fn every_mode_has_a_distinct_pose() {
        let poses: [(f32, f32); MODE_COUNT] =
            core::array::from_fn(|i| mode_pose(match i {
                0 => Mode::Safe,
                1 => Mode::Nominal,
                2 => Mode::Deploying,
                _ => Mode::Deployed,
            }));
        for (i, a) in poses.iter().enumerate() {
            for b in poses.iter().skip(i + 1) {
                assert_ne!(a.0, b.0, "two modes share an antenna angle: {a:?} {b:?}");
            }
        }
    }

    #[test]
    fn stale_link_overrides_vehicle_mode_on_the_lamp() {
        let nominal = lamp(Mode::Nominal, Freshness::Live, 0.0);
        assert!(nominal.visible && nominal.emissive_gain < 0.2);
        let stale = lamp(Mode::Nominal, Freshness::Stale { age: 9.0 }, 0.0);
        assert_ne!(stale, nominal, "stale link presented identically to a live one");
        // And it must actually blink rather than sit on.
        assert!(!lamp(Mode::Nominal, Freshness::Stale { age: 9.0 }, 0.75).visible);
    }

    #[test]
    fn normalized_clamps_and_survives_a_degenerate_range() {
        assert_eq!(normalized(50.0, 0.0, 100.0), 0.5);
        assert_eq!(normalized(-10.0, 0.0, 100.0), 0.0);
        assert_eq!(normalized(999.0, 0.0, 100.0), 1.0);
        assert_eq!(normalized(5.0, 3.0, 3.0), 0.0);
    }
}

#[cfg(test)]
mod measurements {
    use super::*;

    /// Not an assertion — a number for the findings write-up. Run with
    /// `cargo test -p telemetry-anim -- --nocapture measurements`.
    #[test]
    fn report_clip_vs_direct_divergence() {
        let mut worst = 0.0f32;
        let mut at = 0.0f32;
        for step in 0..=2000 {
            let t = step as f32 / 2000.0 * rig::DEPLOY_DURATION_S;
            let (di, _) = deploy_angles_at(t);
            let ci = super::tests::clip_sample_pub(t, rig::PANEL1_WINDOW_S, rig::PANEL1_STOWED_DEG);
            let d = if di > ci { di - ci } else { ci - di };
            if d > worst {
                worst = d;
                at = t;
            }
        }
        std::println!("inner hinge: worst divergence {worst:.3} deg at t={at:.3}s");
    }
}
