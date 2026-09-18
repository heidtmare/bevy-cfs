//! The Phase 3 recommendation table, applied.
//!
//! Finding 0004 compared three mechanisms side by side and ended with a
//! signal-type → mechanism table. This module is that table as code, and there
//! is deliberately nothing clever in it: the whole point of the Phase 3 spike
//! was to make Phase 4's animation layer boring.
//!
//! | Signal | Shape | Mechanism |
//! |---|---|---|
//! | attitude | continuous, unbounded, no authored counterpart | direct transform |
//! | solar array | continuous, unbounded | direct transform |
//! | deploy | authored mechanism, staged, bounded | clip seek |
//! | mode | discrete, few states, needs smoothing | `AnimationGraph` blend |
//! | wheels / link | not rigid-body motion at all | material + visibility |
//!
//! The last row is the one most easily forgotten. A viz that can only move
//! things cannot show a duty cycle, a caution annunciator or a stale link, and
//! most spacecraft telemetry is not rigid-body motion.

use bevy::prelude::*;

use telemetry_anim::{Lamp, ModeBlend, clip_seek_time, lamp, normalized};
use telemetry_model::Freshness;

use crate::rig::{AntennaJoint, LampGeo, LampJoint, PanelInner, PanelOuter, RigAnimations, Yoke};
use crate::rig::{DeployClip, VehicleRoot};

/// The inner hinge, excluding the joints that share its marker-free ancestors.
///
/// The `Without` bounds are Bevy's disjointness requirement, not a filter that
/// means anything here; naming the type keeps it out of two signatures.
pub type InnerPanels<'w, 's> =
    Query<'w, 's, &'static Transform, (With<PanelInner>, Without<PanelOuter>, Without<AntennaJoint>)>;

/// Seconds for a mode pose to cross-fade.
pub const MODE_TRANSITION_S: f32 = 0.6;

/// The vehicle state the scene is showing this frame, after source resolution.
#[derive(Resource, Debug, Clone, Copy)]
pub struct Shown {
    pub state: telemetry_model::SpacecraftState,
    pub freshness: Freshness,
}

#[derive(Resource)]
pub struct Blend(pub ModeBlend);

/// Attitude: straight to the transform.
///
/// The interesting work already happened upstream, in the jitter buffer's
/// `nlerp` between two real samples. Nothing here should smooth anything: a
/// second layer of smoothing on top of interpolated telemetry adds latency and
/// hides exactly the transients an operator is watching for.
pub fn attitude(shown: Res<Shown>, mut roots: Query<&mut Transform, With<VehicleRoot>>) {
    let a = shown.state.attitude.0;
    let q = Quat::from_xyzw(a[0], a[1], a[2], a[3]);
    for mut t in &mut roots {
        t.rotation = q;
    }
}

/// Sun tracking: continuous and unbounded, so no clip could express it.
pub fn solar_array(shown: Res<Shown>, mut yokes: Query<&mut Transform, With<Yoke>>) {
    let angle = Quat::from_rotation_x(shown.state.solar_array_deg.to_radians());
    for mut t in &mut yokes {
        t.rotation = angle;
    }
}

/// Deployment: seek the authored clip, and let the asset own the staging.
///
/// The two hinges overlap in time, ease in and out, and must not sweep through
/// each other. None of that is expressed here, which is the argument for this
/// mechanism: the choreography lives in the asset, where someone can change it
/// without touching Rust.
pub fn deploy(
    shown: Res<Shown>,
    clip: Option<Res<DeployClip>>,
    mut players: Query<(&mut AnimationPlayer, &RigAnimations)>,
) {
    let Some(clip) = clip else { return };
    let t = clip_seek_time(shown.state.deploy_progress, clip.duration_s);
    for (mut player, rig) in &mut players {
        if let Some(active) = player.animation_mut(rig.deploy) {
            active.seek_to(t);
        }
    }
}

/// Mode: cross-fade the authored poses.
///
/// `SpacecraftState::lerp` snaps the mode enum at the interpolation midpoint,
/// because a blended enum would name a state the vehicle was never in. The
/// smoothing belongs here instead, one layer up, where it is honest: the
/// reported mode still steps, and only the *pose* eases across.
pub fn mode(
    time: Res<Time>,
    shown: Res<Shown>,
    mut blend: ResMut<Blend>,
    mut players: Query<(&mut AnimationPlayer, &RigAnimations)>,
) {
    blend.0.set_target(shown.state.mode);
    blend.0.advance(time.delta_secs());
    let weights = *blend.0.weights();
    for (mut player, rig) in &mut players {
        for (idx, w) in rig.modes.iter().zip(weights.iter()) {
            if let Some(active) = player.animation_mut(*idx) {
                active.set_weight(*w);
            }
        }
    }
}

/// The mode also drives a joint the blend does not own, so a scale change is
/// visible even when every blend weight is in motion.
pub fn mode_scale(shown: Res<Shown>, mut lamps: Query<&mut Transform, With<LampJoint>>) {
    let (_, lamp_scale) = telemetry_anim::mode_pose(shown.state.mode);
    for mut t in &mut lamps {
        t.scale = Vec3::splat(lamp_scale);
    }
}

/// Non-transform output: the caution lamp.
///
/// Link health outranks vehicle mode here, deliberately. A caution light driven
/// by a frozen value is worse than no caution light, because it asserts a
/// condition the ground no longer knows to be true — so a stale link takes the
/// lamp over and blinks it slowly regardless of what the last mode was.
pub fn caution_lamp(
    time: Res<Time>,
    shown: Res<Shown>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut geos: Query<(&MeshMaterial3d<StandardMaterial>, &mut Visibility), With<LampGeo>>,
) {
    let Lamp { visible, emissive_gain } =
        lamp(shown.state.mode, shown.freshness, time.elapsed_secs());

    // A continuous signal on the same non-transform channel: wheel speed
    // brightens the base glow.
    let spin = normalized(
        shown.state.wheel_rpm.iter().fold(0.0f32, |a, b| a.max(b.abs())),
        0.0,
        1400.0,
    );

    for (material, mut visibility) in &mut geos {
        *visibility = if visible { Visibility::Inherited } else { Visibility::Hidden };
        if let Some(mut mat) = materials.get_mut(&material.0) {
            let gain = emissive_gain * (0.4 + 0.6 * spin);
            mat.emissive = LinearRgba::new(6.0 * gain, 1.2 * gain, 0.6 * gain, 1.0);
        }
    }
}

/// Read the deploy angle back out of the scene, for the panel.
///
/// Reading the `Transform` rather than recomputing the angle means the panel
/// reports what was actually drawn. Phase 3 learned this the hard way: a
/// readout that recomputes agrees with itself no matter what the renderer did.
pub fn inner_hinge_deg(panels: &InnerPanels<'_, '_>) -> Option<f32> {
    panels.iter().next().map(|t| t.rotation.to_euler(EulerRot::YXZ).0.to_degrees())
}
