//! Loading `assets/spacecraft.gltf`, binding its joints, and refusing to
//! pretend when a joint is missing.
//!
//! Binding is by glTF node *name*, which Phase 3 flagged as the weak point of
//! the whole approach: rename a joint in the modelling tool and the mapping
//! silently unbinds. There is no compile error, no asset error and no runtime
//! error — the joint simply stops moving, and on a rig with a dozen joints
//! nobody notices which one.
//!
//! So this module does what finding 0004 said Phase 4 should do: it enumerates
//! the names it expects, counts what it actually bound, and puts a failure on
//! the screen. A viz that silently stops animating a mechanism is worse than
//! one that refuses to start.

use bevy::animation::{RepeatAnimation, graph::AnimationNodeIndex};
use bevy::gltf::{Gltf, GltfAssetLabel};
use bevy::prelude::*;

use telemetry_anim::{MODE_CLIPS, MODE_COUNT, rig};

pub const MODEL: &str = "spacecraft.gltf";

// -------------------------------------------------------------- markers ---

/// The glTF scene's own root node. Carries vehicle attitude.
#[derive(Component)]
pub struct VehicleRoot;
/// Sun-tracking joint, one per wing.
#[derive(Component)]
pub struct Yoke;
#[derive(Component)]
pub struct PanelInner;
#[derive(Component)]
pub struct PanelOuter;
#[derive(Component)]
pub struct AntennaJoint;
#[derive(Component)]
pub struct LampJoint;
#[derive(Component)]
pub struct LampGeo;

/// Every joint the drivers expect, with how many of it the rig has.
///
/// The counts matter as much as the names: one wing binding and the other not
/// is a real and very confusing failure, and a presence-only check would pass.
const EXPECTED: [(&str, usize); 7] = [
    ("SpacecraftRoot", 1),
    ("Yoke.{L,R}", 2),
    ("Panel.*.1", 2),
    ("Panel.*.2", 2),
    ("Antenna", 1),
    ("Indicator", 1),
    ("IndicatorLamp", 1),
];

/// Graph node indices for the rig's `AnimationPlayer`.
#[derive(Component)]
pub struct RigAnimations {
    pub deploy: AnimationNodeIndex,
    pub modes: [AnimationNodeIndex; MODE_COUNT],
}

#[derive(Resource)]
pub struct Model(pub Handle<Gltf>);

/// Deploy clip length, read from the asset rather than assumed.
#[derive(Resource, Debug)]
pub struct DeployClip {
    pub duration_s: f32,
}

/// Built graph waiting for an `AnimationPlayer` to attach it to.
#[derive(Resource)]
pub struct PendingGraph {
    handle: Handle<AnimationGraph>,
    deploy: AnimationNodeIndex,
    modes: [AnimationNodeIndex; MODE_COUNT],
}

/// Frames to let the scene finish streaming before judging it.
///
/// The glTF scene arrives over several frames, so a check that ran the instant
/// the animation graph existed would report joints missing that were about to
/// appear. Waiting is not a fudge: the alternative is a false alarm, and a
/// false alarm on a "your rig is broken" banner is how such a banner gets
/// ignored.
const SETTLE_FRAMES: u32 = 30;

/// Frames elapsed since the graph was built.
#[derive(Resource, Default)]
pub struct RigSettle(u32);

/// What the rig check found. `Ok` only after every expected joint is bound.
#[derive(Resource, Debug, Default, PartialEq, Eq)]
pub enum RigStatus {
    #[default]
    Loading,
    Ok,
    /// Names that did not bind, with expected and actual counts.
    Broken(Vec<String>),
}

impl RigStatus {
    pub fn message(&self) -> Option<String> {
        match self {
            RigStatus::Broken(missing) => {
                Some(format!("RIG UNBOUND: {} - regenerate with `cargo run -p gltf-gen`", missing.join(", ")))
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------- setup ---

pub fn spawn_scene(commands: &mut Commands, assets: &AssetServer) {
    commands.insert_resource(Model(assets.load(MODEL)));
    commands.spawn((
        WorldAssetRoot(assets.load(GltfAssetLabel::Scene(0).from_asset(MODEL))),
        Transform::IDENTITY,
    ));
}

/// Tag rig nodes by glTF node name as the scene streams in.
pub fn tag_nodes(mut commands: Commands, added: Query<(Entity, &Name), Added<Name>>) {
    for (entity, name) in &added {
        let n = name.as_str();
        let mut e = commands.entity(entity);
        if n.starts_with("Yoke.") {
            e.insert(Yoke);
        } else if n.starts_with("Panel.") && n.ends_with(".1") {
            e.insert(PanelInner);
        } else if n.starts_with("Panel.") && n.ends_with(".2") {
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

/// Build the animation graph once the clips exist.
///
/// The deploy clip goes in at full weight and is then paused and seeked; the
/// four mode clips go in at weight zero, because their weights are the blend
/// and belong in exactly one place.
pub fn build_graph(
    mut commands: Commands,
    model: Option<Res<Model>>,
    gltfs: Res<Assets<Gltf>>,
    clips: Res<Assets<AnimationClip>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    existing: Option<Res<PendingGraph>>,
) {
    if existing.is_some() {
        return;
    }
    let Some(model) = model else { return };
    let Some(gltf) = gltfs.get(&model.0) else { return };

    let Some(deploy) = gltf.named_animations.get("Deploy") else {
        error!("asset has no `Deploy` clip; regenerate it with `cargo run -p gltf-gen`");
        return;
    };
    let Some(deploy_clip) = clips.get(deploy) else { return };

    let duration_s = deploy_clip.duration();
    if (duration_s - rig::DEPLOY_DURATION_S).abs() > 1e-3 {
        warn!(
            "clip duration {duration_s}s differs from rig::DEPLOY_DURATION_S ({}s)",
            rig::DEPLOY_DURATION_S
        );
    }

    let mut mode_handles = Vec::new();
    for name in MODE_CLIPS {
        let Some(h) = gltf.named_animations.get(name) else {
            error!("asset has no `{name}` clip");
            return;
        };
        mode_handles.push(h.clone());
    }

    let mut graph = AnimationGraph::new();
    let deploy_idx = graph.add_clip(deploy.clone(), 1.0, graph.root);
    let modes: Vec<_> =
        mode_handles.iter().map(|h| graph.add_clip(h.clone(), 0.0, graph.root)).collect();

    commands.insert_resource(DeployClip { duration_s });
    commands.insert_resource(PendingGraph {
        handle: graphs.add(graph),
        deploy: deploy_idx,
        modes: modes.try_into().expect("MODE_CLIPS length"),
    });
}

/// Hand the graph to the `AnimationPlayer` the glTF loader inserted.
pub fn attach_player(
    mut commands: Commands,
    players: Query<Entity, (With<AnimationPlayer>, Without<RigAnimations>)>,
    pending: Option<Res<PendingGraph>>,
) {
    let Some(pending) = pending else { return };
    for entity in &players {
        let mut player = AnimationPlayer::default();
        // Paused and then seeked every frame. Bevy evaluates a paused
        // animation's pose normally and only stops advancing its clock, which
        // is exactly what a lookup table needs.
        player.play(pending.deploy).pause();
        for idx in pending.modes {
            player.play(idx).set_weight(0.0).set_repeat(RepeatAnimation::Forever);
        }
        commands.entity(entity).insert((
            player,
            AnimationGraphHandle(pending.handle.clone()),
            RigAnimations { deploy: pending.deploy, modes: pending.modes },
        ));
    }
}

/// Count the bound joints once and report what is missing.
///
/// Runs only after the graph exists, which is the point at which the scene has
/// finished streaming in. Checking earlier would report every joint missing on
/// the first frame.
#[allow(clippy::too_many_arguments)]
pub fn check_rig(
    mut status: ResMut<RigStatus>,
    mut settle: ResMut<RigSettle>,
    graph: Option<Res<PendingGraph>>,
    roots: Query<(), With<VehicleRoot>>,
    yokes: Query<(), With<Yoke>>,
    inner: Query<(), With<PanelInner>>,
    outer: Query<(), With<PanelOuter>>,
    antenna: Query<(), With<AntennaJoint>>,
    lamp_joint: Query<(), With<LampJoint>>,
    lamp_geo: Query<(), With<LampGeo>>,
) {
    if *status != RigStatus::Loading || graph.is_none() {
        return;
    }
    settle.0 += 1;
    if settle.0 < SETTLE_FRAMES {
        return;
    }
    let found = [
        roots.iter().count(),
        yokes.iter().count(),
        inner.iter().count(),
        outer.iter().count(),
        antenna.iter().count(),
        lamp_joint.iter().count(),
        lamp_geo.iter().count(),
    ];
    let missing: Vec<String> = EXPECTED
        .iter()
        .zip(found)
        .filter(|((_, want), got)| got != want)
        .map(|((name, want), got)| format!("{name} (expected {want}, bound {got})"))
        .collect();

    if missing.is_empty() {
        *status = RigStatus::Ok;
    } else {
        error!("rig binding incomplete: {}", missing.join(", "));
        *status = RigStatus::Broken(missing);
    }
}
