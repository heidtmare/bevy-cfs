//! Golden tests against telemetry captured from a real cFS instance.
//!
//! Fixture: `fixtures/cfs-v7.0.1-hk.cfspkt`, recorded from nasa/cFS v7.0.1
//! (Draco, EDS disabled) running in the container defined in `docker/`.
//!
//! These exist because synthetic packets only prove the decoder is
//! self-consistent. Everything here is a property of bytes a real cFE actually
//! emitted.

use std::path::PathBuf;

use ccsds::{PacketType, SpacePacket};

fn fixture() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/cfs-v7.0.1-hk.cfspkt");
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Split the capture's length-prefixed framing: u32 little-endian length, then
/// that many octets.
fn frames(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 4 <= data.len() {
        let len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        assert!(off + len <= data.len(), "truncated frame at offset {off}");
        out.push(&data[off..off + len]);
        off += len;
    }
    assert_eq!(off, data.len(), "trailing bytes after last frame");
    out
}

#[test]
fn every_captured_packet_parses() {
    let data = fixture();
    let frames = frames(&data);
    assert_eq!(frames.len(), 42, "fixture packet count changed");

    for (i, frame) in frames.iter().enumerate() {
        let pkt = SpacePacket::parse(frame)
            .unwrap_or_else(|e| panic!("packet {i} failed to parse: {e}"));

        // The strongest single check on the length field: the datagram size cFE
        // chose must equal data_length + 7 exactly. An off-by-one in either
        // direction shows up here immediately.
        assert_eq!(pkt.total_len(), frame.len(), "packet {i} length mismatch");

        // to_lab forwards only telemetry, and cFE always sets the secondary
        // header flag on it.
        assert_eq!(pkt.primary().packet_type, PacketType::Telemetry, "packet {i}");
        assert!(pkt.primary().secondary_header, "packet {i} lacks a secondary header");
        assert!(pkt.tlm_secondary().is_ok(), "packet {i} secondary header unreadable");
    }
}

#[test]
fn capture_spans_the_recording_window() {
    // Confirms the 32-bit seconds / 16-bit subseconds big-endian layout is
    // right: decoded with the wrong width or byte order, the span is wildly
    // wrong rather than subtly off.
    let data = fixture();
    let times: Vec<f64> = frames(&data)
        .iter()
        .map(|f| SpacePacket::parse(f).unwrap().tlm_secondary().unwrap().as_secs_f64())
        .collect();

    let min = times.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = times.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = max - min;
    assert!((5.0..15.0).contains(&span), "span {span}s outside the 12s capture window");

    // Packets leave to_lab in publication order, so timestamps must not go
    // backwards.
    assert!(
        times.windows(2).all(|w| w[1] >= w[0]),
        "timestamps decreased: to_lab reorders, or the decode is wrong"
    );
}

#[test]
fn known_housekeeping_message_ids_are_present() {
    // These three survived from the historical v1 stream-ID scheme into v7.0.1
    // with EDS disabled. If a future build renumbers them this test fails, which
    // is the point: it is the tripwire for the message-ID assumption.
    let data = fixture();
    let ids: Vec<u16> = frames(&data)
        .iter()
        .map(|f| SpacePacket::parse(f).unwrap().primary().stream_id())
        .collect();

    for (id, name) in [(0x0880u16, "TO_LAB HK"), (0x0883, "SAMPLE_APP HK"), (0x0884, "CI_LAB HK")] {
        assert!(ids.contains(&id), "{name} ({id:#06X}) missing from capture");
    }

    let mut distinct: Vec<u16> = ids.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(distinct.len() >= 15, "expected a busy bus, saw {} ids", distinct.len());
}
