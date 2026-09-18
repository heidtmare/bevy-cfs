//! Decoded telemetry as *domain state* — the vocabulary the visualizer animates.
//!
//! The deliberate boundary: nothing downstream of this crate should ever see a
//! `SpacePacket`, and nothing in this crate may mention Bevy. Phase 3 evaluates
//! several ways to drive animation from these values; all of them consume
//! [`SpacecraftState`], which is what keeps that comparison honest.
//!
//! The field set here is a placeholder standing in for a real mission's
//! telemetry. It exists so the full pipeline runs end to end before any real
//! packet layout is known — replace it once Phase 1 pins the actual packets.

#![no_std]
#![forbid(unsafe_code)]

use ccsds::SpacePacket;
use cfs_msg::MsgId;

/// Unit quaternion, `[x, y, z, w]`, body-to-inertial.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat(pub [f32; 4]);

impl Quat {
    pub const IDENTITY: Self = Quat([0.0, 0.0, 0.0, 1.0]);

    /// Normalized linear interpolation, taking the shorter arc.
    ///
    /// nlerp rather than slerp on purpose: telemetry samples arrive close
    /// together, the angular error is negligible at those step sizes, and nlerp
    /// has no singularity to special-case at near-parallel inputs.
    pub fn nlerp(self, other: Self, t: f32) -> Self {
        let mut b = other.0;
        let dot: f32 = self.0.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        // Antipodal quaternions are the same rotation; flip so we take the short way.
        if dot < 0.0 {
            b = [-b[0], -b[1], -b[2], -b[3]];
        }
        let mut out = [0.0f32; 4];
        for i in 0..4 {
            out[i] = self.0[i] + (b[i] - self.0[i]) * t;
        }
        let norm = out.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > f32::EPSILON {
            for v in &mut out {
                *v /= norm;
            }
        } else {
            return Self::IDENTITY;
        }
        Quat(out)
    }
}

/// Discrete mode. Phase 3 maps this onto animation-graph blending.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Safe,
    Nominal,
    Deploying,
    Deployed,
}

impl Mode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Mode::Nominal,
            2 => Mode::Deploying,
            3 => Mode::Deployed,
            _ => Mode::Safe,
        }
    }
}

/// Everything the visualizer knows about the vehicle at one instant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpacecraftState {
    pub attitude: Quat,
    /// Solar array rotation, degrees. Continuous — Phase 3 "direct drive".
    pub solar_array_deg: f32,
    /// Deployment progress, 0.0..=1.0. Parameterizes an authored clip.
    pub deploy_progress: f32,
    /// Reaction wheel speeds, RPM.
    pub wheel_rpm: [f32; 4],
    pub mode: Mode,
}

impl Default for SpacecraftState {
    fn default() -> Self {
        Self {
            attitude: Quat::IDENTITY,
            solar_array_deg: 0.0,
            deploy_progress: 0.0,
            wheel_rpm: [0.0; 4],
            mode: Mode::Safe,
        }
    }
}

impl SpacecraftState {
    /// Interpolate between two samples.
    ///
    /// Continuous quantities blend; `mode` is discrete and snaps at the
    /// midpoint. Interpolating a mode enum would invent states that never
    /// existed, which is the kind of plausible-looking lie a viz must not tell.
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mut wheel_rpm = [0.0f32; 4];
        for i in 0..4 {
            wheel_rpm[i] = self.wheel_rpm[i] + (other.wheel_rpm[i] - self.wheel_rpm[i]) * t;
        }
        Self {
            attitude: self.attitude.nlerp(other.attitude, t),
            solar_array_deg: lerp_angle_deg(self.solar_array_deg, other.solar_array_deg, t),
            deploy_progress: self.deploy_progress + (other.deploy_progress - self.deploy_progress) * t,
            wheel_rpm,
            mode: if t < 0.5 { self.mode } else { other.mode },
        }
    }
}

/// Interpolate degrees the short way around, so 359° -> 1° sweeps 2°, not 358°.
fn lerp_angle_deg(a: f32, b: f32, t: f32) -> f32 {
    let mut delta = (b - a) % 360.0;
    if delta > 180.0 {
        delta -= 360.0;
    } else if delta < -180.0 {
        delta += 360.0;
    }
    a + delta * t
}

/// One decoded telemetry sample with its packet timestamp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Seconds since the mission epoch, from the telemetry secondary header.
    pub time: f64,
    /// CCSDS sequence count, for gap detection.
    pub seq_count: u16,
    pub state: SpacecraftState,
}

/// Payload layout emitted by `tools/fake-cfs`, and the format the demo decoder
/// understands. Little-endian, matching a native x86/ARM cFS build's struct
/// packing.
///
/// This is a stand-in. Real payload layouts come from the mission's headers or
/// from EDS, and confirming which endianness a given build emits is a Phase 1
/// deliverable in its own right.
pub const DEMO_PAYLOAD_LEN: usize = 4 * 4 + 4 + 4 + 4 * 4 + 1;

/// Decode a demo telemetry packet into a [`Sample`].
///
/// Returns `None` for any packet that is not the expected message ID or is the
/// wrong size — a viz should ignore traffic it does not understand rather than
/// fail, since a live software bus carries plenty of it.
pub fn decode_demo(pkt: &SpacePacket<'_>, expected: MsgId) -> Option<Sample> {
    if pkt.primary().stream_id() != expected.0 {
        return None;
    }
    let time = pkt.tlm_secondary().ok()?.as_secs_f64();
    let p = pkt.payload().ok()?;
    if p.len() < DEMO_PAYLOAD_LEN {
        return None;
    }

    let f32_at = |o: usize| f32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
    let attitude = Quat([f32_at(0), f32_at(4), f32_at(8), f32_at(12)]);
    let mut wheel_rpm = [0.0f32; 4];
    for (i, w) in wheel_rpm.iter_mut().enumerate() {
        *w = f32_at(24 + i * 4);
    }

    Some(Sample {
        time,
        seq_count: pkt.primary().seq_count,
        state: SpacecraftState {
            attitude,
            solar_array_deg: f32_at(16),
            deploy_progress: f32_at(20),
            wheel_rpm,
            mode: Mode::from_u8(p[40]),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nlerp_takes_the_short_arc() {
        let a = Quat([0.0, 0.0, 0.0, 1.0]);
        // Same rotation as `a`, expressed antipodally.
        let b = Quat([0.0, 0.0, 0.0, -1.0]);
        let mid = a.nlerp(b, 0.5);
        assert!((mid.0[3].abs() - 1.0).abs() < 1e-5, "got {mid:?}");
    }

    #[test]
    fn nlerp_output_is_unit_length() {
        let a = Quat([0.0, 0.0, 0.0, 1.0]);
        let b = Quat([0.707, 0.0, 0.0, 0.707]);
        let m = a.nlerp(b, 0.3);
        let n = m.0.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-5);
    }

    #[test]
    fn angles_wrap_the_short_way() {
        assert!((lerp_angle_deg(359.0, 1.0, 0.5) - 360.0).abs() < 1e-3);
        assert!((lerp_angle_deg(10.0, 20.0, 0.5) - 15.0).abs() < 1e-3);
    }

    #[test]
    fn mode_snaps_rather_than_blending() {
        let a = SpacecraftState { mode: Mode::Safe, ..Default::default() };
        let b = SpacecraftState { mode: Mode::Nominal, ..Default::default() };
        assert_eq!(a.lerp(b, 0.49).mode, Mode::Safe);
        assert_eq!(a.lerp(b, 0.51).mode, Mode::Nominal);
    }
}
