//! 3D Stage: block out a shot with stand-ins (posable people, shapes, imported models), frame it
//! with a lens and aspect ratio, then have Nano Banana turn the render into a finished image that
//! keeps the layout. Everything here runs on the user's Mac; only Generate calls Vertex.
//!
//! Viewport: drag to orbit, right-drag to pan, scroll to zoom, click to select, drag the selected
//! object to slide it on the floor (Shift-drag lifts it), Delete removes it.

pub mod render;
pub mod scene;

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{Color32, ColorImage, Key, PointerButton, Rect, Sense, TextureHandle, TextureOptions, Ui, vec2};
use glam::Vec3;

use super::ToolSession;
use crate::backend::local::tools::{EditOp, ToolRequest};
use crate::pages::common::Env;
use crate::theme;
use crate::ui::icons::Icon;
use crate::ui::widgets;
use render::{Frame, View};
use scene::{FLOOR_ID, Kind, Pose, Scene, Shape};

const ASPECTS: [(&str, f32); 5] = [("16:9", 16.0 / 9.0), ("3:2", 1.5), ("1:1", 1.0), ("2:3", 2.0 / 3.0), ("9:16", 9.0 / 16.0)];
const SIZES: [(&str, &str); 3] = [("one_k", "1K"), ("two_k", "2K"), ("four_k", "4K")];
/// The render sent to Nano Banana, on its long side.
const EXPORT_SIDE: f32 = 1536.0;
/// The viewport renders at most this many pixels (it re-renders on every change).
const VIEWPORT_PIXELS: f32 = 900_000.0;
const STAGE_INSTRUCTION: &str = "This is a 3D blocking render of a shot: grey stand-in figures and shapes on a checkered floor. Use it as the exact layout: keep the camera angle, lens, framing, and every figure's and object's position, size and pose. Replace the stand-ins and the floor with the real scene described here, rendered as a finished, believable image:";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
  Orbit,
  Pan,
  Slide(usize),
  Lift(usize),
}

pub struct StagePage {
  pub session: ToolSession,
  scene: Scene,
  scene_path: Option<PathBuf>,
  selected: Option<usize>,
  /// Imported model triangles by file (None when the file couldn't be read).
  models: HashMap<PathBuf, Option<Arc<Vec<[Vec3; 3]>>>>,
  frame: Option<Frame>,
  texture: Option<TextureHandle>,
  rendered: u64,
  drag: Option<Drag>,
  aspect: usize,
  prompt: String,
  pro: bool,
  size: usize,
  save_due: bool,
}

impl Default for StagePage {
  fn default() -> Self {
    Self { session: ToolSession::default(), scene: Scene::default(), scene_path: None, selected: Some(0), models: HashMap::new(), frame: None, texture: None, rendered: 0, drag: None, aspect: 0, prompt: String::new(), pro: true, size: 0, save_due: false }
  }
}

impl StagePage {
  pub fn ui(&mut self, ui: &mut Ui, env: &mut Env<'_>) {
    if self.scene_path.is_none() {
      let path = env.backend.data_root().join("local").join("stage.json");
      self.scene = Scene::load(&path);
      self.scene_path = Some(path);
      self.selected = (!self.scene.objects.is_empty()).then_some(0);
    }
    self.load_models();
    let (stage, panel, strip) = super::layout(ui.max_rect());
    ui.painter().rect_filled(stage, theme::RADIUS, theme::SUNKEN);
    let frame_rect = fit_aspect(stage.shrink(12.0), ASPECTS[self.aspect].1);
    self.viewport(ui, frame_rect);
    if self.session.busy() {
      super::busy_overlay(ui, stage, "Nano Banana is painting the shot\u{2026}");
    }
    self.panel(ui, panel, env);
    if let Some(picked) = super::results_strip(ui, strip, &self.session.results, env.cache, "Images made from the stage appear here (and in your library)") {
      env.toasts.success(format!("{} is in your library: open it there to edit or animate it", picked.name()));
    }
    if self.save_due && self.drag.is_none() {
      self.save_due = false;
      if let Some(path) = &self.scene_path {
        if let Err(err) = self.scene.save(path) {
          log::warn!("Couldn't save the stage: {err}");
        }
      }
    }
  }

  fn viewport(&mut self, ui: &mut Ui, rect: Rect) {
    let resp = ui.interact(rect, ui.id().with("stage-viewport"), Sense::click_and_drag());
    let aspect = rect.width() / rect.height().max(1.0);
    let view = self.scene.camera.view();
    let to_uv = |p: egui::Pos2| ((p.x - rect.left()) / rect.width(), (p.y - rect.top()) / rect.height());

    // Start of a drag: what's under the pointer decides what the drag does.
    if resp.drag_started() {
      let pointer = resp.interact_pointer_pos().unwrap_or(rect.center());
      let hit = self.hit(rect, pointer);
      let shift = ui.input(|i| i.modifiers.shift);
      self.drag = Some(if resp.dragged_by(PointerButton::Secondary) || resp.dragged_by(PointerButton::Middle) {
        Drag::Pan
      } else {
        match (hit, self.selected) {
          (Some(h), Some(s)) if h == s && shift => Drag::Lift(s),
          (Some(h), Some(s)) if h == s => Drag::Slide(s),
          _ => Drag::Orbit,
        }
      });
    }
    if resp.dragged() {
      let delta = resp.drag_delta();
      let cam = &mut self.scene.camera;
      match self.drag {
        Some(Drag::Orbit) => {
          cam.yaw -= delta.x * 0.4;
          cam.pitch = (cam.pitch + delta.y * 0.3).clamp(-5.0, 85.0);
        },
        Some(Drag::Pan) => {
          let forward = (view.target - view.eye).normalize();
          let right = forward.cross(Vec3::Y).normalize();
          let up = right.cross(forward);
          let scale = cam.distance * 0.0016;
          cam.target += (-right * delta.x + up * delta.y) * scale;
        },
        Some(Drag::Slide(i)) => {
          if let Some(pointer) = resp.interact_pointer_pos() {
            let y = self.scene.objects[i].position.y;
            let (u0, v0) = to_uv(pointer - delta);
            let (u1, v1) = to_uv(pointer);
            if let (Some(a), Some(b)) = (on_plane(&view, u0, v0, aspect, y), on_plane(&view, u1, v1, aspect, y)) {
              self.scene.objects[i].position += b - a;
            }
          }
        },
        Some(Drag::Lift(i)) => {
          let o = &mut self.scene.objects[i];
          o.position.y = (o.position.y - delta.y * cam.distance * 0.002).max(0.0);
        },
        None => {},
      }
      self.save_due = true;
    }
    if resp.drag_stopped() {
      self.drag = None;
    }
    if resp.clicked() {
      if let Some(pointer) = resp.interact_pointer_pos() {
        self.selected = self.hit(rect, pointer);
      }
    }
    if resp.hovered() {
      let scroll = ui.input(|i| i.smooth_scroll_delta.y);
      if scroll != 0.0 {
        self.scene.camera.distance = (self.scene.camera.distance * (-scroll * 0.002).exp()).clamp(0.5, 60.0);
        self.save_due = true;
      }
    }
    if let Some(i) = self.selected {
      if !ui.ctx().egui_wants_keyboard_input() && ui.input(|inp| inp.key_pressed(Key::Delete) || inp.key_pressed(Key::Backspace)) && i < self.scene.objects.len() {
        self.scene.objects.remove(i);
        self.selected = None;
        self.save_due = true;
      }
    }

    // Re-render only when something changed.
    let ppp = ui.ctx().pixels_per_point();
    let scale = (VIEWPORT_PIXELS / (rect.width() * rect.height() * ppp * ppp)).sqrt().min(1.0) * ppp;
    let (w, h) = ((rect.width() * scale).max(1.0) as usize, (rect.height() * scale).max(1.0) as usize);
    let key = self.key(w, h);
    if key != self.rendered || self.texture.is_none() {
      let tris = self.triangles();
      let frame = render::render(&tris, &self.scene.camera.view(), w, h, self.selected.map(|i| i as u16 + 1));
      let image = ColorImage::from_rgba_unmultiplied([w, h], &frame.rgba);
      match &mut self.texture {
        Some(t) => t.set(image, TextureOptions::LINEAR),
        None => self.texture = Some(ui.ctx().load_texture("stage-viewport", image, TextureOptions::LINEAR)),
      }
      self.frame = Some(frame);
      self.rendered = key;
    }
    if let Some(t) = &self.texture {
      egui::Image::new(t).paint_at(ui, rect);
    }
    ui.painter().rect_stroke(rect, 0.0, egui::Stroke::new(1.0, theme::LINE_STRONG), egui::StrokeKind::Outside);
    let hint = "Drag: orbit \u{b7} right-drag: pan \u{b7} scroll: zoom \u{b7} click: select \u{b7} drag selected: move (Shift: lift)";
    ui.painter().text(rect.left_bottom() + vec2(10.0, -10.0), egui::Align2::LEFT_BOTTOM, hint, egui::FontId::new(11.5, egui::FontFamily::Proportional), Color32::from_white_alpha(150));
  }

  /// The object under a point of the viewport.
  fn hit(&self, rect: Rect, pointer: egui::Pos2) -> Option<usize> {
    let frame = self.frame.as_ref()?;
    let x = ((pointer.x - rect.left()) / rect.width() * frame.width as f32) as usize;
    let y = ((pointer.y - rect.top()) / rect.height() * frame.height as f32) as usize;
    match frame.id_at(x, y) {
      0 | FLOOR_ID => None,
      id => Some(id as usize - 1),
    }
  }

  fn panel(&mut self, ui: &mut Ui, rect: Rect, env: &mut Env<'_>) {
    let estimate = env.backend.estimate_tool(&self.request(PathBuf::new()));
    let mut generate = false;
    let mut save = false;
    let mut import = false;
    super::panel(ui, rect, "3D Stage", "Block out the shot with stand-ins, frame it, then let Nano Banana paint it. The layout, camera and poses carry over.", |ui| {
      super::field(ui, "Add");
      ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        let adds: [(&str, Kind); 6] = [("Person", Kind::Mannequin(Pose::Standing)), ("Cube", Kind::Shape(Shape::Cube)), ("Sphere", Kind::Shape(Shape::Sphere)), ("Cylinder", Kind::Shape(Shape::Cylinder)), ("Cone", Kind::Shape(Shape::Cone)), ("Plane", Kind::Shape(Shape::Plane))];
        for (label, kind) in adds {
          if widgets::button(ui, Some(Icon::Plus), label, widgets::Kind::Secondary, 0.0, 28.0).clicked() {
            self.selected = Some(self.scene.add(kind));
            self.save_due = true;
          }
        }
        if widgets::button(ui, Some(Icon::Folder), "Import model", widgets::Kind::Secondary, 0.0, 28.0).on_hover_text("GLB, glTF or OBJ").clicked() {
          import = true;
        }
      });

      super::field(ui, "Objects");
      let mut remove = None;
      for (i, object) in self.scene.objects.iter().enumerate() {
        ui.horizontal(|ui| {
          if ui.selectable_label(self.selected == Some(i), &object.name).clicked() {
            self.selected = Some(i);
          }
          ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::ghost(ui, Icon::X, None, "Remove", false).clicked() {
              remove = Some(i);
            }
          });
        });
      }
      if let Some(i) = remove {
        self.scene.objects.remove(i);
        self.selected = None;
        self.save_due = true;
      }

      if let Some(i) = self.selected.filter(|i| *i < self.scene.objects.len()) {
        let before = self.scene.objects[i].clone();
        let object = &mut self.scene.objects[i];
        super::field(ui, "Selected");
        ui.add(egui::TextEdit::singleline(&mut object.name).id_salt("stage-name").desired_width(f32::INFINITY));
        if let Kind::Mannequin(pose) = &mut object.kind {
          if let Some(p) = super::segmented(ui, &Pose::ALL.map(Pose::label), Pose::ALL.iter().position(|x| x == pose).unwrap_or(0)) {
            *pose = Pose::ALL[p];
          }
        }
        let row = |ui: &mut Ui, label: &str, v: &mut Vec3, speed: f64, suffix: &str| {
          ui.horizontal(|ui| {
            ui.add_sized(vec2(72.0, 20.0), egui::Label::new(egui::RichText::new(label).size(12.0).color(theme::MUTED)));
            ui.spacing_mut().interact_size.x = 54.0;
            ui.spacing_mut().item_spacing.x = 4.0;
            for c in [&mut v.x, &mut v.y, &mut v.z] {
              ui.add(egui::DragValue::new(c).speed(speed).max_decimals(2).suffix(suffix));
            }
          });
        };
        row(ui, "Position (m)", &mut object.position, 0.02, "");
        row(ui, "Rotate (\u{b0})", &mut object.rotation, 1.0, "");
        row(ui, "Scale", &mut object.scale, 0.01, "");
        ui.horizontal(|ui| {
          ui.add_sized(vec2(64.0, 20.0), egui::Label::new(egui::RichText::new("Colour").size(12.0).color(theme::MUTED)));
          ui.color_edit_button_srgb(&mut object.color);
        });
        object.scale = object.scale.max(Vec3::splat(0.01));
        if *object != before {
          self.save_due = true;
        }
      }

      super::field(ui, "Camera");
      ui.add(egui::Slider::new(&mut self.scene.camera.focal_mm, 14.0..=135.0).text("mm lens").integer());
      if let Some(i) = super::segmented(ui, &ASPECTS.map(|(l, _)| l), self.aspect) {
        self.aspect = i;
      }
      if widgets::button(ui, None, "Reset view", widgets::Kind::Secondary, 0.0, 28.0).clicked() {
        self.scene.camera = scene::Camera::default();
        self.save_due = true;
      }

      super::field(ui, "Make it real");
      super::prompt_field(ui, "stage-prompt", &mut self.prompt, "Two detectives in trench coats in a rain-soaked neon alley at night");
      if let Some(i) = super::segmented(ui, &["Nano Banana Pro", "Nano Banana 2"], usize::from(!self.pro)) {
        self.pro = i == 0;
      }
      if let Some(i) = super::segmented(ui, &SIZES.map(|(_, l)| l), self.size) {
        self.size = i;
      }
      generate = super::run_button(ui, "Generate image", estimate, !self.prompt.trim().is_empty() && !self.scene.objects.is_empty(), &self.session);
      ui.add_space(6.0);
      save = widgets::button(ui, Some(Icon::Download), "Save render to library", widgets::Kind::Secondary, 0.0, 30.0).clicked();
    });
    if import {
      for path in rfd::FileDialog::new().add_filter("3D models", &["glb", "gltf", "obj"]).pick_files().unwrap_or_default() {
        self.selected = Some(self.scene.add(Kind::Model(path)));
        self.save_due = true;
      }
    }
    if generate || save {
      match self.export(env.backend.data_root()) {
        Ok(path) => {
          let request = if generate { self.request(path) } else { ToolRequest::SaveImage { image: path } };
          self.session.run(env, request);
        },
        Err(err) => env.toasts.error(format!("Couldn't render the stage: {err}")),
      }
    }
  }

  /// Nano Banana's request: the render as the layout, and the description.
  fn request(&self, render: PathBuf) -> ToolRequest {
    let prompt = format!("{STAGE_INSTRUCTION} {}", self.prompt.trim());
    ToolRequest::EditImage { image: render, mask: None, op: EditOp::Instruct { pro: self.pro, prompt, image_size: Some(SIZES[self.size].0.to_owned()) } }
  }

  /// The framed shot at export size, without the selection outline, as a PNG on disk.
  fn export(&self, data_root: &Path) -> anyhow::Result<PathBuf> {
    let aspect = ASPECTS[self.aspect].1;
    let (w, h) = if aspect >= 1.0 { (EXPORT_SIDE, EXPORT_SIDE / aspect) } else { (EXPORT_SIDE * aspect, EXPORT_SIDE) };
    let frame = render::render(&self.triangles(), &self.scene.camera.view(), w as usize, h as usize, None);
    let image = image::RgbaImage::from_raw(frame.width as u32, frame.height as u32, frame.rgba).ok_or_else(|| anyhow::anyhow!("bad frame size"))?;
    let dir = data_root.join("local").join("stage_renders");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("stage_{}.png", chrono::Local::now().format("%Y%m%d_%H%M%S")));
    image.save(&path)?;
    Ok(path)
  }

  fn triangles(&self) -> Vec<render::Tri> {
    let models = &self.models;
    self.scene.triangles(&|path: &Path| models.get(path).cloned().flatten().map(|m| m.as_ref().clone()))
  }

  /// Reads newly added model files once.
  fn load_models(&mut self) {
    for object in &self.scene.objects {
      if let Kind::Model(path) = &object.kind {
        if !self.models.contains_key(path) {
          let loaded = scene::load_model(path).map_err(|e| log::warn!("Couldn't load {}: {e}", path.display())).ok().map(Arc::new);
          self.models.insert(path.clone(), loaded);
        }
      }
    }
  }

  /// Changes when anything drawn changes.
  fn key(&self, w: usize, h: usize) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(&self.scene).unwrap_or_default().hash(&mut hasher);
    (w, h, self.selected, self.models.len()).hash(&mut hasher);
    hasher.finish()
  }
}

/// The largest rect of `aspect` (width / height) inside `area`, centred.
fn fit_aspect(area: Rect, aspect: f32) -> Rect {
  let (w, h) = if area.width() / area.height() > aspect { (area.height() * aspect, area.height()) } else { (area.width(), area.width() / aspect) };
  Rect::from_center_size(area.center(), vec2(w, h))
}

/// Where a viewport point's ray meets the horizontal plane at height `y`.
fn on_plane(view: &View, u: f32, v: f32, aspect: f32, y: f32) -> Option<Vec3> {
  let (origin, dir) = view.ray(u, v, aspect);
  if dir.y.abs() < 1e-4 {
    return None;
  }
  let t = (y - origin.y) / dir.y;
  (t > 0.0).then(|| origin + dir * t)
}

#[cfg(test)]
mod tests {
  use egui::pos2;

  use super::*;

  #[test]
  fn frames_fit_the_stage() {
    let r = fit_aspect(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 500.0)), 1.0);
    assert_eq!(r.size(), vec2(500.0, 500.0));
    let r = fit_aspect(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 900.0)), 16.0 / 9.0);
    assert!((r.width() - 800.0).abs() < 0.01 && (r.height() - 450.0).abs() < 0.01);
  }

  #[test]
  fn the_centre_of_the_view_lands_on_the_target_height() {
    let view = scene::Camera::default().view();
    let p = on_plane(&view, 0.5, 0.5, 1.0, 0.9).unwrap();
    assert!((p - view.target).length() < 1e-3);
  }

  #[test]
  fn stage_renders_show_the_person_and_go_to_nano_banana() {
    let page = StagePage { prompt: "a knight".into(), ..Default::default() };
    let dir = std::env::temp_dir().join(format!("stage-export-{}", uuid::Uuid::new_v4()));
    let path = page.export(&dir).unwrap();
    let img = image::open(&path).unwrap();
    assert_eq!((img.width(), img.height()), (1536, 864));
    let frame = render::render(&page.triangles(), &page.scene.camera.view(), 160, 90, None);
    assert!(frame.ids.iter().any(|id| *id == 1), "the default person is in shot");
    match page.request(path) {
      ToolRequest::EditImage { op: EditOp::Instruct { pro: true, prompt, .. }, .. } => assert!(prompt.ends_with("a knight") && prompt.contains("exact layout")),
      other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(dir);
  }
}
