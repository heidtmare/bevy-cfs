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

/// Octets in `TO_LAB_AddPacket_Payload_t`.
///
/// `{ CFE_SB_MsgId_t Stream; CFE_SB_Qos_t Flags; uint8 BufLimit; }` — a `u32`,
/// two `u8` and one `u8`, which the compiler pads out to 8. The trailing pad is
/// what makes this worth a constant rather than a literal `7`: `to_lab` reads
/// the struct, not the octets, and a 7-octet command is rejected as the wrong
/// length.
pub const ADD_PKT_LEN: usize = 8;

/// Subscribe `to_lab` to `stream`, so it appears on the downlink.
///
/// This is the mechanism that lets a ground tool ask for a packet the mission's
/// compiled-in subscription table never mentioned — which is exactly the
/// position anything published by `spikes/rust-cfs-app` is in, since its
/// message IDs are deliberately not in that table. See [`crate::rust_app`].
///
/// `buf_limit` is how many of this packet `to_lab` will queue; the mission
/// table uses 1 for housekeeping-rate messages and more for bursty ones.
///
/// Re-sending for a stream `to_lab` already has is harmless — it answers with
/// an error event and increments its error counter, but the subscription
/// stands. That matters because the enable-output keepalive re-sends these on
/// the same cycle: `to_lab` forgets everything when cFS restarts, and a viz
/// that subscribed once would go silent after a restart it never noticed.
pub fn add_packet(
    out: &mut [u8],
    to_lab_cmd: MsgId,
    seq_count: u16,
    stream: MsgId,
    buf_limit: u8,
) -> Result<&[u8], Error> {
    let mut payload = [0u8; ADD_PKT_LEN];
    // CFE_SB_MsgId_t is a struct wrapping a u32, little-endian on this target —
    // unlike the stream ID in the packet *header*, which is big-endian by CCSDS
    // rule. The same 16-bit number, spelled two different ways, four octets
    // apart in the same datagram.
    payload[..4].copy_from_slice(&(stream.0 as u32).to_le_bytes());
    // payload[4..6] is CFE_SB_Qos_t { Priority, Reliability }; cFE documents
    // both as currently unused, and the mission table passes {0, 0}.
    payload[6] = buf_limit;
    // payload[7] is the struct's tail padding and stays zero.
    build_command(out, to_lab_cmd, ADD_PKT_CC, seq_count, &payload)
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
    fn add_packet_carries_the_stream_id_little_endian() {
        let mut buf = [0u8; 64];
        let pkt = add_packet(&mut buf, MsgId(0x1880), 4, MsgId(0x0892), 1).unwrap();
        let parsed = SpacePacket::parse(pkt).unwrap();
        assert_eq!(parsed.cmd_secondary().unwrap().function_code, ADD_PKT_CC);

        let payload = parsed.payload().unwrap();
        assert_eq!(payload.len(), ADD_PKT_LEN);
        assert_eq!(&payload[..4], &[0x92, 0x08, 0x00, 0x00], "MsgId must be LE in the payload");
        assert_eq!(payload[6], 1, "BufLimit");
        assert_eq!(&payload[4..6], &[0, 0], "Qos");
    }

    /// The header spells the same number the other way round. Worth pinning:
    /// getting one of the two backwards produces a command cFS accepts and
    /// silently applies to the wrong stream.
    #[test]
    fn the_header_stream_id_stays_big_endian() {
        let mut buf = [0u8; 64];
        let pkt = add_packet(&mut buf, MsgId(0x1880), 0, MsgId(0x0892), 1).unwrap();
        assert_eq!(&pkt[..2], &[0x18, 0x80]);
    }

    #[test]
    fn noop_has_no_payload() {
        let mut buf = [0u8; 64];
        let pkt = noop(&mut buf, MsgId(0x1880), 0).unwrap();
        assert_eq!(pkt.len(), 8);
        assert_eq!(SpacePacket::parse(pkt).unwrap().payload().unwrap().len(), 0);
    }
}
