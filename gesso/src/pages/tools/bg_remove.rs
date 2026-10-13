//! BG Remove: cut the subject out of an image, leaving a transparent background. Runs free on the
//! user's Mac (ISNet), or on Stability's Remove Background on Bedrock.

use std::path::PathBuf;

use egui::Ui;

use super::{Source, ToolSession};
use crate::backend::local::tools::{BackgroundEngine, ToolRequest};
use crate::pages::common::Env;
use crate::prompt_box::refs::RefKind;

const ACCEPTS: [RefKind; 1] = [RefKind::Image];

#[derive(Default)]
pub struct BgRemovePage {
  pub session: ToolSession,
  engine: BackgroundEngine,
  /// The cut-out made from the current source, shown instead of it until "Before" is picked.
  result: Option<Source>,
  show_before: bool,
}

impl BgRemovePage {
  pub fn open(&mut self, path: PathBuf) {
    self.session.source = Some(Source::from_path(path));
    self.result = None;
    self.show_before = false;
    self.session.error = None;
  }

  /// A finished run's first result becomes the shown cut-out.
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
      Some(source) => {
        let shown = match (&self.result, self.show_before) {
          (Some(result), false) => result.url.clone(),
          _ => source.url.clone(),
        };
        super::show_image(ui, stage, env.cache, &shown);
        if let Some(path) = super::source_bar(ui, stage, &source, &ACCEPTS) {
          self.open(path);
        }
        if self.session.busy() {
          let text = if self.engine == BackgroundEngine::Local { "Removing the background on your Mac\u{2026}" } else { "Removing the background on Bedrock\u{2026}" };
          super::busy_overlay(ui, stage, text);
        }
      },
    }
    let mut run = None;
    let cost = env.backend.estimate_tool(&ToolRequest::RemoveBackground { image: PathBuf::new(), engine: self.engine });
    let has_source = self.session.source.is_some();
    let has_result = self.result.is_some();
    super::panel(ui, panel, "Remove Background", "Cut the subject out and make the rest transparent. The result is a PNG you can use as a prop or layer.", |ui| {
      super::field(ui, "Run on");
      if let Some(i) = super::segmented(ui, &["Your Mac \u{b7} free", "Bedrock \u{b7} $0.07"], usize::from(self.engine == BackgroundEngine::Bedrock)) {
        self.engine = if i == 0 { BackgroundEngine::Local } else { BackgroundEngine::Bedrock };
      }
      ui.label(egui::RichText::new(if self.engine == BackgroundEngine::Local { "ISNet runs on your Mac. The first run downloads the model (179 MB) once." } else { "Stability's Remove Background, billed to your AWS account." }).size(12.0).color(crate::theme::FAINT));
      if has_result {
        super::field(ui, "Show");
        if let Some(i) = super::segmented(ui, &["After", "Before"], usize::from(self.show_before)) {
          self.show_before = i == 1;
        }
      }
      if super::run_button(ui, "Remove background", cost, has_source, &self.session) {
        if let Some(source) = &self.session.source {
          run = Some(ToolRequest::RemoveBackground { image: source.path.clone(), engine: self.engine });
        }
      }
    });
    if let Some(request) = run {
      self.session.run(env, request);
    }
    if let Some(picked) = super::results_strip(ui, strip, &self.session.results, env.cache, "Cut-outs appear here") {
      self.result = Some(picked);
      self.show_before = false;
    }
  }
}
