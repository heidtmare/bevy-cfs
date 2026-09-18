//! The telemetry side panel.
//!
//! An animated 3D view is a lie detector's worst case: it looks equally
//! convincing whether the data behind it is a live downlink, a two-minute-old
//! frozen sample, or a generator. So the panel's job is not decoration. It
//! answers three questions at a glance — where did this number come from, how
//! old is it, and did my command do anything — and every one of them is a
//! question the 3D view cannot answer by itself.
//!
//! The text is built by a pure function so the layout can be tested without a
//! window, and so the staleness wording in particular is pinned by a test
//! rather than by whoever last looked at the screen.

use bevy::prelude::*;

use bevy_cfs::{Housekeeping, LinkHealth};
use telemetry_model::{Freshness, SpacecraftState};

use crate::command_loop::CommandLoop;
use crate::sources::{Pipeline, Sources};

/// Everything the panel prints, gathered in one place so the formatting is a
/// pure function of it.
pub struct PanelData<'a> {
    pub endpoint: &'a str,
    pub pipeline: Pipeline,
    pub sources: Sources,
    pub state: SpacecraftState,
    pub freshness: Freshness,
    pub health: &'a LinkHealth,
    pub hk: &'a Housekeeping,
    pub commands: &'a CommandLoop,
    pub drawn_hinge_deg: Option<f32>,
    pub rig_error: Option<String>,
}

/// How a `Freshness` should read to an operator.
///
/// Spelled out rather than `{:?}`-printed because these are the words someone
/// makes a decision on. "Live" and "holding 3.2s" mean different things and the
/// difference must not depend on knowing the enum.
pub fn freshness_line(freshness: Freshness) -> String {
    match freshness {
        Freshness::NoData => "NO DATA - nothing decoded yet".into(),
        Freshness::Warming => "WARMING - buffering before playback".into(),
        Freshness::Live => "LIVE - interpolating between real samples".into(),
        Freshness::Holding { age } => {
            format!("HOLDING {age:.1}s - last known value, not extrapolated")
        }
        Freshness::Stale { age } => {
            format!("STALE {age:.1}s - the vehicle may have moved since")
        }
    }
}

/// What to say about playback, which is not the same question as freshness.
///
/// The jitter buffer's `Freshness` describes the *vehicle-state* stream. When
/// the rig is driven by signals derived from housekeeping there is no such
/// stream, and printing "interpolating between real samples" there would be a
/// straightforward lie: derived signals step once per scheduler tick and
/// nothing interpolates them. The distinction is the difference between smooth
/// motion that is real and smooth motion that is invented, which is the one
/// thing this whole panel exists to keep straight.
pub fn playback_line(pipeline: Pipeline, freshness: Freshness) -> String {
    match pipeline {
        Pipeline::Derived => {
            "LIVE (derived) - housekeeping steps at the scheduler rate, not interpolated".into()
        }
        Pipeline::Waiting => freshness_line(Freshness::NoData),
        _ => freshness_line(freshness),
    }
}

pub fn render(data: &PanelData<'_>) -> String {
    let mut out = String::new();

    if let Some(err) = &data.rig_error {
        out.push_str(&format!("!! {err}\n\n"));
    }

    out.push_str(&format!("LINK   {}\n", data.endpoint));
    let h = data.health;
    out.push_str(&format!(
        "  rx {}  dropped {}  parse err {}  seq gaps {}\n",
        h.packets_received, h.packets_dropped, h.parse_errors, h.sequence_gaps
    ));
    out.push_str(&format!(
        "  ignored {}  decode fail {}  cmds sent {}\n",
        h.packets_ignored, h.decode_failures, h.commands_sent
    ));
    let rate = match h.estimated_rate_hz {
        Some(hz) => format!("{hz:.1} Hz"),
        None => "-- Hz".into(),
    };
    let b = h.buffer;
    out.push_str(&format!(
        "  vehicle-state stream: rate {rate}  buffered {}  inserted {}  reordered {}  resyncs {}\n",
        h.buffered_samples, b.inserted, b.reordered, b.resyncs
    ));
    out.push_str(&format!("  {}\n\n", playback_line(data.pipeline, data.freshness)));

    out.push_str("FLIGHT SOFTWARE  (real cFE housekeeping)\n");
    match data.hk.sample_app {
        Some(v) => out.push_str(&format!(
            "  SAMPLE_APP  cmd {:3}  err {:3}\n",
            v.command_counter, v.command_error_counter
        )),
        None => out.push_str("  SAMPLE_APP  --\n"),
    }
    match data.hk.to_lab {
        Some(v) => out.push_str(&format!(
            "  TO_LAB      cmd {:3}  err {:3}\n",
            v.command_counter, v.command_error_counter
        )),
        None => out.push_str("  TO_LAB      --\n"),
    }
    match data.hk.ci_lab {
        Some(v) => out.push_str(&format!(
            "  CI_LAB      cmd {:3}  err {:3}  ingest {}/{}  socket {}  cksum {}\n",
            v.command_counter,
            v.command_error_counter,
            v.ingest_packets,
            v.ingest_errors,
            v.socket_connected,
            if v.enable_checksums == 0 { "off" } else { "on" },
        )),
        None => out.push_str("  CI_LAB      --\n"),
    }
    match data.hk.last_time {
        Some(t) => out.push_str(&format!("  mission time {t:.2}s since 1980-01-01\n\n")),
        None => out.push_str("  mission time --\n\n"),
    }

    out.push_str(&format!("VEHICLE  {}\n", data.pipeline.label()));
    for (signal, source, mechanism) in data.sources.rows() {
        out.push_str(&format!("  {signal:<12} {:<34} -> {mechanism}\n", source.label()));
    }
    let s = &data.state;
    out.push_str(&format!(
        "  array {:6.1}deg   deploy {:.3}   mode {:?}\n",
        s.solar_array_deg % 360.0,
        s.deploy_progress,
        s.mode
    ));
    match data.drawn_hinge_deg {
        Some(deg) => out.push_str(&format!("  inner hinge as drawn {deg:7.2}deg\n\n")),
        None => out.push_str("  inner hinge as drawn --\n\n"),
    }

    out.push_str("COMMAND  (real packets to ci_lab)\n");
    out.push_str("  [N] SAMPLE_APP NOOP      [R] SAMPLE_APP RESET_COUNTERS\n");
    out.push_str(&format!("  {}\n", data.commands.summary()));

    out
}

#[derive(Component)]
pub struct PanelText;

/// Spawn the panel, bound to the camera it should render into.
pub fn spawn(commands: &mut Commands, camera: Entity) {
    commands.spawn((
        PanelText,
        UiTargetCamera(camera),
        Text::new("connecting..."),
        TextFont { font_size: FontSize::Px(13.0), ..default() },
        TextColor(Color::srgb(0.72, 0.82, 0.95)),
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(14.0),
            top: Val::Px(12.0),
            ..default()
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::Source;
    use cfs_msg::hk::{CiLabHk, SampleAppHk};

    fn data<'a>(
        freshness: Freshness,
        hk: &'a Housekeeping,
        health: &'a LinkHealth,
        commands: &'a CommandLoop,
    ) -> PanelData<'a> {
        PanelData {
            endpoint: "127.0.0.1:1234 -> :2234",
            pipeline: Pipeline::Derived,
            sources: Sources {
                attitude: Source::None,
                solar_array: Source::Derived("CFE mission time"),
                deploy: Source::Derived("CI_LAB.IngestPackets since connect"),
                mode: Source::Derived("SAMPLE_APP.CommandCounter % 4"),
                wheels: Source::None,
            },
            state: SpacecraftState::default(),
            freshness,
            health,
            hk,
            commands,
            drawn_hinge_deg: Some(-150.0),
            rig_error: None,
        }
    }

    /// Claiming interpolation where none happens is the exact failure mode the
    /// panel exists to prevent.
    #[test]
    fn derived_playback_does_not_claim_interpolation() {
        let line = playback_line(Pipeline::Derived, Freshness::Live);
        assert!(line.contains("derived"), "{line}");
        assert!(!line.contains("interpolating"), "{line}");
        assert!(playback_line(Pipeline::Demo, Freshness::Live).contains("interpolating"));
    }

    /// The staleness wording is the panel's most important output and the
    /// easiest to weaken by accident.
    #[test]
    fn stale_and_holding_do_not_read_like_live() {
        assert!(freshness_line(Freshness::Live).contains("LIVE"));

        let holding = freshness_line(Freshness::Holding { age: 3.25 });
        assert!(holding.contains("3.2"), "{holding}");
        assert!(holding.contains("not extrapolated"), "{holding}");

        let stale = freshness_line(Freshness::Stale { age: 30.0 });
        assert!(stale.contains("STALE"));
        assert!(!stale.contains("LIVE"));
    }

    /// A signal with no source must say so in words, not show a plausible zero.
    #[test]
    fn a_missing_signal_is_named_on_screen() {
        let (hk, health, cmds) =
            (Housekeeping::default(), LinkHealth::default(), CommandLoop::default());
        let text = render(&data(Freshness::NoData, &hk, &health, &cmds));
        assert!(text.contains("attitude"));
        assert!(text.contains("-- no source --"), "{text}");
        assert!(text.contains("SAMPLE_APP.CommandCounter % 4"));
    }

    /// Never-heard-from must not print as zero: an operator reading "cmd 0"
    /// concludes the app is idle, not that it is absent.
    #[test]
    fn an_unheard_application_prints_dashes_not_zeroes() {
        let (health, cmds) = (LinkHealth::default(), CommandLoop::default());
        let hk = Housekeeping::default();
        let text = render(&data(Freshness::NoData, &hk, &health, &cmds));
        assert!(text.contains("SAMPLE_APP  --"), "{text}");

        let hk = Housekeeping {
            sample_app: Some(SampleAppHk { command_counter: 0, command_error_counter: 0 }),
            ci_lab: Some(CiLabHk { socket_connected: 1, ..Default::default() }),
            ..Default::default()
        };
        let text = render(&data(Freshness::Live, &hk, &health, &cmds));
        assert!(text.contains("SAMPLE_APP  cmd   0"), "{text}");
    }

    #[test]
    fn a_broken_rig_leads_the_panel() {
        let (hk, health, cmds) =
            (Housekeeping::default(), LinkHealth::default(), CommandLoop::default());
        let mut d = data(Freshness::Live, &hk, &health, &cmds);
        d.rig_error = Some("RIG UNBOUND: Antenna (expected 1, bound 0)".into());
        let text = render(&d);
        assert!(text.starts_with("!! RIG UNBOUND"), "{text}");
    }

    /// The panel names the Phase 3 mechanism for each signal, so the slice reads
    /// as the answer to Phase 3 rather than as an unrelated demo.
    #[test]
    fn every_signal_shows_the_mechanism_driving_it() {
        let (hk, health, cmds) =
            (Housekeeping::default(), LinkHealth::default(), CommandLoop::default());
        let text = render(&data(Freshness::Live, &hk, &health, &cmds));
        for mechanism in ["direct transform", "clip seek", "graph blend", "material emissive"] {
            assert!(text.contains(mechanism), "{mechanism} missing from panel:\n{text}");
        }
    }
}
