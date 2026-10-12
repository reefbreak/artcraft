//! Picking the model for a request: the one the user chose, or for "Auto", Jev's pick among the
//! models that can honor the request's settings, falling back to fixed rules.

use log::{info, warn};
use serde_json::{Map, Value, json};

use super::catalog::{self, LocalModel};
use super::config::LocalConfig;
use super::jev::{JevChoice, JevClient};
use crate::backend::Modality;
use crate::models::ModelInfo;

/// Resolutions a model with no resolution setting can satisfy.
const STANDARD_RESOLUTIONS: [&str; 3] = ["half_k", "one_k", "seven_twenty_p"];

/// A generation request, read from the page's fields (the OmniGen request shape).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Request {
  pub model_id: String,
  pub prompt: String,
  pub aspect_ratio: Option<String>,
  pub resolution: Option<String>,
  pub batch_count: u16,
  /// Reference images: the image page's references, or the video page's in reference mode.
  pub image_tokens: Vec<String>,
  pub start_frame: Option<String>,
  pub end_frame: Option<String>,
  pub duration_seconds: Option<u16>,
  pub generate_audio: Option<bool>,
  /// What to keep out (Lyria's negative prompt).
  pub negative_prompt: Option<String>,
}

impl Request {
  pub fn from_fields(modality: Modality, fields: &Map<String, Value>) -> Self {
    let string = |key: &str| fields.get(key).and_then(Value::as_str).map(str::to_owned);
    let tokens = |key: &str| fields.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
    let batch_key = if modality == Modality::Video { "video_batch_count" } else { "image_batch_count" };
    let mut image_tokens: Vec<String> = tokens("image_media_tokens");
    image_tokens.extend(tokens("reference_image_media_tokens"));
    Self { model_id: string("model").unwrap_or_default(), prompt: string("prompt").unwrap_or_default(), aspect_ratio: string("aspect_ratio"), resolution: string("resolution"), batch_count: fields.get(batch_key).and_then(Value::as_u64).map_or(1, |n| n.clamp(1, 8) as u16), image_tokens, start_frame: string("start_frame_image_media_token"), end_frame: string("end_frame_image_media_token"), duration_seconds: fields.get("duration_seconds").and_then(Value::as_u64).map(|n| n as u16), generate_audio: fields.get("generate_audio").and_then(Value::as_bool), negative_prompt: string("negative_prompt") }
  }

  /// What Jev sees about the request.
  fn state(&self, modality: Modality) -> Value {
    json!({
      "media": match modality { Modality::Video => "video", _ => "image" },
      "prompt": self.prompt,
      "aspect_ratio": self.aspect_ratio.as_deref().and_then(catalog::aspect_ratio),
      "resolution": self.resolution,
      "outputs_requested": self.batch_count,
      "reference_images": self.image_tokens.len(),
      "has_start_frame": self.start_frame.is_some(),
      "has_end_frame": self.end_frame.is_some(),
      "duration_seconds": self.duration_seconds,
      "with_audio": self.generate_audio,
    })
  }
}

/// The model that will run a request, and how it was chosen (for the toast and the lightbox).
#[derive(Clone, Debug)]
pub struct Decision {
  pub model: LocalModel,
  pub decided_by: DecidedBy,
  /// Jev's answer, when it was asked (even if its pick wasn't used).
  pub jev: Option<JevChoice>,
  /// Models that could honor the settings, by name.
  pub candidates: Vec<String>,
  /// Models that couldn't, by name with the reason.
  pub ruled_out: Vec<(String, &'static str)>,
}

/// Who chose the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecidedBy {
  /// The user picked a specific model.
  User,
  /// Auto, and only one model fit.
  OnlyFit,
  /// Auto, Jev's pick (confident enough).
  Jev,
  /// Auto, Jev answered below the confidence bar.
  RulesJevUnsure,
  /// Auto, Jev couldn't be reached or answered with an unknown option.
  RulesJevUnavailable,
  /// Auto, no Jev key.
  Rules,
}

impl DecidedBy {
  pub fn as_str(self) -> &'static str {
    match self {
      DecidedBy::User => "user",
      DecidedBy::OnlyFit => "only_fit",
      DecidedBy::Jev => "jev",
      DecidedBy::RulesJevUnsure => "rules_jev_unsure",
      DecidedBy::RulesJevUnavailable => "rules_jev_unavailable",
      DecidedBy::Rules => "rules",
    }
  }
}

impl Decision {
  /// The toast: which model runs and why.
  pub fn reason(&self) -> String {
    let name = &self.model.info.name;
    match self.decided_by {
      DecidedBy::User => format!("{name} via {}", self.model.route.provider_label()),
      DecidedBy::OnlyFit => format!("Auto \u{2192} {name} (the only fit)"),
      DecidedBy::Jev => format!("Auto \u{2192} {name} (Jev, {:.0}% confident)", self.jev.as_ref().map_or(0.0, |j| j.confidence * 100.0)),
      DecidedBy::RulesJevUnsure => format!("Auto \u{2192} {name} (Jev unsure, used rules)"),
      DecidedBy::RulesJevUnavailable => format!("Auto \u{2192} {name} (Jev unavailable, used rules)"),
      DecidedBy::Rules => format!("Auto \u{2192} {name} (rules)"),
    }
  }
}

/// The model for `request`: the named one, or for Auto, Jev's pick (when its confidence clears
/// the bar) else the first model, in preference order, that can honor the settings.
pub async fn decide(config: &LocalConfig, jev: Option<&JevClient>, modality: Modality, request: &Request) -> anyhow::Result<Decision> {
  let auto = matches!(request.model_id.as_str(), catalog::AUTO_IMAGE | catalog::AUTO_VIDEO);
  if !auto {
    let model = catalog::find(config, &request.model_id).ok_or_else(|| anyhow::anyhow!("{} isn't available with your configured accounts", request.model_id))?;
    return Ok(Decision { candidates: vec![model.info.name.clone()], model, decided_by: DecidedBy::User, jev: None, ruled_out: Vec::new() });
  }
  let candidates = all_candidates(config, modality);
  let ruled_out: Vec<(String, &'static str)> = candidates.iter().filter_map(|m| misfit(&m.info, request).map(|why| (m.info.name.clone(), why))).collect();
  let fitting: Vec<LocalModel> = candidates.into_iter().filter(|m| fits(&m.info, request)).collect();
  let Some(fallback) = rule_pick(&fitting, request) else {
    anyhow::bail!("None of your models can do this combination of settings");
  };
  let names: Vec<String> = fitting.iter().map(|m| m.info.name.clone()).collect();
  let decision = |model: LocalModel, decided_by: DecidedBy, jev: Option<JevChoice>| Decision { model, decided_by, jev, candidates: names.clone(), ruled_out: ruled_out.clone() };
  if fitting.len() == 1 {
    return Ok(decision(fallback, DecidedBy::OnlyFit, None));
  }
  let Some(jev) = jev else {
    return Ok(decision(fallback, DecidedBy::Rules, None));
  };
  let options: Vec<(String, String)> = fitting.iter().map(|m| (m.info.id.clone(), format!("{} on {}: {}", m.info.name, m.route.provider_label(), m.strengths))).collect();
  match jev.choose(&request.state(modality), &options).await {
    Ok(choice) => match fitting.iter().find(|m| m.info.id == choice.choice) {
      Some(model) if choice.confidence >= config.jev_min_confidence => {
        info!("Jev routed to {} (p={:.2}, confidence={:.2})", choice.choice, choice.probability, choice.confidence);
        Ok(decision(model.clone(), DecidedBy::Jev, Some(choice)))
      },
      Some(_) => Ok(decision(fallback, DecidedBy::RulesJevUnsure, Some(choice))),
      None => {
        warn!("Jev picked an unknown option: {}", choice.choice);
        Ok(decision(fallback, DecidedBy::RulesJevUnavailable, Some(choice)))
      },
    },
    Err(err) => {
      warn!("Jev routing failed, using rules: {err}");
      Ok(decision(fallback, DecidedBy::RulesJevUnavailable, None))
    },
  }
}

/// What a request could run on and what the rules would pick, without asking Jev: for the cost
/// estimate shown before generating. `None` when nothing fits.
pub fn plan(config: &LocalConfig, modality: Modality, request: &Request) -> Option<(Vec<LocalModel>, LocalModel)> {
  if !matches!(request.model_id.as_str(), catalog::AUTO_IMAGE | catalog::AUTO_VIDEO) {
    let model = catalog::find(config, &request.model_id)?;
    return Some((vec![model.clone()], model));
  }
  let fitting: Vec<LocalModel> = all_candidates(config, modality).into_iter().filter(|m| fits(&m.info, request)).collect();
  let pick = rule_pick(&fitting, request)?;
  Some((fitting, pick))
}

fn all_candidates(config: &LocalConfig, modality: Modality) -> Vec<LocalModel> {
  match modality {
    Modality::Video => catalog::video_candidates(config),
    Modality::Audio => catalog::audio_candidates(config),
    Modality::Image => catalog::image_candidates(config),
  }
}

/// Whether a model can honor every setting in the request.
pub fn fits(model: &ModelInfo, request: &Request) -> bool {
  misfit(model, request).is_none()
}

/// Why a model can't honor the request, if it can't. A model without resolution options makes
/// about 1K (Stability): it fits standard requests, not 2K and up.
fn misfit(model: &ModelInfo, request: &Request) -> Option<&'static str> {
  if request.aspect_ratio.as_ref().is_some_and(|a| !model.aspect_ratios.is_empty() && !model.aspect_ratios.contains(a)) {
    return Some("aspect ratio");
  }
  if request.resolution.as_ref().is_some_and(|r| !model.resolutions.contains(r) && !(model.resolutions.is_empty() && STANDARD_RESOLUTIONS.contains(&r.as_str()))) {
    return Some("resolution");
  }
  if request.image_tokens.len() > model.image_refs_max {
    return Some("too many references");
  }
  if (request.start_frame.is_some() && !model.start_frame) || (request.end_frame.is_some() && !model.end_frame) {
    return Some("frames");
  }
  if request.duration_seconds.is_some_and(|d| !model.duration_options.is_empty() && !model.duration_options.contains(&d)) {
    return Some("duration");
  }
  None
}

/// The rules: several references go to the model best at combining them; otherwise the first
/// fitting model in the catalog's preference order.
fn rule_pick(fitting: &[LocalModel], request: &Request) -> Option<LocalModel> {
  if request.image_tokens.len() > 3 {
    if let Some(pro) = fitting.iter().find(|m| m.info.id == "nano_banana_pro_vertex") {
      return Some(pro.clone());
    }
  }
  fitting.first().cloned()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn page_fields_become_a_request() {
    let image = json!({ "model": "auto_image", "prompt": "a fox", "aspect_ratio": "square", "resolution": "two_k", "image_batch_count": 2, "image_media_tokens": ["mf_local_a"] });
    let r = Request::from_fields(Modality::Image, image.as_object().unwrap());
    assert_eq!((r.batch_count, r.image_tokens.len(), r.resolution.as_deref()), (2, 1, Some("two_k")));
    let video = json!({ "model": "veo_3p1_vertex", "prompt": "waves", "duration_seconds": 6, "generate_audio": true, "video_batch_count": 2, "start_frame_image_media_token": "mf_local_s" });
    let r = Request::from_fields(Modality::Video, video.as_object().unwrap());
    assert_eq!((r.batch_count, r.duration_seconds, r.generate_audio, r.start_frame.as_deref()), (2, Some(6), Some(true), Some("mf_local_s")));
  }

  #[tokio::test]
  async fn auto_without_jev_uses_the_rules() {
    let config = LocalConfig { gcp_project: Some("p".into()), bedrock_enabled: Some(true), ..Default::default() };
    let pick = |fields: Value| {
      let config = config.clone();
      async move { decide(&config, None, Modality::Image, &Request::from_fields(Modality::Image, fields.as_object().unwrap())).await.unwrap().model.info.id }
    };
    assert_eq!(pick(json!({ "model": "auto_image", "prompt": "x" })).await, "nano_banana_2_vertex", "first in preference order");
    // The image page always sends a resolution for Auto; Stability (which has none) must still fit.
    assert_eq!(pick(json!({ "model": "auto_image", "prompt": "x", "aspect_ratio": "tall_nine_by_twenty_one", "resolution": "one_k" })).await, "stability_ultra_bedrock", "only Stability does 9:21");
    assert_eq!(pick(json!({ "model": "auto_image", "prompt": "x", "resolution": "four_k" })).await, "nano_banana_2_vertex", "Stability can't promise 4K");
    assert_eq!(pick(json!({ "model": "auto_image", "prompt": "x", "image_media_tokens": ["a", "b", "c", "d"] })).await, "nano_banana_pro_vertex");
    assert_eq!(pick(json!({ "model": "stability_core_bedrock", "prompt": "x" })).await, "stability_core_bedrock", "a named model is used as is");
  }

  #[tokio::test]
  async fn impossible_settings_are_reported() {
    let config = LocalConfig { bedrock_enabled: Some(true), ..Default::default() };
    let request = Request { model_id: "auto_image".into(), resolution: Some("four_k".into()), ..Default::default() };
    assert!(decide(&config, None, Modality::Image, &request).await.is_err(), "Stability has no 4K");
  }

  #[test]
  fn veo_tiers_fit_by_resolution() {
    let config = LocalConfig { gcp_project: Some("p".into()), ..Default::default() };
    let request = Request { resolution: Some("four_k".into()), ..Default::default() };
    let fitting: Vec<String> = catalog::video_candidates(&config).into_iter().filter(|m| fits(&m.info, &request)).map(|m| m.info.id).collect();
    assert_eq!(fitting, ["veo_3p1_vertex"]);
  }
}
