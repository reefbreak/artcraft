//! TypeSafe AI's Jev as the "Auto" model's router: given a generation request, pick which of the
//! user's models should run it. Jev answers a `choice` question with calibrated probabilities, so
//! a low-confidence answer can be ignored in favor of the fixed rules.
//!
//! API: `POST https://api.typesafe.ai/v1/systemone` with `Authorization: Bearer <key>`.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const QUESTION: &str = "route";
/// Routing must not hold up the generation for long; the rules take over after this.
const TIMEOUT: Duration = Duration::from_secs(8);
const INSTRUCTIONS: &str = "Pick the model that will best satisfy this image or video generation request. Weigh the prompt's subject, style and complexity, any text that must render, the number of reference images, and the requested resolution and settings against each model's strengths. Prefer the cheaper, faster model when quality would be similar.";

/// Jev's pick.
#[derive(Clone, Debug, PartialEq)]
pub struct JevChoice {
  pub choice: String,
  pub probability: f32,
  pub confidence: f32,
  /// Every option's probability, highest first.
  pub probabilities: Vec<(String, f32)>,
  /// Billed input tokens (`usage.input_tokens`).
  pub input_tokens: u64,
}

#[derive(Clone)]
pub struct JevClient {
  http: reqwest::Client,
  api_key: String,
  model: String,
}

impl JevClient {
  pub fn new(http: reqwest::Client, api_key: String, model: String) -> Self {
    Self { http, api_key, model }
  }

  /// Asks Jev to choose one of `options` (id → what that model is good at) for `state`.
  pub async fn choose(&self, state: &Value, options: &[(String, String)]) -> anyhow::Result<JevChoice> {
    let body = request_body(&self.model, state, options);
    let response = self.http.post(ENDPOINT).bearer_auth(&self.api_key).json(&body).timeout(TIMEOUT).send().await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
      anyhow::bail!("Jev error {status}: {}", text.chars().take(300).collect::<String>());
    }
    parse_choice(&text)
  }
}

fn request_body(model: &str, state: &Value, options: &[(String, String)]) -> Value {
  let criteria: Map<String, Value> = options.iter().map(|(id, strengths)| (id.clone(), json!(strengths))).collect();
  json!({
    "state": state,
    "model": model,
    "questions": { QUESTION: { "type": "choice", "instructions": INSTRUCTIONS, "criteria": criteria } },
  })
}

fn parse_choice(text: &str) -> anyhow::Result<JevChoice> {
  #[derive(Deserialize)]
  struct Response {
    answers: Map<String, Value>,
    #[serde(default)]
    usage: Usage,
  }
  #[derive(Default, Deserialize)]
  struct Usage {
    #[serde(default)]
    input_tokens: u64,
  }
  #[derive(Deserialize)]
  struct Answer {
    choice: String,
    #[serde(default)]
    probabilities: Map<String, Value>,
    #[serde(default)]
    confidence: Option<f32>,
  }
  let response: Response = serde_json::from_str(text)?;
  let answer: Answer = serde_json::from_value(response.answers.get(QUESTION).cloned().ok_or_else(|| anyhow::anyhow!("Jev didn't answer the routing question"))?)?;
  let probability = answer.probabilities.get(&answer.choice).and_then(Value::as_f64).unwrap_or(0.0) as f32;
  let mut probabilities: Vec<(String, f32)> = answer.probabilities.iter().filter_map(|(k, v)| v.as_f64().map(|p| (k.clone(), p as f32))).collect();
  probabilities.sort_by(|a, b| b.1.total_cmp(&a.1));
  Ok(JevChoice { confidence: answer.confidence.unwrap_or(probability), probability, choice: answer.choice, probabilities, input_tokens: response.usage.input_tokens })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn options_become_choice_criteria() {
    let options = vec![("nano_banana_pro_vertex".to_owned(), "quality".to_owned()), ("stability_core_bedrock".to_owned(), "speed".to_owned())];
    let body = request_body("jev-1.13.0", &json!({ "prompt": "a fox" }), &options);
    assert_eq!(body["model"], "jev-1.13.0");
    assert_eq!(body["questions"]["route"]["type"], "choice");
    assert_eq!(body["questions"]["route"]["criteria"]["stability_core_bedrock"], "speed");
    assert_eq!(body["state"]["prompt"], "a fox");
  }

  #[test]
  fn reads_the_choice_and_its_confidence() {
    let text = r#"{"model":"jev-1.13.0","answers":{"route":{"type":"choice","choice":"nano_banana_pro_vertex","probabilities":{"nano_banana_pro_vertex":0.88,"stability_core_bedrock":0.12},"confidence":0.81}},"usage":{"input_tokens":318,"output_tokens":34}}"#;
    let choice = parse_choice(text).unwrap();
    assert_eq!((choice.choice.as_str(), choice.probability, choice.confidence, choice.input_tokens), ("nano_banana_pro_vertex", 0.88, 0.81, 318));
    assert_eq!(choice.probabilities, vec![("nano_banana_pro_vertex".to_owned(), 0.88), ("stability_core_bedrock".to_owned(), 0.12)]);
    assert!(parse_choice(r#"{"answers":{}}"#).is_err());
  }
}
