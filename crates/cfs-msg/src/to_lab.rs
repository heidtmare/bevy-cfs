//! `to_lab` commands.
//!
//! `to_lab` is the lab telemetry-output application: it holds a default
//! subscription list and, once output is enabled, forwards those packets over
//! UDP to whatever address the enable command names. Nothing arrives until that
//! command is sent — a silent telemetry socket is the expected state, not a bug.
//!
//! VERIFY the command codes against `to_lab_msg.h` in the pinned bundle.

use crate::{MsgId, build_command};
use ccsds::Error;

/// `TO_LAB_NOOP_CC`
pub const NOOP_CC: u8 = 0;
/// `TO_LAB_RESET_STATUS_CC`
pub const RESET_STATUS_CC: u8 = 1;
/// `TO_LAB_ADD_PKT_CC`
pub const ADD_PKT_CC: u8 = 2;
/// `TO_LAB_SEND_DATA_TYPES_CC`
pub const SEND_DATA_TYPES_CC: u8 = 3;
/// `TO_LAB_REMOVE_PKT_CC`
pub const REMOVE_PKT_CC: u8 = 4;
/// `TO_LAB_REMOVE_ALL_CC`
pub const REMOVE_ALL_CC: u8 = 5;
/// `TO_LAB_OUTPUT_ENABLE_CC`
pub const OUTPUT_ENABLE_CC: u8 = 6;

/// Width of the `dest_IP` field in `TO_LAB_EnableOutput_Payload_t`.
pub const DEST_IP_LEN: usize = 16;

/// Build the "enable output" command, pointing `to_lab` at `dest_ip`.
///
/// `dest_ip` is the address of the machine running the visualizer as seen *from
/// the cFS host*. Inside a container that is the host gateway, not `127.0.0.1`.
pub fn enable_output<'a>(
    out: &'a mut [u8],
    to_lab_cmd: MsgId,
    seq_count: u16,
    dest_ip: &str,
) -> Result<&'a [u8], Error> {
    // Needs a trailing NUL inside the fixed-width field.
    if dest_ip.len() >= DEST_IP_LEN {
        return Err(Error::PayloadTooLarge(dest_ip.len()));
    }
    let mut payload = [0u8; DEST_IP_LEN];
    payload[..dest_ip.len()].copy_from_slice(dest_ip.as_bytes());
    build_command(out, to_lab_cmd, OUTPUT_ENABLE_CC, seq_count, &payload)
}

/// Build a no-op command. Useful as a liveness probe: `to_lab` answers with an
/// event message, which proves the command path works before any telemetry flows.
pub fn noop(out: &mut [u8], to_lab_cmd: MsgId, seq_count: u16) -> Result<&[u8], Error> {
    build_command(out, to_lab_cmd, NOOP_CC, seq_count, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccsds::SpacePacket;

    #[test]
    fn enable_output_is_nul_padded() {
        let mut buf = [0u8; 64];
        let pkt = enable_output(&mut buf, MsgId(0x1880), 0, "192.168.65.1").unwrap();
        let payload = SpacePacket::parse(pkt).unwrap().payload().unwrap();
        assert_eq!(payload.len(), DEST_IP_LEN);
        assert_eq!(&payload[..12], b"192.168.65.1");
        assert!(payload[12..].iter().all(|b| *b == 0));
    }

    #[test]
    fn rejects_oversized_address() {
        let mut buf = [0u8; 64];
        assert!(enable_output(&mut buf, MsgId(0x1880), 0, "255.255.255.255!").is_err());
    }

    #[test]
    fn noop_has_no_payload() {
        let mut buf = [0u8; 64];
        let pkt = noop(&mut buf, MsgId(0x1880), 0).unwrap();
        assert_eq!(pkt.len(), 8);
        assert_eq!(SpacePacket::parse(pkt).unwrap().payload().unwrap().len(), 0);
    }
}
