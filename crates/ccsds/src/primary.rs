use crate::{Error, MIN_PACKET_LEN, PRIMARY_HEADER_LEN};

/// Packet type bit: telemetry (report) or command (request).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketType {
    Telemetry,
    Command,
}

/// Sequence flags. cFS uses `Unsegmented` for essentially everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqFlags {
    Continuation,
    First,
    Last,
    Unsegmented,
}

impl SeqFlags {
    fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0b00 => SeqFlags::Continuation,
            0b01 => SeqFlags::First,
            0b10 => SeqFlags::Last,
            _ => SeqFlags::Unsegmented,
        }
    }

    fn bits(self) -> u16 {
        match self {
            SeqFlags::Continuation => 0b00,
            SeqFlags::First => 0b01,
            SeqFlags::Last => 0b10,
            SeqFlags::Unsegmented => 0b11,
        }
    }
}

/// The 6-octet CCSDS primary header, big-endian on the wire.
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-----+-+-+-----------------+-----+-------------------------+
/// | Ver |T|S|      APID       |SeqFl|      Sequence Count      |
/// +-----+-+-+-----------------+-----+-------------------------+
/// |              Packet Data Length (total - 7)               |
/// +-----------------------------------------------------------+
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrimaryHeader {
    pub apid: u16,
    pub packet_type: PacketType,
    pub secondary_header: bool,
    pub seq_flags: SeqFlags,
    pub seq_count: u16,
    /// Raw `PacketDataLength` field: total octets minus 7.
    pub data_length: u16,
}

impl PrimaryHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < PRIMARY_HEADER_LEN {
            return Err(Error::Truncated { need: PRIMARY_HEADER_LEN, got: bytes.len() });
        }
        let w0 = u16::from_be_bytes([bytes[0], bytes[1]]);
        let w1 = u16::from_be_bytes([bytes[2], bytes[3]]);
        let w2 = u16::from_be_bytes([bytes[4], bytes[5]]);

        let version = (w0 >> 13) as u8;
        if version != 0 {
            return Err(Error::BadVersion(version));
        }

        Ok(Self {
            apid: w0 & 0x07FF,
            packet_type: if (w0 >> 12) & 1 == 1 { PacketType::Command } else { PacketType::Telemetry },
            secondary_header: (w0 >> 11) & 1 == 1,
            seq_flags: SeqFlags::from_bits((w1 >> 14) as u8),
            seq_count: w1 & 0x3FFF,
            data_length: w2,
        })
    }

    /// The first 16 bits as a single value.
    ///
    /// Under the CCSDS v1 message-ID scheme this *is* the cFE message ID, which
    /// is why the lab tools talk about "stream IDs" and "MsgIDs" interchangeably.
    /// Under msgid v2 / topic IDs the mapping is no longer the identity — see
    /// `cfs-msg` for the indirection.
    pub fn stream_id(&self) -> u16 {
        let mut w0 = self.apid & 0x07FF;
        if self.packet_type == PacketType::Command {
            w0 |= 1 << 12;
        }
        if self.secondary_header {
            w0 |= 1 << 11;
        }
        w0
    }

    /// Total packet length in octets, header included.
    pub fn total_len(&self) -> usize {
        self.data_length as usize + PRIMARY_HEADER_LEN + 1
    }

    /// Serialize into the first 6 octets of `out`.
    pub fn write(&self, out: &mut [u8]) -> Result<(), Error> {
        if out.len() < PRIMARY_HEADER_LEN {
            return Err(Error::BufferTooSmall { need: PRIMARY_HEADER_LEN, got: out.len() });
        }
        let w0 = self.stream_id();
        let w1 = (self.seq_flags.bits() << 14) | (self.seq_count & 0x3FFF);
        out[0..2].copy_from_slice(&w0.to_be_bytes());
        out[2..4].copy_from_slice(&w1.to_be_bytes());
        out[4..6].copy_from_slice(&self.data_length.to_be_bytes());
        Ok(())
    }

    /// Build a header for a packet of `total_len` octets.
    pub fn for_total_len(
        apid: u16,
        packet_type: PacketType,
        secondary_header: bool,
        seq_count: u16,
        total_len: usize,
    ) -> Result<Self, Error> {
        if total_len < MIN_PACKET_LEN {
            return Err(Error::BufferTooSmall { need: MIN_PACKET_LEN, got: total_len });
        }
        let data_length = total_len - MIN_PACKET_LEN;
        let data_length = u16::try_from(data_length).map_err(|_| Error::PayloadTooLarge(total_len))?;
        Ok(Self {
            apid,
            packet_type,
            secondary_header,
            seq_flags: SeqFlags::Unsegmented,
            seq_count,
            data_length,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A telemetry packet: APID 0x080, secondary header present, seq 3,
    /// data length 0x000D -> total 20 octets.
    const TLM: [u8; 6] = [0x08, 0x80, 0xC0, 0x03, 0x00, 0x0D];

    #[test]
    fn parses_telemetry_header() {
        let h = PrimaryHeader::parse(&TLM).unwrap();
        assert_eq!(h.apid, 0x080);
        assert_eq!(h.packet_type, PacketType::Telemetry);
        assert!(h.secondary_header);
        assert_eq!(h.seq_flags, SeqFlags::Unsegmented);
        assert_eq!(h.seq_count, 3);
        assert_eq!(h.total_len(), 20);
        assert_eq!(h.stream_id(), 0x0880);
    }

    #[test]
    fn parses_command_header() {
        // 0x1880: type bit set, secondary header set, APID 0x080.
        let bytes = [0x18, 0x80, 0xC0, 0x00, 0x00, 0x01];
        let h = PrimaryHeader::parse(&bytes).unwrap();
        assert_eq!(h.packet_type, PacketType::Command);
        assert_eq!(h.stream_id(), 0x1880);
        assert_eq!(h.total_len(), 8);
    }

    #[test]
    fn round_trips() {
        let h = PrimaryHeader::parse(&TLM).unwrap();
        let mut out = [0u8; 6];
        h.write(&mut out).unwrap();
        assert_eq!(out, TLM);
    }

    #[test]
    fn rejects_nonzero_version() {
        // Misaligned stream reads as a bogus version far more often than not.
        assert_eq!(PrimaryHeader::parse(&[0x20, 0x00, 0, 0, 0, 0]), Err(Error::BadVersion(1)));
    }

    #[test]
    fn length_field_is_total_minus_seven() {
        let h = PrimaryHeader::for_total_len(0x080, PacketType::Telemetry, true, 0, 20).unwrap();
        assert_eq!(h.data_length, 13);
        assert_eq!(h.total_len(), 20);
    }
}
