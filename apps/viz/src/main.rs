//! Phase 4 vertical slice: one spacecraft, driven by a live cFS, with a
//! command path back.
//!
//! This is the deliverable Architecture A was scoped around — a ground-side
//! Bevy visualizer that is a telemetry consumer over UDP, with no Rust inside
//! the flight build. Everything below it was built for this: the codec
//! (Phase 1), the jitter buffer (Phase 2) and the mapping table (Phase 3).
//!
//! What it demonstrates, in order of how hard each was to get right:
//!
//! 1. **Live telemetry from containerized cFS**, decoded into typed values.
//! 2. **Rate matching** — 1-4 Hz housekeeping driving a 60 Hz render without
//!    stepping, and never extrapolating past the newest sample.
//! 3. **A closed command loop** — a keypress becomes a real `SAMPLE_APP` no-op
//!    on the software bus, and the counter it increments comes back on the
//!    downlink and moves the model. The round-trip time is measured and shown.
//! 4. **Honest sourcing** — the panel names where every animated signal came
//!    from, including the ones stock cFS does not supply at all.
//!
//! ```text
//! cargo run -p viz                                   # localhost cFS or fake-cfs
//! cargo run -p viz -- --dest-ip 192.168.65.254       # cFS in Docker Desktop
//! cargo run -p viz -- --offline                      # no socket, synthetic
//! cargo run -p viz -- --offline --screenshot out.png --at 6.0
//! cargo run -p viz -- --offline --frames dir/ --from 8 --to 30 --fps 20
//! cargo run -p viz -- --dest-ip 192.168.65.254 --noop-every 4
//! ```
//!
//! `--noop-every` drives the command path on a timer instead of from the
//! keyboard. It exists for two unglamorous reasons: a screen recording needs
//! both hands free, and a round trip that only happens when someone presses a
//! key cannot be verified from a terminal.
//!
//! `--dest-ip` is the address of *this machine as cFS sees it*, which is what
//! `to_lab` is told to send telemetry to. Inside Docker Desktop that is the
//! host gateway; see `docker/README.md`.

mod command_loop;
mod drive;
mod panel;
mod rig;
mod sources;

use std::net::SocketAddr;

use bevy::app::AnimationSystems;
use bevy::prelude::*;
use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::image::Image;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use bevy::window::WindowResolution;

use bevy_cfs::{CfsCommand, CfsPlugin, CfsSet, Housekeeping, LinkHealth, Telemetry};
use cfs_link::LinkConfig;
use telemetry_anim::ModeBlend;
use telemetry_model::{Freshness, Mode};

use command_loop::CommandLoop;
use drive::{Blend, Shown};
use rig::{RigSettle, RigStatus};
use sources::{Baseline, Pipeline};

// ------------------------------------------------------------------ args ---

#[derive(Resource, Debug, Clone)]
struct Args {
    cfs_host: String,
    cmd_port: u16,
    tlm_port: u16,
    dest_ip: String,
    offline: bool,
    screenshot: Option<String>,
    at: Option<f32>,
    /// Directory to write a numbered PNG sequence into.
    frames: Option<String>,
    /// Sequence bounds and sampling rate, seconds and frames per second.
    from: f32,
    to: f32,
    fps: f32,
    exit_after: Option<f32>,
    /// Send a `SAMPLE_APP` no-op this often, in seconds.
    noop_every: Option<f32>,
}

impl Args {
    /// Is the clock being driven by the capture rather than by wall time?
    fn capturing(&self) -> bool {
        self.screenshot.is_some() || self.frames.is_some()
    }

    fn endpoint(&self) -> String {
        if self.offline {
            "offline - no socket".to_string()
        } else {
            format!(
                "cmd {}:{}  tlm :{}  (to_lab -> {})",
                self.cfs_host, self.cmd_port, self.tlm_port, self.dest_ip
            )
        }
    }
}

fn parse_args() -> Args {
    let mut args = Args {
        cfs_host: "127.0.0.1".into(),
        cmd_port: cfs_link::DEFAULT_CMD_PORT,
        tlm_port: cfs_link::DEFAULT_TLM_PORT,
        dest_ip: "127.0.0.1".into(),
        offline: false,
        screenshot: None,
        at: None,
        frames: None,
        from: sources::OFFLINE_CLIP_FROM_S,
        to: sources::OFFLINE_CLIP_TO_S,
        fps: 20.0,
        exit_after: None,
        noop_every: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut next = || it.next().unwrap_or_default();
        match a.as_str() {
            "--cfs-host" => args.cfs_host = next(),
            "--cmd-port" => args.cmd_port = next().parse().unwrap_or(args.cmd_port),
            "--tlm-port" => args.tlm_port = next().parse().unwrap_or(args.tlm_port),
            "--dest-ip" => args.dest_ip = next(),
            "--offline" => args.offline = true,
            "--screenshot" => args.screenshot = it.next(),
            "--at" => args.at = it.next().and_then(|v| v.parse().ok()),
            "--frames" => args.frames = it.next(),
            "--from" => args.from = it.next().and_then(|v| v.parse().ok()).unwrap_or(args.from),
            "--to" => args.to = it.next().and_then(|v| v.parse().ok()).unwrap_or(args.to),
            "--fps" => args.fps = it.next().and_then(|v| v.parse().ok()).unwrap_or(args.fps),
            "--exit-after" => args.exit_after = it.next().and_then(|v| v.parse().ok()),
            "--noop-every" => args.noop_every = it.next().and_then(|v| v.parse().ok()),
            "--help" | "-h" => {
                println!(
                    "viz [--cfs-host HOST] [--cmd-port N] [--tlm-port N] [--dest-ip ADDR]\n    \
                     [--offline] [--screenshot PATH] [--at SECONDS] [--exit-after SECONDS]\n    \
                     [--frames DIR] [--from SECONDS] [--to SECONDS] [--fps N]\n    \
                     [--noop-every SECONDS]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    args
}

// ------------------------------------------------------------- resources ---

/// The clock the slice runs on.
///
/// Wall time normally; a fixed step in capture mode, and only once the rig has
/// loaded, so a screenshot lands on the same frame regardless of disk speed.
/// Phase 3 learned that freezing telemetry instead does not work: the mode
/// cross-fade is stateful, and a frozen mode always settles.
#[derive(Resource)]
struct Timeline {
    t: f32,
    step: Option<f32>,
    target: Option<f32>,
}

impl Timeline {
    fn at_target(&self) -> bool {
        self.target.is_some_and(|target| self.t >= target - 1e-6)
    }
}

/// Screenshot sequencing: wait for the rig, settle, capture, quit.
///
/// A recorded sequence is the same machinery with more than one entry. Offline,
/// the clock stops at each listed instant and only moves on once that frame has
/// been requested, so the recording is a function of the timeline rather than of
/// how fast this machine renders: a laptop that manages 9 fps and one that
/// manages 200 write the same frames. Against live cFS there is nothing to stop
/// — the entries are wall-clock instants and a frame is taken at the first
/// render past each one.
#[derive(Resource)]
struct Capture {
    /// `(timeline position, output path)`, in ascending order.
    frames: Vec<(f32, String)>,
    next: usize,
    settled_frames: u32,
}

/// Expand `--frames DIR --from A --to B --fps N` into the capture list.
///
/// Frame times are computed from the index rather than accumulated, so frame
/// 200 sits exactly where the fps says it does instead of wherever 200 additions
/// of `1.0 / fps` in `f32` happened to land.
fn frame_plan(args: &Args) -> Vec<(f32, String)> {
    if let Some(dir) = &args.frames {
        let dir = std::path::Path::new(dir);
        std::fs::create_dir_all(dir).expect("cannot create --frames directory");
        let count = (((args.to - args.from) * args.fps).round() as i32).max(0) as usize + 1;
        return (0..count)
            .map(|i| {
                let t = args.from + i as f32 / args.fps;
                (t, dir.join(format!("frame_{i:05}.png")).to_string_lossy().into_owned())
            })
            .collect();
    }
    let Some(path) = args.screenshot.clone() else { return Vec::new() };
    // Offline captures with no `--at` aim at the deployment, which is the one
    // moment on the timeline worth a still image.
    match args.at.or(args.offline.then_some(sources::OFFLINE_MID_DEPLOY_S)) {
        Some(at) => vec![(at, path)],
        None => Vec::new(),
    }
}

/// Window size, and the size of the offscreen image captures render into.
const VIEW_W: u32 = 1280;
const VIEW_H: u32 = 820;

/// Offscreen colour target used for screenshots.
///
/// Capturing the *window* is the obvious approach and it is not reliable:
/// `Screenshot::primary_window` reads back the swapchain, and a macOS window
/// that is not frontmost is not composited, so the file comes out solid black.
/// Nothing errors — the capture succeeds and the image is empty, which is
/// exactly the kind of quiet failure that gets committed. Rendering the camera
/// into an `Image` and capturing that does not involve the compositor at all,
/// so it works with the window buried, on a second desktop, or from a script.
#[derive(Resource)]
struct CaptureTarget(Handle<Image>);

/// Everything that reads telemetry and writes to the scene.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DriveSet;

// ---------------------------------------------------------------- framing ---

/// Radius of the sphere the rig sweeps out, in world units.
///
/// Measured from `tools/gltf-gen`'s own numbers rather than guessed: the outer
/// panel's far edge sits at `0.45` (yoke) `+ 0.3` (inner hinge) `+ 1.0` (outer
/// hinge) `+ 1.0` (panel half-length past its origin) = `2.75` from the vehicle
/// origin when deployed, and the vehicle rotates freely about that origin, so
/// the *sphere* of that radius is what has to stay in frame at every attitude.
///
/// This was found the unglamorous way. The camera was framed when the only
/// thing on screen was a stowed vehicle — radius about `1.2` — and the first
/// capture after the arrays started deploying themselves had both wings running
/// off the top and bottom of the image.
const RIG_RADIUS: f32 = 2.75;

/// Where the camera looks.
///
/// Left of the vehicle, not at it. The panel occupies the left third of the
/// window, so aiming off-centre pushes the spacecraft into the clear space on
/// the right instead of putting it behind the text.
const AIM: Vec3 = Vec3::new(-1.3, 0.0, 0.0);

/// Direction from [`AIM`] to the camera. A three-quarter view: enough off-axis
/// that the solar arrays read as a plane rather than a line, and enough above
/// that the deployment's hinge staging is visible.
const VIEW_DIR: Vec3 = Vec3::new(0.574, 0.223, 0.788);

/// Margin over the distance at which the rig exactly fills the frame.
///
/// Room for the horizontal offset [`AIM`] introduces, and so a fully deployed
/// vehicle does not touch the edges at any attitude.
const FRAMING_MARGIN: f32 = 1.35;

/// Distance from [`AIM`] to the camera, so that a sphere of [`RIG_RADIUS`]
/// fits the frame with [`FRAMING_MARGIN`] to spare.
///
/// Derived rather than written down, so that changing the rig or the field of
/// view moves the camera instead of silently cropping the vehicle.
fn camera_distance() -> f32 {
    // Bevy's default vertical field of view is 45°.
    let half_fov = std::f32::consts::FRAC_PI_8;
    RIG_RADIUS / half_fov.tan() * FRAMING_MARGIN
}

// ------------------------------------------------------------------ main ---

fn main() {
    let args = parse_args();

    // Bevy resolves `assets/` against the running crate's manifest directory,
    // which in a workspace is the crate and not the root.
    let asset_root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets");

    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(AssetPlugin { file_path: asset_root.to_string(), ..default() })
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: "cFS -> Bevy: live telemetry, and a command path back".into(),
                    resolution: WindowResolution::new(VIEW_W, VIEW_H),
                    ..default()
                }),
                ..default()
            }),
    );

    let cmd_addr: SocketAddr = format!("{}:{}", args.cfs_host, args.cmd_port)
        .parse()
        .unwrap_or_else(|_| {
            eprintln!("viz: bad cFS address {}:{}", args.cfs_host, args.cmd_port);
            std::process::exit(2);
        });

    let msg_ids = cfs_msg::MsgIds::LAB_DEFAULTS;
    app.add_plugins(CfsPlugin {
        link: LinkConfig {
            cmd_addr,
            tlm_bind: SocketAddr::from(([0, 0, 0, 0], args.tlm_port)),
            dest_ip: args.dest_ip.clone(),
            msg_ids,
            ..Default::default()
        },
        // The Rust flight application's vehicle-state message, which `fake-cfs`
        // also publishes on. One message ID for one payload layout, whoever
        // produced it.
        tlm_msg_id: msg_ids.rust_app_vehicle_tlm,
        connect: !args.offline,
        ..default()
    });

    let plan = frame_plan(&args);

    app.insert_resource(Timeline {
        t: 0.0,
        // A fixed step only makes a capture reproducible when the state is
        // generated from the same clock. Against a live cFS the state comes off
        // the wire, so stepping faster than wall time would just capture an
        // earlier moment — `--at` there means "after this many seconds".
        step: (args.capturing() && args.offline).then_some(1.0 / 60.0),
        target: plan.first().map(|&(t, _)| t),
    })
    .insert_resource(Blend(ModeBlend::new(Mode::Safe, drive::MODE_TRANSITION_S)))
    .insert_resource(Shown {
        state: Default::default(),
        freshness: Freshness::NoData,
    })
    .insert_resource(ClearColor(Color::srgb(0.02, 0.03, 0.05)))
    .init_resource::<RigStatus>()
    .init_resource::<RigSettle>()
    .init_resource::<Baseline>()
    .init_resource::<CurrentPipeline>()
    .init_resource::<CurrentSources>()
    .init_resource::<CommandLoop>()
    .insert_resource(args.clone())
    .add_systems(Startup, setup)
    .add_systems(
        Update,
        (rig::tag_nodes, rig::build_graph, rig::attach_player, rig::check_rig).chain(),
    );

    // Resolve the source before anything drives the scene, and after the plugin
    // has published this frame's interpolated state.
    app.configure_sets(Update, DriveSet.after(CfsSet::Sample));
    app.add_systems(Update, (advance_timeline, resolve_source).chain().before(DriveSet));
    app.add_systems(
        Update,
        (
            drive::attitude,
            drive::solar_array,
            drive::deploy,
            drive::mode,
            drive::mode_scale,
            drive::caution_lamp,
        )
            .in_set(DriveSet),
    );

    // Keyboard is the only thing that writes CfsCommand, and CfsSet::Command
    // sends it, so the keypress and the datagram happen in the same frame.
    app.add_systems(Update, command_keys.before(CfsSet::Command));
    if args.noop_every.is_some() {
        app.insert_resource(NextAutoNoop(0.0))
            .add_systems(Update, auto_noop.before(CfsSet::Command));
    }
    app.add_systems(Update, report_round_trips);

    // The panel reports the hinge angle as *drawn*, so it must run after the
    // animation system has written the transforms — PostUpdate, not Update.
    app.add_systems(PostUpdate, update_panel.after(AnimationSystems));

    if !plan.is_empty() {
        app.insert_resource(Capture { frames: plan, next: 0, settled_frames: 0 })
            // After the drivers: the screenshot reads the render target at the
            // end of the frame, and the frame has to be the one the drivers just
            // posed.
            .add_systems(Update, capture_when_ready.after(DriveSet));
    }
    if let Some(after) = args.exit_after {
        app.add_systems(Update, move |time: Res<Time>, mut exit: MessageWriter<AppExit>| {
            if time.elapsed_secs() >= after {
                exit.write(AppExit::Success);
            }
        });
    }

    app.run();
}

#[derive(Resource, Default)]
struct CurrentPipeline(Option<Pipeline>);

/// Wall-clock time of the next scheduled no-op.
#[derive(Resource)]
struct NextAutoNoop(f64);

/// The timed command driver behind `--noop-every`.
fn auto_noop(
    args: Res<Args>,
    time: Res<Time>,
    hk: Res<Housekeeping>,
    mut next: ResMut<NextAutoNoop>,
    mut loop_state: ResMut<CommandLoop>,
    mut out: MessageWriter<CfsCommand>,
) {
    let Some(period) = args.noop_every else { return };
    let now = time.elapsed_secs_f64();
    if now < next.0 {
        return;
    }
    next.0 = now + period.max(0.1) as f64;
    // Nothing to measure against until housekeeping has been seen once, and a
    // command sent into that gap would look lost. Wait for the first packet.
    if hk.sample_app.is_none() {
        return;
    }
    out.write(CfsCommand::SampleAppNoop);
    loop_state.on_sent(hk.sample_app, now);
}

/// Print each round trip, so the loop can be verified without watching the
/// window — which is what makes it checkable in CI, in a terminal, or from a
/// log after the fact.
fn report_round_trips(commands: Res<CommandLoop>, mut last: Local<u32>) {
    let done = commands.confirmed + commands.timed_out;
    if done == *last {
        return;
    }
    *last = done;
    match commands.last {
        Some(command_loop::Outcome::Confirmed { seconds }) => {
            println!("command loop closed: SAMPLE_APP CommandCounter changed {seconds:.2}s after send");
        }
        Some(command_loop::Outcome::TimedOut) => {
            println!("command loop TIMED OUT: no counter change within {}s", command_loop::TIMEOUT_S);
        }
        None => {}
    }
}

fn setup(
    mut commands: Commands,
    assets: Res<AssetServer>,
    args: Res<Args>,
    mut images: ResMut<Assets<Image>>,
) {
    rig::spawn_scene(&mut commands, &assets);

    let target = match args.capturing() {
        true => {
            let size = Extent3d { width: VIEW_W, height: VIEW_H, depth_or_array_layers: 1 };
            let mut image = Image::new_fill(
                size,
                TextureDimension::D2,
                &[0, 0, 0, 255],
                TextureFormat::Bgra8UnormSrgb,
                RenderAssetUsages::default(),
            );
            image.texture_descriptor.usage = TextureUsages::TEXTURE_BINDING
                | TextureUsages::COPY_DST
                | TextureUsages::COPY_SRC
                | TextureUsages::RENDER_ATTACHMENT;
            let handle = images.add(image);
            commands.insert_resource(CaptureTarget(handle.clone()));
            RenderTarget::Image(handle.into())
        }
        false => RenderTarget::default(),
    };

    // `RenderTarget` is its own component in 0.19; it is no longer a field of
    // `Camera`.
    let camera = commands
        .spawn((
            Camera3d::default(),
            target,
            Transform::from_translation(AIM + VIEW_DIR * camera_distance())
                .looking_at(AIM, Vec3::Y),
        ))
        .id();

    // UI has to be told which camera it belongs to. Bevy's default is the
    // camera rendering to the primary window, and in capture mode there is no
    // such camera — which is why the first offscreen capture came out with the
    // spacecraft and no panel.
    panel::spawn(&mut commands, camera);
    commands.spawn((
        DirectionalLight { illuminance: 8_000.0, shadow_maps_enabled: true, ..default() },
        Transform::from_xyz(4.0, 8.0, 6.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(0.6, 0.7, 1.0),
        brightness: 220.0,
        ..default()
    });
}

fn advance_timeline(
    time: Res<Time>,
    clip: Option<Res<rig::DeployClip>>,
    mut timeline: ResMut<Timeline>,
) {
    if clip.is_none() || timeline.at_target() {
        return;
    }
    let dt = timeline.step.unwrap_or_else(|| time.delta_secs());
    timeline.t = match timeline.target {
        Some(target) => (timeline.t + dt).min(target),
        None => timeline.t + dt,
    };
}

/// Decide what the scene is showing this frame, and where it came from.
#[allow(clippy::too_many_arguments)]
fn resolve_source(
    args: Res<Args>,
    timeline: Res<Timeline>,
    telemetry: Res<Telemetry>,
    hk: Res<Housekeeping>,
    mut baseline: ResMut<Baseline>,
    mut shown: ResMut<Shown>,
    mut pipeline: ResMut<CurrentPipeline>,
    mut sources: ResMut<CurrentSources>,
) {
    if args.offline {
        shown.state = sources::offline_state(timeline.t);
        shown.freshness = Freshness::Live;
        pipeline.0 = Some(Pipeline::Offline);
        sources.0 = Some(sources::Sources::offline());
        return;
    }

    let (state, resolved, which) = sources::resolve(&telemetry, &hk, &mut baseline);
    shown.state = state;
    // Freshness describes the *vehicle-state* stream. When the rig is driven by
    // derived housekeeping there is no such stream, so the lamp must not be
    // told the link is dead — it is the derivation that has no vehicle packet,
    // not the link that has no packets.
    shown.freshness = match which {
        Pipeline::Derived if hk.sample_app.is_some() => Freshness::Live,
        _ => telemetry.freshness,
    };
    pipeline.0 = Some(which);
    sources.0 = Some(resolved);
}

#[derive(Resource, Default)]
struct CurrentSources(Option<sources::Sources>);

/// Keys that fly the vehicle, as (key, function code, label for the panel).
///
/// These reach `RUST_APP` — the Rust application running inside cFE — and change
/// what the spacecraft is *doing*, not just what a counter reads. The round
/// trip is therefore visible as motion: press `T` and the model slews, because
/// flight software integrated a new attitude and published it.
pub const VEHICLE_KEYS: [(KeyCode, u8, &str); 6] = [
    (KeyCode::KeyT, cfs_msg::rust_app::NEXT_TARGET_CC, "[T] slew to next target"),
    (KeyCode::KeyH, cfs_msg::rust_app::HOLD_CC, "[H] inertial hold"),
    (KeyCode::KeyD, cfs_msg::rust_app::DEPLOY_CC, "[D] deploy arrays"),
    (KeyCode::KeyS, cfs_msg::rust_app::STOW_CC, "[S] stow arrays"),
    (KeyCode::KeyF, cfs_msg::rust_app::SAFE_CC, "[F] safe mode"),
    (KeyCode::KeyM, cfs_msg::rust_app::DUMP_MOMENTUM_CC, "[M] dump momentum"),
];

/// The command path. One keypress, one real packet to `ci_lab`.
fn command_keys(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    hk: Res<Housekeeping>,
    mut loop_state: ResMut<CommandLoop>,
    mut out: MessageWriter<CfsCommand>,
) {
    let now = time.elapsed_secs_f64();
    if keys.just_pressed(KeyCode::KeyN) {
        out.write(CfsCommand::SampleAppNoop);
        loop_state.on_sent(hk.sample_app, now);
    }
    if keys.just_pressed(KeyCode::KeyR) {
        out.write(CfsCommand::SampleAppResetCounters);
        loop_state.on_sent(hk.sample_app, now);
    }
    for (key, function_code, _) in VEHICLE_KEYS {
        if keys.just_pressed(key) {
            out.write(CfsCommand::RustApp(function_code));
        }
    }
    // Only the `sample_app` pair is tracked by the round-trip timer: its
    // CommandCounter is a scalar that provably changed, whereas "the vehicle
    // started slewing" is not something this can time without reimplementing
    // the controller's notion of when a command took effect.
    loop_state.observe(hk.sample_app, now);
}

#[allow(clippy::too_many_arguments)]
fn update_panel(
    args: Res<Args>,
    shown: Res<Shown>,
    hk: Res<Housekeeping>,
    health: Res<LinkHealth>,
    commands: Res<CommandLoop>,
    status: Res<RigStatus>,
    pipeline: Res<CurrentPipeline>,
    sources: Res<CurrentSources>,
    panels: drive::InnerPanels,
    mut text: Query<&mut Text, With<panel::PanelText>>,
) {
    let Ok(mut text) = text.single_mut() else { return };
    let data = panel::PanelData {
        endpoint: &args.endpoint(),
        pipeline: pipeline.0.unwrap_or(Pipeline::Waiting),
        sources: sources.0.unwrap_or(sources::Sources::standin()),
        state: shown.state,
        freshness: shown.freshness,
        health: &health,
        hk: &hk,
        commands: &commands,
        drawn_hinge_deg: drive::inner_hinge_deg(&panels),
        rig_error: status.message(),
    };
    **text = panel::render(&data);
}

fn capture_when_ready(
    mut commands: Commands,
    mut capture: ResMut<Capture>,
    target: Res<CaptureTarget>,
    mut timeline: ResMut<Timeline>,
    yokes: Query<(), With<rig::Yoke>>,
    mut exit: MessageWriter<AppExit>,
) {
    if capture.next >= capture.frames.len() {
        capture.settled_frames += 1;
        // `save_to_disk` encodes on a task pool, so exiting the instant the last
        // request is made truncates the tail of a sequence. Wait long enough for
        // the queue to drain — cheap, and the alternative failure is a run that
        // looks successful and is missing its last few frames.
        if capture.settled_frames > 120 {
            exit.write(AppExit::Success);
        }
        return;
    }
    if !timeline.at_target() || yokes.iter().count() < 2 {
        return;
    }
    let total = capture.frames.len();
    let (_, path) = capture.frames[capture.next].clone();
    if total == 1 {
        println!("capturing {path}");
    } else if capture.next % 20 == 0 || capture.next + 1 == total {
        println!("capturing frame {} of {total}", capture.next + 1);
    }
    commands.spawn(Screenshot::image(target.0.clone())).observe(save_to_disk(path));
    capture.next += 1;
    capture.settled_frames = 0;
    // Leave the clock parked on the last frame once the plan is exhausted, so
    // the settling frames render the same image rather than drifting onwards.
    if let Some(&(t, _)) = capture.frames.get(capture.next) {
        timeline.target = Some(t);
    }
}
