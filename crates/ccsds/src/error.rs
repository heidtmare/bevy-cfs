use core::fmt;

/// Everything that can go wrong decoding a space packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Buffer shorter than the packet claims to be.
    Truncated { need: usize, got: usize },
    /// Primary header version field was not 0 (the only version cFS emits).
    ///
    /// In practice this firing means the byte stream is misaligned, not that a
    /// future CCSDS version showed up.
    BadVersion(u8),
    /// Asked for a command header on a telemetry packet, or vice versa.
    WrongPacketType,
    /// Secondary-header flag is clear, so there is no secondary header to read.
    NoSecondaryHeader,
    /// Buffer too small to write the requested packet.
    BufferTooSmall { need: usize, got: usize },
    /// Packet body exceeds what the 16-bit length field can express.
    PayloadTooLarge(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated { need, got } => {
                write!(f, "truncated packet: need {need} octets, got {got}")
            }
            Error::BadVersion(v) => write!(f, "unsupported CCSDS version {v} (expected 0)"),
            Error::WrongPacketType => f.write_str("packet type does not match the requested header"),
            Error::NoSecondaryHeader => f.write_str("secondary header flag is not set"),
            Error::BufferTooSmall { need, got } => {
                write!(f, "output buffer too small: need {need} octets, got {got}")
            }
            Error::PayloadTooLarge(n) => write!(f, "payload of {n} octets exceeds the length field"),
        }
    }
}

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "std")]
impl std::error::Error for Error {}
