//! Phase 0 tool: connect to a running cFS, record raw telemetry, and print what
//! arrived.
//!
//! The output file is the Phase 1 gate. Once real packets are on disk the
//! decoder can be developed and regression-tested with cFS switched off, which
//! is the difference between a week of work and a week of container wrangling.
//!
//! It also prints a per-message-ID summary — the fastest way to discover what a
//! given build actually publishes, since the message IDs are build-specific.
//!
//! Usage:
//!   tlm-capture [--cfs-host 127.0.0.1] [--cmd-port 1234] [--tlm-port 1235]
//!               [--dest-ip <addr>] [--seconds 10] [--out fixtures/hk.cfspkt]

use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use cfs_link::{CfsLink, LinkConfig};

fn flag<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tlm-capture: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> std::io::Result<()> {
    let host: String = flag(args, "--cfs-host", "127.0.0.1".to_string());
    let cmd_port: u16 = flag(args, "--cmd-port", cfs_link::DEFAULT_CMD_PORT);
    let tlm_port: u16 = flag(args, "--tlm-port", cfs_link::DEFAULT_TLM_PORT);
    let seconds: u64 = flag(args, "--seconds", 10);
    let out: String = flag(args, "--out", "fixtures/capture.cfspkt".to_string());
    // Where cFS should send telemetry. Defaults to the loopback, which is wrong
    // for a containerized cFS — pass the host gateway address there.
    let dest_ip: String = flag(args, "--dest-ip", "127.0.0.1".to_string());

    let cmd_addr: SocketAddr = format!("{host}:{cmd_port}")
        .parse()
        .map_err(|_| std::io::Error::other(format!("bad cFS address {host}:{cmd_port}")))?;

    let link = CfsLink::connect(LinkConfig {
        cmd_addr,
        tlm_bind: SocketAddr::from(([0, 0, 0, 0], tlm_port)),
        dest_ip: dest_ip.clone(),
        ..Default::default()
    })?;

    println!("tlm-capture: enabled output to {dest_ip}, listening on :{tlm_port} for {seconds}s");

    if let Some(parent) = std::path::Path::new(&out).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = BufWriter::new(File::create(&out)?);

    // stream_id -> (count, packet length, min/max payload length seen)
    let mut seen: BTreeMap<u16, (usize, usize)> = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut total = 0usize;

    while Instant::now() < deadline {
        let mut idle = true;
        for raw in link.drain() {
            idle = false;
            total += 1;
            // Length-prefixed framing: 4-octet little-endian length, then bytes.
            file.write_all(&(raw.bytes.len() as u32).to_le_bytes())?;
            file.write_all(&raw.bytes)?;
            if let Ok(pkt) = raw.parse() {
                let e = seen.entry(pkt.primary().stream_id()).or_insert((0, pkt.total_len()));
                e.0 += 1;
            }
        }
        if idle {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    file.flush()?;

    let stats = link.stats();
    use std::sync::atomic::Ordering::Relaxed;
    println!("\ncaptured {total} packets -> {out}");
    println!(
        "link: received={} dropped={} parse_errors={} seq_gaps={}",
        stats.packets_received.load(Relaxed),
        stats.packets_dropped.load(Relaxed),
        stats.parse_errors.load(Relaxed),
        stats.sequence_gaps.load(Relaxed),
    );

    if seen.is_empty() {
        eprintln!(
            "\nNo telemetry. Check, in order:\n  \
             1. cFS is running and `to_lab` started (look for TO_LAB events on its console)\n  \
             2. --dest-ip is reachable *from the cFS host* (not 127.0.0.1 if cFS is in a container)\n  \
             3. to_lab_cmd message ID matches this build's cfs_msgids.h\n  \
             4. nothing else already holds UDP :{tlm_port}"
        );
        return Ok(());
    }

    println!("\nmessage IDs observed:");
    for (sid, (count, len)) in &seen {
        println!("  {sid:#06X}  {count:>6} packets  {len:>5} octets");
    }
    Ok(())
}
