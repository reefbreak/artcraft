//! The cost shown beside the generate button, before anything is sent: what the request would
//! cost on the user's accounts at list prices.
//!
//! For a named model it's one figure. For Auto it's the range across the models Jev may choose
//! between (Jev isn't asked until Generate, so the estimate itself costs nothing), with the
//! rules' pick as the likely figure.

use super::catalog::{self, LocalModel, Route};
use super::config::LocalConfig;
use super::pricing::PriceBook;
use super::router::{self, Request};
use crate::backend::Modality;

/// Veo's only clip length at 1080p and 4K.
pub const VEO_HD_SECONDS: u16 = 8;
/// A Jev routing call is a few hundred input tokens: the prompt, settings and each option.
const JEV_TOKENS_ESTIMATE: u64 = 600;
/// Roughly four characters per token.
const CHARS_PER_TOKEN: usize = 4;

/// What a request would cost.
#[derive(Clone, Debug, PartialEq)]
pub struct CostEstimate {
  /// The cheapest and dearest models the request could run on.
  pub low: f64,
  pub high: f64,
  /// The rules' pick, and its cost.
  pub likely_name: String,
  pub likely: f64,
  /// Every candidate with its cost, cheapest first (one entry for a named model).
  pub options: Vec<(String, f64)>,
  /// Jev's routing charge, when Auto will ask it.
  pub routing: f64,
}

impl CostEstimate {
  /// One figure, or a range for Auto (`$0.07–0.27`).
  pub fn label(&self) -> String {
    let low = self.low + self.routing;
    let high = self.high + self.routing;
    if (high - low).abs() < 0.005 {
      format!("${high:.2}")
    } else {
      format!("${low:.2}\u{2013}{high:.2}")
    }
  }

  /// The hover text: each option, and what's billed where.
  pub fn details(&self) -> String {
    let mut lines: Vec<String> = Vec::new();
    if self.options.len() > 1 {
      lines.push(format!("Auto picks one of these (rules would pick {}):", self.likely_name));
    }
    for (name, cost) in &self.options {
      lines.push(format!("{name}  ${cost:.3}"));
    }
    if self.routing > 0.0 {
      lines.push("Plus Jev routing, under $0.01".to_owned());
    }
    lines.push("Estimated at list prices; billed to your own accounts.".to_owned());
    lines.join("\n")
  }
}

/// The estimate for a request, or `None` when nothing configured can run it.
pub fn estimate(config: &LocalConfig, prices: &PriceBook, modality: Modality, request: &Request) -> Option<CostEstimate> {
  let (candidates, pick) = router::plan(config, modality, request)?;
  let mut options: Vec<(String, f64)> = candidates.iter().map(|m| (m.info.name.clone(), model_cost(prices, m, request))).collect();
  options.sort_by(|a, b| a.1.total_cmp(&b.1));
  let low = options.first().map_or(0.0, |o| o.1);
  let high = options.last().map_or(0.0, |o| o.1);
  let routed = candidates.len() > 1 && config.jev_enabled();
  Some(CostEstimate { low, high, likely: model_cost(prices, &pick, request), likely_name: pick.info.name.clone(), options, routing: if routed { prices.jev_call(JEV_TOKENS_ESTIMATE) } else { 0.0 } })
}

/// One model's cost for the request: outputs at list price, plus input tokens for Gemini.
pub fn model_cost(prices: &PriceBook, model: &LocalModel, request: &Request) -> f64 {
  let batch = f64::from(request.batch_count.max(1));
  let resolution = request.resolution.as_deref().filter(|r| model.info.resolutions.iter().any(|m| m == r));
  match model.route {
    Route::GeminiImage { model: upstream } => {
      let output = prices.gemini_call(upstream, None, resolution.and_then(catalog::image_size)).unwrap_or(0.0);
      let input = prices.gemini_rates(upstream).map_or(0.0, |rates| {
        let per_reference = if upstream == "gemini-3-pro-image" { 560 } else { 1120 };
        let tokens = request.prompt.len() / CHARS_PER_TOKEN + request.image_tokens.len() * per_reference;
        tokens as f64 * rates.input / 1_000_000.0
      });
      (output + input) * batch
    },
    Route::Stability { model: upstream } => prices.stability_per_image(upstream).unwrap_or(0.0) * batch,
    Route::Lyria { model: upstream } => prices.lyria_per_clip(upstream).unwrap_or(0.0) * batch,
    Route::Veo { model: upstream } => {
      let resolution = resolution.and_then(catalog::video_resolution);
      let seconds = f64::from(veo_duration(model, request, resolution).unwrap_or(VEO_HD_SECONDS));
      let audio = request.generate_audio.unwrap_or(true);
      prices.veo_per_second(upstream, audio, resolution.unwrap_or("720p")).unwrap_or(0.0) * seconds * batch
    },
  }
}

/// The clip length Veo will make: 8 seconds at 1080p and 4K, else the request's (when the model
/// offers it) or the model's default.
pub fn veo_duration(model: &LocalModel, request: &Request, resolution: Option<&str>) -> Option<u16> {
  match resolution {
    Some("1080p" | "4k") => Some(VEO_HD_SECONDS),
    _ => request.duration_seconds.filter(|d| model.info.duration_options.contains(d)).or(model.info.duration_default),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_named_model_has_one_figure() {
    let config = vertex_and_bedrock();
    let request = Request { model_id: "stability_ultra_bedrock".into(), batch_count: 2, ..Default::default() };
    let e = estimate(&config, &PriceBook::default(), Modality::Image, &request).unwrap();
    assert_eq!(e.label(), "$0.28");
    assert_eq!(e.options.len(), 1);
    assert_eq!(e.routing, 0.0);
  }

  #[test]
  fn auto_shows_the_range_and_the_likely_pick() {
    let config = LocalConfig { jev_api_key: Some("k".into()), ..vertex_and_bedrock() };
    let request = Request { model_id: "auto_image".into(), resolution: Some("one_k".into()), batch_count: 1, ..Default::default() };
    let e = estimate(&config, &PriceBook::default(), Modality::Image, &request).unwrap();
    assert_eq!(e.likely_name, "Nano Banana 2");
    assert!((e.low - 0.034).abs() < 0.001, "Lite at 1K is the cheapest: {}", e.low);
    assert!((e.high - 0.14).abs() < 0.001, "Ultra is the dearest: {}", e.high);
    assert_eq!(e.label(), "$0.03\u{2013}0.14");
    assert!(e.routing > 0.0 && e.routing < 0.01);
  }

  #[test]
  fn veo_charges_by_the_second_and_forces_eight_at_hd() {
    let config = vertex_and_bedrock();
    let request = Request { model_id: "veo_3p1_fast_vertex".into(), resolution: Some("ten_eighty_p".into()), duration_seconds: Some(4), generate_audio: Some(true), batch_count: 1, ..Default::default() };
    let e = estimate(&config, &PriceBook::default(), Modality::Video, &request).unwrap();
    assert!((e.likely - 8.0 * 0.12).abs() < 1e-9, "{}", e.likely);
    let silent_720 = Request { resolution: Some("seven_twenty_p".into()), duration_seconds: Some(4), generate_audio: Some(false), ..request };
    let e = estimate(&config, &PriceBook::default(), Modality::Video, &silent_720).unwrap();
    assert!((e.likely - 4.0 * 0.08).abs() < 1e-9, "{}", e.likely);
  }

  #[test]
  fn nothing_to_estimate_without_a_fitting_model() {
    let config = LocalConfig { bedrock_enabled: Some(true), ..Default::default() };
    let request = Request { model_id: "auto_image".into(), resolution: Some("four_k".into()), ..Default::default() };
    assert!(estimate(&config, &PriceBook::default(), Modality::Image, &request).is_none());
  }

  fn vertex_and_bedrock() -> LocalConfig {
    LocalConfig { gcp_project: Some("p".into()), bedrock_enabled: Some(true), ..Default::default() }
  }
}
