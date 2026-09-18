//! `sample_app` commands — the command path the vertical slice closes.
//!
//! `sample_app` is the stock example application. It is the right target for a
//! round-trip proof because its no-op does something *observable in telemetry*:
//! `SAMPLE_APP_HkTlm_Payload_t::CommandCounter` increments, and that counter
//! comes back to the ground on the next housekeeping cycle.
//!
//! That property is what makes the loop closeable without writing a flight app.
//! `to_lab`'s no-op only emits an event message, which `to_lab` does not
//! forward; a viz would have to read the container's console to see it, which
//! proves nothing about the telemetry path.
//!
//! Function codes from `default_sample_app_fcncode_values.h`, v7.0.1.

use crate::{MsgId, build_command};
use ccsds::Error;

/// `SAMPLE_APP_NOOP_CC`. Increments `CommandCounter`.
pub const NOOP_CC: u8 = 0;
/// `SAMPLE_APP_RESET_COUNTERS_CC`. Zeroes both counters.
pub const RESET_COUNTERS_CC: u8 = 1;
/// `SAMPLE_APP_PROCESS_CC`.
pub const PROCESS_CC: u8 = 2;
/// `SAMPLE_APP_DISPLAY_PARAM_CC`.
pub const DISPLAY_PARAM_CC: u8 = 3;

/// No-op: the cheapest command that changes a value on the downlink.
pub fn noop(out: &mut [u8], sample_app_cmd: MsgId, seq_count: u16) -> Result<&[u8], Error> {
    build_command(out, sample_app_cmd, NOOP_CC, seq_count, &[])
}

/// Reset both housekeeping counters to zero.
///
/// Useful as the *second* half of a round-trip proof: an incrementing counter
/// could in principle be something else counting, but a counter that drops to
/// zero exactly when you ask it to cannot be.
pub fn reset_counters(
    out: &mut [u8],
    sample_app_cmd: MsgId,
    seq_count: u16,
) -> Result<&[u8], Error> {
    build_command(out, sample_app_cmd, RESET_COUNTERS_CC, seq_count, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MsgIds;
    use ccsds::SpacePacket;

    #[test]
    fn noop_is_eight_octets_and_addresses_sample_app() {
        let mut buf = [0u8; 32];
        let pkt = noop(&mut buf, MsgIds::LAB_DEFAULTS.sample_app_cmd, 1).unwrap();
        assert_eq!(pkt.len(), 8, "a no-op carries no payload");

        let parsed = SpacePacket::parse(pkt).unwrap();
        assert_eq!(parsed.primary().stream_id(), 0x1882);
        assert_eq!(parsed.cmd_secondary().unwrap().function_code, NOOP_CC);
    }

    /// The exact octets this crate puts on the wire for a no-op, pinned.
    ///
    /// This is the packet that made `SAMPLE: NOOP command v7.0.0+dev0` appear in
    /// a real cFS console, so it is worth freezing: if a header change ever
    /// alters these bytes, it should be a deliberate act with a re-test, not a
    /// silent regression discovered against live hardware.
    #[test]
    fn noop_bytes_are_the_ones_cfs_accepted() {
        let mut buf = [0u8; 32];
        let pkt = noop(&mut buf, MsgIds::LAB_DEFAULTS.sample_app_cmd, 1).unwrap();
        assert_eq!(pkt, [0x18, 0x82, 0xC0, 0x01, 0x00, 0x01, 0x00, 0xA5]);
    }

    #[test]
    fn reset_differs_from_noop_only_in_the_function_code() {
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        let id = MsgIds::LAB_DEFAULTS.sample_app_cmd;
        let n = noop(&mut a, id, 3).unwrap().to_vec();
        let r = reset_counters(&mut b, id, 3).unwrap().to_vec();
        assert_eq!(n[..6], r[..6]);
        assert_ne!(n[6], r[6]);
    }
}
