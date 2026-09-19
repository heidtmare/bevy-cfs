//! Just enough linear algebra, written out rather than pulled in.
//!
//! `glam` would do all of this and more, but it is a `std`-by-default crate
//! aimed at renderers, and this crate is compiled into a cFE application. The
//! operations actually needed here are a dozen lines; carrying a dependency
//! across the FFI boundary to avoid writing them would be the wrong trade.
//!
//! [`Quat`] here is a *working* quaternion with maths on it.
//! [`telemetry_model::Quat`] is the same four numbers as a wire value with no
//! behavior beyond interpolation. They convert freely; keeping them separate
//! stops dynamics maths from leaking into the crate that ground software
//! depends on.

use crate::math::sqrt;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, z: 0.0 };

    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Self) -> Self {
        Self::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    pub fn length(self) -> f32 {
        sqrt(self.dot(self))
    }

    pub fn normalize_or_zero(self) -> Self {
        let n = self.length();
        if n > 1e-9 { self * (1.0 / n) } else { Self::ZERO }
    }

    /// Componentwise product with the inverse of a diagonal inertia tensor.
    pub fn div_elem(self, o: Self) -> Self {
        Self::new(self.x / o.x, self.y / o.y, self.z / o.z)
    }

    /// Componentwise product — a diagonal inertia applied to a rate.
    pub fn mul_elem(self, o: Self) -> Self {
        Self::new(self.x * o.x, self.y * o.y, self.z * o.z)
    }

    pub fn to_array(self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }
}

impl core::ops::Add for Vec3 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl core::ops::Sub for Vec3 {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl core::ops::Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Self::new(self.x * k, self.y * k, self.z * k)
    }
}

impl core::ops::Neg for Vec3 {
    type Output = Self;
    fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }
}

/// Unit quaternion, `(x, y, z, w)`, rotating body coordinates into inertial.
///
/// Same storage order and same convention as [`telemetry_model::Quat`], so the
/// conversion is a move and there is no chance of a transposed rotation
/// sneaking in at the boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Default for Quat {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Quat {
    pub const IDENTITY: Self = Self { x: 0.0, y: 0.0, z: 0.0, w: 1.0 };

    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    /// Rotation of `angle` radians about a unit `axis`.
    pub fn from_axis_angle(axis: Vec3, angle: f32) -> Self {
        let half = angle * 0.5;
        let (s, c) = (crate::math::sin(half), crate::math::cos(half));
        let a = axis.normalize_or_zero();
        Self::new(a.x * s, a.y * s, a.z * s, c)
    }

    pub fn vector(self) -> Vec3 {
        Vec3::new(self.x, self.y, self.z)
    }

    /// Hamilton product. `a.then(b)` — no: `a.mul(b)` is the rotation that
    /// applies `b` first, then `a`, the same order as composing the matrices.
    ///
    /// Named `mul` rather than given an `impl Mul` because quaternion
    /// multiplication does not commute and `*` reads as though it does. Clippy
    /// flags the name as confusable with `std::ops::Mul::mul`, which is fair;
    /// the alternative spellings (`compose`, `then`) each imply an argument
    /// order, and picking one that disagrees with the maths is worse than a
    /// familiar name.
    #[allow(clippy::should_implement_trait)]
    pub fn mul(self, o: Self) -> Self {
        Self::new(
            self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
            self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
        )
    }

    /// Inverse of a unit quaternion — the conjugate, with no normalization.
    pub fn conjugate(self) -> Self {
        Self::new(-self.x, -self.y, -self.z, self.w)
    }

    /// Rotate a vector from body into inertial coordinates.
    pub fn rotate(self, v: Vec3) -> Vec3 {
        let u = self.vector();
        let t = u.cross(v) * 2.0;
        v + t * self.w + u.cross(t)
    }

    /// Rotate a vector from inertial into body coordinates.
    pub fn inverse_rotate(self, v: Vec3) -> Vec3 {
        self.conjugate().rotate(v)
    }

    pub fn normalize(self) -> Self {
        let n = sqrt(self.x * self.x + self.y * self.y + self.z * self.z + self.w * self.w);
        if n < 1e-9 {
            return Self::IDENTITY;
        }
        Self::new(self.x / n, self.y / n, self.z / n, self.w / n)
    }

    /// The same rotation with a non-negative scalar part.
    ///
    /// `q` and `-q` are the same attitude but opposite *errors*: a controller
    /// fed the wrong sign slews the long way round, 358° instead of 2°. This is
    /// the one line that prevents it, and it is easy to leave out because
    /// nothing looks wrong until the error crosses 180°.
    pub fn canonical(self) -> Self {
        if self.w < 0.0 { Self::new(-self.x, -self.y, -self.z, -self.w) } else { self }
    }

    /// Integrate `self` forward by body rate `omega` over `dt`.
    ///
    /// First-order, then renormalized. The step sizes here are milliseconds
    /// against slews measured in seconds, so the truncation error is far below
    /// the f32 noise floor; the renormalization is what actually matters,
    /// because without it the quaternion drifts off the unit sphere and the
    /// rotation it describes starts scaling vectors.
    pub fn integrate(self, omega: Vec3, dt: f32) -> Self {
        let half = dt * 0.5;
        let dq = Self::new(omega.x * half, omega.y * half, omega.z * half, 0.0);
        let q = self.mul(dq);
        Self::new(self.x + q.x, self.y + q.y, self.z + q.z, self.w + q.w).normalize()
    }

    /// Rotation angle in radians, `0..=π`.
    pub fn angle(self) -> f32 {
        let c = self.canonical().w.clamp(-1.0, 1.0);
        // 2·acos(w), via atan2 so the small-angle end stays accurate — acos
        // loses most of its precision exactly where a settled controller lives.
        2.0 * crate::math::atan2(self.vector().length(), c)
    }
}

impl From<Quat> for telemetry_model::Quat {
    fn from(q: Quat) -> Self {
        telemetry_model::Quat([q.x, q.y, q.z, q.w])
    }
}

impl From<telemetry_model::Quat> for Quat {
    fn from(q: telemetry_model::Quat) -> Self {
        Self::new(q.0[0], q.0[1], q.0[2], q.0[3])
    }
}

/// Approximate equality, for tests.
#[cfg(test)]
pub(crate) fn close(a: f32, b: f32, tol: f32) -> bool {
    crate::math::abs(a - b) <= tol
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::{FRAC_PI_2, PI};

    #[test]
    fn rotation_matches_the_right_hand_rule() {
        let q = Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), FRAC_PI_2);
        let v = q.rotate(Vec3::new(1.0, 0.0, 0.0));
        assert!(close(v.x, 0.0, 1e-5) && close(v.y, 1.0, 1e-5) && close(v.z, 0.0, 1e-5), "{v:?}");
    }

    #[test]
    fn inverse_rotate_undoes_rotate() {
        let q = Quat::from_axis_angle(Vec3::new(0.3, -0.5, 0.8), 1.1);
        let v = Vec3::new(2.0, -1.0, 0.5);
        let back = q.inverse_rotate(q.rotate(v));
        assert!(close(back.x, v.x, 1e-5) && close(back.y, v.y, 1e-5) && close(back.z, v.z, 1e-5));
    }

    /// Integrating a constant rate for a known time must produce the known angle.
    #[test]
    fn integration_accumulates_the_right_angle() {
        let mut q = Quat::IDENTITY;
        let omega = Vec3::new(0.0, 0.0, 0.2);
        for _ in 0..500 {
            q = q.integrate(omega, 0.01);
        }
        // 0.2 rad/s for 5 s = 1.0 rad.
        assert!(close(q.angle(), 1.0, 1e-3), "got {}", q.angle());
    }

    #[test]
    fn integration_stays_on_the_unit_sphere() {
        let mut q = Quat::IDENTITY;
        for i in 0..20_000 {
            q = q.integrate(Vec3::new(0.4, -0.3, 0.9), 0.002);
            if i % 1000 == 0 {
                let n = sqrt(q.x * q.x + q.y * q.y + q.z * q.z + q.w * q.w);
                assert!(close(n, 1.0, 1e-4), "norm drifted to {n}");
            }
        }
    }

    /// The sign convention the controller depends on.
    #[test]
    fn canonical_picks_the_short_way_round() {
        let almost_full = Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), 2.0 * PI - 0.1);
        assert!(almost_full.angle() < 0.2, "got {}", almost_full.angle());
        assert!(almost_full.canonical().w >= 0.0);
    }
}
