//! The tool pages (Frame Extract, BG Remove, Edit Image, Edit Video): a stage showing the file
//! being worked on, a side panel of options with the run button and its cost, and a strip of this
//! session's results underneath. Results also land in the library; clicking one in the strip makes
//! it the new source, so edits can be chained.
//!
//! A source comes from Open file, a file dropped on the window, or the lightbox's "Open in" buttons.

pub mod bg_remove;
pub mod edit_image;
pub mod edit_video;
pub mod frames;
pub mod stage;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use egui::{Align2, Color32, FontId, Rect, Sense, Ui, pos2, vec2};

use crate::backend::local::library::{file_path, file_url};
use crate::backend::local::tools::ToolRequest;
use crate::backend::media_cache::{Lookup, MediaCache};
use crate::backend::wire::MediaFile;
use crate::feed::types::MediaKind;
use crate::overlays::lightbox::fit_inside;
use crate::pages::common::{Env, pick_files};
use crate::prompt_box::refs::RefKind;
use crate::theme;
use crate::ui::icons::{self, Icon};
use crate::ui::widgets;

/// The options column.
pub const PANEL_WIDTH: f32 = 320.0;
const STRIP_HEIGHT: f32 = 112.0;
const GAP: f32 = 16.0;
const THUMB: f32 = 80.0;

static NEXT_TOOL_ID: AtomicU64 = AtomicU64::new(1);

/// A file being worked on.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
  pub path: PathBuf,
  /// Its `file://` link (what the media cache and player read).
  pub url: String,
  pub kind: MediaKind,
}

impl Source {
  pub fn from_path(path: PathBuf) -> Self {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let kind = match ext.as_str() {
      "mp4" | "mov" | "m4v" | "webm" => MediaKind::Video,
      "mp3" | "wav" | "m4a" | "ogg" | "flac" => MediaKind::Audio,
      _ => MediaKind::Image,
    };
    Self { url: file_url(&path), path, kind }
  }

  /// A library file (local mode links are `file://`).
  pub fn from_url(url: &str) -> Option<Self> {
    file_path(url).map(Self::from_path)
  }

  pub fn name(&self) -> String {
    self.path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_owned()
  }
}

/// What a tool page shares: its source, its results, and the run in flight.
#[derive(Default)]
pub struct ToolSession {
  pub source: Option<Source>,
  /// Newest first.
  pub results: Vec<Source>,
  pub running: Option<u64>,
  pub error: Option<String>,
}

impl ToolSession {
  /// Sends `request` to the engine.
  pub fn run(&mut self, env: &mut Env<'_>, request: ToolRequest) {
    let id = NEXT_TOOL_ID.fetch_add(1, Ordering::Relaxed);
    self.running = Some(id);
    self.error = None;
    env.backend.run_tool(id, request);
  }

  /// Takes a finished run if it's this page's; `true` when it was.
  pub fn finish(&mut self, id: u64, result: &Result<Vec<MediaFile>, String>) -> bool {
    if self.running != Some(id) {
      return false;
    }
    self.running = None;
    match result {
      Ok(files) => {
        for file in files.iter().rev() {
          if let Some(source) = Source::from_url(&file.media_links.cdn_url) {
            self.results.insert(0, source);
          }
        }
      },
      Err(message) => self.error = Some(message.clone()),
    }
    true
  }

  pub fn busy(&self) -> bool {
    self.running.is_some()
  }
}

/// The page's three areas: stage, options panel, results strip.
pub fn layout(area: Rect) -> (Rect, Rect, Rect) {
  let inner = area.shrink(GAP);
  let strip = Rect::from_min_max(pos2(inner.left(), inner.bottom() - STRIP_HEIGHT), inner.max);
  let top = Rect::from_min_max(inner.min, pos2(inner.right(), strip.top() - GAP));
  let panel = Rect::from_min_max(pos2(top.right() - PANEL_WIDTH, top.top()), top.max);
  let stage = Rect::from_min_max(top.min, pos2(panel.left() - GAP, top.bottom()));
  (stage, panel, strip)
}

/// The empty stage: open a file or drop one. Returns a picked or dropped file of an accepted kind.
pub fn empty_stage(ui: &mut Ui, rect: Rect, accepts: &[RefKind], what: &str) -> Option<PathBuf> {
  let p = ui.painter();
  p.rect(rect, theme::RADIUS, theme::fade(theme::INK, 0.015), egui::Stroke::new(1.0, theme::LINE_DASHED), egui::StrokeKind::Inside);
  widgets::corner_marks(p, rect);
  let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_center_size(rect.center(), vec2(rect.width().min(460.0), 150.0))).layout(egui::Layout::top_down(egui::Align::Center)));
  child.label(widgets::heading(&format!("Open {what}"), 26.0));
  child.add_space(6.0);
  child.label(egui::RichText::new("Drop a file here, choose one, or use \u{201c}Open in\u{201d} from any item in your library.").size(13.0).color(theme::MUTED));
  child.add_space(14.0);
  let mut picked = None;
  if widgets::button(&mut child, Some(Icon::Folder), "Open file", widgets::Kind::Primary, 0.0, theme::CONTROL_H).clicked() {
    picked = pick_files(accepts, false).into_iter().next();
  }
  picked.or_else(|| dropped_file(ui, accepts))
}

/// A file dropped on the window, if it's a kind the page accepts.
pub fn dropped_file(ui: &Ui, accepts: &[RefKind]) -> Option<PathBuf> {
  let files = ui.ctx().input(|i| i.raw.dropped_files.clone());
  files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).find(|p| accepts.contains(&kind_of(p)))
}

fn kind_of(path: &Path) -> RefKind {
  match Source::from_path(path.to_path_buf()).kind {
    MediaKind::Video => RefKind::Video,
    MediaKind::Audio => RefKind::Audio,
    MediaKind::Image => RefKind::Image,
  }
}

/// An image fitted into `rect`. Returns where it was drawn and the image's pixel size, once loaded.
pub fn show_image(ui: &mut Ui, rect: Rect, cache: &mut MediaCache, url: &str) -> Option<(Rect, [usize; 2])> {
  ui.painter().rect_filled(rect, theme::RADIUS, theme::SUNKEN);
  checkerboard(ui, rect);
  match cache.get(ui.ctx(), url) {
    Lookup::Ready(texture) => {
      let size = fit_inside(texture.size_vec2(), rect.shrink(12.0).size());
      let shown = Rect::from_center_size(rect.center(), size);
      egui::Image::new(&texture).paint_at(ui, shown);
      Some((shown, texture.size()))
    },
    Lookup::Loading => {
      icons::paint_spinner(ui, Rect::from_center_size(rect.center(), vec2(28.0, 28.0)), theme::MUTED);
      None
    },
    Lookup::Failed => {
      ui.painter().text(rect.center(), Align2::CENTER_CENTER, "Couldn't open this image", FontId::new(14.0, egui::FontFamily::Proportional), theme::FAINT);
      None
    },
  }
}

/// A faint checkerboard, so transparent cut-outs read as transparent.
fn checkerboard(ui: &Ui, rect: Rect) {
  let cell = 12.0;
  let painter = ui.painter_at(rect);
  let cols = (rect.width() / cell).ceil() as i32;
  let rows = (rect.height() / cell).ceil() as i32;
  for y in 0..rows {
    for x in 0..cols {
      if (x + y) % 2 == 0 {
        let min = pos2(rect.left() + x as f32 * cell, rect.top() + y as f32 * cell);
        painter.rect_filled(Rect::from_min_size(min, vec2(cell, cell)), 0.0, theme::fade(theme::INK, 0.03));
      }
    }
  }
}

/// The source's name and a "Change" button over the stage's top-left corner.
pub fn source_bar(ui: &mut Ui, rect: Rect, source: &Source, accepts: &[RefKind]) -> Option<PathBuf> {
  let bar = Rect::from_min_size(rect.min + vec2(10.0, 10.0), vec2(rect.width() - 20.0, 30.0));
  let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(egui::Layout::left_to_right(egui::Align::Center)));
  let mut picked = None;
  if widgets::button(&mut child, Some(Icon::Folder), "Change", widgets::Kind::Secondary, 0.0, 28.0).clicked() {
    picked = pick_files(accepts, false).into_iter().next();
  }
  child.add_space(8.0);
  child.label(egui::RichText::new(source.name()).size(12.0).family(theme::mono()).color(theme::MUTED));
  picked.or_else(|| dropped_file(ui, accepts))
}

/// The options panel's frame and heading; `body` draws the options.
pub fn panel(ui: &mut Ui, rect: Rect, title: &str, blurb: &str, body: impl FnOnce(&mut Ui)) {
  ui.painter().rect(rect, theme::RADIUS, theme::PANEL, theme::hairline(), egui::StrokeKind::Inside);
  let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(16.0)).layout(egui::Layout::top_down(egui::Align::Min)));
  egui::ScrollArea::vertical().id_salt(("tool-panel", title)).auto_shrink([false, false]).show(&mut child, |ui| {
    ui.set_width(rect.width() - 40.0);
    ui.label(widgets::heading(title, 24.0));
    ui.add_space(4.0);
    ui.label(egui::RichText::new(blurb).size(12.5).color(theme::MUTED));
    ui.add_space(14.0);
    body(ui);
  });
}

/// The run button, with the cost beside it ("Free" for local tools). Returns `true` when clicked.
pub fn run_button(ui: &mut Ui, label: &str, cost: Option<f64>, enabled: bool, session: &ToolSession) -> bool {
  ui.add_space(12.0);
  if let Some(error) = &session.error {
    ui.label(egui::RichText::new(error).size(12.0).color(theme::BAD));
    ui.add_space(8.0);
  }
  let mut clicked = false;
  ui.horizontal(|ui| {
    let text = if session.busy() { "Working\u{2026}" } else { label };
    let button = ui.add_enabled_ui(enabled && !session.busy(), |ui| widgets::button(ui, Some(Icon::Sparkles), text, widgets::Kind::Accent, 0.0, theme::CONTROL_H)).inner;
    clicked = button.clicked();
    if session.busy() {
      let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
      icons::paint_spinner(ui, r, theme::MUTED);
    }
    if let Some(cost) = cost {
      ui.add_space(6.0);
      let text = if cost <= 0.0 { "Free \u{b7} on your Mac".to_owned() } else { format!("\u{2248} {}", crate::shell::usd(cost)) };
      ui.label(egui::RichText::new(text).size(12.5).family(theme::mono()).color(theme::MUTED)).on_hover_text("Estimated at list prices; billed to your own account");
    }
  });
  clicked
}

/// A labelled group of options in the panel.
pub fn field(ui: &mut Ui, label: &str) {
  ui.add_space(10.0);
  widgets::hud(ui, label, theme::FAINT);
  ui.add_space(4.0);
}

/// A row of mutually exclusive choices; returns the picked index.
pub fn segmented(ui: &mut Ui, options: &[&str], selected: usize) -> Option<usize> {
  let mut picked = None;
  ui.horizontal_wrapped(|ui| {
    ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
    for (i, label) in options.iter().enumerate() {
      let on = i == selected;
      let galley = ui.painter().layout_no_wrap((*label).to_owned(), FontId::new(12.5, theme::medium()), if on { theme::INVERT_FG } else { theme::INK });
      let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 22.0, 30.0), Sense::click());
      let fill = if on {
        theme::INVERT_BG
      } else if resp.hovered() {
        theme::WASH_HOVER
      } else {
        theme::WASH
      };
      ui.painter().rect(rect, theme::RADIUS, fill, if on { egui::Stroke::NONE } else { theme::hairline() }, egui::StrokeKind::Inside);
      ui.painter().galley(rect.center() - galley.size() / 2.0, galley, theme::INK);
      if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        picked = Some(i);
      }
    }
  });
  picked
}

/// A multi-line text box for prompts.
pub fn prompt_field(ui: &mut Ui, id: &str, text: &mut String, hint: &str) {
  egui::Frame::new().fill(theme::SUNKEN).stroke(theme::hairline()).corner_radius(theme::RADIUS).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
    ui.add(egui::TextEdit::multiline(text).id_salt(id).frame(egui::Frame::NONE).desired_rows(3).desired_width(f32::INFINITY).hint_text(egui::RichText::new(hint).color(theme::FAINT)));
  });
}

/// This session's results; returns one clicked to use as the next source.
pub fn results_strip(ui: &mut Ui, rect: Rect, results: &[Source], cache: &mut MediaCache, empty: &str) -> Option<Source> {
  ui.painter().rect(rect, theme::RADIUS, theme::fade(theme::INK, 0.01), theme::hairline(), egui::StrokeKind::Inside);
  let label = Rect::from_min_size(rect.min + vec2(14.0, 10.0), vec2(rect.width(), 16.0));
  ui.painter().galley(label.min, widgets::caps(ui, "Results \u{b7} also in your library", 10.0, theme::mono(), 1.4, theme::FAINT), theme::FAINT);
  let row = Rect::from_min_max(pos2(rect.left() + 14.0, rect.top() + 30.0), pos2(rect.right() - 14.0, rect.bottom() - 8.0));
  if results.is_empty() {
    ui.painter().text(row.left_center(), Align2::LEFT_CENTER, empty, FontId::new(12.5, egui::FontFamily::Proportional), theme::FAINT);
    return None;
  }
  let mut child = ui.new_child(egui::UiBuilder::new().max_rect(row));
  let mut picked = None;
  egui::ScrollArea::horizontal().id_salt(("tool-results", empty)).show(&mut child, |ui| {
    ui.horizontal(|ui| {
      ui.spacing_mut().item_spacing.x = 8.0;
      for result in results {
        let (r, resp) = ui.allocate_exact_size(vec2(THUMB, THUMB - 8.0), Sense::click());
        ui.painter().rect_filled(r, theme::RADIUS, theme::SUNKEN);
        let thumb = match result.kind {
          MediaKind::Image => Some(result.url.clone()),
          _ => None,
        };
        match thumb.map(|u| cache.get(ui.ctx(), &u)) {
          Some(Lookup::Ready(t)) => {
            let fit = fit_inside(t.size_vec2(), r.size());
            egui::Image::new(&t).paint_at(ui, Rect::from_center_size(r.center(), fit));
          },
          _ => icons::paint(ui.painter(), Rect::from_center_size(r.center(), vec2(20.0, 20.0)), if result.kind == MediaKind::Video { Icon::Video } else { Icon::Image }, theme::MUTED),
        }
        if resp.hovered() {
          ui.painter().rect_stroke(r, theme::RADIUS, egui::Stroke::new(1.5, theme::ACCENT), egui::StrokeKind::Inside);
        }
        if resp.on_hover_text(format!("{} \u{b7} click to keep working on it", result.name())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
          picked = Some(result.clone());
        }
      }
    });
  });
  picked
}

/// A dark wash over the stage while a run is in flight.
pub fn busy_overlay(ui: &Ui, rect: Rect, text: &str) {
  ui.painter().rect_filled(rect, theme::RADIUS, Color32::from_black_alpha(140));
  icons::paint_spinner(ui, Rect::from_center_size(rect.center() - vec2(0.0, 14.0), vec2(28.0, 28.0)), Color32::WHITE);
  ui.painter().text(rect.center() + vec2(0.0, 22.0), Align2::CENTER_CENTER, text, FontId::new(13.0, egui::FontFamily::Proportional), Color32::WHITE);
}
