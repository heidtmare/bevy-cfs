//! Headless demo: the plugin against a live telemetry source, printing what a
//! renderer would draw.
//!
//! ```sh
//! cargo run -p fake-cfs -- serve --rate 10          # terminal 1
//! cargo run -p bevy_cfs --example headless_viz      # terminal 2
//! ```
//!
//! Point it at a real cFS with `--cfs-host`/`--tlm-port` (telemetry is on 2234,
//! not 1235 — see docs/findings/0002). Note that Docker Desktop does not forward
//! UDP from a container to the macOS host, so a containerized cFS will not reach
//! this process; `fake-cfs` and fixture replay are the local loop.

use std::net::SocketAddr;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;

use bevy_cfs::{CfsPlugin, LinkHealth, Telemetry};
use cfs_link::LinkConfig;
use cfs_msg::MsgIds;
use telemetry_model::BufferConfig;

const RUN_FOR: Duration = Duration::from_secs(10);

fn arg<T: std::str::FromStr>(name: &str, default: T) -> T {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let host: String = arg("--cfs-host", "127.0.0.1".to_string());
    let cmd_port: u16 = arg("--cmd-port", cfs_link::DEFAULT_CMD_PORT);
    // fake-cfs defaults to the historical 1235; a real cFS v7.0.1 uses 2234.
    let tlm_port: u16 = arg("--tlm-port", 1235);
    let rate: f64 = arg("--rate", 10.0);

    let cmd_addr: SocketAddr =
        format!("{host}:{cmd_port}").parse().expect("bad --cfs-host/--cmd-port");

    println!("connecting to {cmd_addr}, listening for telemetry on :{tlm_port}");

    App::new()
        .add_plugins(
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(1.0 / 60.0))),
        )
        .add_plugins(CfsPlugin {
            link: LinkConfig {
                cmd_addr,
                tlm_bind: SocketAddr::from(([0, 0, 0, 0], tlm_port)),
                dest_ip: "127.0.0.1".into(),
                ..Default::default()
            },
            buffer: BufferConfig::for_rate(1.0 / rate),
            tlm_msg_id: MsgIds::LAB_DEFAULTS.sample_app_hk_tlm,
            connect: true,
        })
        .add_systems(Update, report)
        .run();
}

/// Print at a readable rate rather than every frame, and stop after `RUN_FOR`.
fn report(time: Res<Time>, telemetry: Res<Telemetry>, health: Res<LinkHealth>, mut frame: Local<u32>) {
    *frame += 1;
    if (*frame).is_multiple_of(15) {
        let t = telemetry.state;
        println!(
            "t={:5.1}s  {:>26}  solar={:7.2}deg  deploy={:4.2}  mode={:?}  \
             rx={} drop={} gaps={} reorder={} buf={} rate={:.1}Hz",
            time.elapsed_secs(),
            format!("{:?}", telemetry.freshness),
            t.solar_array_deg,
            t.deploy_progress,
            t.mode,
            health.packets_received,
            health.packets_dropped,
            health.sequence_gaps,
            health.buffer.reordered,
            health.buffered_samples,
            health.estimated_rate_hz.unwrap_or(0.0),
        );
    }

    if time.elapsed() >= RUN_FOR {
        println!("\ndone after {RUN_FOR:?}");
        // std::process::exit rather than an AppExit event: this is a demo with
        // nothing to flush, and it keeps the example off Bevy's event API, which
        // has churned across releases.
        std::process::exit(0);
    }
}
