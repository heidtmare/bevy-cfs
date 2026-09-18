//! Real cFE housekeeping payloads, as the pinned v7.0.1 build emits them.
//!
//! Everything else in this repository decodes a *demo* payload invented for the
//! investigation. These are the genuine article: struct definitions copied from
//! the build's own headers, decoded from bytes a real cFE wrote.
//!
//! # Endianness
//!
//! **Little-endian**, for this build. CCSDS *headers* are big-endian by
//! standard, but payloads are raw C structs and follow the target's native byte
//! order — a `native_std` build on x86-64 or aarch64 is little-endian. That is
//! now measured rather than assumed; see [`CiLabHk::ingest_packets`] and
//! `crates/cfs-msg/tests/real_payloads.rs`.
//!
//! A big-endian target (SPARC/LEON, PowerPC — not hypothetical in flight
//! software) flips this, and nothing in the packet says which it was. Byte
//! order is a property of the build, so it belongs in the same config that
//! carries the message IDs.
//!
//! # Offsets
//!
//! Every payload here starts at octet **16** of the packet, not 12:
//! `CFE_MSG_TelemetryHeader_t` carries four octets of alignment spare after the
//! 6-octet timestamp. Use [`ccsds::SpacePacket::cfe_tlm_payload`], which
//! accounts for it.

use ccsds::SpacePacket;

/// `SAMPLE_APP_HkTlm_Payload_t`.
///
/// Two octets. `CommandCounter` is **first** — worth stating because several
/// other cFS apps order the pair the other way, and with both counters usually
/// at zero the mistake is invisible until something goes wrong.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleAppHk {
    pub command_counter: u8,
    pub command_error_counter: u8,
}

impl SampleAppHk {
    pub const LEN: usize = 2;

    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < Self::LEN {
            return None;
        }
        Some(Self { command_counter: payload[0], command_error_counter: payload[1] })
    }

    /// Decode straight from a packet, accounting for the cFE telemetry spare.
    pub fn from_packet(pkt: &SpacePacket<'_>) -> Option<Self> {
        Self::decode(pkt.cfe_tlm_payload().ok()?)
    }
}

/// `TO_LAB_HkTlm_Payload_t`. Same shape and same field order as
/// [`SampleAppHk`], but a distinct type: they are different messages and
/// conflating them is how a panel ends up showing the wrong app's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToLabHk {
    pub command_counter: u8,
    pub command_error_counter: u8,
}

impl ToLabHk {
    pub const LEN: usize = 2;

    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < Self::LEN {
            return None;
        }
        Some(Self { command_counter: payload[0], command_error_counter: payload[1] })
    }

    pub fn from_packet(pkt: &SpacePacket<'_>) -> Option<Self> {
        Self::decode(pkt.cfe_tlm_payload().ok()?)
    }
}

/// `CI_LAB_HkTlm_Payload_t` — twelve octets, and the most useful packet on the
/// bus for proving a command actually arrived.
///
/// [`CiLabHk::ingest_packets`] counts datagrams `ci_lab` read off its uplink
/// socket, which is a different thing from [`CiLabHk::command_counter`]: the
/// latter counts commands addressed to `ci_lab` itself. A command sent to some
/// *other* application bumps `ingest_packets` and leaves `command_counter`
/// alone, so the pair together distinguishes "the datagram reached cFS" from
/// "the datagram was addressed to me".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CiLabHk {
    pub command_counter: u8,
    pub command_error_counter: u8,
    /// `EnableChecksums`. Observed **0** on the pinned build — see finding 0005.
    pub enable_checksums: u8,
    pub socket_connected: u8,
    /// Datagrams read off the uplink socket since startup.
    pub ingest_packets: u32,
    pub ingest_errors: u32,
}

impl CiLabHk {
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
            enable_checksums: payload[2],
            socket_connected: payload[3],
            ingest_packets: u32_at(4),
            ingest_errors: u32_at(8),
        })
    }

    pub fn from_packet(pkt: &SpacePacket<'_>) -> Option<Self> {
        Self::decode(pkt.cfe_tlm_payload().ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_lab_reads_counters_little_endian() {
        // Exactly the payload octets of the second CI_LAB HK packet in the
        // committed capture.
        let payload = [0u8, 0, 0, 1, 0x08, 0, 0, 0, 0, 0, 0, 0];
        let hk = CiLabHk::decode(&payload).unwrap();
        assert_eq!(hk.ingest_packets, 8);
        assert_eq!(hk.socket_connected, 1);
        assert_eq!(hk.enable_checksums, 0);
        assert_eq!(hk.ingest_errors, 0);
    }

    #[test]
    fn short_payloads_are_rejected_rather_than_padded() {
        assert!(CiLabHk::decode(&[0u8; 11]).is_none());
        assert!(SampleAppHk::decode(&[0u8; 1]).is_none());
    }
}
