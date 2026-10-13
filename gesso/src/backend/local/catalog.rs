//! Local mode's model list: what the user's own Vertex and Bedrock accounts can run, in the shape
//! the pickers already understand ([`ModelInfo`]), plus how each one is called ([`Route`]).
//!
//! Model IDs checked against the providers' docs in Oct 2026: Imagen 4 and Amazon Nova
//! Canvas/Reel are retired, so they aren't offered.

use crate::models::ModelInfo;

use super::config::LocalConfig;

/// The "Auto" entries: Jev (or the fallback rules) picks one of the other models per request.
pub const AUTO_IMAGE: &str = "auto_image";
pub const AUTO_VIDEO: &str = "auto_video";

/// Aspect ratios the Gemini image models share (UI ids; see [`aspect_ratio`]).
const GEMINI_ASPECTS: [&str; 10] = ["square", "wide_sixteen_by_nine", "tall_nine_by_sixteen", "wide_four_by_three", "tall_three_by_four", "wide_three_by_two", "tall_two_by_three", "wide_five_by_four", "tall_four_by_five", "wide_twenty_one_by_nine"];
/// Stability's text-to-image ratios on Bedrock.
const STABILITY_ASPECTS: [&str; 9] = ["square", "wide_sixteen_by_nine", "tall_nine_by_sixteen", "wide_twenty_one_by_nine", "tall_nine_by_twenty_one", "wide_three_by_two", "tall_two_by_three", "wide_five_by_four", "tall_four_by_five"];
const VEO_ASPECTS: [&str; 2] = ["wide_sixteen_by_nine", "tall_nine_by_sixteen"];
/// Gemini 3.x takes up to 14 reference images.
const GEMINI_REFS_MAX: usize = 14;
/// Several images per request are made as parallel calls.
const IMAGE_BATCHES: [u16; 3] = [1, 2, 4];

/// How a model is called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
  /// Vertex `generateContent` with a Gemini image model.
  GeminiImage { model: &'static str },
  /// Vertex `predictLongRunning` with a Veo model.
  Veo { model: &'static str },
  /// Bedrock `InvokeModel` with a Stability model.
  Stability { model: &'static str },
  /// Vertex `predict` with a Lyria music model.
  Lyria { model: &'static str },
}

impl Route {
  pub fn provider_label(self) -> &'static str {
    match self {
      Route::GeminiImage { .. } | Route::Veo { .. } | Route::Lyria { .. } => "Vertex AI",
      Route::Stability { .. } => "Bedrock",
    }
  }
}

/// One model in the local catalog.
#[derive(Clone, Debug)]
pub struct LocalModel {
  pub info: ModelInfo,
  pub route: Route,
  /// What it's good at, for Jev's choice criteria.
  pub strengths: &'static str,
}

/// The image models the configured accounts can run, "Auto" first when there's a choice.
pub fn image_models(config: &LocalConfig) -> Vec<ModelInfo> {
  with_auto(AUTO_IMAGE, "Auto", "Jev picks the model for each prompt", image_candidates(config))
}

/// The video models the configured accounts can run, "Auto" first when there's a choice.
pub fn video_models(config: &LocalConfig) -> Vec<ModelInfo> {
  with_auto(AUTO_VIDEO, "Auto", "Jev picks the Veo tier for each prompt", video_candidates(config))
}

/// The concrete image models, in preference order (the fallback rules' default comes first).
pub fn image_candidates(config: &LocalConfig) -> Vec<LocalModel> {
  let mut out = Vec::new();
  if config.vertex_enabled() {
    out.push(gemini("nano_banana_2_vertex", "Nano Banana 2", "gemini-3.1-flash-image", &["half_k", "one_k", "two_k", "four_k"], 15.0, "fast, strong all-rounder; good text rendering; edits and multi-reference compositing; cheap"));
    out.push(gemini("nano_banana_pro_vertex", "Nano Banana Pro", "gemini-3-pro-image", &["one_k", "two_k", "four_k"], 30.0, "highest quality and reasoning; complex scenes, precise instructions, infographics, dense text, character consistency across many references; slower and more expensive"));
    out.push(gemini("nano_banana_2_lite_vertex", "Nano Banana 2 Lite", "gemini-3.1-flash-lite-image", &["one_k"], 8.0, "quickest and cheapest; drafts, thumbnails and rough ideas at 1K"));
  }
  if config.bedrock_enabled() {
    out.push(stability("stability_ultra_bedrock", "Stable Image Ultra", "stability.stable-image-ultra-v1:1", 1, 20.0, "photorealism, rich lighting and fine detail; artistic and painterly styles"));
    out.push(stability("stability_sd3p5_large_bedrock", "SD 3.5 Large", "stability.sd3-5-large-v1:0", 1, 15.0, "diverse art styles, illustration, concept art; image-to-image restyling of one reference"));
    out.push(stability("stability_core_bedrock", "Stable Image Core", "stability.stable-image-core-v1:1", 0, 6.0, "fast, low-cost text-to-image without references"));
  }
  out
}

/// The concrete video models, in preference order.
pub fn video_candidates(config: &LocalConfig) -> Vec<LocalModel> {
  if !config.veo_enabled() {
    return Vec::new();
  }
  vec![veo("veo_3p1_fast_vertex", "Veo 3.1 Fast", "veo-3.1-fast-generate-001", &["seven_twenty_p", "ten_eighty_p"], 60.0, "good quality at lower cost and latency; most everyday shots"), veo("veo_3p1_vertex", "Veo 3.1", "veo-3.1-generate-001", &["seven_twenty_p", "ten_eighty_p", "four_k"], 120.0, "best motion, physics, lip-synced dialogue and cinematic detail; hero shots and 4K"), veo("veo_3p1_lite_vertex", "Veo 3.1 Lite", "veo-3.1-lite-generate-001", &["seven_twenty_p"], 40.0, "cheapest and quickest; previz, animatics and drafts at 720p")]
}

/// The music models (Lyria 2 on Vertex: 30-second instrumental clips). Lyria 3 is in preview
/// behind an allowlist, so it isn't offered by default.
pub fn audio_models(config: &LocalConfig) -> Vec<ModelInfo> {
  audio_candidates(config).into_iter().map(|m| m.info).collect()
}

pub fn audio_candidates(config: &LocalConfig) -> Vec<LocalModel> {
  if !config.veo_enabled() {
    return Vec::new();
  }
  let info = ModelInfo { id: "lyria_2_vertex".to_owned(), name: "Lyria 2".to_owned(), description: "30-second instrumental music \u{b7} your Vertex AI account".to_owned(), creator: "google".to_owned(), family: "Lyria".to_owned(), prompt_supported: true, expected_secs: 25.0, ..Default::default() };
  vec![LocalModel { info, route: Route::Lyria { model: "lyria-002" }, strengths: "instrumental music from a description of genre, mood and instruments" }]
}

/// The concrete model behind `id` (not the Auto entries).
pub fn find(config: &LocalConfig, id: &str) -> Option<LocalModel> {
  image_candidates(config).into_iter().chain(video_candidates(config)).chain(audio_candidates(config)).find(|m| m.info.id == id)
}

/// A UI aspect ratio id as the providers write it (`wide_sixteen_by_nine` → `16:9`).
pub fn aspect_ratio(ui_id: &str) -> Option<&'static str> {
  Some(match ui_id {
    "square" => "1:1",
    "wide_sixteen_by_nine" => "16:9",
    "tall_nine_by_sixteen" => "9:16",
    "wide_four_by_three" => "4:3",
    "tall_three_by_four" => "3:4",
    "wide_three_by_two" => "3:2",
    "tall_two_by_three" => "2:3",
    "wide_five_by_four" => "5:4",
    "tall_four_by_five" => "4:5",
    "wide_twenty_one_by_nine" => "21:9",
    "tall_nine_by_twenty_one" => "9:21",
    _ => return None,
  })
}

/// A UI image resolution id as Gemini's `imageSize` (uppercase K; lowercase is rejected).
pub fn image_size(ui_id: &str) -> Option<&'static str> {
  Some(match ui_id {
    "half_k" => "512",
    "one_k" => "1K",
    "two_k" => "2K",
    "four_k" => "4K",
    _ => return None,
  })
}

/// A UI video resolution id as Veo's `resolution`.
pub fn video_resolution(ui_id: &str) -> Option<&'static str> {
  Some(match ui_id {
    "seven_twenty_p" => "720p",
    "ten_eighty_p" => "1080p",
    "four_k" => "4k",
    _ => return None,
  })
}

/// The candidates with an Auto entry in front, when there are at least two to choose between.
/// Auto offers the union of their settings, so any pick can be honored by some model.
fn with_auto(id: &str, name: &str, description: &str, candidates: Vec<LocalModel>) -> Vec<ModelInfo> {
  let mut models: Vec<ModelInfo> = candidates.iter().map(|m| m.info.clone()).collect();
  if candidates.len() < 2 {
    return models;
  }
  let first = &candidates[0].info;
  let mut auto = ModelInfo { id: id.to_owned(), name: name.to_owned(), description: description.to_owned(), creator: "gesso".to_owned(), family: "Auto".to_owned(), ..first.clone() };
  for m in candidates.iter().map(|m| &m.info) {
    union(&mut auto.aspect_ratios, &m.aspect_ratios);
    union(&mut auto.resolutions, &m.resolutions);
    union(&mut auto.batch_sizes, &m.batch_sizes);
    union(&mut auto.duration_options, &m.duration_options);
    auto.image_refs_max = auto.image_refs_max.max(m.image_refs_max);
    auto.end_frame |= m.end_frame;
    auto.start_frame |= m.start_frame;
    auto.sound_toggle |= m.sound_toggle;
  }
  auto.batch_sizes.sort_unstable();
  auto.duration_options.sort_unstable();
  models.insert(0, auto);
  models
}

fn union<T: Clone + PartialEq>(into: &mut Vec<T>, from: &[T]) {
  for item in from {
    if !into.contains(item) {
      into.push(item.clone());
    }
  }
}

fn gemini(id: &str, name: &str, model: &'static str, resolutions: &[&str], expected_secs: f32, strengths: &'static str) -> LocalModel {
  let info = ModelInfo { id: id.to_owned(), name: name.to_owned(), description: "Your Vertex AI account".to_owned(), creator: "google".to_owned(), family: "Nano Banana".to_owned(), prompt_supported: true, aspect_ratios: strings(&GEMINI_ASPECTS), aspect_default: Some("square".to_owned()), resolutions: strings(resolutions), resolution_default: Some(resolutions.iter().find(|r| **r == "one_k").unwrap_or(&resolutions[0]).to_string()), batch_sizes: IMAGE_BATCHES.to_vec(), batch_default: 1, image_refs_max: GEMINI_REFS_MAX, text_to_video: true, expected_secs, ..Default::default() };
  LocalModel { info, route: Route::GeminiImage { model }, strengths }
}

fn stability(id: &str, name: &str, model: &'static str, refs: usize, expected_secs: f32, strengths: &'static str) -> LocalModel {
  let info = ModelInfo { id: id.to_owned(), name: name.to_owned(), description: "Your AWS Bedrock account".to_owned(), creator: "stability".to_owned(), family: "Stability".to_owned(), prompt_supported: true, prompt_max: Some(10_000), aspect_ratios: strings(&STABILITY_ASPECTS), aspect_default: Some("square".to_owned()), batch_sizes: IMAGE_BATCHES.to_vec(), batch_default: 1, image_refs_max: refs, text_to_video: true, expected_secs, ..Default::default() };
  LocalModel { info, route: Route::Stability { model }, strengths }
}

fn veo(id: &str, name: &str, model: &'static str, resolutions: &[&str], expected_secs: f32, strengths: &'static str) -> LocalModel {
  let info = ModelInfo { id: id.to_owned(), name: name.to_owned(), description: "Your Vertex AI account".to_owned(), creator: "google".to_owned(), family: "Veo".to_owned(), prompt_supported: true, aspect_ratios: strings(&VEO_ASPECTS), aspect_default: Some("wide_sixteen_by_nine".to_owned()), resolutions: strings(resolutions), resolution_default: Some("seven_twenty_p".to_owned()), batch_sizes: vec![1, 2], batch_default: 1, text_to_video: true, start_frame: true, end_frame: true, sound_toggle: true, duration_options: vec![4, 6, 8], duration_default: Some(8), expected_secs, ..Default::default() };
  LocalModel { info, route: Route::Veo { model }, strengths }
}

fn strings<S: AsRef<str>>(items: &[S]) -> Vec<String> {
  items.iter().map(|s| s.as_ref().to_owned()).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn only_configured_accounts_are_offered() {
    let none = LocalConfig { bedrock_enabled: Some(false), ..Default::default() };
    assert!(image_models(&none).is_empty());
    assert!(video_models(&none).is_empty());
    let vertex = LocalConfig { gcp_project: Some("p".into()), bedrock_enabled: Some(false), ..Default::default() };
    let images = image_models(&vertex);
    assert_eq!(images[0].id, AUTO_IMAGE);
    assert!(images.iter().all(|m| m.id == AUTO_IMAGE || m.creator == "google"));
    assert_eq!(video_models(&vertex)[0].id, AUTO_VIDEO);
  }

  #[test]
  fn auto_offers_every_candidates_settings() {
    let both = LocalConfig { gcp_project: Some("p".into()), bedrock_enabled: Some(true), ..Default::default() };
    let auto = &image_models(&both)[0];
    assert!(auto.aspect_ratios.contains(&"tall_nine_by_twenty_one".to_owned()), "a Stability-only ratio");
    assert!(auto.resolutions.contains(&"four_k".to_owned()));
    assert_eq!(auto.image_refs_max, GEMINI_REFS_MAX);
  }

  #[test]
  fn a_single_candidate_gets_no_auto_entry() {
    let one = LocalConfig { gcp_api_key: Some("k".into()), bedrock_enabled: Some(false), ..Default::default() };
    assert!(video_models(&one).is_empty(), "express keys can't run Veo");
    assert_eq!(image_models(&one).len(), 4);
  }

  #[test]
  fn ui_settings_map_to_provider_values() {
    assert_eq!(aspect_ratio("wide_sixteen_by_nine"), Some("16:9"));
    assert_eq!(aspect_ratio("auto"), None);
    assert_eq!(image_size("two_k"), Some("2K"));
    assert_eq!(video_resolution("ten_eighty_p"), Some("1080p"));
  }
}
