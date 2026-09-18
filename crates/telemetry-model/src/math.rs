//! Float maths that `core` does not provide.
//!
//! `f32::sqrt` and `f64::abs` are inherent methods on `std`, not `core`. A
//! `#![no_std]` crate can still call them *if* something else in the build
//! graph happens to link `std` — which is why this crate compiled for months
//! while being nominally no_std, and why the omission only surfaced when
//! `telemetry-anim` became the first consumer to depend on it without `std`.
//!
//! The fix is to stop relying on that accident: pick an implementation
//! explicitly, and make a build that picks neither fail loudly.

#[cfg(all(not(feature = "std"), not(feature = "libm")))]
compile_error!(
    "telemetry-model needs float maths: enable either the `std` or the `libm` feature"
);

/// Absolute value. Exact in `core` — no library needed, and no sign-bit
/// trickery, because the comparison form is already branch-predictable and
/// handles -0.0 acceptably for the tolerance checks that use it.
#[inline]
pub(crate) fn abs(x: f64) -> f64 {
    if x < 0.0 { -x } else { x }
}

#[cfg(feature = "std")]
#[inline]
pub(crate) fn sqrtf(x: f32) -> f32 {
    x.sqrt()
}

#[cfg(all(not(feature = "std"), feature = "libm"))]
#[inline]
pub(crate) fn sqrtf(x: f32) -> f32 {
    libm::sqrtf(x)
}
