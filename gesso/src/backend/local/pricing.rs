//! List prices for local mode's spend meter, in USD (October 2026).
//!
//! Neither cloud reports spend in real time: Google Cloud Billing and its BigQuery export trail by
//! hours, AWS Cost Explorer by up to a day. So the app meters each call itself: the token counts
//! a call reports (Gemini, Jev) or the units it produced (seconds of Veo, Stability images), times
//! these prices. Any price can be overridden in `local_providers.json` under `prices` (keys on
//! [`PriceBook`]) when a rate changes or a contract price applies.
//!
//! Sources: cloud.google.com/gemini-enterprise-agent-platform/generative-ai/pricing (Gemini image
//! and Veo, global endpoint), AWS Marketplace listings for the Stability models on Bedrock, and
//! docs.typesafe.ai/models (Jev).

use std::collections::HashMap;

/// Gemini image-model token rates, per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeminiRates {
  pub input: f64,
  /// Text output, including thinking tokens.
  pub text_out: f64,
  pub image_out: f64,
}

/// Token counts from an Omni Flash interaction's `usage`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OmniUsage {
  pub input_tokens: u64,
  /// Output tokens of the VIDEO modality.
  pub video_tokens: u64,
  /// Any other output tokens (text).
  pub other_output_tokens: u64,
}

/// Token counts from a `generateContent` reply's `usageMetadata`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GeminiUsage {
  pub prompt_tokens: u64,
  /// All output tokens (`candidatesTokenCount`).
  pub candidates_tokens: u64,
  pub thoughts_tokens: u64,
  /// Output tokens of the IMAGE modality, when the reply breaks them out.
  pub image_tokens: Option<u64>,
}

/// Prices, with the user's overrides on top. Override keys in the settings file's `prices` map:
/// `gemini-3-pro-image.input` / `.text_out` / `.image_out` (per million tokens),
/// `veo-3.1-fast-generate-001.audio.1080p` (per second; `silent` for no audio),
/// `stability.stable-image-ultra-v1:1` (per image), `jev.input` (per million tokens),
/// `gemini-omni-flash.input` / `.text_out` / `.video_out` (per million tokens).
#[derive(Clone, Debug, Default)]
pub struct PriceBook {
  overrides: HashMap<String, f64>,
}

impl PriceBook {
  pub fn new(overrides: HashMap<String, f64>) -> Self {
    Self { overrides }
  }

  pub fn gemini_rates(&self, model: &str) -> Option<GeminiRates> {
    let base = match model {
      "gemini-3.1-flash-image" => GeminiRates { input: 0.50, text_out: 3.00, image_out: 60.00 },
      "gemini-3-pro-image" => GeminiRates { input: 2.00, text_out: 12.00, image_out: 120.00 },
      "gemini-3.1-flash-lite-image" => GeminiRates { input: 0.25, text_out: 1.50, image_out: 30.00 },
      "gemini-2.5-flash-image" => GeminiRates { input: 0.30, text_out: 2.50, image_out: 30.00 },
      _ => return None,
    };
    Some(GeminiRates { input: self.get(&format!("{model}.input"), base.input), text_out: self.get(&format!("{model}.text_out"), base.text_out), image_out: self.get(&format!("{model}.image_out"), base.image_out) })
  }

  /// One Gemini call. With `usage`, the reported tokens are priced; without it (or when the
  /// reply doesn't break out image tokens), the documented tokens per image at `image_size` are.
  pub fn gemini_call(&self, model: &str, usage: Option<GeminiUsage>, image_size: Option<&str>) -> Option<f64> {
    let rates = self.gemini_rates(model)?;
    let per_image = gemini_tokens_per_image(model, image_size) as f64;
    let Some(u) = usage else {
      return Some(per_image * rates.image_out / MILLION);
    };
    let image = u.image_tokens.map_or(per_image.min(u.candidates_tokens as f64), |t| t as f64);
    let text = (u.candidates_tokens as f64 - image).max(0.0) + u.thoughts_tokens as f64;
    Some((u.prompt_tokens as f64 * rates.input + image * rates.image_out + text * rates.text_out) / MILLION)
  }

  /// Veo, per second of output video.
  pub fn veo_per_second(&self, model: &str, audio: bool, resolution: &str) -> Option<f64> {
    let base = match (model, audio, resolution) {
      ("veo-3.1-generate-001", true, "4k") => 0.60,
      ("veo-3.1-generate-001", true, _) => 0.40,
      ("veo-3.1-generate-001", false, "4k") => 0.40,
      ("veo-3.1-generate-001", false, _) => 0.20,
      ("veo-3.1-fast-generate-001", true, "720p") => 0.10,
      ("veo-3.1-fast-generate-001", true, "1080p") => 0.12,
      ("veo-3.1-fast-generate-001", true, _) => 0.30,
      ("veo-3.1-fast-generate-001", false, "720p") => 0.08,
      ("veo-3.1-fast-generate-001", false, "1080p") => 0.10,
      ("veo-3.1-fast-generate-001", false, _) => 0.25,
      ("veo-3.1-lite-generate-001", true, "720p") => 0.05,
      ("veo-3.1-lite-generate-001", true, _) => 0.08,
      ("veo-3.1-lite-generate-001", false, "720p") => 0.03,
      ("veo-3.1-lite-generate-001", false, _) => 0.05,
      _ => return None,
    };
    Some(self.get(&format!("{model}.{}.{resolution}", if audio { "audio" } else { "silent" }), base))
  }

  /// A Stability image on Bedrock.
  pub fn stability_per_image(&self, model: &str) -> Option<f64> {
    let base = match model {
      "stability.stable-image-ultra-v1:1" => 0.14,
      "stability.stable-image-core-v1:1" => 0.04,
      // Not listed on AWS's pricing page; SD3 Large's $0.08 is used until it's confirmed.
      "stability.sd3-5-large-v1:0" => 0.08,
      _ => return None,
    };
    Some(self.get(model, base))
  }

  /// One of Stability's image services on Bedrock, per call (key: the model id).
  pub fn stability_service(&self, model: &str) -> Option<f64> {
    let base = match model {
      "stability.stable-image-inpaint-v1:0" | "stability.stable-image-erase-object-v1:0" | "stability.stable-image-remove-background-v1:0" | "stability.stable-image-search-replace-v1:0" | "stability.stable-image-search-recolor-v1:0" => 0.07,
      "stability.stable-outpaint-v1:0" => 0.06,
      _ => return None,
    };
    Some(self.get(model, base))
  }

  /// A Lyria clip (Lyria 2 makes 30 seconds per clip).
  pub fn lyria_per_clip(&self, model: &str) -> Option<f64> {
    let base = match model {
      "lyria-002" => 0.06,
      "lyria-3-clip-preview" => 0.04,
      "lyria-3-pro-preview" => 0.08,
      _ => return None,
    };
    Some(self.get(model, base))
  }

  /// Gemini Omni Flash (both 1.1 and the preview) token rates, per million tokens: video counts
  /// as `image_out`.
  pub fn omni_rates(&self) -> GeminiRates {
    GeminiRates { input: self.get("gemini-omni-flash.input", 1.50), text_out: self.get("gemini-omni-flash.text_out", 9.00), image_out: self.get("gemini-omni-flash.video_out", 17.50) }
  }

  /// An Omni Flash edit of `seconds` of video at `resolution`, before it runs: the input clip
  /// at 5,792 tokens a second and the output at the resolution's documented rate.
  pub fn omni_edit_estimate(&self, seconds: f64, resolution: &str) -> f64 {
    let rates = self.omni_rates();
    seconds.max(0.0) * (OMNI_INPUT_TOKENS_PER_SECOND as f64 * rates.input + omni_output_tokens_per_second(resolution) as f64 * rates.image_out) / MILLION
  }

  /// An Omni Flash call from the tokens it reported.
  pub fn omni_call(&self, usage: OmniUsage) -> f64 {
    let rates = self.omni_rates();
    (usage.input_tokens as f64 * rates.input + usage.video_tokens as f64 * rates.image_out + usage.other_output_tokens as f64 * rates.text_out) / MILLION
  }

  /// A Jev routing call: input tokens only (output is free).
  pub fn jev_call(&self, input_tokens: u64) -> f64 {
    input_tokens as f64 * self.get("jev.input", 0.042) / MILLION
  }

  fn get(&self, key: &str, default: f64) -> f64 {
    self.overrides.get(key).copied().unwrap_or(default)
  }
}

const MILLION: f64 = 1_000_000.0;
/// Omni Flash bills a second of input video as this many tokens.
const OMNI_INPUT_TOKENS_PER_SECOND: u64 = 5792;

/// Omni Flash's documented output tokens per second of video (with audio).
fn omni_output_tokens_per_second(resolution: &str) -> u64 {
  match resolution {
    "360p" => 1931,
    "1080p" => 8688,
    "4k" => 17376,
    _ => 5792,
  }
}

/// Documented output tokens per image by size (`1K`, `2K`, `4K`, `512`).
fn gemini_tokens_per_image(model: &str, image_size: Option<&str>) -> u64 {
  match (model, image_size.unwrap_or("1K")) {
    ("gemini-3.1-flash-image", "512") => 747,
    ("gemini-3.1-flash-image", "2K") => 1680,
    ("gemini-3.1-flash-image", "4K") => 2520,
    ("gemini-3-pro-image", "4K") => 2000,
    ("gemini-2.5-flash-image", _) => 1290,
    _ => 1120,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn per_image_estimates_match_the_price_sheet() {
    let book = PriceBook::default();
    let close = |a: f64, b: f64| (a - b).abs() < 0.001;
    assert!(close(book.gemini_call("gemini-3.1-flash-image", None, Some("1K")).unwrap(), 0.067));
    assert!(close(book.gemini_call("gemini-3.1-flash-image", None, Some("4K")).unwrap(), 0.151));
    assert!(close(book.gemini_call("gemini-3-pro-image", None, Some("2K")).unwrap(), 0.134));
    assert!(close(book.gemini_call("gemini-3-pro-image", None, Some("4K")).unwrap(), 0.24));
  }

  #[test]
  fn reported_tokens_are_priced_by_kind() {
    let book = PriceBook::default();
    // 1,000 prompt tokens, 1,120 image + 80 text out, 500 thinking.
    let usage = GeminiUsage { prompt_tokens: 1000, candidates_tokens: 1200, thoughts_tokens: 500, image_tokens: Some(1120) };
    let cost = book.gemini_call("gemini-3-pro-image", Some(usage), Some("1K")).unwrap();
    let expected = (1000.0 * 2.0 + 1120.0 * 120.0 + 580.0 * 12.0) / MILLION;
    assert!((cost - expected).abs() < 1e-9);
  }

  #[test]
  fn video_and_bedrock_prices() {
    let book = PriceBook::default();
    assert_eq!(book.veo_per_second("veo-3.1-fast-generate-001", true, "1080p"), Some(0.12));
    assert_eq!(book.veo_per_second("veo-3.1-generate-001", false, "720p"), Some(0.20));
    assert_eq!(book.stability_per_image("stability.stable-image-core-v1:1"), Some(0.04));
    assert!((book.jev_call(1_000_000) - 0.042).abs() < 1e-12);
  }

  #[test]
  fn omni_edits_are_priced_by_seconds_and_resolution() {
    let book = PriceBook::default();
    // 10 s at 720p: 57,920 input tokens at $1.50/M plus 57,920 video tokens at $17.50/M.
    let expected = 57_920.0 * (1.50 + 17.50) / MILLION;
    assert!((book.omni_edit_estimate(10.0, "720p") - expected).abs() < 1e-9);
    assert!(book.omni_edit_estimate(10.0, "1080p") > book.omni_edit_estimate(10.0, "720p"));
    let usage = OmniUsage { input_tokens: 57_920, video_tokens: 57_920, other_output_tokens: 100 };
    assert!((book.omni_call(usage) - (expected + 100.0 * 9.0 / MILLION)).abs() < 1e-9);
  }

  #[test]
  fn overrides_win() {
    let book = PriceBook::new(HashMap::from([("stability.sd3-5-large-v1:0".to_owned(), 0.065), ("jev.input".to_owned(), 0.1)]));
    assert_eq!(book.stability_per_image("stability.sd3-5-large-v1:0"), Some(0.065));
    assert!((book.jev_call(1_000_000) - 0.1).abs() < 1e-12);
  }
}
