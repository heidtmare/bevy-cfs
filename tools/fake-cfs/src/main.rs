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
//! Usage:
//!   fake-cfs serve  [--cmd-port 1234] [--tlm-port 1235] [--rate 10]
//!   fake-cfs replay <fixture> [--tlm-port 1235] [--rate 10] [--loop]

use std::env;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ccsds::{PacketType, PrimaryHeader, TlmSecondaryHeader};
use cfs_msg::{MsgIds, to_lab};
use telemetry_model::{DEMO_PAYLOAD_LEN, Mode};

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
    cmd_socket.set_read_timeout(Some(Duration::from_millis(50)))?;
    let tlm_socket = UdpSocket::bind(("0.0.0.0", 0))?;
    println!("fake-cfs: listening for commands on {cmd_port}, telemetry at {rate} Hz once enabled");

    let mut dest: Option<SocketAddr> = None;
    let mut seq: u16 = 0;
    let period = Duration::from_secs_f64(1.0 / rate.max(0.1));
    let start = Instant::now();
    let mut next_send = Instant::now();
    let mut buf = [0u8; 2048];

    loop {
        // Commands first, so enabling output takes effect immediately.
        if let Ok((n, from)) = cmd_socket.recv_from(&mut buf) {
            match handle_command(&buf[..n], from.ip(), tlm_port) {
                Some(addr) => {
                    if dest != Some(addr) {
                        println!("fake-cfs: output enabled -> {addr}");
                    }
                    dest = Some(addr);
                }
                None => println!("fake-cfs: command received (not enable-output)"),
            }
        }

        let now = Instant::now();
        if now >= next_send {
            next_send = now + period;
            if let Some(addr) = dest {
                let pkt = build_demo_packet(&mut seq, start.elapsed().as_secs_f64());
                tlm_socket.send_to(&pkt, addr)?;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Returns the telemetry destination if this was an enable-output command.
///
/// The address in the payload is what a real `to_lab` honors, but it is often
/// unroutable from here (a container's view of the host). Falling back to the
/// command's source address is what makes this usable on a laptop.
fn handle_command(packet: &[u8], from: IpAddr, tlm_port: u16) -> Option<SocketAddr> {
    let pkt = ccsds::SpacePacket::parse(packet).ok()?;
    if pkt.primary().stream_id() != MsgIds::LAB_DEFAULTS.to_lab_cmd.0 {
        return None;
    }
    if pkt.cmd_secondary().ok()?.function_code != to_lab::OUTPUT_ENABLE_CC {
        return None;
    }
    let payload = pkt.payload().ok()?;
    let ip = std::str::from_utf8(payload)
        .ok()
        .map(|s| s.trim_end_matches('\0'))
        .and_then(|s| s.parse::<IpAddr>().ok())
        .filter(|ip| !ip.is_unspecified())
        .unwrap_or(from);
    Some(SocketAddr::new(ip, tlm_port))
}

/// A demo telemetry packet matching `telemetry_model::decode_demo`.
fn build_demo_packet(seq: &mut u16, t: f64) -> Vec<u8> {
    let total = 6 + TlmSecondaryHeader::LEN + DEMO_PAYLOAD_LEN;
    let mut pkt = vec![0u8; total];

    let hdr = PrimaryHeader::for_total_len(
        MsgIds::LAB_DEFAULTS.sample_app_hk_tlm.apid(),
        PacketType::Telemetry,
        true,
        *seq,
        total,
    )
    .expect("demo packet fits the length field");
    hdr.write(&mut pkt[..6]).unwrap();
    *seq = seq.wrapping_add(1) & 0x3FFF;

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    TlmSecondaryHeader {
        seconds: now.as_secs() as u32,
        subseconds: (now.subsec_nanos() as f64 / 1e9 * 65536.0) as u16,
    }
    .write(&mut pkt[6..12])
    .unwrap();

    // Slow yaw sweep, a rotating solar array, and a deployment that runs once
    // over the first 20 seconds — enough motion to exercise every Phase 3
    // animation mapping at once.
    let yaw = (t * 0.2) % std::f64::consts::TAU;
    let q = [0.0f32, 0.0, (yaw / 2.0).sin() as f32, (yaw / 2.0).cos() as f32];
    let solar_array_deg = ((t * 6.0) % 360.0) as f32;
    let deploy = ((t - 2.0) / 18.0).clamp(0.0, 1.0) as f32;
    let mode = if deploy <= 0.0 {
        Mode::Nominal
    } else if deploy < 1.0 {
        Mode::Deploying
    } else {
        Mode::Deployed
    };

    let p = &mut pkt[12..];
    for (i, v) in q.iter().enumerate() {
        p[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    p[16..20].copy_from_slice(&solar_array_deg.to_le_bytes());
    p[20..24].copy_from_slice(&deploy.to_le_bytes());
    for i in 0..4 {
        let rpm = (1000.0 + 200.0 * (t * 0.5 + i as f64).sin()) as f32;
        p[24 + i * 4..28 + i * 4].copy_from_slice(&rpm.to_le_bytes());
    }
    p[40] = mode as u8;

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
    use telemetry_model::decode_demo;

    #[test]
    fn generated_packets_decode() {
        let mut seq = 0;
        let pkt = build_demo_packet(&mut seq, 10.0);
        let parsed = ccsds::SpacePacket::parse(&pkt).unwrap();
        let sample = decode_demo(&parsed, MsgIds::LAB_DEFAULTS.sample_app_hk_tlm)
            .expect("demo packet must decode");
        assert_eq!(sample.state.mode, Mode::Deploying);
        assert!(sample.state.deploy_progress > 0.0 && sample.state.deploy_progress < 1.0);
        assert_eq!(seq, 1);
    }

    #[test]
    fn enable_output_falls_back_to_sender_address() {
        let mut buf = [0u8; 64];
        // Unroutable-from-here address, as a container would send.
        let cmd =
            to_lab::enable_output(&mut buf, MsgIds::LAB_DEFAULTS.to_lab_cmd, 0, "0.0.0.0").unwrap();
        let from: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(handle_command(cmd, from, 1235), Some(SocketAddr::from(([127, 0, 0, 1], 1235))));
    }
}
