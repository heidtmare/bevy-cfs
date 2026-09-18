//! cFE message identifiers and the handful of lab-application messages the
//! investigation needs.
//!
//! No Bevy, no std: same reasoning as `ccsds`.
//!
//! # Message IDs are not portable
//!
//! Every value in [`MsgIds`] is a property of a specific cFS *build*, not of cFS
//! in general. Older bundles used CCSDS v1 stream IDs (`0x1880` and friends);
//! Caelum-era and later builds generate message IDs from topic IDs, and a
//! mission tree can renumber anything. Hardcoding these is the single easiest
//! way to spend a day debugging silence.
//!
//! So: [`MsgIds::LAB_DEFAULTS`] is a *starting guess*, and [`MsgIds::parse`]
//! reads the real values from a config file filled in from the build's
//! generated `cfs_msgids.h`. Confirm them before Phase 2 and record them in
//! `docs/findings/`.

#![no_std]
#![forbid(unsafe_code)]

pub mod to_lab;

use ccsds::{Error, PacketType, PrimaryHeader};

/// A cFE message ID.
///
/// Under the v1 scheme this is the CCSDS stream ID (first 16 header bits); the
/// type bit therefore distinguishes commands from telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MsgId(pub u16);

impl MsgId {
    /// True if the command bit is set (v1 scheme only).
    pub fn is_command(self) -> bool {
        self.0 & 0x1000 != 0
    }

    pub fn apid(self) -> u16 {
        self.0 & 0x07FF
    }
}

impl core::fmt::Display for MsgId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#06X}", self.0)
    }
}

/// The message IDs this project talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsgIds {
    pub to_lab_cmd: MsgId,
    pub to_lab_hk_tlm: MsgId,
    pub ci_lab_cmd: MsgId,
    pub ci_lab_hk_tlm: MsgId,
    pub sample_app_cmd: MsgId,
    pub sample_app_hk_tlm: MsgId,
}

impl MsgIds {
    /// Historical `*_lab` values under the CCSDS v1 scheme. **Verify, do not trust.**
    pub const LAB_DEFAULTS: Self = Self {
        to_lab_cmd: MsgId(0x1880),
        to_lab_hk_tlm: MsgId(0x0880),
        ci_lab_cmd: MsgId(0x1884),
        ci_lab_hk_tlm: MsgId(0x0884),
        sample_app_cmd: MsgId(0x1882),
        sample_app_hk_tlm: MsgId(0x0883),
    };

    /// Parse overrides from a minimal `name = 0x1880` config.
    ///
    /// Hand-rolled rather than pulling in a TOML parser, so this crate stays
    /// dependency-free and `no_std` for the Architecture B spike. Unknown keys
    /// are an error: a typo'd key that silently kept a default would reproduce
    /// exactly the bug this config exists to prevent.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut ids = Self::LAB_DEFAULTS;
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or(ConfigError { line: lineno + 1 })?;
            let value = value.trim();
            let parsed = match value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
                Some(hex) => u16::from_str_radix(hex, 16),
                None => value.parse::<u16>(),
            }
            .map_err(|_| ConfigError { line: lineno + 1 })?;

            let slot = match key.trim() {
                "to_lab_cmd" => &mut ids.to_lab_cmd,
                "to_lab_hk_tlm" => &mut ids.to_lab_hk_tlm,
                "ci_lab_cmd" => &mut ids.ci_lab_cmd,
                "ci_lab_hk_tlm" => &mut ids.ci_lab_hk_tlm,
                "sample_app_cmd" => &mut ids.sample_app_cmd,
                "sample_app_hk_tlm" => &mut ids.sample_app_hk_tlm,
                _ => return Err(ConfigError { line: lineno + 1 }),
            };
            *slot = MsgId(parsed);
        }
        Ok(ids)
    }
}

/// Bad line in a message-ID config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub line: usize,
}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "invalid message-id config at line {}", self.line)
    }
}

/// Build a cFE command packet into `out`, returning the occupied prefix.
///
/// Layout: 6-octet primary header, 2-octet command secondary header, payload.
/// The checksum is computed last, over the finished packet.
pub fn build_command<'a>(
    out: &'a mut [u8],
    msg_id: MsgId,
    function_code: u8,
    seq_count: u16,
    payload: &[u8],
) -> Result<&'a [u8], Error> {
    const HDR: usize = 8;
    let total = HDR + payload.len();
    if out.len() < total {
        return Err(Error::BufferTooSmall { need: total, got: out.len() });
    }

    let primary =
        PrimaryHeader::for_total_len(msg_id.apid(), PacketType::Command, true, seq_count, total)?;
    primary.write(&mut out[..6])?;
    out[6] = function_code & 0x7F;
    out[7] = 0; // checksum placeholder
    out[HDR..total].copy_from_slice(payload);

    out[7] = ccsds::compute_checksum(&out[..total], 7);
    Ok(&out[..total])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccsds::SpacePacket;

    #[test]
    fn built_command_parses_back() {
        let mut buf = [0u8; 64];
        let pkt = build_command(&mut buf, MsgId(0x1880), 6, 0, b"192.168.1.5\0\0\0\0\0").unwrap();
        assert_eq!(pkt.len(), 8 + 16);

        let parsed = SpacePacket::parse(pkt).unwrap();
        assert_eq!(parsed.primary().stream_id(), 0x1880);
        assert_eq!(parsed.cmd_secondary().unwrap().function_code, 6);
        assert_eq!(parsed.payload().unwrap().len(), 16);
    }

    #[test]
    fn config_overrides_defaults() {
        let ids = MsgIds::parse("# from cfs_msgids.h\nto_lab_cmd = 0x1881\nci_lab_cmd=6276\n").unwrap();
        assert_eq!(ids.to_lab_cmd, MsgId(0x1881));
        assert_eq!(ids.ci_lab_cmd, MsgId(6276));
        // Untouched keys keep their defaults.
        assert_eq!(ids.to_lab_hk_tlm, MsgIds::LAB_DEFAULTS.to_lab_hk_tlm);
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert_eq!(MsgIds::parse("to_lab_command = 0x1880"), Err(ConfigError { line: 1 }));
    }
}
