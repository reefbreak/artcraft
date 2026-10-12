//! Edit Video: trim a clip and join clips on the user's Mac (FFmpeg, free), or extend a clip by
//! about 7 seconds with Veo on Vertex.

use std::path::PathBuf;

use egui::{Rect, Ui};

use super::{Source, ToolSession};
use crate::backend::local::tools::{ToolRequest, VeoTier};
use crate::backend::video::{self, VideoPlayer};
use crate::pages::common::{Env, pick_files};
use crate::prompt_box::refs::RefKind;
use crate::theme;
use crate::ui::icons::Icon;
use crate::ui::widgets;

const ACCEPTS: [RefKind; 1] = [RefKind::Video];
const TIERS: [VeoTier; 3] = [VeoTier::Fast, VeoTier::Standard, VeoTier::Lite];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
  #[default]
  Trim,
  Join,
  Extend,
}

#[derive(Default)]
pub struct EditVideoPage {
  pub session: ToolSession,
  mode: Mode,
  player: Option<VideoPlayer>,
  start: Option<f64>,
  end: Option<f64>,
  /// Clips to join, in order (the source is added first).
  clips: Vec<Source>,
  prompt: String,
  tier: usize,
}

impl EditVideoPage {
  pub fn open(&mut self, path: PathBuf) {
    let source = Source::from_path(path);
    if !self.clips.contains(&source) {
      self.clips.push(source.clone());
    }
    self.session.source = Some(source);
    self.player = None;
    self.start = None;
    self.end = None;
    self.session.error = None;
  }

  pub fn ui(&mut self, ui: &mut Ui, env: &mut Env<'_>) {
    let (stage, panel, strip) = super::layout(ui.max_rect());
    match self.session.source.clone() {
      None => {
        if let Some(path) = super::empty_stage(ui, stage, &ACCEPTS, "a video") {
          self.open(path);
        }
      },
      Some(source) => {
        ui.painter().rect_filled(stage, theme::RADIUS, theme::SUNKEN);
        let player = self.player.get_or_insert_with(|| VideoPlayer::new(&source.url));
        player.ui(ui, stage.shrink(12.0).with_min_y(stage.top() + 48.0), None);
        if let Some(path) = super::source_bar(ui, stage, &source, &ACCEPTS) {
          self.open(path);
        }
        if self.session.busy() {
          super::busy_overlay(ui, stage, if self.mode == Mode::Extend { "Veo is extending the clip (about a minute)\u{2026}" } else { "Working\u{2026}" });
        }
      },
    }
    if !video::ffmpeg_available() {
      self.session.error = Some("Edit Video needs FFmpeg. Install it with `brew install ffmpeg`, then restart the app.".to_owned());
    }
    self.panel(ui, panel, env);
    if let Some(picked) = super::results_strip(ui, strip, &self.session.results, env.cache, "Edited clips appear here. Click one to keep working on it.") {
      self.open(picked.path);
    }
  }

  fn panel(&mut self, ui: &mut Ui, rect: Rect, env: &mut Env<'_>) {
    let position = self.player.as_ref().map_or(0.0, VideoPlayer::position);
    let duration = self.player.as_ref().and_then(VideoPlayer::duration);
    let has_source = self.session.source.is_some();
    let extend_cost = env.backend.estimate_tool(&ToolRequest::ExtendVideo { video: PathBuf::new(), prompt: String::new(), tier: TIERS[self.tier] });
    let mut run = None;
    super::panel(ui, rect, "Edit Video", "Trim and join clips on your Mac, or have Veo continue a clip.", |ui| {
      let modes = [Mode::Trim, Mode::Join, Mode::Extend];
      if let Some(i) = super::segmented(ui, &["Trim", "Join", "Extend"], modes.iter().position(|m| *m == self.mode).unwrap_or(0)) {
        self.mode = modes[i];
      }
      match self.mode {
        Mode::Trim => {
          super::field(ui, "Keep from \u{2192} to");
          ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(has_source, |ui| {
              if widgets::button(ui, None, &format!("Start here ({position:.2}s)"), widgets::Kind::Secondary, 0.0, 30.0).clicked() {
                self.start = Some(position);
              }
              if widgets::button(ui, None, &format!("End here ({position:.2}s)"), widgets::Kind::Secondary, 0.0, 30.0).clicked() {
                self.end = Some(position);
              }
            });
          });
          let (start, end) = (self.start.unwrap_or(0.0), self.end.or(duration).unwrap_or(0.0));
          ui.label(egui::RichText::new(format!("{start:.2}s \u{2192} {end:.2}s  ({:.2}s)", (end - start).max(0.0))).size(13.0).family(theme::mono()));
          if super::run_button(ui, "Trim clip", Some(0.0), has_source && end > start + 0.05, &self.session) {
            if let Some(source) = &self.session.source {
              run = Some(ToolRequest::TrimVideo { video: source.path.clone(), start, end });
            }
          }
        },
        Mode::Join => {
          super::field(ui, "Clips, in order");
          let mut action = None;
          for (i, clip) in self.clips.iter().enumerate() {
            ui.horizontal(|ui| {
              ui.label(egui::RichText::new(format!("{}. {}", i + 1, clip.name())).size(12.5));
              ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::ghost(ui, Icon::X, None, "Remove", false).clicked() {
                  action = Some((i, 0));
                }
                if i > 0 && widgets::ghost(ui, Icon::ChevronUp, None, "Move up", false).clicked() {
                  action = Some((i, 1));
                }
              });
            });
          }
          match action {
            Some((i, 0)) => {
              self.clips.remove(i);
            },
            Some((i, _)) => self.clips.swap(i, i - 1),
            None => {},
          }
          ui.add_space(6.0);
          if widgets::button(ui, Some(Icon::Plus), "Add clips", widgets::Kind::Secondary, 0.0, 30.0).clicked() {
            for path in pick_files(&ACCEPTS, true) {
              self.clips.push(Source::from_path(path));
            }
          }
          ui.label(egui::RichText::new("Clips are scaled to the first one's size. Sound is kept when every clip has it.").size(12.0).color(theme::FAINT));
          if super::run_button(ui, &format!("Join {} clips", self.clips.len()), Some(0.0), self.clips.len() >= 2, &self.session) {
            run = Some(ToolRequest::JoinVideos { videos: self.clips.iter().map(|c| c.path.clone()).collect() });
          }
        },
        Mode::Extend => {
          super::field(ui, "What happens next");
          super::prompt_field(ui, "extend-prompt", &mut self.prompt, "The camera keeps tracking her as she walks into the market");
          super::field(ui, "Model");
          if let Some(i) = super::segmented(ui, &TIERS.map(VeoTier::label), self.tier) {
            self.tier = i;
          }
          ui.label(egui::RichText::new("Adds about 7 seconds. Clips up to 30 s; they're converted to 24 fps 720p first.").size(12.0).color(theme::FAINT));
          if super::run_button(ui, "Extend clip", extend_cost, has_source && !self.prompt.trim().is_empty(), &self.session) {
            if let Some(source) = &self.session.source {
              run = Some(ToolRequest::ExtendVideo { video: source.path.clone(), prompt: self.prompt.trim().to_owned(), tier: TIERS[self.tier] });
            }
          }
        },
      }
    });
    if let Some(request) = run {
      self.session.run(env, request);
    }
  }
}
