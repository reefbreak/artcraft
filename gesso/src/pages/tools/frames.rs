//! Frame Extract: play a clip, mark moments (or take one every few seconds), and save them as
//! stills in the library. Runs on FFmpeg, free.

use std::path::PathBuf;

use egui::{Ui, vec2};

use super::{Source, ToolSession};
use crate::backend::local::tools::ToolRequest;
use crate::backend::video::{self, VideoPlayer};
use crate::pages::common::Env;
use crate::prompt_box::refs::RefKind;
use crate::theme;
use crate::ui::icons::Icon;
use crate::ui::widgets;

const ACCEPTS: [RefKind; 1] = [RefKind::Video];
const INTERVALS: [(f64, &str); 4] = [(1.0, "1s"), (2.0, "2s"), (5.0, "5s"), (10.0, "10s")];
/// More frames than this in one run is almost certainly a mistake (a long clip at 1 s).
const MAX_FRAMES: usize = 120;

#[derive(Default)]
pub struct FrameExtractPage {
  pub session: ToolSession,
  player: Option<VideoPlayer>,
  /// Marked moments, in seconds, in the order marked.
  marks: Vec<f64>,
  /// 0: marked moments; 1: every few seconds.
  mode: usize,
  interval: usize,
}

impl FrameExtractPage {
  pub fn open(&mut self, path: PathBuf) {
    self.session.source = Some(Source::from_path(path));
    self.player = None;
    self.marks.clear();
    self.session.error = None;
  }

  pub fn ui(&mut self, ui: &mut Ui, env: &mut Env<'_>) {
    let (stage, panel, strip) = super::layout(ui.max_rect());
    let Some(source) = self.session.source.clone() else {
      if let Some(path) = super::empty_stage(ui, stage, &ACCEPTS, "a video") {
        self.open(path);
      }
      self.panel(ui, panel, env);
      self.strip(ui, strip, env);
      return;
    };
    if !video::ffmpeg_available() {
      self.session.error = Some("Frame Extract needs FFmpeg. Install it with `brew install ffmpeg`, then restart the app.".to_owned());
    }
    ui.painter().rect_filled(stage, theme::RADIUS, theme::SUNKEN);
    let player = self.player.get_or_insert_with(|| VideoPlayer::new(&source.url));
    let video_rect = stage.shrink2(vec2(12.0, 12.0)).with_min_y(stage.top() + 48.0);
    player.ui(ui, video_rect, None);
    if let Some(path) = super::source_bar(ui, stage, &source, &ACCEPTS) {
      self.open(path);
    }
    if self.session.busy() {
      super::busy_overlay(ui, stage, "Extracting frames\u{2026}");
    }
    self.panel(ui, panel, env);
    self.strip(ui, strip, env);
  }

  fn panel(&mut self, ui: &mut Ui, rect: egui::Rect, env: &mut Env<'_>) {
    let duration = self.player.as_ref().and_then(VideoPlayer::duration);
    let position = self.player.as_ref().map_or(0.0, VideoPlayer::position);
    let has_source = self.session.source.is_some();
    let mut run = None;
    super::panel(ui, rect, "Frame Extract", "Save stills from a clip into your library, to use as references or start frames.", |ui| {
      if let Some(m) = super::segmented(ui, &["Marked moments", "Every few seconds"], self.mode) {
        self.mode = m;
      }
      if self.mode == 0 {
        super::field(ui, "Mark moments");
        ui.horizontal_wrapped(|ui| {
          ui.add_enabled_ui(has_source, |ui| {
            if widgets::button(ui, Some(Icon::Plus), &format!("This frame ({position:.2}s)"), widgets::Kind::Secondary, 0.0, 30.0).clicked() {
              self.marks.push(position);
            }
            if widgets::button(ui, None, "First", widgets::Kind::Secondary, 0.0, 30.0).clicked() {
              self.marks.push(0.0);
            }
            if let Some(d) = duration {
              if widgets::button(ui, None, "Last", widgets::Kind::Secondary, 0.0, 30.0).clicked() {
                self.marks.push((d - 0.05).max(0.0));
              }
            }
          });
        });
        ui.add_space(6.0);
        let mut remove = None;
        for (i, t) in self.marks.iter().enumerate() {
          ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("{t:.2}s")).size(13.0).family(theme::mono()));
            if ui.add(egui::Button::new(egui::RichText::new("Remove").size(11.5).color(theme::MUTED)).frame(false)).clicked() {
              remove = Some(i);
            }
          });
        }
        if let Some(i) = remove {
          self.marks.remove(i);
        }
        if self.marks.is_empty() {
          ui.label(egui::RichText::new("Pause where you want a still, then mark it.").size(12.0).color(theme::FAINT));
        }
      } else {
        super::field(ui, "One frame every");
        if let Some(i) = super::segmented(ui, &INTERVALS.map(|(_, l)| l), self.interval) {
          self.interval = i;
        }
      }
      let times = self.times(duration);
      let enabled = has_source && !times.is_empty() && times.len() <= MAX_FRAMES;
      if times.len() > MAX_FRAMES {
        ui.label(egui::RichText::new(format!("That's {} frames; pick a longer interval (max {MAX_FRAMES}).", times.len())).size(12.0).color(theme::BAD));
      }
      let label = format!("Extract {} frame{}", times.len(), if times.len() == 1 { "" } else { "s" });
      if super::run_button(ui, &label, Some(0.0), enabled, &self.session) {
        if let Some(source) = &self.session.source {
          run = Some(ToolRequest::ExtractFrames { video: source.path.clone(), times });
        }
      }
    });
    if let Some(request) = run {
      self.session.run(env, request);
    }
  }

  fn strip(&mut self, ui: &mut Ui, rect: egui::Rect, env: &mut Env<'_>) {
    if let Some(picked) = super::results_strip(ui, rect, &self.session.results, env.cache, "Extracted frames appear here") {
      // A still can go straight to the image tools; here it just opens in the library viewer.
      env.toasts.success(format!("{} is in your library", picked.name()));
    }
  }

  /// The moments to extract.
  fn times(&self, duration: Option<f64>) -> Vec<f64> {
    match (self.mode, duration) {
      (0, _) => self.marks.clone(),
      (_, Some(d)) => {
        let step = INTERVALS[self.interval].0;
        (0..).map(|i| i as f64 * step).take_while(|t| *t < d).collect()
      },
      _ => Vec::new(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn intervals_cover_the_clip() {
    let page = FrameExtractPage { mode: 1, interval: 2, ..Default::default() };
    assert_eq!(page.times(Some(12.0)), [0.0, 5.0, 10.0]);
    assert!(page.times(None).is_empty(), "nothing until the clip is probed");
    let marked = FrameExtractPage { marks: vec![3.0, 1.0], ..Default::default() };
    assert_eq!(marked.times(Some(12.0)), [3.0, 1.0]);
  }
}
