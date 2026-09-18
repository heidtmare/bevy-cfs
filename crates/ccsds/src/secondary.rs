use crate::Error;

/// cFE command secondary header: a function code and a checksum.
///
/// The high bit of the function-code octet is reserved, so the code itself is
/// 7 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CmdSecondaryHeader {
    pub function_code: u8,
    pub checksum: u8,
}

impl CmdSecondaryHeader {
    pub const LEN: usize = 2;

    pub fn parse(data_field: &[u8]) -> Result<Self, Error> {
        if data_field.len() < Self::LEN {
            return Err(Error::Truncated { need: Self::LEN, got: data_field.len() });
        }
        Ok(Self { function_code: data_field[0] & 0x7F, checksum: data_field[1] })
    }

    pub fn write(&self, out: &mut [u8]) -> Result<(), Error> {
        if out.len() < Self::LEN {
            return Err(Error::BufferTooSmall { need: Self::LEN, got: out.len() });
        }
        out[0] = self.function_code & 0x7F;
        out[1] = self.checksum;
        Ok(())
    }
}

/// cFE telemetry secondary header: a 6-octet timestamp.
///
/// Decoded here as the default `CFE_SB_TIME_32_16_SUBS` layout — 32 bits of
/// seconds, 16 bits of subseconds in units of 2^-16 s.
///
/// VERIFY before trusting decoded times: the layout is a mission configuration
/// (`CFE_MISSION_SB_PACKET_TIME_FORMAT`), and the epoch is a second one
/// (`CFE_MISSION_TIME_EPOCH_*`, TAI by default, not Unix). Both are recorded in
/// `docs/findings/` once the cFS build is pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlmSecondaryHeader {
    pub seconds: u32,
    pub subseconds: u16,
}

impl TlmSecondaryHeader {
    pub const LEN: usize = 6;
    /// One subsecond tick, in seconds.
    const SUBSECOND: f64 = 1.0 / 65536.0;

    pub fn parse(data_field: &[u8]) -> Result<Self, Error> {
        if data_field.len() < Self::LEN {
            return Err(Error::Truncated { need: Self::LEN, got: data_field.len() });
        }
        Ok(Self {
            seconds: u32::from_be_bytes([data_field[0], data_field[1], data_field[2], data_field[3]]),
            subseconds: u16::from_be_bytes([data_field[4], data_field[5]]),
        })
    }

    pub fn write(&self, out: &mut [u8]) -> Result<(), Error> {
        if out.len() < Self::LEN {
            return Err(Error::BufferTooSmall { need: Self::LEN, got: out.len() });
        }
        out[0..4].copy_from_slice(&self.seconds.to_be_bytes());
        out[4..6].copy_from_slice(&self.subseconds.to_be_bytes());
        Ok(())
    }

    /// Timestamp as fractional seconds since the mission epoch.
    ///
    /// Only meaningful relative to other timestamps from the same instance until
    /// the epoch is confirmed.
    pub fn as_secs_f64(&self) -> f64 {
        self.seconds as f64 + self.subseconds as f64 * Self::SUBSECOND
    }
}

/// cFE command checksum: XOR of every octet of the packet, seeded with `0xFF`,
/// computed with the checksum octet itself zeroed.
///
/// VERIFY against `CFE_MSG_ComputeCheckSum` in the pinned cFE before relying on
/// it. It matters less than it looks: `ci_lab` historically accepts commands
/// without validating the checksum, so a wrong value here will not fail loudly.
/// That is exactly why it needs checking against the source rather than against
/// observed behavior.
pub fn compute_checksum(packet: &[u8], checksum_offset: usize) -> u8 {
    let mut sum: u8 = 0xFF;
    for (i, b) in packet.iter().enumerate() {
        if i != checksum_offset {
            sum ^= *b;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_secondary() {
        // Reserved high bit set on the function-code octet must be masked off.
        let h = CmdSecondaryHeader::parse(&[0x86, 0x2A]).unwrap();
        assert_eq!(h.function_code, 6);
        assert_eq!(h.checksum, 0x2A);
    }

    #[test]
    fn telemetry_time_round_trips() {
        let h = TlmSecondaryHeader { seconds: 0x1234_5678, subseconds: 0x8000 };
        let mut out = [0u8; 6];
        h.write(&mut out).unwrap();
        assert_eq!(out, [0x12, 0x34, 0x56, 0x78, 0x80, 0x00]);
        assert_eq!(TlmSecondaryHeader::parse(&out).unwrap(), h);
        // 0x8000 subseconds is exactly half a second.
        assert!((h.as_secs_f64() - (0x1234_5678 as f64 + 0.5)).abs() < 1e-9);
    }

    #[test]
    fn checksum_ignores_the_checksum_octet() {
        let mut pkt = [0x18, 0x80, 0xC0, 0x00, 0x00, 0x01, 0x06, 0x00];
        let c = compute_checksum(&pkt, 7);
        pkt[7] = c;
        // Recomputing over the filled-in packet yields the same value.
        assert_eq!(compute_checksum(&pkt, 7), c);
    }
}
