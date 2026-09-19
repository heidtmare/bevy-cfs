//! The four `f32` functions `core` does not provide.
//!
//! Same arrangement, and same reasoning, as `telemetry_model::math`: a no_std
//! crate can call `f32::sqrt` and friends only if something *else* in the
//! build graph happens to link `std`, which is an accident waiting to break
//! the first consumer that doesn't. Picking an implementation explicitly, and
//! failing the build when neither feature is on, is the whole point.
//!
//! This matters more here than anywhere else in the workspace: this crate is
//! the one that gets compiled into a cFE application and loaded by flight
//! software, where "it happened to link std" is not an answer.

#[cfg(all(not(feature = "std"), not(feature = "libm")))]
compile_error!("vehicle-dyn needs float maths: enable either the `std` or the `libm` feature");

macro_rules! shim {
    ($name:ident, $std:expr, $libm:path) => {
        #[cfg(feature = "std")]
        #[inline]
        pub(crate) fn $name(x: f32) -> f32 {
            #[allow(clippy::redundant_closure_call)]
            ($std)(x)
        }

        #[cfg(all(not(feature = "std"), feature = "libm"))]
        #[inline]
        pub(crate) fn $name(x: f32) -> f32 {
            $libm(x)
        }
    };
}

shim!(sqrt, |x: f32| x.sqrt(), libm::sqrtf);
shim!(sin, |x: f32| x.sin(), libm::sinf);
shim!(cos, |x: f32| x.cos(), libm::cosf);

#[cfg(feature = "std")]
#[inline]
pub(crate) fn atan2(y: f32, x: f32) -> f32 {
    y.atan2(x)
}

#[cfg(all(not(feature = "std"), feature = "libm"))]
#[inline]
pub(crate) fn atan2(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}

/// Absolute value. Exact in `core`, so no library is needed.
#[inline]
pub(crate) fn abs(x: f32) -> f32 {
    if x < 0.0 { -x } else { x }
}
