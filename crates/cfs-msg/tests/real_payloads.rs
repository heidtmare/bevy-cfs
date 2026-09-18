//! The first decode of a real cFE *payload*, and the test that settles payload
//! endianness (verification backlog item 7).
//!
//! Fixture: `fixtures/cfs-v7.0.1-hk.cfspkt`, 42 packets captured from nasa/cFS
//! v7.0.1 (Draco, EDS disabled, linux/arm64). Everything asserted here is a
//! property of bytes a real cFE wrote, decoded against struct definitions read
//! out of that same build's headers.
//!
//! What makes the endianness conclusion sound is not the magnitude of one
//! field, which could be argued either way, but the *rate* of two independent
//! counters. The capture was taken by `tlm-capture`, which re-sends the
//! enable-output command every five seconds. Over the two CI_LAB housekeeping
//! packets in the capture — five seconds apart — `IngestPackets` moves by
//! exactly one. Under the other byte order the same two packets read
//! 117,440,512 and 134,217,728, an increase of seventeen million datagrams in
//! five seconds. Only one of those readings describes what the machine did.

use ccsds::SpacePacket;
use cfs_msg::hk::{CiLabHk, SampleAppHk, ToLabHk};
use std::path::PathBuf;

fn frames() -> Vec<Vec<u8>> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cfs-v7.0.1-hk.cfspkt");
    let data =
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 4 <= data.len() {
        let len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        assert!(off + len <= data.len(), "truncated frame at {off}");
        out.push(data[off..off + len].to_vec());
        off += len;
    }
    out
}

fn packets_with(stream_id: u16) -> Vec<Vec<u8>> {
    frames()
        .into_iter()
        .filter(|f| SpacePacket::parse(f).unwrap().primary().stream_id() == stream_id)
        .collect()
}

/// The reason this whole file exists.
#[test]
fn payloads_are_little_endian() {
    let raw = packets_with(0x0884);
    assert_eq!(raw.len(), 2, "capture should hold two CI_LAB HK packets");

    let hk: Vec<CiLabHk> = raw
        .iter()
        .map(|f| CiLabHk::from_packet(&SpacePacket::parse(f).unwrap()).expect("decodes"))
        .collect();

    // Five seconds of wall time separate these two packets, and `tlm-capture`
    // sends exactly one command in that interval.
    assert_eq!(hk[0].ingest_packets, 7);
    assert_eq!(hk[1].ingest_packets, 8);

    // And the same octets read the other way round, stated explicitly so the
    // conclusion is visible in the test rather than only in the comment.
    let payload = SpacePacket::parse(&raw[0]).unwrap().cfe_tlm_payload().unwrap();
    let big_endian = u32::from_be_bytes(payload[4..8].try_into().unwrap());
    assert_eq!(big_endian, 117_440_512, "the reading this test rules out");

    assert_eq!(hk[0].ingest_errors, 0, "no uplink parse errors during the capture");
}

/// The four-octet `Spare` in `CFE_MSG_TelemetryHeader_t` is real, and reading
/// past it is not optional.
///
/// Without the spare, `CI_LAB_HkTlm_Payload_t` is read from the four zero
/// octets of padding onward. Every field slides four octets: the real
/// `IngestPackets` count lands in `IngestErrors`, `SocketConnected` becomes the
/// top byte of a sixteen-million-packet ingest count, and nothing anywhere
/// reports an error. A panel built on this would have shown a busy, faulty
/// uplink on an idle, healthy one.
#[test]
fn skipping_the_cfe_spare_would_decode_plausible_garbage() {
    let raw = packets_with(0x0884);
    let pkt = SpacePacket::parse(&raw[1]).unwrap();

    let correct = CiLabHk::from_packet(&pkt).unwrap();
    // `payload()` is CCSDS-correct and cFE-wrong: it stops after the 6-octet
    // timestamp and hands back the spare.
    let wrong = CiLabHk::decode(pkt.payload().unwrap()).unwrap();

    assert_eq!(correct.ingest_packets, 8);
    assert_eq!(correct.socket_connected, 1);

    assert_ne!(wrong.ingest_packets, correct.ingest_packets);
    assert_eq!(wrong.ingest_packets, 1 << 24, "SocketConnected read as a byte of the count");
    assert_eq!(wrong.ingest_errors, 8, "the real ingest count, landing in the error field");
    assert_eq!(wrong.socket_connected, 0);
}

/// Cross-check on the same capture from a different app, so the conclusion does
/// not rest on one struct definition being read correctly.
#[test]
fn to_lab_command_counter_tracks_the_same_commands() {
    let hk: Vec<ToLabHk> = packets_with(0x0880)
        .iter()
        .map(|f| ToLabHk::from_packet(&SpacePacket::parse(f).unwrap()).expect("decodes"))
        .collect();

    // Same two enable-output commands, counted by a different application:
    // 7 then 8, matching CI_LAB's ingest count exactly.
    assert_eq!(hk[0].command_counter, 7);
    assert_eq!(hk[1].command_counter, 8);
    assert_eq!(hk[0].command_error_counter, 0, "to_lab rejected a command");
}

/// `CommandCounter` is the first octet, not the second.
///
/// Both counters are zero in the capture, so this cannot be proved from these
/// bytes alone — it is asserted from `default_sample_app_msgdefs.h`, and the
/// live round-trip in finding 0005 is what actually confirms it: the counter
/// that moves when a no-op is sent is the one at offset 0.
#[test]
fn sample_app_starts_idle() {
    let hk: Vec<SampleAppHk> = packets_with(0x0883)
        .iter()
        .map(|f| SampleAppHk::from_packet(&SpacePacket::parse(f).unwrap()).expect("decodes"))
        .collect();
    assert!(!hk.is_empty());
    for h in &hk {
        assert_eq!(h.command_counter, 0, "nobody commanded sample_app during the capture");
        assert_eq!(h.command_error_counter, 0);
    }
}

/// `ci_lab` does not validate command checksums on this build.
///
/// Backlog item 6 assumed the checksum algorithm and noted it would fail
/// silently if wrong. This is the telemetry-side half of the answer: the flag
/// that would turn validation on reads zero, so acceptance of our commands says
/// nothing about our checksum. The algorithm itself is settled separately, by
/// reading `CFE_MSG_ComputeCheckSum`.
#[test]
fn ci_lab_reports_checksum_validation_disabled() {
    let hk = packets_with(0x0884);
    let first = CiLabHk::from_packet(&SpacePacket::parse(&hk[0]).unwrap()).unwrap();
    assert_eq!(first.enable_checksums, 0);
}
