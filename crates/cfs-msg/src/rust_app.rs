//! `RUST_APP` — the messages of the Rust cFE application in
//! `spikes/rust-cfs-app`.
//!
//! Every other module here describes messages someone else defined and this
//! project had to reverse-engineer. These are the opposite: this file *is* the
//! definition, and the flight application is compiled against it. That is the
//! point of Architecture B — one `no_std` crate is the single source of truth
//! for the wire format, linked into the ground software and into the cFE app,
//! so there is no C header to drift away from.
//!
//! # Choosing the message IDs, and getting it wrong twice
//!
//! These three constants cost more effort than the rest of the file, and both
//! mistakes are worth recording because neither produced an error message.
//!
//! **First mistake: a telemetry ID that worked by accident.** The earlier
//! version of this app published housekeeping on `0x0890` and it reached the
//! ground, which looked like success. `0x0890` is `MD_HK_TLM_MID` — the Memory
//! Dwell application's housekeeping — and it arrived on the downlink only
//! because that ID is already in `to_lab`'s compiled-in subscription table.
//! The `md` application is not in this build's startup script, so nothing
//! collided; had it been, two applications would have published different
//! payloads under one ID and the ground would have decoded whichever arrived
//! last, silently.
//!
//! **Second mistake: checking only half the tables.** The replacement IDs were
//! chosen by reading `to_lab`'s subscription table and picking values not in
//! it. `0x1891` passed that check and is nevertheless claimed: it is in
//! `sch_lab`'s *schedule* table, so the scheduler sends it a message every few
//! seconds. The symptom was this application's command counter incrementing on
//! its own, with a `RUST_APP: NOOP` event nobody had commanded — a bug that
//! would have been extremely confusing to meet later, since the obvious
//! suspect is the ground software.
//!
//! The lesson is not "check two tables", it is that **a message ID is only
//! free if the mission says it is.** Both tables are downstream of the topic-ID
//! allocation in the bundle's `*_topicid_values.h` headers, and the correct
//! way to add an application is to allocate a topic ID there and regenerate.
//! These constants are deliberately *not* that — they are hand-picked values
//! verified empirically against this one build — and that is the single
//! biggest thing standing between this spike and something flyable.
//!
//! What "verified empirically" means concretely, and what to repeat if the
//! pinned cFS version ever changes:
//!
//! ```text
//! # nothing else may publish these — run with the Rust app NOT loaded
//! docker run --rm cfs-build od -A d -t x4 \
//!     /src/build-native_std/exe/cpu1/cf/to_lab_sub.tbl
//! # nothing else may command these
//! docker run --rm cfs-build od -A d -t x4 \
//!     /src/build-native_std/exe/cpu1/cf/sch_lab_table.tbl
//! ```
//!
//! Telemetry IDs sit in the `0x08xx` range and commands in `0x18xx`, matching
//! the v1 stream-ID convention the rest of this build follows. Since these are
//! in no subscription table, `to_lab` has to be told about them at runtime —
//! see [`crate::to_lab::add_packet`]. That is the right default anyway: a
//! packet nobody asked for should not arrive.

use crate::{MsgId, build_command};
use ccsds::Error;

/// `RUST_APP` housekeeping: the application's own counters.
pub const HK_TLM_MID: MsgId = MsgId(0x0891);

/// `RUST_APP` vehicle state: attitude, array angle, deployment, wheels, mode.
///
/// Carries [`telemetry_model::encode_vehicle_state`]'s layout.
///
/// [`telemetry_model::encode_vehicle_state`]: https://docs.rs/telemetry-model
pub const VEHICLE_TLM_MID: MsgId = MsgId(0x0892);

/// `RUST_APP` command message.
///
/// Not `0x1891`, which `sch_lab` already sends to on a timer — see the module
/// docs. This is the value checked against both of the bundle's tables.
pub const CMD_MID: MsgId = MsgId(0x1892);

/// No-op. Increments the command counter and nothing else — the same
/// cheap round-trip proof `sample_app`'s no-op provides.
pub const NOOP_CC: u8 = 0;
/// Zero both command counters.
pub const RESET_COUNTERS_CC: u8 = 1;
/// Drop to `Safe`: stop pointing, damp the body rates.
pub const SAFE_CC: u8 = 2;
/// Leave `Safe` and resume attitude control.
pub const NOMINAL_CC: u8 = 3;
/// Slew to the next pointing target in the survey.
pub const NEXT_TARGET_CC: u8 = 4;
/// Inertial hold: freeze on the current attitude, stop the sequencer.
pub const HOLD_CC: u8 = 5;
/// Run the array deployment.
pub const DEPLOY_CC: u8 = 6;
/// Re-stow the arrays.
pub const STOW_CC: u8 = 7;
/// Dump the wheels' stored momentum.
pub const DUMP_MOMENTUM_CC: u8 = 8;

/// Build a `RUST_APP` command. All of them are payload-free.
///
/// One constructor rather than nine, because unlike `to_lab`'s commands these
/// differ in nothing but the function code, and nine identical wrappers would
/// be nine places for a wrong constant to hide.
pub fn command(
    out: &mut [u8],
    cmd_mid: MsgId,
    function_code: u8,
    seq_count: u16,
) -> Result<&[u8], Error> {
    build_command(out, cmd_mid, function_code, seq_count, &[])
}

/// `RUST_APP_HkTlm_Payload_t`, as the flight application lays it out.
///
/// Compiled into the cFE application as well as decoded here — the struct in
/// `spikes/rust-cfs-app` is `#[repr(C)]` over these same fields in this order,
/// and [`RustAppHk::LEN`] is asserted against `size_of` on the flight side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RustAppHk {
    pub command_counter: u8,
    pub command_error_counter: u8,
    /// Mode as [`telemetry_model::Mode::from_u8`] reads it.
    pub mode: u8,
    /// Non-zero while any reaction wheel is at its momentum limit.
    pub wheels_saturated: u8,
    /// Control cycles executed since startup — the app's own heartbeat.
    pub control_cycles: u32,
    /// Pointing targets commanded since startup.
    pub targets_commanded: u32,
}

impl RustAppHk {
    /// Octets on the wire. Four `u8` then two `u32`, naturally aligned with no
    /// padding, so this is also `size_of` on both sides.
    pub const LEN: usize = 12;

    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < Self::LEN {
            return None;
        }
        let u32_at = |o: usize| {
            u32::from_le_bytes([payload[o], payload[o + 1], payload[o + 2], payload[o + 3]])
        };
        Some(Self {
            command_counter: payload[0],
            command_error_counter: payload[1],
            mode: payload[2],
            wheels_saturated: payload[3],
            control_cycles: u32_at(4),
            targets_commanded: u32_at(8),
        })
    }

    /// Encode, for the flight application and the round-trip test.
    pub fn encode(&self, out: &mut [u8; Self::LEN]) {
        out[0] = self.command_counter;
        out[1] = self.command_error_counter;
        out[2] = self.mode;
        out[3] = self.wheels_saturated;
        out[4..8].copy_from_slice(&self.control_cycles.to_le_bytes());
        out[8..12].copy_from_slice(&self.targets_commanded.to_le_bytes());
    }

    pub fn from_packet(pkt: &ccsds::SpacePacket<'_>) -> Option<Self> {
        Self::decode(pkt.cfe_tlm_payload().ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccsds::SpacePacket;

    #[test]
    fn a_command_is_eight_octets_and_carries_its_function_code() {
        let mut buf = [0u8; 32];
        let pkt = command(&mut buf, CMD_MID, DEPLOY_CC, 7).unwrap();
        assert_eq!(pkt.len(), 8);

        let parsed = SpacePacket::parse(pkt).unwrap();
        assert_eq!(parsed.primary().stream_id(), CMD_MID.0);
        assert_eq!(parsed.cmd_secondary().unwrap().function_code, DEPLOY_CC);
    }

    #[test]
    fn hk_round_trips() {
        let hk = RustAppHk {
            command_counter: 3,
            command_error_counter: 1,
            mode: 2,
            wheels_saturated: 1,
            control_cycles: 123_456,
            targets_commanded: 9,
        };
        let mut bytes = [0u8; RustAppHk::LEN];
        hk.encode(&mut bytes);
        assert_eq!(RustAppHk::decode(&bytes), Some(hk));
    }

    #[test]
    fn short_payloads_are_rejected_rather_than_padded() {
        assert!(RustAppHk::decode(&[0u8; RustAppHk::LEN - 1]).is_none());
    }

    /// Both collisions this spike actually hit, asserted so they cannot be
    /// reintroduced by someone tidying the numbers into a run.
    ///
    /// The values come from the pinned v7.0.1 build's own tables, read with the
    /// commands in this module's documentation. A unit test cannot check a
    /// binary table it has no access to, so what it pins is the *result* of
    /// having checked, with the provenance written down next to it.
    #[test]
    fn the_ids_avoid_the_collisions_this_spike_hit() {
        /// `MD_HK_TLM_MID` — in `to_lab_sub.tbl`, so a packet published here
        /// reaches the ground while belonging to another application.
        const MD_HK_TLM_MID: u16 = 0x0890;
        /// In `sch_lab_table.tbl`: the scheduler sends this one a message every
        /// few seconds, so an app subscribing to it receives phantom commands.
        const SCH_LAB_DRIVEN_CMD_MID: u16 = 0x1891;

        for id in [HK_TLM_MID, VEHICLE_TLM_MID] {
            assert_ne!(id.0, MD_HK_TLM_MID, "{id} is Memory Dwell's housekeeping");
            assert!(!id.is_command(), "{id} is in the command range");
        }
        assert_ne!(CMD_MID.0, SCH_LAB_DRIVEN_CMD_MID, "{CMD_MID} is driven by sch_lab");
        assert!(CMD_MID.is_command(), "{CMD_MID} is not in the command range");

        // Distinct from each other, which a copy-paste would break silently.
        assert_ne!(HK_TLM_MID, VEHICLE_TLM_MID);
    }
}
