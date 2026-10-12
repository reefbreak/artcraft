//! Edit Image: change an existing image. Describe a change for Nano Banana (optionally painting
//! where it applies), or use Stability's mask tools on Bedrock: inpaint, erase, search and
//! replace, and expand the canvas.

use std::path::PathBuf;

use egui::{Color32, ColorImage, Pos2, Rect, Sense, TextureHandle, TextureOptions, Ui, pos2, vec2};
use image::{GrayImage, Luma};

use super::{Source, ToolSession};
use crate::backend::local::tools::{EditOp, ToolRequest};
use crate::pages::common::Env;
use crate::prompt_box::refs::RefKind;
use crate::theme;
use crate::ui::icons::Icon;
use crate::ui::widgets;

const ACCEPTS: [RefKind; 1] = [RefKind::Image];
/// The mask is painted at most this many pixels on its long side, then scaled to the image.
const MASK_SIDE: u32 = 1024;
const MODES: [&str; 5] = ["Describe", "Inpaint", "Erase", "Replace", "Expand"];
const SIZES: [(&str, &str); 3] = [("one_k", "1K"), ("two_k", "2K"), ("four_k", "4K")];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
  #[default]
  Describe,
  Inpaint,
  Erase,
  Replace,
  Expand,
}

impl Mode {
  fn all() -> [Mode; 5] {
    [Mode::Describe, Mode::Inpaint, Mode::Erase, Mode::Replace, Mode::Expand]
  }

  fn paints(self) -> bool {
    matches!(self, Mode::Describe | Mode::Inpaint | Mode::Erase)
  }

  fn needs_mask(self) -> bool {
    matches!(self, Mode::Inpaint | Mode::Erase)
  }
}

pub struct EditImagePage {
  pub session: ToolSession,
  mode: Mode,
  prompt: String,
  search: String,
  pro: bool,
  size: usize,
  expand: [u32; 4],
  brush: f32,
  erasing: bool,
  /// White where the edit applies, at preview resolution.
  mask: Option<GrayImage>,
  mask_texture: Option<TextureHandle>,
  mask_dirty: bool,
  last_dab: Option<Pos2>,
  /// The source image's pixel size, once loaded.
  image_size: Option<[usize; 2]>,
  /// The edit made from the current source, shown until "Before" is picked.
  result: Option<Source>,
  show_before: bool,
}

impl Default for EditImagePage {
  fn default() -> Self {
    Self { session: ToolSession::default(), mode: Mode::Describe, prompt: String::new(), search: String::new(), pro: false, size: 0, expand: [0, 0, 0, 0], brush: 40.0, erasing: false, mask: None, mask_texture: None, mask_dirty: false, last_dab: None, image_size: None, result: None, show_before: false }
  }
}

impl EditImagePage {
  pub fn open(&mut self, path: PathBuf) {
    self.session.source = Some(Source::from_path(path));
    self.session.error = None;
    self.mask = None;
    self.mask_texture = None;
    self.image_size = None;
    self.result = None;
    self.show_before = false;
  }

  pub fn after_finish(&mut self) {
    self.result = self.session.results.first().cloned();
    self.show_before = false;
  }

  pub fn ui(&mut self, ui: &mut Ui, env: &mut Env<'_>) {
    let (stage, panel, strip) = super::layout(ui.max_rect());
    match self.session.source.clone() {
      None => {
        if let Some(path) = super::empty_stage(ui, stage, &ACCEPTS, "an image") {
          self.open(path);
        }
      },
      Some(source) => self.stage(ui, stage, env, &source),
    }
    self.panel(ui, panel, env);
    if let Some(picked) = super::results_strip(ui, strip, &self.session.results, env.cache, "Edits appear here. Click one to keep editing it.") {
      self.open(picked.path);
    }
  }

  fn stage(&mut self, ui: &mut Ui, stage: Rect, env: &mut Env<'_>, source: &Source) {
    let showing_result = self.result.is_some() && !self.show_before;
    let url = if showing_result { self.result.as_ref().map(|r| r.url.clone()).unwrap_or_default() } else { source.url.clone() };
    let shown = super::show_image(ui, stage.with_min_y(stage.top() + 48.0), env.cache, &url);
    if let (Some((rect, size)), false) = (shown, showing_result) {
      self.image_size = Some(size);
      if self.mode.paints() {
        self.paint(ui, rect, size);
      }
    }
    if let Some(path) = super::source_bar(ui, stage, source, &ACCEPTS) {
      self.open(path);
    }
    if self.session.busy() {
      super::busy_overlay(ui, stage, "Editing\u{2026}");
    }
  }

  /// Brush strokes on the image paint the mask; the mask shows as a blue wash.
  fn paint(&mut self, ui: &mut Ui, rect: Rect, size: [usize; 2]) {
    let (mw, mh) = mask_dims(size);
    let mask = self.mask.get_or_insert_with(|| GrayImage::new(mw, mh));
    let resp = ui.interact(rect, ui.id().with("edit-mask"), Sense::drag());
    if let Some(pointer) = resp.interact_pointer_pos().filter(|_| resp.dragged() || resp.drag_started()) {
      let to_mask = |p: Pos2| pos2((p.x - rect.left()) / rect.width() * mw as f32, (p.y - rect.top()) / rect.height() * mh as f32);
      let here = to_mask(pointer);
      let radius = self.brush / rect.width() * mw as f32 / 2.0;
      let from = self.last_dab.unwrap_or(here);
      let steps = ((here - from).length() / (radius * 0.4).max(1.0)).ceil().max(1.0) as usize;
      for i in 0..=steps {
        let p = from + (here - from) * (i as f32 / steps as f32);
        dab(mask, p, radius, if self.erasing { 0 } else { 255 });
      }
      self.last_dab = Some(here);
      self.mask_dirty = true;
    } else if !resp.dragged() {
      self.last_dab = None;
    }
    if self.mask_dirty || self.mask_texture.is_none() {
      let rgba: Vec<Color32> = mask.pixels().map(|Luma([v])| if *v > 0 { Color32::from_rgba_unmultiplied(45, 129, 255, 110) } else { Color32::TRANSPARENT }).collect();
      let image = ColorImage::new([mw as usize, mh as usize], rgba);
      self.mask_texture = Some(ui.ctx().load_texture("edit-mask", image, TextureOptions::LINEAR));
      self.mask_dirty = false;
    }
    if let Some(texture) = &self.mask_texture {
      egui::Image::new(texture).paint_at(ui, rect);
    }
    if let Some(hover) = resp.hover_pos() {
      ui.painter().circle_stroke(hover, self.brush / 2.0, egui::Stroke::new(1.5, Color32::WHITE));
      ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }
  }

  fn panel(&mut self, ui: &mut Ui, rect: Rect, env: &mut Env<'_>) {
    let has_source = self.session.source.is_some();
    let has_mask = self.mask.as_ref().is_some_and(|m| m.pixels().any(|p| p.0[0] > 0));
    let request = self.request(PathBuf::new(), None);
    let cost = env.backend.estimate_tool(&request);
    let mut run = false;
    let mut clear = false;
    super::panel(ui, rect, "Edit Image", "Change part of an image and keep the rest. Paint over the area to change where the mode uses a mask.", |ui| {
      if let Some(i) = super::segmented(ui, &MODES, Mode::all().iter().position(|m| *m == self.mode).unwrap_or(0)) {
        self.mode = Mode::all()[i];
      }
      ui.add_space(4.0);
      let note = match self.mode {
        Mode::Describe => "Nano Banana on Vertex. Painting is optional: it limits the change to that area.",
        Mode::Inpaint => "Stability Inpaint on Bedrock. Paint the area, then describe what goes there.",
        Mode::Erase => "Stability Erase Object on Bedrock. Paint over what should disappear.",
        Mode::Replace => "Stability Search and Replace on Bedrock. Name what to find and what replaces it.",
        Mode::Expand => "Stability Outpaint on Bedrock. Grows the canvas and fills in the new area.",
      };
      ui.label(egui::RichText::new(note).size(12.0).color(theme::FAINT));
      if self.session.source.is_some() && self.result.is_some() {
        super::field(ui, "Show");
        if let Some(i) = super::segmented(ui, &["After", "Before"], usize::from(self.show_before)) {
          self.show_before = i == 1;
        }
      }
      if self.mode.paints() {
        super::field(ui, "Brush");
        ui.horizontal(|ui| {
          if let Some(i) = super::segmented(ui, &["Paint", "Erase"], usize::from(self.erasing)) {
            self.erasing = i == 1;
          }
          if widgets::ghost(ui, Icon::Eraser, None, "Clear the mask", false).clicked() {
            clear = true;
          }
        });
        ui.add(egui::Slider::new(&mut self.brush, 8.0..=160.0).text("size").show_value(false));
      }
      match self.mode {
        Mode::Describe => {
          super::field(ui, "Change");
          super::prompt_field(ui, "edit-prompt", &mut self.prompt, "Make the sky a stormy sunset");
          super::field(ui, "Model");
          if let Some(i) = super::segmented(ui, &["Nano Banana 2", "Nano Banana Pro"], usize::from(self.pro)) {
            self.pro = i == 1;
          }
          super::field(ui, "Output size");
          if let Some(i) = super::segmented(ui, &SIZES.map(|(_, l)| l), self.size) {
            self.size = i;
          }
        },
        Mode::Inpaint => {
          super::field(ui, "What goes there");
          super::prompt_field(ui, "inpaint-prompt", &mut self.prompt, "A wooden door with brass handles");
        },
        Mode::Erase => {},
        Mode::Replace => {
          super::field(ui, "Find");
          ui.add(egui::TextEdit::singleline(&mut self.search).id_salt("replace-search").hint_text("the red car").desired_width(f32::INFINITY));
          super::field(ui, "Replace with");
          super::prompt_field(ui, "replace-prompt", &mut self.prompt, "a vintage blue motorbike");
        },
        Mode::Expand => {
          super::field(ui, "Add pixels");
          for (i, side) in ["Left", "Right", "Top", "Bottom"].iter().enumerate() {
            ui.add(egui::Slider::new(&mut self.expand[i], 0..=1000).text(*side).step_by(16.0));
          }
          super::field(ui, "Describe the new area (optional)");
          super::prompt_field(ui, "expand-prompt", &mut self.prompt, "More of the beach and sky");
        },
      }
      let ready = has_source
        && match self.mode {
          Mode::Describe | Mode::Inpaint => !self.prompt.trim().is_empty() && (self.mode != Mode::Inpaint || has_mask),
          Mode::Erase => has_mask,
          Mode::Replace => !self.search.trim().is_empty() && !self.prompt.trim().is_empty(),
          Mode::Expand => self.expand.iter().any(|v| *v > 0),
        };
      if self.mode.needs_mask() && !has_mask && has_source {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Paint over the area first.").size(12.0).color(theme::FAINT));
      }
      run = super::run_button(ui, "Apply edit", cost, ready, &self.session);
    });
    if clear {
      if let Some(mask) = &mut self.mask {
        mask.pixels_mut().for_each(|p| p.0[0] = 0);
        self.mask_dirty = true;
      }
    }
    if run {
      if let Some(source) = self.session.source.clone() {
        let mask = if has_mask { self.mask_png() } else { None };
        let request = self.request(source.path, mask);
        self.session.run(env, request);
      }
    }
  }

  fn request(&self, image: PathBuf, mask: Option<Vec<u8>>) -> ToolRequest {
    let prompt = self.prompt.trim().to_owned();
    let op = match self.mode {
      Mode::Describe => EditOp::Instruct { pro: self.pro, prompt, image_size: Some(SIZES[self.size].0.to_owned()) },
      Mode::Inpaint => EditOp::Inpaint { prompt },
      Mode::Erase => EditOp::Erase,
      Mode::Replace => EditOp::SearchReplace { search: self.search.trim().to_owned(), prompt },
      Mode::Expand => EditOp::Outpaint { left: self.expand[0], right: self.expand[1], up: self.expand[2], down: self.expand[3], prompt },
    };
    ToolRequest::EditImage { image, mask, op }
  }

  /// The mask at the image's own size, as PNG.
  fn mask_png(&self) -> Option<Vec<u8>> {
    let (mask, [w, h]) = (self.mask.as_ref()?, self.image_size?);
    let full = image::imageops::resize(mask, w as u32, h as u32, image::imageops::FilterType::Triangle);
    let mut png = Vec::new();
    image::DynamicImage::ImageLuma8(full).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(png)
  }
}

/// The mask's size for an image: its aspect, at most `MASK_SIDE` on the long side.
fn mask_dims([w, h]: [usize; 2]) -> (u32, u32) {
  let scale = (MASK_SIDE as f32 / w.max(h).max(1) as f32).min(1.0);
  (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1))
}

/// A filled circle of `value` at `center` (mask pixels).
fn dab(mask: &mut GrayImage, center: Pos2, radius: f32, value: u8) {
  let (w, h) = mask.dimensions();
  let r = radius.max(0.5);
  let (x0, x1) = (((center.x - r).floor().max(0.0)) as u32, ((center.x + r).ceil().min(w as f32 - 1.0)).max(0.0) as u32);
  let (y0, y1) = (((center.y - r).floor().max(0.0)) as u32, ((center.y + r).ceil().min(h as f32 - 1.0)).max(0.0) as u32);
  for y in y0..=y1.min(h.saturating_sub(1)) {
    for x in x0..=x1.min(w.saturating_sub(1)) {
      if (vec2(x as f32 + 0.5, y as f32 + 0.5) - center.to_vec2()).length() <= r {
        mask.put_pixel(x, y, Luma([value]));
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn masks_keep_the_image_aspect() {
    assert_eq!(mask_dims([4096, 2048]), (1024, 512));
    assert_eq!(mask_dims([640, 480]), (640, 480), "small images aren't upscaled");
  }

  #[test]
  fn dabs_paint_and_erase() {
    let mut mask = GrayImage::new(100, 100);
    dab(&mut mask, pos2(50.0, 50.0), 10.0, 255);
    assert_eq!(mask.get_pixel(50, 50).0[0], 255);
    assert_eq!(mask.get_pixel(70, 50).0[0], 0);
    dab(&mut mask, pos2(50.0, 50.0), 3.0, 0);
    assert_eq!(mask.get_pixel(50, 50).0[0], 0);
    assert_eq!(mask.get_pixel(57, 50).0[0], 255);
    dab(&mut mask, pos2(-5.0, 120.0), 8.0, 255);
  }

  #[test]
  fn the_mode_decides_the_request() {
    let mut page = EditImagePage { mode: Mode::Replace, search: " cat ".into(), prompt: "dog".into(), ..Default::default() };
    assert_eq!(page.request(PathBuf::from("a.png"), None), ToolRequest::EditImage { image: PathBuf::from("a.png"), mask: None, op: EditOp::SearchReplace { search: "cat".into(), prompt: "dog".into() } });
    page.mode = Mode::Describe;
    page.pro = true;
    page.size = 1;
    assert!(matches!(page.request(PathBuf::new(), None), ToolRequest::EditImage { op: EditOp::Instruct { pro: true, ref image_size, .. }, .. } if image_size.as_deref() == Some("two_k")));
  }
}
