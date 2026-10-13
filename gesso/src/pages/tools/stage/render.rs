//! The 3D Stage's software renderer: a small z-buffered rasterizer on the CPU. It draws the same
//! picture in the viewport and in the render sent to Nano Banana, needs no GPU plumbing, and is
//! quick enough for blocking scenes (thousands to tens of thousands of triangles).
//!
//! Flat-shaded with one key light and ambient fill; the floor gets a 1 m checker so depth reads.
//! An id buffer records which object covers each pixel, for clicking and the selection outline.

use glam::{Mat4, Vec3, Vec4};

/// Near clipping distance (metres).
const NEAR: f32 = 0.05;
const KEY_LIGHT: Vec3 = Vec3::new(-0.45, 0.8, 0.4);
const AMBIENT: f32 = 0.38;
const OUTLINE: [u8; 3] = [45, 129, 255];

/// A triangle in world space with its colour and owner.
#[derive(Clone, Copy, Debug)]
pub struct Tri {
  pub v: [Vec3; 3],
  pub color: [u8; 3],
  /// Object index + 1 (0 = nothing).
  pub id: u16,
  /// The floor: shaded with a checker by world position.
  pub floor: bool,
}

/// Where the camera is and how it sees.
#[derive(Clone, Copy, Debug)]
pub struct View {
  pub eye: Vec3,
  pub target: Vec3,
  /// Vertical field of view, radians.
  pub fov_y: f32,
}

/// A rendered frame.
pub struct Frame {
  pub width: usize,
  pub height: usize,
  pub rgba: Vec<u8>,
  pub ids: Vec<u16>,
}

impl Frame {
  /// Which object covers a pixel (index + 1), if any.
  pub fn id_at(&self, x: usize, y: usize) -> u16 {
    if x < self.width && y < self.height {
      self.ids[y * self.width + x]
    } else {
      0
    }
  }
}

impl View {
  pub fn view_matrix(&self) -> Mat4 {
    Mat4::look_at_rh(self.eye, self.target, Vec3::Y)
  }

  /// A world-space ray through a point of the frame (0..1 across and down).
  pub fn ray(&self, u: f32, v: f32, aspect: f32) -> (Vec3, Vec3) {
    let forward = (self.target - self.eye).normalize();
    let right = forward.cross(Vec3::Y).normalize();
    let up = right.cross(forward);
    let half_h = (self.fov_y / 2.0).tan();
    let half_w = half_h * aspect;
    let dir = (forward + right * ((u * 2.0 - 1.0) * half_w) + up * ((1.0 - v * 2.0) * half_h)).normalize();
    (self.eye, dir)
  }
}

/// Draws `tris` from `view` into a `width`×`height` frame, outlining object `selected` (index+1).
pub fn render(tris: &[Tri], view: &View, width: usize, height: usize, selected: Option<u16>) -> Frame {
  let mut frame = Frame { width, height, rgba: vec![0; width * height * 4], ids: vec![0; width * height] };
  background(&mut frame);
  let mut depth = vec![0.0f32; width * height];
  let view_m = view.view_matrix();
  let aspect = width as f32 / height.max(1) as f32;
  let focal = 1.0 / (view.fov_y / 2.0).tan();
  let light = KEY_LIGHT.normalize();
  for tri in tris {
    let normal = (tri.v[1] - tri.v[0]).cross(tri.v[2] - tri.v[0]).normalize_or_zero();
    // Two-sided: imported meshes aren't always closed or consistently wound.
    let diffuse = normal.dot(light).abs();
    let shade = AMBIENT + (1.0 - AMBIENT) * diffuse;
    let cam: Vec<Vec3> = tri.v.iter().map(|p| (view_m * Vec4::new(p.x, p.y, p.z, 1.0)).truncate()).collect();
    for (a, b, c) in clip_near([(cam[0], tri.v[0]), (cam[1], tri.v[1]), (cam[2], tri.v[2])]) {
      raster(&mut frame, &mut depth, [a, b, c], focal, aspect, tri, shade);
    }
  }
  if let Some(sel) = selected {
    outline(&mut frame, sel);
  }
  frame
}

/// A soft vertical gradient behind everything.
fn background(frame: &mut Frame) {
  for y in 0..frame.height {
    let t = y as f32 / frame.height.max(1) as f32;
    let c = [lerp(36.0, 18.0, t), lerp(40.0, 19.0, t), lerp(48.0, 22.0, t)];
    for x in 0..frame.width {
      let i = (y * frame.width + x) * 4;
      frame.rgba[i..i + 4].copy_from_slice(&[c[0] as u8, c[1] as u8, c[2] as u8, 255]);
    }
  }
}

/// Clips a camera-space triangle against the near plane; each vertex carries its world position.
fn clip_near(tri: [(Vec3, Vec3); 3]) -> Vec<((Vec3, Vec3), (Vec3, Vec3), (Vec3, Vec3))> {
  let inside = |v: &(Vec3, Vec3)| v.0.z <= -NEAR;
  let mut poly: Vec<(Vec3, Vec3)> = Vec::with_capacity(4);
  for i in 0..3 {
    let (cur, next) = (tri[i], tri[(i + 1) % 3]);
    if inside(&cur) {
      poly.push(cur);
    }
    if inside(&cur) != inside(&next) {
      let t = (-NEAR - cur.0.z) / (next.0.z - cur.0.z);
      poly.push((cur.0.lerp(next.0, t), cur.1.lerp(next.1, t)));
    }
  }
  (1..poly.len().saturating_sub(1)).map(|i| (poly[0], poly[i], poly[i + 1])).collect()
}

#[allow(clippy::too_many_arguments)]
fn raster(frame: &mut Frame, depth: &mut [f32], verts: [(Vec3, Vec3); 3], focal: f32, aspect: f32, tri: &Tri, shade: f32) {
  let (w, h) = (frame.width as f32, frame.height as f32);
  // Screen position and 1/z (perspective-correct interpolation) per vertex.
  let screen: Vec<(f32, f32, f32)> = verts
    .iter()
    .map(|(c, _)| {
      let inv_z = 1.0 / -c.z;
      let x = (focal / aspect) * c.x * inv_z;
      let y = focal * c.y * inv_z;
      ((x + 1.0) * 0.5 * w, (1.0 - y) * 0.5 * h, inv_z)
    })
    .collect();
  let area = edge(screen[0], screen[1], (screen[2].0, screen[2].1));
  if area.abs() < 1e-6 {
    return;
  }
  let min_x = screen.iter().map(|s| s.0).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
  let max_x = screen.iter().map(|s| s.0).fold(f32::MIN, f32::max).ceil().min(w - 1.0).max(0.0) as usize;
  let min_y = screen.iter().map(|s| s.1).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
  let max_y = screen.iter().map(|s| s.1).fold(f32::MIN, f32::max).ceil().min(h - 1.0).max(0.0) as usize;
  if min_x > max_x || min_y > max_y {
    return;
  }
  for y in min_y..=max_y {
    for x in min_x..=max_x {
      let p = (x as f32 + 0.5, y as f32 + 0.5);
      let w0 = edge(screen[1], screen[2], p) / area;
      let w1 = edge(screen[2], screen[0], p) / area;
      let w2 = 1.0 - w0 - w1;
      if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
        continue;
      }
      let inv_z = w0 * screen[0].2 + w1 * screen[1].2 + w2 * screen[2].2;
      let i = y * frame.width + x;
      if inv_z <= depth[i] {
        continue;
      }
      depth[i] = inv_z;
      frame.ids[i] = tri.id;
      let mut color = tri.color;
      if tri.floor {
        // World position, perspective-correct, for the 1 m checker.
        let world = (verts[0].1 * w0 * screen[0].2 + verts[1].1 * w1 * screen[1].2 + verts[2].1 * w2 * screen[2].2) / inv_z;
        if (world.x.floor() as i64 + world.z.floor() as i64).rem_euclid(2) == 0 {
          color = color.map(|c| c.saturating_sub(14));
        }
      }
      let o = i * 4;
      for k in 0..3 {
        frame.rgba[o + k] = (f32::from(color[k]) * shade).min(255.0) as u8;
      }
      frame.rgba[o + 3] = 255;
    }
  }
}

fn edge(a: (f32, f32, f32), b: (f32, f32, f32), p: (f32, f32)) -> f32 {
  (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// A 2 px outline around the selected object.
fn outline(frame: &mut Frame, sel: u16) {
  let (w, h) = (frame.width, frame.height);
  let ids = frame.ids.clone();
  for y in 0..h {
    for x in 0..w {
      if ids[y * w + x] != sel {
        continue;
      }
      let edge = [(-2i32, 0i32), (2, 0), (0, -2), (0, 2)].iter().any(|(dx, dy)| {
        let (nx, ny) = (x as i32 + dx, y as i32 + dy);
        nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 || ids[ny as usize * w + nx as usize] != sel
      });
      if edge {
        let o = (y * w + x) * 4;
        frame.rgba[o..o + 3].copy_from_slice(&OUTLINE);
      }
    }
  }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
  a + (b - a) * t
}

#[cfg(test)]
mod tests {
  use super::*;

  fn quad(z: f32, id: u16, color: [u8; 3]) -> Vec<Tri> {
    let (a, b, c, d) = (Vec3::new(-1.0, -1.0, z), Vec3::new(1.0, -1.0, z), Vec3::new(1.0, 1.0, z), Vec3::new(-1.0, 1.0, z));
    vec![Tri { v: [a, b, c], color, id, floor: false }, Tri { v: [a, c, d], color, id, floor: false }]
  }

  fn front_view() -> View {
    View { eye: Vec3::new(0.0, 0.0, 5.0), target: Vec3::ZERO, fov_y: 60f32.to_radians() }
  }

  #[test]
  fn nearer_surfaces_win() {
    let mut tris = quad(-1.0, 1, [200, 0, 0]);
    tris.extend(quad(1.0, 2, [0, 200, 0]));
    let frame = render(&tris, &front_view(), 64, 64, None);
    assert_eq!(frame.id_at(32, 32), 2, "the quad at z=1 is nearer the camera at z=5");
    assert_eq!(frame.id_at(0, 0), 0, "corners show the background");
  }

  #[test]
  fn geometry_behind_the_camera_is_clipped_not_smeared() {
    let tris = vec![Tri { v: [Vec3::new(-1.0, 0.0, 10.0), Vec3::new(1.0, 0.0, 10.0), Vec3::new(0.0, 1.0, 10.0)], color: [255; 3], id: 1, floor: false }];
    let frame = render(&tris, &front_view(), 32, 32, None);
    assert!(frame.ids.iter().all(|i| *i == 0));
  }

  #[test]
  fn the_selection_gets_an_outline() {
    let frame = render(&quad(0.0, 1, [100, 100, 100]), &front_view(), 64, 64, Some(1));
    let edge_pixel = (0..64).find(|&x| frame.id_at(x, 32) == 1).unwrap();
    let o = (32 * 64 + edge_pixel) * 4;
    assert_eq!(&frame.rgba[o..o + 3], &OUTLINE);
  }

  #[test]
  fn rays_go_through_the_centre_at_the_target() {
    let (origin, dir) = front_view().ray(0.5, 0.5, 1.0);
    assert!((origin + dir * 5.0).length() < 1e-4);
  }
}
