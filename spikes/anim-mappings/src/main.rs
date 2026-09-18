//! Phase 3 spike: three ways to drive one rig from one telemetry stream.
//!
//! Three copies of `assets/spacecraft.gltf` stand side by side, all reading the
//! same [`Telemetry`] resource in the same frame. Adjacent columns differ in
//! exactly one mechanism, which is what makes the comparison readable:
//!
//! | Column | Solar yoke | Deploy mechanism | Mode pose |
//! |--------|-----------|------------------|-----------|
//! | Direct | transform | transform, angles computed in Rust | snapped |
//! | ClipSeek | transform | authored clip, seeked by telemetry | snapped |
//! | GraphBlend | transform | authored clip, seeked by telemetry | `AnimationGraph` weights |
//!
//! So Direct vs ClipSeek isolates the *mechanism* question, and ClipSeek vs
//! GraphBlend isolates the *discrete mode* question. The yoke is transform-driven
//! everywhere on purpose: continuous sun tracking has no authored clip to seek,
//! and pretending otherwise would have hidden the most important result — that a
//! real scene mixes all three.
//!
//! Run it:
//!
//! ```text
//! cargo run -p anim-mappings                       # synthetic telemetry
//! cargo run -p anim-mappings -- --live             # a running cFS / fake-cfs
//! cargo run -p anim-mappings -- --screenshot out.png --at 6.0
//! ```

use bevy::animation::{RepeatAnimation, graph::AnimationNodeIndex};
use bevy::app::AnimationSystems;
use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::image::Image;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::gltf::{Gltf, GltfAssetLabel};
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use bevy::window::WindowResolution;

use bevy_cfs::{CfsPlugin, CfsSet, Telemetry, TelemetryBuffer};
use telemetry_anim::{
    Lamp, MODE_CLIPS, MODE_COUNT, ModeBlend, clip_seek_time, deploy_angles, lamp, mode_pose,
    normalized, rig,
};
use telemetry_model::{Freshness, Mode, Quat as TQuat, SpacecraftState};

const MODEL: &str = "spacecraft.gltf";
const VIEW_W: u32 = 1440;
const VIEW_H: u32 = 640;
const COLUMN_SPACING: f32 = 3.6;
/// How long a mode pose takes to cross-fade in the blended column.
const MODE_TRANSITION_S: f32 = 0.6;

// ------------------------------------------------------------------- args ---

#[derive(Resource, Debug, Clone)]
struct Args {
    live: bool,
    screenshot: Option<String>,
    /// Timeline position to capture at, seconds.
    at: Option<f32>,
    exit_after: Option<f32>,
}

fn parse_args() -> Args {
    let mut args =
        Args { live: false, screenshot: None, at: None, exit_after: None };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--live" => args.live = true,
            "--screenshot" => args.screenshot = it.next(),
            "--at" => args.at = it.next().and_then(|v| v.parse().ok()),
            "--exit-after" => args.exit_after = it.next().and_then(|v| v.parse().ok()),
            "--help" | "-h" => {
                println!(
                    "anim-mappings [--live] [--screenshot PATH] [--at SECONDS] [--exit-after SECONDS]"
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

// --------------------------------------------------------------- markers ---

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
enum Mapping {
    Direct,
    ClipSeek,
    GraphBlend,
}

impl Mapping {
    const ALL: [Mapping; 3] = [Mapping::Direct, Mapping::ClipSeek, Mapping::GraphBlend];

    fn label(self) -> &'static str {
        match self {
            Mapping::Direct => "1. DIRECT TRANSFORM",
            Mapping::ClipSeek => "2. CLIP AS LOOKUP TABLE",
            Mapping::GraphBlend => "3. ANIMATION GRAPH BLEND",
        }
    }

    /// Does this column let the animation system own the mode pose?
    fn blends_modes(self) -> bool {
        matches!(self, Mapping::GraphBlend)
    }

    /// Does this column drive the panels itself rather than seeking a clip?
    fn drives_panels(self) -> bool {
        matches!(self, Mapping::Direct)
    }
}

/// Marks the column root. The rig nodes below it are found by walking up.
#[derive(Component, Clone, Copy)]
struct Column(Mapping);

#[derive(Component)]
struct Yoke;
#[derive(Component)]
struct PanelInner;
#[derive(Component)]
struct PanelOuter;
#[derive(Component)]
struct AntennaJoint;
#[derive(Component)]
struct LampJoint;
#[derive(Component)]
struct LampGeo;
/// The glTF scene's own root node, which carries vehicle attitude.
#[derive(Component)]
struct VehicleRoot;

/// Graph node indices for a column's `AnimationPlayer`.
#[derive(Component, Default)]
struct RigAnimations {
    deploy: Option<AnimationNodeIndex>,
    modes: Option<[AnimationNodeIndex; MODE_COUNT]>,
}

// ------------------------------------------------------------- resources ---

#[derive(Resource)]
struct Model(Handle<Gltf>);

/// Clip duration read from the asset rather than assumed.
#[derive(Resource, Debug)]
struct DeployClip {
    duration_s: f32,
}

#[derive(Resource)]
struct Blend(ModeBlend);

/// The clock the whole spike runs on.
///
/// Screenshots step it at a fixed rate instead of using wall time. Freezing the
/// telemetry value would have been simpler but useless: `ModeBlend` is a
/// stateful cross-fade, so a frozen mode always settles and the blended column
/// would look identical to the snapping ones in every capture. Replaying the
/// timeline deterministically means a capture at t=2.05s shows a fade genuinely
/// in progress, and shows the same one every run.
#[derive(Resource)]
struct Timeline {
    t: f32,
    last_dt: f32,
    /// `Some` in capture mode: the fixed step, and where to stop.
    step: Option<f32>,
    target: Option<f32>,
}

impl Timeline {
    fn at_target(&self) -> bool {
        self.target.is_some_and(|target| self.t >= target - 1e-6)
    }
}

#[derive(Resource, Default)]
struct Wiring {
    graphs_built: bool,
}

/// Offscreen colour target used for screenshots.
///
/// `Screenshot::primary_window` reads back the swapchain, and a macOS window
/// that is not frontmost is not composited — so the capture succeeds and writes
/// a solid black PNG. Rendering into an `Image` and capturing that bypasses the
/// compositor entirely.
#[derive(Resource)]
struct CaptureTarget(Handle<Image>);

/// Screenshot sequencing: wait for the scene, settle, capture, quit.
#[derive(Resource)]
struct Capture {
    path: String,
    settled_frames: u32,
    shot: bool,
}

// ------------------------------------------------------------------ main ---

fn main() {
    let args = parse_args();

    // Bevy resolves `assets/` against the running crate's manifest directory,
    // which in a workspace is the crate, not the root. Point it at the shared
    // asset folder explicitly so `cargo run -p anim-mappings` works from
    // anywhere.
    let asset_root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets");

    let mut app = App::new();
    app.add_plugins(DefaultPlugins
        .set(AssetPlugin { file_path: asset_root.to_string(), ..default() })
        .set(WindowPlugin {
            primary_window: Some(Window {
                title: "cFS -> Bevy: three telemetry-to-animation mappings".into(),
                resolution: WindowResolution::new(VIEW_W, VIEW_H),
                ..default()
            }),
            ..default()
        }));

    // The same plugin the headless tests use. `connect: false` keeps the socket
    // shut for synthetic runs so a screenshot is reproducible and a missing cFS
    // is not an error.
    app.add_plugins(CfsPlugin { connect: args.live, ..default() });

    app.insert_resource(Timeline {
        t: 0.0,
        last_dt: 0.0,
        step: args.screenshot.as_ref().map(|_| 1.0 / 60.0),
        target: args.screenshot.as_ref().and(args.at),
    })
    .insert_resource(Blend(ModeBlend::new(Mode::Safe, MODE_TRANSITION_S)))
        .insert_resource(ClearColor(Color::srgb(0.02, 0.03, 0.05)))
        .init_resource::<Wiring>()
        .insert_resource(args.clone())
        .configure_sets(Update, DriveSet.after(CfsSet::Sample))
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                build_graphs,
                tag_rig_nodes,
                attach_players.after(build_graphs).after(tag_rig_nodes),
            ),
        );

    app.add_systems(Update, advance_timeline.before(DriveSet));
    if !args.live {
        // After the plugin publishes its own (empty) state, so this wins; before
        // the drivers, so they see it in the same frame.
        app.add_systems(
            Update,
            synthetic_telemetry.after(CfsSet::Sample).after(advance_timeline).before(DriveSet),
        );
    }

    app.add_systems(
        Update,
        (
            drive_attitude,
            drive_yoke,
            drive_direct_panels,
            drive_clip_seek,
            drive_mode_blend,
            drive_mode_snap,
            drive_lamp,
        )
            .in_set(DriveSet),
    );

    // The readout compares a value this crate computed against one the
    // animation system wrote, so it has to run after the animation system —
    // in PostUpdate, not Update. Reading it a frame early silently inflated
    // the measured divergence by the distance the mechanism moves in one
    // frame, which looked exactly like a real finding.
    app.add_systems(PostUpdate, update_readout.after(AnimationSystems));

    if let Some(path) = args.screenshot.clone() {
        app.insert_resource(Capture { path, settled_frames: 0, shot: false })
            .add_systems(Update, capture_when_ready);
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

/// Everything that reads `Telemetry` and writes to the scene.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DriveSet;

fn setup(
    mut commands: Commands,
    assets: Res<AssetServer>,
    args: Res<Args>,
    mut images: ResMut<Assets<Image>>,
) {
    commands.insert_resource(Model(assets.load(MODEL)));

    let target = match args.screenshot {
        Some(_) => {
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
        None => RenderTarget::default(),
    };

    let camera = commands
        .spawn((
            Camera3d::default(),
            target,
            Transform::from_xyz(0.0, 1.9, 7.4).looking_at(Vec3::new(0.0, 0.15, 0.0), Vec3::Y),
        ))
        .id();
    commands.spawn((
        DirectionalLight { illuminance: 8_000.0, shadow_maps_enabled: true, ..default() },
        Transform::from_xyz(4.0, 8.0, 6.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    // 0.19 moved ambient light onto the camera; `GlobalAmbientLight` is the
    // resource-shaped fallback.
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(0.6, 0.7, 1.0),
        brightness: 220.0,
        ..default()
    });

    for (i, mapping) in Mapping::ALL.iter().enumerate() {
        let x = (i as f32 - 1.0) * COLUMN_SPACING;
        let column = commands
            .spawn((Column(*mapping), Transform::from_xyz(x, 0.0, 0.0), Visibility::default()))
            .id();
        commands.spawn((
            WorldAssetRoot(assets.load(GltfAssetLabel::Scene(0).from_asset(MODEL))),
            Transform::IDENTITY,
            ChildOf(column),
        ));

        commands.spawn((
            UiTargetCamera(camera),
            Text::new(mapping.label()),
            TextFont { font_size: FontSize::Px(15.0), ..default() },
            TextColor(Color::srgb(0.75, 0.85, 1.0)),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Percent(4.0 + i as f32 * 32.0),
                top: Val::Px(12.0),
                ..default()
            },
        ));
    }

    commands.spawn((
        Readout,
        UiTargetCamera(camera),
        Text::new("waiting for telemetry"),
        TextFont { font_size: FontSize::Px(13.0), ..default() },
        TextColor(Color::srgb(0.55, 0.62, 0.7)),
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(16.0),
            bottom: Val::Px(12.0),
            ..default()
        },
    ));
}

#[derive(Component)]
struct Readout;

// ------------------------------------------------------- synthetic source ---

/// A deterministic telemetry stand-in.
///
/// Deliberately not a straight ramp: it holds in Safe, deploys, then holds in
/// Deployed, so the mode transition and the end stops are both visible without
/// waiting for a real vehicle to do something interesting.
fn synthetic_state(t: f32) -> SpacecraftState {
    let progress = ((t - 2.0) / 8.0).clamp(0.0, 1.0);
    let mode = if t < 1.5 {
        Mode::Safe
    } else if t < 2.0 {
        Mode::Nominal
    } else if progress < 1.0 {
        Mode::Deploying
    } else {
        Mode::Deployed
    };
    let yaw = t * 0.18;
    SpacecraftState {
        attitude: TQuat([0.0, (yaw * 0.5).sin(), 0.0, (yaw * 0.5).cos()]),
        solar_array_deg: t * 12.0,
        deploy_progress: progress,
        wheel_rpm: [t * 40.0, -t * 25.0, t * 10.0, 0.0],
        mode,
    }
}

/// Advance the timeline, but only once the rig exists.
///
/// Waiting matters in capture mode: asset loading takes an unpredictable number
/// of frames, and stepping the clock through them would make the captured
/// moment depend on disk speed.
fn advance_timeline(
    time: Res<Time>,
    clip: Option<Res<DeployClip>>,
    mut timeline: ResMut<Timeline>,
) {
    if clip.is_none() || timeline.at_target() {
        timeline.last_dt = 0.0;
        return;
    }
    let dt = timeline.step.unwrap_or_else(|| time.delta_secs());
    let next = match timeline.target {
        Some(target) => (timeline.t + dt).min(target),
        None => timeline.t + dt,
    };
    timeline.last_dt = next - timeline.t;
    timeline.t = next;
}

fn synthetic_telemetry(timeline: Res<Timeline>, mut tlm: ResMut<Telemetry>) {
    tlm.state = synthetic_state(timeline.t);
    tlm.freshness = Freshness::Live;
}

// -------------------------------------------------------------- rig setup ---

fn column_of(
    mut entity: Entity,
    parents: &Query<&ChildOf>,
    columns: &Query<&Column>,
) -> Option<Mapping> {
    loop {
        if let Ok(c) = columns.get(entity) {
            return Some(c.0);
        }
        entity = parents.get(entity).ok()?.0;
    }
}

/// Tag rig nodes by glTF node name as the scene streams in.
///
/// Name matching is the weak point of this whole approach and it is worth
/// stating plainly: a rename in Blender silently unbinds the mapping, with no
/// compile error and no runtime error — the joint simply stops moving. Phase 4
/// should fail loudly when an expected name is missing.
fn tag_rig_nodes(
    mut commands: Commands,
    added: Query<(Entity, &Name), Added<Name>>,
    parents: Query<&ChildOf>,
    columns: Query<&Column>,
) {
    for (entity, name) in &added {
        if column_of(entity, &parents, &columns).is_none() {
            continue;
        }
        let n = name.as_str();
        let mut e = commands.entity(entity);
        if n.starts_with("Yoke.") {
            e.insert(Yoke);
        } else if n.ends_with(".1") && n.starts_with("Panel.") {
            e.insert(PanelInner);
        } else if n.ends_with(".2") && n.starts_with("Panel.") {
            e.insert(PanelOuter);
        } else if n == "Antenna" {
            e.insert(AntennaJoint);
        } else if n == "Indicator" {
            e.insert(LampJoint);
        } else if n == "IndicatorLamp" {
            e.insert(LampGeo);
        } else if n == "SpacecraftRoot" {
            e.insert(VehicleRoot);
        }
    }
}

/// Build one animation graph per mapping, once the glTF's clips exist.
fn build_graphs(
    mut commands: Commands,
    model: Option<Res<Model>>,
    gltfs: Res<Assets<Gltf>>,
    clips: Res<Assets<AnimationClip>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    mut wiring: ResMut<Wiring>,
) {
    if wiring.graphs_built {
        return;
    }
    let Some(model) = model else { return };
    let Some(gltf) = gltfs.get(&model.0) else { return };

    let Some(deploy) = gltf.named_animations.get("Deploy") else {
        error!("asset has no `Deploy` clip; regenerate it with `cargo run -p gltf-gen`");
        return;
    };
    let Some(deploy_clip) = clips.get(deploy) else { return };

    // Read the duration from the asset instead of trusting the constant. If an
    // artist lengthens the clip, the seek mapping follows automatically — which
    // is one of its advantages, and worth proving rather than asserting.
    let duration_s = deploy_clip.duration();
    if (duration_s - rig::DEPLOY_DURATION_S).abs() > 1e-3 {
        warn!(
            "clip duration {duration_s}s differs from rig::DEPLOY_DURATION_S \
             ({}s); direct drive and clip seek will disagree",
            rig::DEPLOY_DURATION_S
        );
    }
    commands.insert_resource(DeployClip { duration_s });

    let mut mode_handles = Vec::new();
    for name in MODE_CLIPS {
        let Some(h) = gltf.named_animations.get(name) else {
            error!("asset has no `{name}` clip");
            return;
        };
        mode_handles.push(h.clone());
    }

    let mut built = Vec::new();
    for mapping in Mapping::ALL {
        let mut graph = AnimationGraph::new();
        let mut rig_anim = RigAnimations::default();

        if !mapping.drives_panels() {
            rig_anim.deploy = Some(graph.add_clip(deploy.clone(), 1.0, graph.root));
        }
        if mapping.blends_modes() {
            let idx: Vec<_> = mode_handles
                .iter()
                // Weight 0 at the node; the per-frame weight comes from
                // ActiveAnimation::set_weight, so there is exactly one place
                // that decides how much of each pose is showing.
                .map(|h| graph.add_clip(h.clone(), 0.0, graph.root))
                .collect();
            rig_anim.modes = Some(idx.try_into().expect("MODE_CLIPS length"));
        }

        let handle = graphs.add(graph);
        built.push(PendingGraph(mapping, handle, rig_anim.deploy, rig_anim.modes));
    }
    commands.insert_resource(PendingGraphs(built));
    wiring.graphs_built = true;
}

/// One per mapping; consumed by `attach_players`.
#[derive(Resource)]
struct PendingGraph(
    Mapping,
    Handle<AnimationGraph>,
    Option<AnimationNodeIndex>,
    Option<[AnimationNodeIndex; MODE_COUNT]>,
);

/// Give each column's `AnimationPlayer` its graph and start the clips it needs.
///
/// The Direct column gets no graph at all. That is not an oversight: it is the
/// point of the column, and it means the animation system does no work for it.
fn attach_players(
    mut commands: Commands,
    players: Query<Entity, (With<AnimationPlayer>, Without<RigAnimations>)>,
    parents: Query<&ChildOf>,
    columns: Query<&Column>,
    graph_res: Option<Res<PendingGraphs>>,
) {
    let Some(graphs) = graph_res else { return };
    for entity in &players {
        let Some(mapping) = column_of(entity, &parents, &columns) else { continue };
        let Some(g) = graphs.0.iter().find(|g| g.0 == mapping) else { continue };

        let mut player = AnimationPlayer::default();
        if let Some(deploy) = g.2 {
            // Paused, then seeked every frame. Bevy evaluates a paused
            // animation's pose normally and only skips advancing its clock and
            // firing its events, which is exactly the behaviour a lookup table
            // needs.
            player.play(deploy).pause();
        }
        if let Some(modes) = g.3 {
            for idx in modes {
                player.play(idx).set_weight(0.0).set_repeat(RepeatAnimation::Forever);
            }
        }
        commands.entity(entity).insert((
            player,
            AnimationGraphHandle(g.1.clone()),
            RigAnimations { deploy: g.2, modes: g.3 },
        ));
    }
}

#[derive(Resource)]
struct PendingGraphs(Vec<PendingGraph>);

// --------------------------------------------------------------- drivers ---

/// Vehicle attitude, in every column.
///
/// The clearest case for direct transform drive and the reason it cannot simply
/// be replaced by the other two: a quaternion from an estimator is a continuous
/// value with no authored counterpart, no end stops and no discrete states. It
/// is written straight to the transform, and the interesting work already
/// happened upstream in the jitter buffer's `nlerp`.
fn drive_attitude(
    tlm: Res<Telemetry>,
    columns: Query<&Column>,
    parents: Query<&ChildOf>,
    roots: Query<Entity, With<VehicleRoot>>,
    mut transforms: Query<&mut Transform>,
) {
    let a = tlm.state.attitude.0;
    for entity in &roots {
        if column_of(entity, &parents, &columns).is_some()
            && let Ok(mut t) = transforms.get_mut(entity)
        {
            t.rotation = Quat::from_xyzw(a[0], a[1], a[2], a[3]);
        }
    }
}

/// Continuous sun tracking, in every column. No clip could express it: the
/// angle is unbounded and the vehicle never repeats a cycle exactly.
fn drive_yoke(
    tlm: Res<Telemetry>,
    columns: Query<&Column>,
    parents: Query<&ChildOf>,
    mut yokes: Query<(Entity, &mut Transform), With<Yoke>>,
) {
    for (entity, mut transform) in &mut yokes {
        if column_of(entity, &parents, &columns).is_none() {
            continue;
        }
        transform.rotation = Quat::from_rotation_x(tlm.state.solar_array_deg.to_radians());
    }
}

/// The Direct column recomputes the staged deploy in Rust.
fn drive_direct_panels(
    tlm: Res<Telemetry>,
    columns: Query<&Column>,
    parents: Query<&ChildOf>,
    mut inner: Query<&mut Transform, (With<PanelInner>, Without<PanelOuter>)>,
    mut outer: Query<&mut Transform, (With<PanelOuter>, Without<PanelInner>)>,
    inner_ids: Query<Entity, With<PanelInner>>,
    outer_ids: Query<Entity, With<PanelOuter>>,
) {
    let (inner_deg, outer_deg) = deploy_angles(tlm.state.deploy_progress);
    for entity in &inner_ids {
        if column_of(entity, &parents, &columns) == Some(Mapping::Direct)
            && let Ok(mut t) = inner.get_mut(entity)
        {
            t.rotation = Quat::from_rotation_y(inner_deg.to_radians());
        }
    }
    for entity in &outer_ids {
        if column_of(entity, &parents, &columns) == Some(Mapping::Direct)
            && let Ok(mut t) = outer.get_mut(entity)
        {
            t.rotation = Quat::from_rotation_y(outer_deg.to_radians());
        }
    }
}

/// The seek mapping: one line of logic, and the artist owns everything else.
fn drive_clip_seek(
    tlm: Res<Telemetry>,
    clip: Option<Res<DeployClip>>,
    mut players: Query<(&mut AnimationPlayer, &RigAnimations)>,
) {
    let Some(clip) = clip else { return };
    let t = clip_seek_time(tlm.state.deploy_progress, clip.duration_s);
    for (mut player, rig) in &mut players {
        if let Some(deploy) = rig.deploy
            && let Some(active) = player.animation_mut(deploy)
        {
            active.seek_to(t);
        }
    }
}

fn drive_mode_blend(
    timeline: Res<Timeline>,
    tlm: Res<Telemetry>,
    mut blend: ResMut<Blend>,
    mut players: Query<(&mut AnimationPlayer, &RigAnimations)>,
) {
    blend.0.set_target(tlm.state.mode);
    blend.0.advance(timeline.last_dt);
    let weights = *blend.0.weights();
    for (mut player, rig) in &mut players {
        let Some(modes) = rig.modes else { continue };
        for (idx, w) in modes.iter().zip(weights.iter()) {
            if let Some(active) = player.animation_mut(*idx) {
                active.set_weight(*w);
            }
        }
    }
}

/// The columns without a graph snap straight to the mode's pose.
///
/// This is the comparison that matters for discrete signals: put it next to the
/// blended column and the snap is unmistakable at every mode change.
fn drive_mode_snap(
    tlm: Res<Telemetry>,
    columns: Query<&Column>,
    parents: Query<&ChildOf>,
    antennas: Query<Entity, With<AntennaJoint>>,
    lamps: Query<Entity, With<LampJoint>>,
    mut transforms: Query<&mut Transform>,
) {
    let (antenna_deg, lamp_scale) = mode_pose(tlm.state.mode);
    for entity in &antennas {
        if column_of(entity, &parents, &columns).is_some_and(|m| !m.blends_modes())
            && let Ok(mut t) = transforms.get_mut(entity)
        {
            t.rotation = Quat::from_rotation_x(antenna_deg.to_radians());
        }
    }
    for entity in &lamps {
        if column_of(entity, &parents, &columns).is_some_and(|m| !m.blends_modes())
            && let Ok(mut t) = transforms.get_mut(entity)
        {
            t.scale = Vec3::splat(lamp_scale);
        }
    }
}

/// Non-`Transform` animation: visibility and material emissive.
///
/// The material handle is shared by all three scene instances, so one write
/// changes every column. That is fine here — the lamp reports link health, which
/// is the same for all of them — but it is a trap worth naming: per-instance
/// appearance requires cloning the material asset, which the scene loader will
/// not do for you.
fn drive_lamp(
    timeline: Res<Timeline>,
    tlm: Res<Telemetry>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut geos: Query<(&MeshMaterial3d<StandardMaterial>, &mut Visibility), With<LampGeo>>,
) {
    let phase = timeline.t;
    let Lamp { visible, emissive_gain } = lamp(tlm.state.mode, tlm.freshness, phase);

    // A second non-transform channel, from a continuous signal rather than a
    // discrete one: wheel speed brightens the lamp's base glow.
    let spin = normalized(
        tlm.state.wheel_rpm.iter().fold(0.0f32, |a, b| a.max(if *b < 0.0 { -*b } else { *b })),
        0.0,
        400.0,
    );

    for (material, mut visibility) in &mut geos {
        *visibility = if visible { Visibility::Inherited } else { Visibility::Hidden };
        if let Some(mut mat) = materials.get_mut(&material.0) {
            let gain = emissive_gain * (0.4 + 0.6 * spin);
            mat.emissive = LinearRgba::new(6.0 * gain, 1.2 * gain, 0.6 * gain, 1.0);
        }
    }
}

// --------------------------------------------------------------- readout ---

/// Reads the clip-driven joint back out of the scene and prints it next to the
/// directly computed one.
///
/// This is the measurement the whole spike exists to make. Both columns are
/// built from the same constants and the same curve, so any difference is the
/// mechanism itself — Bevy interpolating a sparse clip linearly between
/// keyframes, against a continuously evaluated function. Reading it from the
/// `Transform` rather than recomputing it means the number is what the renderer
/// actually drew.
#[allow(clippy::too_many_arguments)]
fn update_readout(
    timeline: Res<Timeline>,
    tlm: Res<Telemetry>,
    blend: Res<Blend>,
    buffer: Option<Res<TelemetryBuffer>>,
    columns: Query<&Column>,
    parents: Query<&ChildOf>,
    panels: Query<(Entity, &Transform), With<PanelInner>>,
    mut readout: Query<&mut Text, With<Readout>>,
) {
    let Ok(mut text) = readout.single_mut() else { return };
    let s = &tlm.state;
    let (inner, _outer) = deploy_angles(s.deploy_progress);
    let w = blend.0.weights();
    let buffered = buffer.map(|b| b.0.len()).unwrap_or(0);

    let clip_inner = panels
        .iter()
        .find(|(e, _)| column_of(*e, &parents, &columns) == Some(Mapping::ClipSeek))
        .map(|(_, t)| t.rotation.to_euler(EulerRot::YXZ).0.to_degrees());

    let divergence = clip_inner
        .map(|c| format!("{:+.2}deg", inner - c))
        .unwrap_or_else(|| "--".into());

    **text = format!(
        "t {:5.2}s  {:?} ({:?})  progress {:.3}  yoke {:.0}deg  |  inner panel  \
         direct {:.2}deg  clip {}  diff {divergence}  |  seek {:.3}s  \
         w [{:.2} {:.2} {:.2} {:.2}]  buf {buffered}",
        timeline.t,
        s.mode,
        tlm.freshness,
        s.deploy_progress,
        s.solar_array_deg % 360.0,
        inner,
        clip_inner.map(|c| format!("{c:.2}deg")).unwrap_or_else(|| "--".into()),
        clip_seek_time(s.deploy_progress, rig::DEPLOY_DURATION_S),
        w[0],
        w[1],
        w[2],
        w[3],
    );
}

fn capture_when_ready(
    mut commands: Commands,
    mut capture: ResMut<Capture>,
    target: Res<CaptureTarget>,
    timeline: Res<Timeline>,
    yokes: Query<(), With<Yoke>>,
    mut exit: MessageWriter<AppExit>,
) {
    if capture.shot {
        capture.settled_frames += 1;
        // A few frames after the request so the file is flushed before exit.
        if capture.settled_frames > 40 {
            exit.write(AppExit::Success);
        }
        return;
    }
    // Two yokes per column, three columns.
    if !timeline.at_target() || yokes.iter().count() < 6 {
        return;
    }
    let path = capture.path.clone();
    println!("capturing {path}");
    commands.spawn(Screenshot::image(target.0.clone())).observe(save_to_disk(path));
    capture.shot = true;
    capture.settled_frames = 0;
}
