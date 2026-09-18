//! Generates `assets/spacecraft.gltf` — the model the Phase 3 animation spike drives.
//!
//! # Why generate it instead of shipping a binary asset
//!
//! The investigation's claim is about *mechanisms*, not artwork, and a
//! checked-in `.glb` is opaque: you cannot diff it, review it, or tell whether a
//! failing animation is the mapping's fault or the model's. Here the rig is
//! source code — joint axes, rest poses and keyframe timings are all readable
//! and reviewable, and `cargo run -p gltf-gen` reproduces the asset exactly.
//!
//! # What it stands in for
//!
//! In a real pipeline an artist authors the deploy motion in Blender. The
//! `Deploy` clip below imitates the properties that matter for the comparison:
//! it is **staged** (the outer panel does not unfold until the inner one has
//! swung clear) and **eased** (smoothstep, not linear). Those are precisely the
//! qualities that a "just compute the joint angles in Rust" mapping has to
//! reimplement by hand, so omitting them would rig the comparison in favour of
//! direct drive.
//!
//! # Rig
//!
//! ```text
//! SpacecraftRoot          <- vehicle attitude (quaternion, all three mappings)
//! |- Bus
//! |- ArrayRoot.L / .R     <- fixed; .R is yawed 180 deg so both arrays share
//! |   |- Yoke.{L,R}           one set of local joint angles and one sampler
//! |   |   |- Panel.{L,R}.1  <- inner hinge, swings out
//! |   |       |- Panel.{L,R}.2  <- outer hinge, unfolds
//! |- Antenna              <- mode-dependent pose (graph blending)
//! |- Indicator            <- non-Transform demo: emissive + visibility
//! ```
//!
//! Yoke rotation is about local X (sun tracking); panel hinges are about local Y.

use std::f32::consts::PI;
use std::fmt::Write as _;

use telemetry_anim::{Mode, deploy_angles_at, mode_pose, rig};

const D2R: f32 = PI / 180.0;

fn quat(axis: [f32; 3], deg: f32) -> [f32; 4] {
    let h = deg * D2R * 0.5;
    let s = h.sin();
    [axis[0] * s, axis[1] * s, axis[2] * s, h.cos()]
}

// ---------------------------------------------------------------- buffer ----

#[derive(Default)]
struct Buf {
    data: Vec<u8>,
    views: Vec<String>,
    accessors: Vec<String>,
}

impl Buf {
    fn view(&mut self, bytes: &[u8], target: Option<u32>) -> usize {
        while !self.data.len().is_multiple_of(4) {
            self.data.push(0);
        }
        let offset = self.data.len();
        self.data.extend_from_slice(bytes);
        let t = target.map(|t| format!(r#","target":{t}"#)).unwrap_or_default();
        self.views.push(format!(
            r#"{{"buffer":0,"byteOffset":{offset},"byteLength":{}{t}}}"#,
            bytes.len()
        ));
        self.views.len() - 1
    }

    /// `ty` is the glTF accessor type: SCALAR, VEC3, VEC4.
    fn floats(&mut self, vals: &[f32], ty: &str, target: Option<u32>) -> usize {
        let per = match ty {
            "SCALAR" => 1,
            "VEC3" => 3,
            "VEC4" => 4,
            other => panic!("unsupported accessor type {other}"),
        };
        assert!(vals.len().is_multiple_of(per), "{ty} accessor needs a multiple of {per} floats");
        let count = vals.len() / per;
        let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let view = self.view(&bytes, target);

        // min/max is required on POSITION and on every animation sampler input;
        // emitting it everywhere is cheap and keeps validators quiet.
        let mut min = vec![f32::INFINITY; per];
        let mut max = vec![f32::NEG_INFINITY; per];
        for chunk in vals.chunks(per) {
            for i in 0..per {
                min[i] = min[i].min(chunk[i]);
                max[i] = max[i].max(chunk[i]);
            }
        }
        let fmt = |v: &Vec<f32>| {
            v.iter().map(|x| fmt_f32(*x)).collect::<Vec<_>>().join(",")
        };
        self.accessors.push(format!(
            r#"{{"bufferView":{view},"componentType":5126,"count":{count},"type":"{ty}","min":[{}],"max":[{}]}}"#,
            fmt(&min),
            fmt(&max)
        ));
        self.accessors.len() - 1
    }

    fn indices(&mut self, vals: &[u16]) -> usize {
        let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let view = self.view(&bytes, Some(34963));
        self.accessors.push(format!(
            r#"{{"bufferView":{view},"componentType":5123,"count":{},"type":"SCALAR"}}"#,
            vals.len()
        ));
        self.accessors.len() - 1
    }
}

/// Shortest round-trippable rendering. `{:?}` on f32 gives that, but emits
/// `1.0` where JSON is happier with `1.0` anyway, and `inf` never occurs here.
fn fmt_f32(v: f32) -> String {
    let v = if v == 0.0 { 0.0 } else { v }; // normalise -0.0
    format!("{v:?}")
}

// ------------------------------------------------------------------ mesh ----

/// Unit cube centred on the origin, one normal per face (24 verts, 36 indices).
fn cube() -> (Vec<f32>, Vec<f32>, Vec<u16>) {
    let mut pos = Vec::new();
    let mut nrm = Vec::new();
    let mut idx = Vec::new();
    for axis in 0..3usize {
        for sign in [1.0f32, -1.0] {
            let mut n = [0.0f32; 3];
            n[axis] = sign;
            // Swapping the tangent basis for the negative face keeps every
            // face wound counter-clockwise seen from outside.
            let (ua, va) = if sign > 0.0 {
                ((axis + 1) % 3, (axis + 2) % 3)
            } else {
                ((axis + 2) % 3, (axis + 1) % 3)
            };
            let base = (pos.len() / 3) as u16;
            for (su, sv) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let mut p = [0.0f32; 3];
                p[axis] = sign * 0.5;
                p[ua] = su * 0.5;
                p[va] = sv * 0.5;
                pos.extend_from_slice(&p);
                nrm.extend_from_slice(&n);
            }
            idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
    }
    (pos, nrm, idx)
}

// ----------------------------------------------------------------- nodes ----

#[derive(Default)]
struct Node {
    name: String,
    mesh: Option<usize>,
    children: Vec<usize>,
    t: Option<[f32; 3]>,
    r: Option<[f32; 4]>,
    s: Option<[f32; 3]>,
}

#[derive(Default)]
struct Scene {
    nodes: Vec<Node>,
}

impl Scene {
    fn add(&mut self, node: Node) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// A joint: named, transform-animated, no geometry of its own.
    fn joint(&mut self, name: &str, t: [f32; 3], r: [f32; 4]) -> usize {
        self.add(Node { name: name.into(), t: Some(t), r: Some(r), ..Default::default() })
    }

    /// Geometry hangs off a joint in its own node so that the box's scale never
    /// leaks onto the joint's children. Rigs that skip this are why a hinge
    /// sometimes stretches everything below it.
    fn geo(&mut self, name: &str, mesh: usize, t: [f32; 3], s: [f32; 3]) -> usize {
        self.add(Node {
            name: name.into(),
            mesh: Some(mesh),
            t: Some(t),
            s: Some(s),
            ..Default::default()
        })
    }

    fn json(&self) -> String {
        let mut out = String::new();
        for (i, n) in self.nodes.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let mut parts = vec![format!(r#""name":"{}""#, n.name)];
            if let Some(m) = n.mesh {
                parts.push(format!(r#""mesh":{m}"#));
            }
            if !n.children.is_empty() {
                let c: Vec<String> = n.children.iter().map(|c| c.to_string()).collect();
                parts.push(format!(r#""children":[{}]"#, c.join(",")));
            }
            let arr = |v: &[f32]| v.iter().map(|x| fmt_f32(*x)).collect::<Vec<_>>().join(",");
            if let Some(t) = n.t {
                parts.push(format!(r#""translation":[{}]"#, arr(&t)));
            }
            if let Some(r) = n.r {
                parts.push(format!(r#""rotation":[{}]"#, arr(&r)));
            }
            if let Some(s) = n.s {
                parts.push(format!(r#""scale":[{}]"#, arr(&s)));
            }
            let _ = write!(out, "{{{}}}", parts.join(","));
        }
        out
    }
}

// ------------------------------------------------------------ animations ----

struct Sampler {
    input: usize,
    output: usize,
}

struct Channel {
    node: usize,
    path: &'static str,
    sampler: usize,
}

struct Anim {
    name: String,
    samplers: Vec<Sampler>,
    channels: Vec<Channel>,
}

impl Anim {
    fn json(&self) -> String {
        let s: Vec<String> = self
            .samplers
            .iter()
            .map(|s| {
                format!(
                    r#"{{"input":{},"output":{},"interpolation":"LINEAR"}}"#,
                    s.input, s.output
                )
            })
            .collect();
        let c: Vec<String> = self
            .channels
            .iter()
            .map(|c| {
                format!(
                    r#"{{"sampler":{},"target":{{"node":{},"path":"{}"}}}}"#,
                    c.sampler, c.node, c.path
                )
            })
            .collect();
        format!(
            r#"{{"name":"{}","samplers":[{}],"channels":[{}]}}"#,
            self.name,
            s.join(","),
            c.join(",")
        )
    }
}

// ---------------------------------------------------------------- base64 ----

fn base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

// ------------------------------------------------------------------ main ----

fn main() {
    let mut buf = Buf::default();

    let (pos, nrm, idx) = cube();
    let a_pos = buf.floats(&pos, "VEC3", Some(34962));
    let a_nrm = buf.floats(&nrm, "VEC3", Some(34962));
    let a_idx = buf.indices(&idx);

    // One primitive shape, three materials. Sharing the accessors keeps the
    // buffer tiny; separate meshes are how glTF assigns a material per node.
    let meshes: Vec<String> = ["BusMesh", "PanelMesh", "IndicatorMesh"]
        .iter()
        .enumerate()
        .map(|(mat, name)| {
            format!(
                r#"{{"name":"{name}","primitives":[{{"attributes":{{"POSITION":{a_pos},"NORMAL":{a_nrm}}},"indices":{a_idx},"material":{mat}}}]}}"#
            )
        })
        .collect();
    let (bus_mesh, panel_mesh, ind_mesh) = (0usize, 1usize, 2usize);

    let mut sc = Scene::default();

    let bus_geo = sc.geo("Bus", bus_mesh, [0.0, 0.0, 0.0], [0.8, 0.8, 1.0]);

    // Antenna: a joint (mode-blended) carrying a flat dish.
    let antenna = sc.joint("Antenna", [0.0, 0.45, -0.3], quat([1.0, 0.0, 0.0], 0.0));
    let antenna_geo = sc.geo("AntennaDish", bus_mesh, [0.0, 0.25, 0.0], [0.5, 0.06, 0.5]);
    sc.nodes[antenna].children.push(antenna_geo);

    // Indicator: driven by material/visibility, not transform, except for the
    // mode-pose scale pulse.
    let indicator = sc.joint("Indicator", [0.0, 0.0, 0.55], quat([1.0, 0.0, 0.0], 0.0));
    let indicator_geo = sc.geo("IndicatorLamp", ind_mesh, [0.0, 0.0, 0.0], [0.12, 0.12, 0.12]);
    sc.nodes[indicator].children.push(indicator_geo);

    // Both wings, built identically in local space. ArrayRoot.R is yawed 180
    // degrees, which is what lets one sampler drive both sides.
    let mut wings = Vec::new();
    let mut panel_joints = Vec::new(); // (inner, outer) per side
    for side in ["L", "R"] {
        let yaw = if side == "L" { 0.0 } else { 180.0 };
        let root = sc.joint(&format!("ArrayRoot.{side}"), [0.0, 0.0, 0.0], quat([0.0, 1.0, 0.0], yaw));

        let yoke = sc.joint(&format!("Yoke.{side}"), [-0.45, 0.0, 0.0], quat([1.0, 0.0, 0.0], 0.0));
        let yoke_geo = sc.geo(&format!("YokeArm.{side}"), bus_mesh, [-0.15, 0.0, 0.0], [0.3, 0.08, 0.08]);

        let p1 = sc.joint(
            &format!("Panel.{side}.1"),
            [-0.3, 0.0, 0.0],
            quat([0.0, 1.0, 0.0], rig::PANEL1_STOWED_DEG),
        );
        let p1_geo = sc.geo(&format!("PanelGeo.{side}.1"), panel_mesh, [-0.5, 0.0, 0.0], [1.0, 0.02, 0.7]);

        let p2 = sc.joint(
            &format!("Panel.{side}.2"),
            [-1.0, 0.0, 0.0],
            quat([0.0, 1.0, 0.0], rig::PANEL2_STOWED_DEG),
        );
        let p2_geo = sc.geo(&format!("PanelGeo.{side}.2"), panel_mesh, [-0.5, 0.0, 0.0], [1.0, 0.02, 0.7]);

        sc.nodes[p2].children.push(p2_geo);
        sc.nodes[p1].children.extend_from_slice(&[p1_geo, p2]);
        sc.nodes[yoke].children.extend_from_slice(&[yoke_geo, p1]);
        sc.nodes[root].children.push(yoke);

        wings.push(root);
        panel_joints.push((p1, p2));
    }

    let mut root_children = vec![bus_geo, antenna, indicator];
    root_children.extend_from_slice(&wings);
    let root = sc.add(Node {
        name: "SpacecraftRoot".into(),
        children: root_children,
        t: Some([0.0, 0.0, 0.0]),
        r: Some(quat([0.0, 1.0, 0.0], 0.0)),
        ..Default::default()
    });

    // --- Deploy: the authored clip the seek mapping consumes ---------------
    //
    // Staged on purpose. The inner hinge finishes at t=1.2 and the outer one
    // does not start until t=0.8, so the panels never sweep through each other.
    // Any mapping that computes joint angles directly has to re-encode this
    // overlap and both easing curves by hand; that is the cost being measured.
    // Knot times span each hinge's window; the angle at every knot comes from
    // telemetry-anim's continuous curve, so the clip is a sampling of the same
    // motion the direct-drive mapping computes rather than a second authoring
    // of it. What the two still disagree about is the interpolation *between*
    // knots, and that residue is measured in telemetry-anim's tests.
    let knot_times = |(start, end): (f32, f32)| -> Vec<f32> {
        (0..rig::KNOTS)
            .map(|i| start + (end - start) * i as f32 / (rig::KNOTS - 1) as f32)
            .collect()
    };
    let p1_times = knot_times(rig::PANEL1_WINDOW_S);
    let p2_times = knot_times(rig::PANEL2_WINDOW_S);
    let p1_rot: Vec<f32> =
        p1_times.iter().flat_map(|t| quat([0.0, 1.0, 0.0], deploy_angles_at(*t).0)).collect();
    let p2_rot: Vec<f32> =
        p2_times.iter().flat_map(|t| quat([0.0, 1.0, 0.0], deploy_angles_at(*t).1)).collect();
    assert_eq!(
        *p2_times.last().unwrap(),
        rig::DEPLOY_DURATION_S,
        "the last keyframe must land on the declared clip duration"
    );

    let s_p1 = Sampler {
        input: buf.floats(&p1_times, "SCALAR", None),
        output: buf.floats(&p1_rot, "VEC4", None),
    };
    let s_p2 = Sampler {
        input: buf.floats(&p2_times, "SCALAR", None),
        output: buf.floats(&p2_rot, "VEC4", None),
    };

    let deploy = Anim {
        name: "Deploy".into(),
        samplers: vec![s_p1, s_p2],
        channels: panel_joints
            .iter()
            .flat_map(|(a, b)| {
                [
                    Channel { node: *a, path: "rotation", sampler: 0 },
                    Channel { node: *b, path: "rotation", sampler: 1 },
                ]
            })
            .collect(),
    };

    // --- Mode poses: what the animation graph blends between ---------------
    //
    // Two identical keyframes rather than one. A single-key sampler gives the
    // clip zero duration, which several runtimes treat as "already finished".
    let hold = buf.floats(&[0.0, 1.0], "SCALAR", None);
    let mut anims = vec![deploy];
    for (name, mode) in [
        ("ModeSafe", Mode::Safe),
        ("ModeNominal", Mode::Nominal),
        ("ModeDeploying", Mode::Deploying),
        ("ModeDeployed", Mode::Deployed),
    ] {
        let (antenna_deg, lamp_scale) = mode_pose(mode);
        let q = quat([1.0, 0.0, 0.0], antenna_deg);
        let rot: Vec<f32> = [q, q].iter().flatten().copied().collect();
        let scl = vec![lamp_scale, lamp_scale, lamp_scale, lamp_scale, lamp_scale, lamp_scale];
        anims.push(Anim {
            name: name.into(),
            samplers: vec![
                Sampler { input: hold, output: buf.floats(&rot, "VEC4", None) },
                Sampler { input: hold, output: buf.floats(&scl, "VEC3", None) },
            ],
            channels: vec![
                Channel { node: antenna, path: "rotation", sampler: 0 },
                Channel { node: indicator, path: "scale", sampler: 1 },
            ],
        });
    }

    // --- assemble ----------------------------------------------------------
    let materials = [
        r#"{"name":"Bus","pbrMetallicRoughness":{"baseColorFactor":[0.72,0.73,0.75,1],"metallicFactor":0.6,"roughnessFactor":0.45}}"#,
        r#"{"name":"Panel","pbrMetallicRoughness":{"baseColorFactor":[0.08,0.13,0.35,1],"metallicFactor":0.3,"roughnessFactor":0.35}}"#,
        // emissiveFactor is non-zero in the asset so the spike is modulating an
        // existing channel rather than discovering whether one exists.
        r#"{"name":"Indicator","pbrMetallicRoughness":{"baseColorFactor":[0.9,0.15,0.1,1],"metallicFactor":0,"roughnessFactor":0.6},"emissiveFactor":[1,0.2,0.1]}"#,
    ];

    let uri = format!("data:application/octet-stream;base64,{}", base64(&buf.data));
    let gltf = format!(
        concat!(
            r#"{{"asset":{{"version":"2.0","generator":"cFS-investigation/gltf-gen"}},"#,
            r#""scene":0,"scenes":[{{"name":"Spacecraft","nodes":[{root}]}}],"#,
            r#""nodes":[{nodes}],"#,
            r#""meshes":[{meshes}],"#,
            r#""materials":[{materials}],"#,
            r#""animations":[{anims}],"#,
            r#""accessors":[{accessors}],"#,
            r#""bufferViews":[{views}],"#,
            r#""buffers":[{{"byteLength":{len},"uri":"{uri}"}}]}}"#
        ),
        root = root,
        nodes = sc.json(),
        meshes = meshes.join(","),
        materials = materials.join(","),
        anims = anims.iter().map(Anim::json).collect::<Vec<_>>().join(","),
        accessors = buf.accessors.join(","),
        views = buf.views.join(","),
        len = buf.data.len(),
        uri = uri,
    );

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/spacecraft.gltf");
    std::fs::write(&path, &gltf).expect("write spacecraft.gltf");
    println!(
        "wrote {} ({} bytes gltf, {} bytes buffer, {} nodes, {} animations)",
        path.display(),
        gltf.len(),
        buf.data.len(),
        sc.nodes.len(),
        anims.len()
    );
}
