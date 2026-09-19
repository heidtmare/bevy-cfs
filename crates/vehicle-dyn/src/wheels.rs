//! A four-wheel reaction-wheel array, and the distribution law that turns a
//! three-axis torque command into four wheel torques.
//!
//! Four wheels rather than three is the usual flight choice — three wheels have
//! no redundancy and no spare control authority — and it is also why this is
//! worth writing out rather than hand-waving: with four wheels and three axes
//! the mapping is underdetermined, and *which* solution you pick is a real
//! design decision that shows up directly on the downlink as four different
//! numbers.

use crate::math::abs;
use crate::vec3::Vec3;

/// Wheels in the standard pyramid arrangement: four spin axes tilted off the
/// body +Z axis by [`PYRAMID_TILT`], spaced 90° apart in azimuth.
pub const WHEEL_COUNT: usize = 4;

/// Tilt of each spin axis from body +Z, as `(sin, cos)`.
///
/// `cos = 1/sqrt(3)`, i.e. 54.7356° — the tetrahedral angle. Chosen because it
/// makes the array's Gram matrix `A·Aᵀ` come out as exactly `(4/3)·I`, which
/// collapses the minimum-norm pseudo-inverse to a scalar multiple of `Aᵀ`. See
/// [`distribute`]: the whole allocation is four dot products, with no matrix
/// inverse anywhere. That is not a simplification of the real thing — it *is*
/// the real thing, for this geometry.
pub const PYRAMID_TILT: (f32, f32) = (0.816_496_6, 0.577_350_3);

/// Wheel spin axes in body coordinates, unit length.
pub const AXES: [Vec3; WHEEL_COUNT] = {
    let (s, c) = PYRAMID_TILT;
    [
        Vec3 { x: s, y: 0.0, z: c },
        Vec3 { x: 0.0, y: s, z: c },
        Vec3 { x: -s, y: 0.0, z: c },
        Vec3 { x: 0.0, y: -s, z: c },
    ]
};

/// Scalar that replaces the pseudo-inverse, `= 3/4`. See [`PYRAMID_TILT`].
const PINV_SCALE: f32 = 0.75;

/// Wheel moment of inertia about its spin axis, kg·m².
///
/// Sized with [`WHEEL_MOMENTUM_LIMIT`] so that a saturated wheel reads about
/// 4800 RPM — the right order of magnitude for a small reaction wheel, and the
/// reason the RPM numbers on the downlink look like wheel speeds rather than
/// like an arbitrary scale.
pub const WHEEL_INERTIA: f32 = 2.4e-4;

/// Momentum one wheel can store before it saturates, N·m·s.
///
/// Saturation is modelled rather than ignored because it is the failure this
/// telemetry exists to show: a saturated wheel silently stops producing torque,
/// the vehicle stops tracking, and the only warning is the wheel-speed trace
/// flattening out at its limit.
pub const WHEEL_MOMENTUM_LIMIT: f32 = 0.12;

/// Radians per second per RPM.
const RPM_TO_RAD_S: f32 = core::f32::consts::TAU / 60.0;

/// Stored angular momentum in each wheel, N·m·s.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Wheels {
    pub momentum: [f32; WHEEL_COUNT],
}

/// What a torque request actually achieved, once wheel limits were applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Applied {
    /// Torque the body really received, N·m. Equals the request only while no
    /// wheel is saturated.
    pub body_torque: Vec3,
    /// True if any wheel hit [`WHEEL_MOMENTUM_LIMIT`] this step.
    pub saturated: bool,
}

/// Minimum-norm allocation of a body torque across the four wheels.
///
/// Returns the torque applied to each *wheel*. The reaction on the body is the
/// negative of the sum, which for this geometry recovers the request exactly —
/// asserted in the tests rather than claimed here, because "the allocation is
/// exact" is the kind of statement that stays in a comment long after it stops
/// being true.
pub fn distribute(body_torque: Vec3) -> [f32; WHEEL_COUNT] {
    let mut out = [0.0; WHEEL_COUNT];
    for (o, axis) in out.iter_mut().zip(AXES.iter()) {
        // Negative: spinning a wheel up one way pushes the body the other way,
        // and `body_torque` is what the *body* is meant to feel.
        *o = -PINV_SCALE * axis.dot(body_torque);
    }
    out
}

impl Wheels {
    /// Spin the wheels for `dt` to produce `request` on the body.
    ///
    /// A wheel already at its momentum limit still accepts torque that unloads
    /// it; it only refuses torque that would push it further out. That
    /// asymmetry is what makes saturation recoverable instead of terminal, and
    /// it is the reason this is a clamp on the *result* rather than a rejection
    /// of the request.
    pub fn apply(&mut self, request: Vec3, dt: f32) -> Applied {
        let wheel_torque = distribute(request);
        let mut delivered = Vec3::ZERO;
        let mut saturated = false;

        for (h, (&tau, &axis)) in
            self.momentum.iter_mut().zip(wheel_torque.iter().zip(AXES.iter()))
        {
            let wanted = *h + tau * dt;
            let clamped = wanted.clamp(-WHEEL_MOMENTUM_LIMIT, WHEEL_MOMENTUM_LIMIT);
            if abs(wanted - clamped) > 0.0 {
                saturated = true;
            }
            // Only the momentum the wheel actually took shows up as a reaction.
            let achieved = (clamped - *h) / dt;
            *h = clamped;
            delivered = delivered - axis * achieved;
        }

        Applied { body_torque: delivered, saturated }
    }

    /// Total wheel momentum in body coordinates. Enters the body's own
    /// gyroscopic term, which is why it is not merely a display quantity.
    pub fn total_momentum(&self) -> Vec3 {
        let mut sum = Vec3::ZERO;
        for (&h, &axis) in self.momentum.iter().zip(AXES.iter()) {
            sum = sum + axis * h;
        }
        sum
    }

    /// Wheel speeds in RPM — the form the downlink carries and an operator reads.
    pub fn rpm(&self) -> [f32; WHEEL_COUNT] {
        let mut out = [0.0; WHEEL_COUNT];
        for (o, &h) in out.iter_mut().zip(self.momentum.iter()) {
            *o = h / WHEEL_INERTIA / RPM_TO_RAD_S;
        }
        out
    }

    /// Fraction of the momentum limit used by the busiest wheel, `0..=1`.
    pub fn saturation(&self) -> f32 {
        let mut worst = 0.0f32;
        for &h in &self.momentum {
            let f = abs(h) / WHEEL_MOMENTUM_LIMIT;
            if f > worst {
                worst = f;
            }
        }
        worst.min(1.0)
    }
}

/// RPM at which a wheel is saturated — the top of the scale a display should use.
pub fn saturation_rpm() -> f32 {
    WHEEL_MOMENTUM_LIMIT / WHEEL_INERTIA / RPM_TO_RAD_S
}

/// Length of a wheel-torque vector, used by the tests.
#[cfg(test)]
pub(crate) fn norm4(v: &[f32; WHEEL_COUNT]) -> f32 {
    crate::math::sqrt(v.iter().map(|x| x * x).sum::<f32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim [`distribute`] is built on. If the geometry is ever changed,
    /// this fails rather than quietly delivering the wrong torque.
    #[test]
    fn allocation_delivers_exactly_the_requested_torque() {
        for request in [
            Vec3::new(0.01, 0.0, 0.0),
            Vec3::new(0.0, -0.02, 0.0),
            Vec3::new(0.0, 0.0, 0.015),
            Vec3::new(0.004, -0.007, 0.002),
        ] {
            let mut wheels = Wheels::default();
            let applied = wheels.apply(request, 0.01);
            let err = (applied.body_torque - request).length();
            assert!(err < 1e-5, "requested {request:?}, delivered {:?}", applied.body_torque);
            assert!(!applied.saturated);
        }
    }

    /// Minimum-norm means exactly this: no other allocation producing the same
    /// body torque spins the wheels harder. Checked against the one-parameter
    /// family of alternatives, `u + k·n`, where `n` spans the null space.
    #[test]
    fn allocation_is_the_smallest_that_works() {
        let request = Vec3::new(0.006, 0.003, -0.004);
        let chosen = distribute(request);
        // A·n = 0 for this geometry: opposite wheels cancel in X and Y, and the
        // alternating sign cancels the common Z component.
        let null = [1.0f32, -1.0, 1.0, -1.0];

        for k in [-0.01f32, -0.003, 0.003, 0.01] {
            let mut alt = chosen;
            for (a, n) in alt.iter_mut().zip(null.iter()) {
                *a += k * n;
            }
            // Same body torque...
            let mut sum = Vec3::ZERO;
            for (&tau, &axis) in alt.iter().zip(AXES.iter()) {
                sum = sum - axis * tau;
            }
            assert!((sum - request).length() < 1e-5, "null-space vector was not null");
            // ...but more wheel effort.
            assert!(
                norm4(&alt) > norm4(&chosen),
                "k={k}: alternative {:.6} <= chosen {:.6}",
                norm4(&alt),
                norm4(&chosen)
            );
        }
    }

    #[test]
    fn a_saturated_wheel_stops_delivering_torque() {
        let mut wheels = Wheels { momentum: [WHEEL_MOMENTUM_LIMIT; WHEEL_COUNT] };
        // Torque that would push every wheel further positive.
        let request = Vec3::new(0.0, 0.0, -0.05);
        let applied = wheels.apply(request, 0.05);
        assert!(applied.saturated);
        assert!(
            applied.body_torque.length() < request.length() * 0.5,
            "saturated wheels still delivered {:?}",
            applied.body_torque
        );
        assert_eq!(wheels.momentum, [WHEEL_MOMENTUM_LIMIT; WHEEL_COUNT]);
    }

    /// Saturation has to be escapable, or the vehicle would be permanently lost
    /// the first time a slew ran long.
    #[test]
    fn a_saturated_wheel_still_accepts_unloading_torque() {
        let mut wheels = Wheels { momentum: [WHEEL_MOMENTUM_LIMIT; WHEEL_COUNT] };
        let applied = wheels.apply(Vec3::new(0.0, 0.0, 0.05), 0.05);
        assert!(!applied.saturated);
        assert!(wheels.momentum.iter().all(|h| *h < WHEEL_MOMENTUM_LIMIT));
    }

    #[test]
    fn rpm_and_momentum_agree_at_the_limit() {
        let wheels = Wheels { momentum: [WHEEL_MOMENTUM_LIMIT, 0.0, 0.0, 0.0] };
        assert!((wheels.rpm()[0] - saturation_rpm()).abs() < 1e-3);
        assert!((wheels.saturation() - 1.0).abs() < 1e-6);
    }
}
