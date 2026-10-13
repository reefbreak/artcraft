//! The 3D Stage's scene: stand-in objects (shapes, posable mannequins, imported models) with their
//! placement, and the camera. Saved as JSON so a stage survives a restart; imported models are
//! re-read from their files.

use std::path::{Path, PathBuf};

use glam::{EulerRot, Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

use super::render::{Tri, View};

/// Mesh detail for round shapes.
const SEGMENTS: usize = 24;
/// The floor's half-size (metres).
const FLOOR_HALF: f32 = 20.0;
pub const FLOOR_ID: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Shape {
  Cube,
  Sphere,
  Cylinder,
  Cone,
  Plane,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pose {
  #[default]
  Standing,
  Walking,
  Sitting,
  Pointing,
  ArmsOut,
}

impl Pose {
  pub const ALL: [Pose; 5] = [Pose::Standing, Pose::Walking, Pose::Sitting, Pose::Pointing, Pose::ArmsOut];

  pub fn label(self) -> &'static str {
    match self {
      Pose::Standing => "Standing",
      Pose::Walking => "Walking",
      Pose::Sitting => "Sitting",
      Pose::Pointing => "Pointing",
      Pose::ArmsOut => "Arms out",
    }
  }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Kind {
  Shape(Shape),
  Mannequin(Pose),
  /// A model file (GLB, glTF or OBJ), read when the stage loads.
  Model(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Object {
  pub name: String,
  pub kind: Kind,
  pub position: Vec3,
  /// Yaw, pitch, roll in degrees.
  pub rotation: Vec3,
  pub scale: Vec3,
  pub color: [u8; 3],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Camera {
  /// Orbit around `target`: yaw and pitch in degrees, distance in metres.
  pub yaw: f32,
  pub pitch: f32,
  pub distance: f32,
  pub target: Vec3,
  /// Lens in mm (35 mm full-frame equivalent).
  pub focal_mm: f32,
}

impl Default for Camera {
  fn default() -> Self {
    Self { yaw: 30.0, pitch: 12.0, distance: 6.0, target: Vec3::new(0.0, 0.9, 0.0), focal_mm: 35.0 }
  }
}

impl Camera {
  pub fn view(&self) -> View {
    let (yaw, pitch) = (self.yaw.to_radians(), self.pitch.to_radians());
    let offset = Vec3::new(yaw.sin() * pitch.cos(), pitch.sin(), yaw.cos() * pitch.cos()) * self.distance;
    // A full-frame sensor is 24 mm tall.
    View { eye: self.target + offset, target: self.target, fov_y: 2.0 * (12.0 / self.focal_mm.max(1.0)).atan() }
  }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scene {
  pub objects: Vec<Object>,
  pub camera: Camera,
}

impl Default for Scene {
  fn default() -> Self {
    let mut scene = Self { objects: Vec::new(), camera: Camera::default() };
    scene.add(Kind::Mannequin(Pose::Standing));
    scene
  }
}

impl Scene {
  pub fn load(path: &Path) -> Self {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
  }

  pub fn save(&self, path: &Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
      std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(self)?)?;
    Ok(())
  }

  /// Adds an object a little to the side of the others; returns its index.
  pub fn add(&mut self, kind: Kind) -> usize {
    let n = self.objects.len();
    let (name, color, scale) = match &kind {
      Kind::Shape(s) => (format!("{s:?}"), [176, 170, 160], Vec3::ONE),
      Kind::Mannequin(_) => ("Person".to_owned(), [200, 196, 188], Vec3::ONE),
      Kind::Model(path) => (path.file_stem().and_then(|s| s.to_str()).unwrap_or("Model").to_owned(), [180, 184, 190], Vec3::ONE),
    };
    let x = if n == 0 { 0.0 } else { ((n + 1) / 2) as f32 * if n % 2 == 0 { -1.3 } else { 1.3 } };
    let lift = if matches!(kind, Kind::Shape(Shape::Plane)) { 0.01 } else { 0.0 };
    self.objects.push(Object { name: format!("{name} {}", n + 1), kind, position: Vec3::new(x, lift, 0.0), rotation: Vec3::ZERO, scale, color });
    self.objects.len() - 1
  }

  /// Every triangle in world space, floor included.
  pub fn triangles(&self, models: &impl Fn(&Path) -> Option<Vec<[Vec3; 3]>>) -> Vec<Tri> {
    let mut out = floor();
    for (i, object) in self.objects.iter().enumerate() {
      let local = match &object.kind {
        Kind::Shape(shape) => shape_mesh(*shape),
        Kind::Mannequin(pose) => mannequin(*pose),
        Kind::Model(path) => models(path).unwrap_or_else(|| shape_mesh(Shape::Cube)),
      };
      let m = object.matrix();
      out.extend(local.iter().map(|t| Tri { v: t.map(|p| m.transform_point3(p)), color: object.color, id: i as u16 + 1, floor: false }));
    }
    out
  }
}

impl Object {
  pub fn matrix(&self) -> Mat4 {
    let r = self.rotation * (std::f32::consts::PI / 180.0);
    Mat4::from_scale_rotation_translation(self.scale, Quat::from_euler(EulerRot::YXZ, r.x, r.y, r.z), self.position)
  }
}

fn floor() -> Vec<Tri> {
  let h = FLOOR_HALF;
  let (a, b, c, d) = (Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, -h), Vec3::new(h, 0.0, h), Vec3::new(-h, 0.0, h));
  let color = [92, 94, 100];
  vec![Tri { v: [a, b, c], color, id: FLOOR_ID, floor: true }, Tri { v: [a, c, d], color, id: FLOOR_ID, floor: true }]
}

/// Unit shapes resting on the floor (1 m across, origin at the base).
pub fn shape_mesh(shape: Shape) -> Vec<[Vec3; 3]> {
  match shape {
    Shape::Cube => cuboid(Vec3::new(0.0, 0.5, 0.0), Vec3::splat(0.5)),
    Shape::Plane => {
      let (a, b, c, d) = (Vec3::new(-1.0, 0.0, -1.0), Vec3::new(1.0, 0.0, -1.0), Vec3::new(1.0, 0.0, 1.0), Vec3::new(-1.0, 0.0, 1.0));
      vec![[a, b, c], [a, c, d]]
    },
    Shape::Sphere => sphere(Vec3::new(0.0, 0.5, 0.0), 0.5),
    Shape::Cylinder => cylinder(Vec3::ZERO, Vec3::Y, 0.5, 0.5),
    Shape::Cone => cylinder(Vec3::ZERO, Vec3::Y, 0.5, 0.0),
  }
}

fn cuboid(center: Vec3, half: Vec3) -> Vec<[Vec3; 3]> {
  let c = |x: f32, y: f32, z: f32| center + half * Vec3::new(x, y, z);
  let faces = [[c(-1., -1., 1.), c(1., -1., 1.), c(1., 1., 1.), c(-1., 1., 1.)], [c(1., -1., -1.), c(-1., -1., -1.), c(-1., 1., -1.), c(1., 1., -1.)], [c(-1., 1., 1.), c(1., 1., 1.), c(1., 1., -1.), c(-1., 1., -1.)], [c(-1., -1., -1.), c(1., -1., -1.), c(1., -1., 1.), c(-1., -1., 1.)], [c(1., -1., 1.), c(1., -1., -1.), c(1., 1., -1.), c(1., 1., 1.)], [c(-1., -1., -1.), c(-1., -1., 1.), c(-1., 1., 1.), c(-1., 1., -1.)]];
  faces.iter().flat_map(|q| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]]).collect()
}

fn sphere(center: Vec3, r: f32) -> Vec<[Vec3; 3]> {
  let rings = SEGMENTS / 2;
  let point = |i: usize, j: usize| {
    let (theta, phi) = (std::f32::consts::PI * i as f32 / rings as f32, std::f32::consts::TAU * j as f32 / SEGMENTS as f32);
    center + Vec3::new(theta.sin() * phi.cos(), theta.cos(), theta.sin() * phi.sin()) * r
  };
  let mut out = Vec::new();
  for i in 0..rings {
    for j in 0..SEGMENTS {
      let (a, b, c, d) = (point(i, j), point(i + 1, j), point(i + 1, j + 1), point(i, j + 1));
      out.push([a, b, c]);
      out.push([a, c, d]);
    }
  }
  out
}

/// A tapered cylinder from `a` to `b` (radius `ra` at a, `rb` at b), capped.
fn cylinder(a: Vec3, b: Vec3, ra: f32, rb: f32) -> Vec<[Vec3; 3]> {
  let axis = (b - a).normalize_or(Vec3::Y);
  let side = if axis.y.abs() < 0.9 { axis.cross(Vec3::Y).normalize() } else { axis.cross(Vec3::X).normalize() };
  let other = axis.cross(side);
  let ring = |center: Vec3, r: f32, j: usize| {
    let t = std::f32::consts::TAU * j as f32 / SEGMENTS as f32;
    center + (side * t.cos() + other * t.sin()) * r
  };
  let mut out = Vec::new();
  for j in 0..SEGMENTS {
    let (p0, p1, q0, q1) = (ring(a, ra, j), ring(a, ra, j + 1), ring(b, rb, j), ring(b, rb, j + 1));
    out.push([p0, p1, q1]);
    out.push([p0, q1, q0]);
    out.push([a, p1, p0]);
    if rb > 0.0 {
      out.push([b, q0, q1]);
    }
  }
  out
}

/// A posable stand-in person, about 1.8 m tall, facing +Z.
pub fn mannequin(pose: Pose) -> Vec<[Vec3; 3]> {
  // Limb directions per pose: (left arm, right arm, left leg, right leg) as (upper, lower) unit
  // vectors in the body's frame (x left/right, y up, z forward).
  let down = Vec3::NEG_Y;
  let d = |x: f32, y: f32, z: f32| Vec3::new(x, y, z).normalize();
  let (la, ra, ll, rl, hip_y) = match pose {
    Pose::Standing => ((d(0.15, -1.0, 0.0), d(0.1, -1.0, 0.15)), (d(-0.15, -1.0, 0.0), d(-0.1, -1.0, 0.15)), (down, down), (down, down), 0.95),
    Pose::Walking => ((d(0.1, -1.0, -0.45), d(0.05, -1.0, 0.1)), (d(-0.1, -1.0, 0.45), d(-0.05, -0.8, 0.6)), (d(0.0, -1.0, 0.45), d(0.0, -1.0, -0.1)), (d(0.0, -1.0, -0.4), d(0.0, -1.0, -0.7)), 0.93),
    Pose::Sitting => ((d(0.1, -1.0, 0.3), d(0.0, -0.3, 1.0)), (d(-0.1, -1.0, 0.3), d(0.0, -0.3, 1.0)), (d(0.0, -0.05, 1.0), down), (d(0.0, -0.05, 1.0), down), 0.5),
    Pose::Pointing => ((d(0.15, -1.0, 0.0), d(0.1, -1.0, 0.15)), (d(-0.15, 0.1, 1.0), d(-0.1, 0.12, 1.0)), (down, down), (down, down), 0.95),
    Pose::ArmsOut => ((d(1.0, -0.1, 0.0), d(1.0, -0.05, 0.0)), (d(-1.0, -0.1, 0.0), d(-1.0, -0.05, 0.0)), (d(0.05, -1.0, 0.0), down), (d(-0.05, -1.0, 0.0), down), 0.95),
  };
  let hip = Vec3::new(0.0, hip_y, 0.0);
  let chest = hip + Vec3::Y * 0.5;
  let mut out = Vec::new();
  out.extend(cuboid(hip + Vec3::Y * 0.27, Vec3::new(0.17, 0.27, 0.1)));
  out.extend(cylinder(chest, chest + Vec3::Y * 0.1, 0.05, 0.05));
  out.extend(sphere(chest + Vec3::Y * 0.21, 0.11));
  let limb = |out: &mut Vec<[Vec3; 3]>, root: Vec3, (upper, lower): (Vec3, Vec3), lengths: (f32, f32), radius: (f32, f32)| {
    let joint = root + upper * lengths.0;
    out.extend(cylinder(root, joint, radius.0, radius.1));
    out.extend(sphere(joint, radius.1));
    out.extend(cylinder(joint, joint + lower * lengths.1, radius.1, radius.1 * 0.8));
  };
  for (side, arm, leg) in [(1.0, la, ll), (-1.0, ra, rl)] {
    limb(&mut out, chest + Vec3::new(0.21 * side, -0.05, 0.0), arm, (0.3, 0.28), (0.05, 0.04));
    limb(&mut out, hip + Vec3::new(0.1 * side, 0.0, 0.0), leg, (0.45, 0.45), (0.075, 0.055));
  }
  out
}

/// Reads a model file's triangles: GLB/glTF (every mesh, with node transforms) or OBJ. The model
/// is scaled to about 1 m on its longest side and stood on the floor, so imports start sensible.
pub fn load_model(path: &Path) -> anyhow::Result<Vec<[Vec3; 3]>> {
  let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
  let tris = match ext.as_str() {
    "glb" | "gltf" => load_gltf(path)?,
    "obj" => load_obj(&std::fs::read_to_string(path)?),
    other => anyhow::bail!("Can't read .{other} models; use GLB, glTF or OBJ"),
  };
  anyhow::ensure!(!tris.is_empty(), "The model has no triangles");
  Ok(normalize(tris))
}

fn load_gltf(path: &Path) -> anyhow::Result<Vec<[Vec3; 3]>> {
  let (doc, buffers, _) = gltf::import(path)?;
  let mut out = Vec::new();
  for scene in doc.scenes() {
    for node in scene.nodes() {
      gltf_node(&node, Mat4::IDENTITY, &buffers, &mut out);
    }
  }
  Ok(out)
}

fn gltf_node(node: &gltf::Node<'_>, parent: Mat4, buffers: &[gltf::buffer::Data], out: &mut Vec<[Vec3; 3]>) {
  let m = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
  if let Some(mesh) = node.mesh() {
    for prim in mesh.primitives() {
      let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
      let Some(positions) = reader.read_positions() else {
        continue;
      };
      let pts: Vec<Vec3> = positions.map(|p| m.transform_point3(Vec3::from(p))).collect();
      let indices: Vec<usize> = match reader.read_indices() {
        Some(i) => i.into_u32().map(|i| i as usize).collect(),
        None => (0..pts.len()).collect(),
      };
      out.extend(indices.chunks_exact(3).filter(|c| c.iter().all(|&i| i < pts.len())).map(|c| [pts[c[0]], pts[c[1]], pts[c[2]]]));
    }
  }
  for child in node.children() {
    gltf_node(&child, m, buffers, out);
  }
}

/// Wavefront OBJ: `v` lines and `f` faces (fans for polygons; `v/vt/vn` and negative indices).
fn load_obj(text: &str) -> Vec<[Vec3; 3]> {
  let mut verts: Vec<Vec3> = Vec::new();
  let mut out = Vec::new();
  for line in text.lines() {
    let mut parts = line.split_whitespace();
    match parts.next() {
      Some("v") => {
        let n: Vec<f32> = parts.take(3).filter_map(|p| p.parse().ok()).collect();
        if n.len() == 3 {
          verts.push(Vec3::new(n[0], n[1], n[2]));
        }
      },
      Some("f") => {
        let idx: Vec<usize> = parts.filter_map(|p| p.split('/').next()?.parse::<i64>().ok()).filter_map(|i| if i < 0 { verts.len().checked_sub(i.unsigned_abs() as usize) } else { (i as usize).checked_sub(1) }).collect();
        for k in 1..idx.len().saturating_sub(1) {
          if let (Some(a), Some(b), Some(c)) = (verts.get(idx[0]), verts.get(idx[k]), verts.get(idx[k + 1])) {
            out.push([*a, *b, *c]);
          }
        }
      },
      _ => {},
    }
  }
  out
}

fn normalize(tris: Vec<[Vec3; 3]>) -> Vec<[Vec3; 3]> {
  let (min, max) = tris.iter().flatten().fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| (lo.min(*p), hi.max(*p)));
  let size = (max - min).max_element().max(1e-6);
  let base = Vec3::new((min.x + max.x) / 2.0, min.y, (min.z + max.z) / 2.0);
  tris.into_iter().map(|t| t.map(|p| (p - base) / size)).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn shapes_rest_on_the_floor() {
    for shape in [Shape::Cube, Shape::Sphere, Shape::Cylinder, Shape::Cone] {
      let min_y = shape_mesh(shape).iter().flatten().map(|p| p.y).fold(f32::MAX, f32::min);
      assert!(min_y.abs() < 1e-4, "{shape:?} starts at y={min_y}");
    }
  }

  #[test]
  fn mannequins_are_person_sized_in_every_pose() {
    for pose in Pose::ALL {
      let pts: Vec<Vec3> = mannequin(pose).into_iter().flatten().collect();
      let top = pts.iter().map(|p| p.y).fold(f32::MIN, f32::max);
      let bottom = pts.iter().map(|p| p.y).fold(f32::MAX, f32::min);
      assert!(top > 1.2 && top < 1.95, "{pose:?} top {top}");
      assert!(bottom > -0.1, "{pose:?} goes below the floor: {bottom}");
    }
  }

  #[test]
  fn obj_files_are_read_and_normalized() {
    let tris = load_obj("v 0 0 0\nv 10 0 0\nv 10 20 0\nv 0 20 0\nf 1 2 3 4\nf -4/1/1 -3/1/1 -2/1/1\n");
    assert_eq!(tris.len(), 3, "a quad fans into two, plus one with negative indices");
    let norm = normalize(tris);
    let top = norm.iter().flatten().map(|p| p.y).fold(f32::MIN, f32::max);
    assert!((top - 1.0).abs() < 1e-5);
  }

  #[test]
  fn scenes_round_trip_through_json() {
    let mut scene = Scene::default();
    scene.add(Kind::Shape(Shape::Cube));
    let path = std::env::temp_dir().join(format!("stage-{}.json", uuid::Uuid::new_v4()));
    scene.save(&path).unwrap();
    assert_eq!(Scene::load(&path), scene);
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn the_camera_looks_at_its_target() {
    let view = Camera::default().view();
    assert!(((view.eye - view.target).length() - 6.0).abs() < 1e-4);
    assert!((view.fov_y.to_degrees() - 37.85).abs() < 0.1, "35 mm lens");
  }
}
