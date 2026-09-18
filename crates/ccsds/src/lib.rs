//! CCSDS Space Packet Protocol (CCSDS 133.0-B) primary header plus the two cFE
//! secondary headers.
//!
//! Deliberately `no_std` and dependency-free: this crate is the one piece of the
//! investigation that may eventually need to compile into a cFS application
//! (see PLAN.md, Architecture B), so it must never grow a Bevy or std dependency.
//!
//! Decoding is zero-copy — [`SpacePacket`] is a borrowed view over a byte slice.

#![no_std]
#![forbid(unsafe_code)]

mod error;
mod primary;
mod secondary;

pub use error::Error;
pub use primary::{PacketType, PrimaryHeader, SeqFlags};
pub use secondary::{CmdSecondaryHeader, TlmSecondaryHeader, compute_checksum};

/// Size of the CCSDS primary header, in octets.
pub const PRIMARY_HEADER_LEN: usize = 6;

/// The `PacketDataLength` field is `total_length - 7`, so the smallest legal
/// packet carries one octet of data.
pub const MIN_PACKET_LEN: usize = PRIMARY_HEADER_LEN + 1;

/// Octets of padding cFE puts between the telemetry secondary header and the
/// application payload.
///
/// `CFE_MSG_TelemetryHeader_t` is `{ Msg, Sec, uint8 Spare[4] }` — the spare
/// exists so a payload needing 64-bit alignment does not make the compiler
/// insert padding of its own. It is **not** part of CCSDS and it is **not**
/// present on command packets, whose header is just `{ Msg, Sec }`.
///
/// Verified against `option_inc/default_cfe_msg_hdr_pri.h` in v7.0.1 and
/// against the committed capture, where every payload begins at octet 16.
pub const CFE_TLM_SPARE_LEN: usize = 4;

/// A borrowed, validated view over a complete space packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpacePacket<'a> {
    primary: PrimaryHeader,
    bytes: &'a [u8],
}

impl<'a> SpacePacket<'a> {
    /// Parse a packet from the front of `bytes`.
    ///
    /// The slice may be longer than the packet; trailing bytes are ignored and
    /// [`SpacePacket::total_len`] tells the caller how far to advance. This is
    /// what makes stream framing possible, though over UDP each datagram is
    /// expected to hold exactly one packet.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        let primary = PrimaryHeader::parse(bytes)?;
        let total = primary.total_len();
        if bytes.len() < total {
            return Err(Error::Truncated { need: total, got: bytes.len() });
        }
        Ok(Self { primary, bytes: &bytes[..total] })
    }

    /// The parsed primary header.
    pub fn primary(&self) -> PrimaryHeader {
        self.primary
    }

    /// Whole packet, header included.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Total packet length in octets (`PacketDataLength + 7`).
    pub fn total_len(&self) -> usize {
        self.bytes.len()
    }

    /// Everything after the primary header, secondary header included.
    pub fn data_field(&self) -> &'a [u8] {
        &self.bytes[PRIMARY_HEADER_LEN..]
    }

    /// The command secondary header, if this is a command packet carrying one.
    pub fn cmd_secondary(&self) -> Result<CmdSecondaryHeader, Error> {
        if self.primary.packet_type != PacketType::Command {
            return Err(Error::WrongPacketType);
        }
        if !self.primary.secondary_header {
            return Err(Error::NoSecondaryHeader);
        }
        CmdSecondaryHeader::parse(self.data_field())
    }

    /// The telemetry secondary header, if this is a telemetry packet carrying one.
    pub fn tlm_secondary(&self) -> Result<TlmSecondaryHeader, Error> {
        if self.primary.packet_type != PacketType::Telemetry {
            return Err(Error::WrongPacketType);
        }
        if !self.primary.secondary_header {
            return Err(Error::NoSecondaryHeader);
        }
        TlmSecondaryHeader::parse(self.data_field())
    }

    /// Application payload: the data field with the secondary header removed.
    ///
    /// Returns the whole data field when no secondary header is present.
    pub fn payload(&self) -> Result<&'a [u8], Error> {
        let skip = if !self.primary.secondary_header {
            0
        } else {
            match self.primary.packet_type {
                PacketType::Command => CmdSecondaryHeader::LEN,
                PacketType::Telemetry => TlmSecondaryHeader::LEN,
            }
        };
        self.data_field().get(skip..).ok_or(Error::Truncated {
            need: PRIMARY_HEADER_LEN + skip,
            got: self.bytes.len(),
        })
    }

    /// The **cFE** application payload of a telemetry packet.
    ///
    /// [`SpacePacket::payload`] is CCSDS-correct: data field minus secondary
    /// header. cFE puts [`CFE_TLM_SPARE_LEN`] octets of alignment padding after
    /// the telemetry secondary header, so for real cFE telemetry it returns the
    /// spare followed by the payload, and every field is four octets early.
    ///
    /// This matters more than an off-by-four usually does. Nothing rejects the
    /// packet, no length check trips, and the decoded values are plausible
    /// garbage rather than obvious garbage — which is exactly the failure mode
    /// item 7 of the verification backlog was opened to catch. Use this
    /// accessor for anything cFE emitted.
    pub fn cfe_tlm_payload(&self) -> Result<&'a [u8], Error> {
        if self.primary.packet_type != PacketType::Telemetry {
            return Err(Error::WrongPacketType);
        }
        if !self.primary.secondary_header {
            return Err(Error::NoSecondaryHeader);
        }
        let skip = TlmSecondaryHeader::LEN + CFE_TLM_SPARE_LEN;
        self.data_field().get(skip..).ok_or(Error::Truncated {
            need: PRIMARY_HEADER_LEN + skip,
            got: self.bytes.len(),
        })
    }
}

/// Iterator over back-to-back packets in a buffer, stopping at the first
/// malformed or truncated one.
///
/// Used by the fixture replayer; also the shape a TCP/stream transport would need.
pub struct PacketIter<'a> {
    rest: &'a [u8],
}

impl<'a> PacketIter<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    /// Bytes not yet consumed — a partial packet at the end of a stream buffer.
    pub fn remainder(&self) -> &'a [u8] {
        self.rest
    }
}

impl<'a> Iterator for PacketIter<'a> {
    type Item = SpacePacket<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        match SpacePacket::parse(self.rest) {
            Ok(pkt) => {
                self.rest = &self.rest[pkt.total_len()..];
                Some(pkt)
            }
            Err(_) => None,
        }
    }
}
