//! A stand-in for a running cFS instance.
//!
//! Exists so the entire Bevy side can be built, tested and demoed with no cFS,
//! no container and no network — and so animation behavior is reproducible
//! instead of depending on whatever a live instance happened to be doing.
//!
//! It deliberately imitates `to_lab`'s handshake: nothing is sent until an
//! enable-output command arrives. If the visualizer has a handshake bug, it
//! fails here rather than during the first live test.
//!
//! # What is and is not a stand-in
//!
//! The transport is a stand-in. The *spacecraft* is not: the telemetry this
//! sends is produced by `crates/vehicle-dyn`, stepped here exactly as
//! `spikes/rust-cfs-app` steps it inside cFE, and encoded by the same
//! `telemetry_model::encode_vehicle_state`. So `fake-cfs` and a live cFS
//! publish the same packets describing the same vehicle, and the only
//! difference is which process the integration ran in.
//!
//! That was not true of the earlier version, which generated a sine wave. The
//! difference matters because it is what makes an offline screenshot evidence
//! about the live system rather than merely a picture of the renderer.
//!
//! Usage:
//!   fake-cfs serve  [--cmd-port 1234] [--tlm-port 1235] [--rate 10]
//!   fake-cfs replay <fixture> [--tlm-port 1235] [--rate 10] [--loop]

use std::env;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ccsds::{PacketType, PrimaryHeader, TlmSecondaryHeader};
use cfs_msg::{MsgIds, to_lab};
use telemetry_model::{VEHICLE_PAYLOAD_LEN, VEHICLE_PAYLOAD_OFFSET, encode_vehicle_state};
use vehicle_dyn::{Command, Vehicle};

const USAGE: &str = "\
fake-cfs — stand-in cFS telemetry source

  fake-cfs serve  [--cmd-port N] [--tlm-port N] [--rate HZ]
  fake-cfs replay <fixture> [--tlm-port N] [--rate HZ] [--loop]
";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("replay") => replay(&args[1..]),
        _ => {
            eprint!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fake-cfs: {e}");
            ExitCode::FAILURE
        }
    }
}

fn flag<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Act as `ci_lab` + `to_lab`: wait for enable-output, then stream telemetry.
fn serve(args: &[String]) -> std::io::Result<()> {
    let cmd_port: u16 = flag(args, "--cmd-port", 1234);
    let tlm_port: u16 = flag(args, "--tlm-port", 1235);
    let rate: f64 = flag(args, "--rate", 10.0);

    let cmd_socket = UdpSocket::bind(("0.0.0.0", cmd_port))?;
    // Short, because this timeout is also the resolution of the send schedule:
    // the loop cannot send more often than it wakes. At 50 ms a requested 8 Hz
    // came out as 6.2 Hz measured, which is exactly the kind of quiet
    // inaccuracy a stand-in must not have — the whole point of `fake-cfs` is
    // that behaviour observed against it transfers to cFS.
    cmd_socket.set_read_timeout(Some(Duration::from_millis(2)))?;
    let tlm_socket = UdpSocket::bind(("0.0.0.0", 0))?;
    println!("fake-cfs: listening for commands on {cmd_port}, telemetry at {rate} Hz once enabled");

    let mut dest: Option<SocketAddr> = None;
    let mut seq: u16 = 0;
    let period = Duration::from_secs_f64(1.0 / rate.max(0.1));
    let mut next_send = Instant::now();
    let mut buf = [0u8; 2048];

    // The vehicle runs whether or not anyone is listening, exactly as it does
    // on the flight side — so a visualizer connecting late finds a spacecraft
    // already part-way through its survey rather than one that starts when it
    // is observed.
    let mut vehicle = Vehicle::new();
    let mut last_step = Instant::now();

    loop {
        // Commands first, so enabling output takes effect immediately.
        if let Ok((n, from)) = cmd_socket.recv_from(&mut buf) {
            match handle_command(&buf[..n], from.ip(), tlm_port) {
                Incoming::EnableOutput(addr) => {
                    if dest != Some(addr) {
                        println!("fake-cfs: output enabled -> {addr}");
                    }
                    dest = Some(addr);
                }
                Incoming::Vehicle(cmd) => {
                    println!("fake-cfs: vehicle command {cmd:?}");
                    vehicle.command(cmd);
                }
                Incoming::Other => println!("fake-cfs: command received (not one we model)"),
            }
        }

        let now = Instant::now();
        // Step on every wake-up, not only on send: the integrator wants small
        // steps, and the publish rate is a downlink property that should not
        // change how the vehicle flies.
        vehicle.step(now.duration_since(last_step).as_secs_f32());
        last_step = now;

        if now >= next_send {
            // Advance the schedule by exactly one period rather than restarting
            // it from now, so a late wake-up does not push every later packet
            // late as well.
            next_send += period;
            if next_send < now {
                next_send = now + period;
            }
            if let Some(addr) = dest {
                let pkt = build_vehicle_packet(&mut seq, &vehicle);
                tlm_socket.send_to(&pkt, addr)?;
            }
        }
    }
}

/// What an uplinked datagram turned out to be.
#[derive(Debug, PartialEq)]
enum Incoming {
    /// `to_lab` enable-output, naming where telemetry should go.
    EnableOutput(SocketAddr),
    /// A `RUST_APP` command this stand-in knows how to apply.
    Vehicle(Command),
    /// Anything else, including `to_lab` add-packet: a real `to_lab` has a
    /// subscription table to maintain and this does not, so those are
    /// acknowledged by being ignored rather than by being wrong.
    Other,
}

/// Classify an uplinked command.
///
/// For enable-output, the address in the payload is what a real `to_lab`
/// honors, but it is often unroutable from here (a container's view of the
/// host). Falling back to the command's source address is what makes this
/// usable on a laptop.
fn handle_command(packet: &[u8], from: IpAddr, tlm_port: u16) -> Incoming {
    let Ok(pkt) = ccsds::SpacePacket::parse(packet) else { return Incoming::Other };
    let Ok(secondary) = pkt.cmd_secondary() else { return Incoming::Other };
    let ids = MsgIds::LAB_DEFAULTS;
    let stream_id = pkt.primary().stream_id();

    if stream_id == ids.rust_app_cmd.0 {
        return match vehicle_command(secondary.function_code) {
            Some(cmd) => Incoming::Vehicle(cmd),
            None => Incoming::Other,
        };
    }

    if stream_id != ids.to_lab_cmd.0 || secondary.function_code != to_lab::OUTPUT_ENABLE_CC {
        return Incoming::Other;
    }
    let Ok(payload) = pkt.payload() else { return Incoming::Other };
    let ip = std::str::from_utf8(payload)
        .ok()
        .map(|s| s.trim_end_matches('\0'))
        .and_then(|s| s.parse::<IpAddr>().ok())
        .filter(|ip| !ip.is_unspecified())
        .unwrap_or(from);
    Incoming::EnableOutput(SocketAddr::new(ip, tlm_port))
}

/// The same function-code table the flight application uses.
///
/// Duplicated here rather than shared, because the flight app's copy lives in
/// its own workspace — and the duplication is bounded by the constants in
/// `cfs_msg::rust_app`, which both sides import. A new command added to one and
/// not the other is ignored, not misinterpreted.
fn vehicle_command(function_code: u8) -> Option<Command> {
    use cfs_msg::rust_app::*;
    Some(match function_code {
        SAFE_CC => Command::Safe,
        NOMINAL_CC => Command::Nominal,
        NEXT_TARGET_CC => Command::NextTarget,
        HOLD_CC => Command::Hold,
        DEPLOY_CC => Command::Deploy,
        STOW_CC => Command::Stow,
        DUMP_MOMENTUM_CC => Command::DumpMomentum,
        _ => return None,
    })
}

/// A vehicle-state packet, framed exactly as cFE frames telemetry — alignment
/// spare included — so the offline path exercises the same offsets as the live
/// one, and on the same message ID the flight application publishes.
fn build_vehicle_packet(seq: &mut u16, vehicle: &Vehicle) -> Vec<u8> {
    let total = VEHICLE_PAYLOAD_OFFSET + VEHICLE_PAYLOAD_LEN;
    let mut pkt = vec![0u8; total];

    let hdr = PrimaryHeader::for_total_len(
        MsgIds::LAB_DEFAULTS.rust_app_vehicle_tlm.apid(),
        PacketType::Telemetry,
        true,
        *seq,
        total,
    )
    .expect("vehicle packet fits the length field");
    hdr.write(&mut pkt[..6]).unwrap();
    *seq = seq.wrapping_add(1) & 0x3FFF;

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    TlmSecondaryHeader {
        seconds: now.as_secs() as u32,
        subseconds: (now.subsec_nanos() as f64 / 1e9 * 65536.0) as u16,
    }
    .write(&mut pkt[6..6 + TlmSecondaryHeader::LEN])
    .unwrap();
    // pkt[12..16] is CFE_MSG_TelemetryHeader_t::Spare — four octets of
    // alignment padding that cFE writes and every decoder must skip.

    let mut payload = [0u8; VEHICLE_PAYLOAD_LEN];
    encode_vehicle_state(&vehicle.state(), &mut payload);
    pkt[VEHICLE_PAYLOAD_OFFSET..].copy_from_slice(&payload);

    pkt
}

/// Replay a capture written by `tlm-capture`: 4-octet little-endian length
/// prefix, then that many octets, repeated.
fn replay(args: &[String]) -> std::io::Result<()> {
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| std::io::Error::other("replay needs a fixture path"))?;
    let tlm_port: u16 = flag(args, "--tlm-port", 1235);
    let rate: f64 = flag(args, "--rate", 10.0);
    let looping = args.iter().any(|a| a == "--loop");

    let data = std::fs::read(path)?;
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    let dest = SocketAddr::from(([127, 0, 0, 1], tlm_port));
    let period = Duration::from_secs_f64(1.0 / rate.max(0.1));

    loop {
        let mut off = 0usize;
        let mut sent = 0usize;
        while off + 4 <= data.len() {
            let len =
                u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
            off += 4;
            if off + len > data.len() {
                eprintln!("fake-cfs: truncated fixture at offset {off}");
                break;
            }
            socket.send_to(&data[off..off + len], dest)?;
            off += len;
            sent += 1;
            std::thread::sleep(period);
        }
        println!("fake-cfs: replayed {sent} packets from {path}");
        if !looping {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry_model::decode_vehicle_state;

    fn stepped(seconds: f32) -> Vehicle {
        let mut v = Vehicle::new();
        for _ in 0..((seconds / 0.02) as usize) {
            v.step(0.02);
        }
        v
    }

    #[test]
    fn generated_packets_decode() {
        let mut seq = 0;
        let vehicle = stepped(45.0);
        let pkt = build_vehicle_packet(&mut seq, &vehicle);
        let parsed = ccsds::SpacePacket::parse(&pkt).unwrap();
        let sample = decode_vehicle_state(&parsed, MsgIds::LAB_DEFAULTS.rust_app_vehicle_tlm)
            .expect("vehicle packet must decode");
        assert_eq!(sample.state, vehicle.state());
        assert_eq!(seq, 1);
    }

    /// The packet must carry the flight application's message ID, or the viz
    /// would need two decoder configurations for one payload layout.
    #[test]
    fn packets_use_the_flight_applications_message_id() {
        let mut seq = 0;
        let pkt = build_vehicle_packet(&mut seq, &Vehicle::new());
        let parsed = ccsds::SpacePacket::parse(&pkt).unwrap();
        assert_eq!(parsed.primary().stream_id(), cfs_msg::rust_app::VEHICLE_TLM_MID.0);
    }

    #[test]
    fn enable_output_falls_back_to_sender_address() {
        let mut buf = [0u8; 64];
        // Unroutable-from-here address, as a container would send.
        let cmd =
            to_lab::enable_output(&mut buf, MsgIds::LAB_DEFAULTS.to_lab_cmd, 0, "0.0.0.0").unwrap();
        let from: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(
            handle_command(cmd, from, 1235),
            Incoming::EnableOutput(SocketAddr::from(([127, 0, 0, 1], 1235)))
        );
    }

    /// A vehicle command has to reach the model here as well as in flight, or
    /// the keys would work against a container and silently do nothing offline.
    #[test]
    fn vehicle_commands_are_recognized_and_applied() {
        let mut buf = [0u8; 64];
        let cmd = cfs_msg::rust_app::command(
            &mut buf,
            MsgIds::LAB_DEFAULTS.rust_app_cmd,
            cfs_msg::rust_app::DEPLOY_CC,
            0,
        )
        .unwrap();
        let from: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(handle_command(cmd, from, 1235), Incoming::Vehicle(Command::Deploy));

        let mut vehicle = stepped(45.0);
        vehicle.command(Command::Deploy);
        for _ in 0..500 {
            vehicle.step(0.02);
        }
        assert!(vehicle.state().deploy_progress > 0.0);
    }

    /// `to_lab` add-packet arrives here on every keepalive cycle; it must be
    /// ignored rather than mistaken for an enable-output.
    #[test]
    fn add_packet_is_ignored_not_misread() {
        let mut buf = [0u8; 64];
        let cmd = to_lab::add_packet(
            &mut buf,
            MsgIds::LAB_DEFAULTS.to_lab_cmd,
            0,
            MsgIds::LAB_DEFAULTS.rust_app_vehicle_tlm,
            4,
        )
        .unwrap();
        let from: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(handle_command(cmd, from, 1235), Incoming::Other);
    }
}
