//! Google Vertex AI, with the user's own project: Gemini image models (`generateContent`) and Veo
//! (`predictLongRunning` + `fetchPredictOperation`).
//!
//! Auth is Application Default Credentials (`gcloud auth application-default login`, a service
//! account in `GOOGLE_APPLICATION_CREDENTIALS`, or the metadata server). With no project set, an
//! express-mode API key works for Gemini images only.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use gcp_auth::TokenProvider;
use log::{info, warn};
use serde_json::{Value, json};
use tokio::sync::OnceCell;

use super::GeneratedFile;
use super::RefImage;
use super::config::LocalConfig;
use super::pricing::GeminiUsage;

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const IMAGE_TIMEOUT: Duration = Duration::from_secs(180);
const VIDEO_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Veo jobs not done by then are given up on.
const VIDEO_GIVE_UP: Duration = Duration::from_secs(20 * 60);

/// One Gemini image request (one image comes back per call).
pub struct GeminiImageRequest<'a> {
  pub model: &'a str,
  pub prompt: &'a str,
  pub images: &'a [RefImage],
  /// `16:9` etc.
  pub aspect_ratio: Option<&'a str>,
  /// `1K`, `2K`, `4K`, `512`.
  pub image_size: Option<&'a str>,
}

/// One Veo request.
pub struct VeoRequest<'a> {
  pub model: &'a str,
  pub prompt: &'a str,
  pub start_frame: Option<&'a RefImage>,
  pub end_frame: Option<&'a RefImage>,
  pub aspect_ratio: Option<&'a str>,
  pub resolution: Option<&'a str>,
  pub duration_seconds: Option<u16>,
  pub sample_count: u16,
  pub generate_audio: Option<bool>,
  /// A clip to extend (MP4, 1–30 s, 24 fps, 720p or 1080p): Veo continues it by about 7 s.
  pub extend: Option<&'a [u8]>,
}

#[derive(Clone)]
pub struct VertexClient {
  http: reqwest::Client,
  config: LocalConfig,
  auth: Arc<OnceCell<Arc<dyn TokenProvider>>>,
}

impl VertexClient {
  pub fn new(http: reqwest::Client, config: LocalConfig) -> Self {
    Self { http, config, auth: Arc::new(OnceCell::new()) }
  }

  /// Makes one image; an error explains a missing one (usually a safety block).
  pub async fn generate_image(&self, request: GeminiImageRequest<'_>) -> anyhow::Result<GeneratedFile> {
    let body = gemini_image_body(&request);
    let response = match (&self.config.gcp_project, &self.config.gcp_api_key) {
      (Some(project), _) => {
        let url = format!("{}v1/projects/{project}/locations/{}/publishers/google/models/{}:generateContent", self.host(&self.config.gcp_location), self.config.gcp_location, request.model);
        self.post_with_token(&url, &body, IMAGE_TIMEOUT).await?
      },
      (None, Some(key)) => {
        let url = format!("{}v1beta1/publishers/google/models/{}:generateContent", self.host("global"), request.model);
        read_json(self.http.post(url).header("x-goog-api-key", key).json(&body).timeout(IMAGE_TIMEOUT).send().await?).await?
      },
      (None, None) => anyhow::bail!("Set a Google Cloud project (or a Vertex API key) in local_providers.json"),
    };
    let mut file = first_image(&response)?;
    file.usage = usage(&response);
    Ok(file)
  }

  /// Starts a Veo job and waits for it, returning every video it made.
  pub async fn generate_video(&self, request: VeoRequest<'_>) -> anyhow::Result<Vec<GeneratedFile>> {
    let project = self.config.gcp_project.as_deref().ok_or_else(|| anyhow::anyhow!("Veo needs a Google Cloud project in local_providers.json"))?;
    let location = &self.config.gcp_video_location;
    let model_path = format!("{}v1/projects/{project}/locations/{location}/publishers/google/models/{}", self.host(location), request.model);
    let started = self.post_with_token(&format!("{model_path}:predictLongRunning"), &veo_body(&request), IMAGE_TIMEOUT).await?;
    let operation = started.get("name").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Veo didn't return an operation"))?.to_owned();
    info!("Veo operation started: {operation}");
    let deadline = tokio::time::Instant::now() + VIDEO_GIVE_UP;
    loop {
      tokio::time::sleep(VIDEO_POLL_INTERVAL).await;
      let status = match self.post_with_token(&format!("{model_path}:fetchPredictOperation"), &json!({ "operationName": operation }), IMAGE_TIMEOUT).await {
        Ok(status) => status,
        Err(err) if tokio::time::Instant::now() < deadline => {
          warn!("Polling Veo failed (will retry): {err:#}");
          continue;
        },
        Err(err) => return Err(err),
      };
      if !status.get("done").and_then(Value::as_bool).unwrap_or(false) {
        if tokio::time::Instant::now() >= deadline {
          anyhow::bail!("Veo didn't finish in time");
        }
        continue;
      }
      if let Some(message) = status.pointer("/error/message").and_then(Value::as_str) {
        anyhow::bail!("Veo failed: {message}");
      }
      return self.collect_videos(&status).await;
    }
  }

  async fn collect_videos(&self, status: &Value) -> anyhow::Result<Vec<GeneratedFile>> {
    let videos = status.pointer("/response/videos").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for video in videos {
      if let Some(data) = video.get("bytesBase64Encoded").and_then(Value::as_str) {
        out.push(GeneratedFile { bytes: BASE64.decode(data)?, extension: "mp4", usage: None });
      } else if let Some(uri) = video.get("gcsUri").and_then(Value::as_str) {
        out.push(GeneratedFile { bytes: self.download_gcs(uri).await?, extension: "mp4", usage: None });
      }
    }
    if out.is_empty() {
      let filtered = status.pointer("/response/raiMediaFilteredCount").and_then(Value::as_u64).unwrap_or(0);
      let reasons = status.pointer("/response/raiMediaFilteredReasons").map(Value::to_string).unwrap_or_default();
      anyhow::bail!(if filtered > 0 { format!("Veo filtered the result: {reasons}") } else { "Veo returned no videos".to_owned() });
    }
    Ok(out)
  }

  /// Makes `count` music clips with Lyria (`predict`, in the video region).
  pub async fn generate_music(&self, model: &str, prompt: &str, negative_prompt: Option<&str>, count: u16) -> anyhow::Result<Vec<GeneratedFile>> {
    let project = self.config.gcp_project.as_deref().ok_or_else(|| anyhow::anyhow!("Lyria needs a Google Cloud project in local_providers.json"))?;
    let location = &self.config.gcp_video_location;
    let url = format!("{}v1/projects/{project}/locations/{location}/publishers/google/models/{model}:predict", self.host(location));
    let response = self.post_with_token(&url, &lyria_body(prompt, negative_prompt, count), IMAGE_TIMEOUT).await?;
    let clips: Vec<GeneratedFile> = response.get("predictions").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.get("bytesBase64Encoded").and_then(Value::as_str)).filter_map(|data| BASE64.decode(data).ok()).map(|bytes| GeneratedFile { bytes, extension: "wav", usage: None }).collect();
    anyhow::ensure!(!clips.is_empty(), "Lyria returned no audio (the prompt may have been blocked)");
    Ok(clips)
  }

  /// Reads `gs://bucket/object` with the same credentials.
  async fn download_gcs(&self, uri: &str) -> anyhow::Result<Vec<u8>> {
    let (bucket, object) = uri.strip_prefix("gs://").and_then(|rest| rest.split_once('/')).ok_or_else(|| anyhow::anyhow!("Unexpected GCS URI {uri}"))?;
    let object: String = url::form_urlencoded::byte_serialize(object.as_bytes()).collect();
    let url = format!("https://storage.googleapis.com/storage/v1/b/{bucket}/o/{object}?alt=media");
    let token = self.token().await?;
    let response = self.http.get(url).bearer_auth(token).timeout(IMAGE_TIMEOUT).send().await?.error_for_status()?;
    Ok(response.bytes().await?.to_vec())
  }

  /// The configured override, else the location's real host.
  fn host(&self, location: &str) -> String {
    self.config.vertex_base_url.clone().unwrap_or_else(|| host(location))
  }

  async fn post_with_token(&self, url: &str, body: &Value, timeout: Duration) -> anyhow::Result<Value> {
    let token = self.token().await?;
    read_json(self.http.post(url).bearer_auth(token).json(body).timeout(timeout).send().await?).await
  }

  async fn token(&self) -> anyhow::Result<String> {
    let provider = self.auth.get_or_try_init(|| async { gcp_auth::provider().await.map_err(|err| anyhow::anyhow!("Google Cloud credentials not found ({err}). Run `gcloud auth application-default login`.")) }).await?;
    Ok(provider.token(&[SCOPE]).await.map_err(|err| anyhow::anyhow!("Couldn't get a Google Cloud token: {err}"))?.as_str().to_owned())
  }
}

/// The API host for a location: `global`, a multi-region (`us`, `eu`), or a region.
fn host(location: &str) -> String {
  match location {
    "global" => "https://aiplatform.googleapis.com/".to_owned(),
    "us" | "eu" => format!("https://aiplatform.{location}.rep.googleapis.com/"),
    region => format!("https://{region}-aiplatform.googleapis.com/"),
  }
}

fn gemini_image_body(request: &GeminiImageRequest<'_>) -> Value {
  let mut parts: Vec<Value> = request.images.iter().map(|image| json!({ "inlineData": { "mimeType": image.mime_type, "data": BASE64.encode(&image.bytes) } })).collect();
  parts.push(json!({ "text": request.prompt }));
  let mut image_config = serde_json::Map::new();
  if let Some(ratio) = request.aspect_ratio {
    image_config.insert("aspectRatio".into(), json!(ratio));
  }
  // 2.5 Flash Image has no size setting.
  if let Some(size) = request.image_size.filter(|_| !request.model.starts_with("gemini-2.5")) {
    image_config.insert("imageSize".into(), json!(size));
  }
  json!({
    "contents": [{ "role": "user", "parts": parts }],
    "generationConfig": { "responseModalities": ["TEXT", "IMAGE"], "imageConfig": image_config },
  })
}

/// Lyria 2's request: the count goes inside the instance (Google's samples), not in parameters.
fn lyria_body(prompt: &str, negative_prompt: Option<&str>, count: u16) -> Value {
  let mut instance = serde_json::Map::new();
  instance.insert("prompt".into(), json!(prompt));
  if let Some(negative) = negative_prompt.filter(|n| !n.trim().is_empty()) {
    instance.insert("negative_prompt".into(), json!(negative));
  }
  if count > 1 {
    instance.insert("sample_count".into(), json!(count));
  }
  json!({ "instances": [instance], "parameters": {} })
}

fn veo_body(request: &VeoRequest<'_>) -> Value {
  let frame = |image: &RefImage| json!({ "bytesBase64Encoded": BASE64.encode(&image.bytes), "mimeType": image.mime_type });
  let mut instance = serde_json::Map::new();
  instance.insert("prompt".into(), json!(request.prompt));
  if let Some(image) = request.start_frame {
    instance.insert("image".into(), frame(image));
  }
  if let Some(image) = request.end_frame {
    instance.insert("lastFrame".into(), frame(image));
  }
  if let Some(video) = request.extend {
    instance.insert("video".into(), json!({ "bytesBase64Encoded": BASE64.encode(video), "mimeType": "video/mp4" }));
  }
  let mut parameters = serde_json::Map::new();
  parameters.insert("sampleCount".into(), json!(request.sample_count.max(1)));
  if let Some(ratio) = request.aspect_ratio {
    parameters.insert("aspectRatio".into(), json!(ratio));
  }
  if let Some(resolution) = request.resolution {
    parameters.insert("resolution".into(), json!(resolution));
  }
  if let Some(seconds) = request.duration_seconds {
    parameters.insert("durationSeconds".into(), json!(seconds));
  }
  if let Some(audio) = request.generate_audio {
    parameters.insert("generateAudio".into(), json!(audio));
  }
  json!({ "instances": [instance], "parameters": parameters })
}

/// The first non-thought image in a `generateContent` reply.
fn first_image(response: &Value) -> anyhow::Result<GeneratedFile> {
  let candidates = response.get("candidates").and_then(Value::as_array).cloned().unwrap_or_default();
  for candidate in &candidates {
    for part in candidate.pointer("/content/parts").and_then(Value::as_array).into_iter().flatten() {
      if part.get("thought").and_then(Value::as_bool).unwrap_or(false) {
        continue;
      }
      if let Some(data) = part.pointer("/inlineData/data").and_then(Value::as_str) {
        let mime = part.pointer("/inlineData/mimeType").and_then(Value::as_str).unwrap_or("image/png");
        return Ok(GeneratedFile { bytes: BASE64.decode(data)?, extension: extension_for(mime), usage: None });
      }
    }
  }
  let reason = candidates.first().and_then(|c| c.get("finishReason")).and_then(Value::as_str).or_else(|| response.pointer("/promptFeedback/blockReason").and_then(Value::as_str));
  let text: String = candidates.iter().flat_map(|c| c.pointer("/content/parts").and_then(Value::as_array).cloned().unwrap_or_default()).filter_map(|p| p.get("text").and_then(Value::as_str).map(str::to_owned)).collect::<Vec<_>>().join(" ");
  match (reason, text.trim()) {
    (Some(reason), "") => anyhow::bail!("The model returned no image ({reason})"),
    (_, text) if !text.is_empty() => anyhow::bail!("The model returned no image: {text}"),
    _ => anyhow::bail!("The model returned no image"),
  }
}

/// The token counts in a reply's `usageMetadata`, for the spend meter.
fn usage(response: &Value) -> Option<GeminiUsage> {
  let meta = response.get("usageMetadata")?;
  let count = |key: &str| meta.get(key).and_then(Value::as_u64).unwrap_or(0);
  let image_tokens = meta.get("candidatesTokensDetails").and_then(Value::as_array).and_then(|details| details.iter().find(|d| d.get("modality").and_then(Value::as_str) == Some("IMAGE"))).and_then(|d| d.get("tokenCount")).and_then(Value::as_u64);
  Some(GeminiUsage { prompt_tokens: count("promptTokenCount"), candidates_tokens: count("candidatesTokenCount"), thoughts_tokens: count("thoughtsTokenCount"), image_tokens })
}

fn extension_for(mime: &str) -> &'static str {
  match mime {
    "image/jpeg" | "image/jpg" => "jpg",
    "image/webp" => "webp",
    _ => "png",
  }
}

/// The JSON body of a reply, or Google's error message.
async fn read_json(response: reqwest::Response) -> anyhow::Result<Value> {
  let status = response.status();
  let text = response.text().await?;
  if !status.is_success() {
    let message = serde_json::from_str::<Value>(&text).ok().and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_owned)).unwrap_or_else(|| text.chars().take(300).collect());
    anyhow::bail!("Vertex AI error {status}: {message}");
  }
  Ok(serde_json::from_str(&text)?)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn hosts_follow_the_location() {
    assert_eq!(host("global"), "https://aiplatform.googleapis.com/");
    assert_eq!(host("us"), "https://aiplatform.us.rep.googleapis.com/");
    assert_eq!(host("us-central1"), "https://us-central1-aiplatform.googleapis.com/");
  }

  #[test]
  fn image_requests_put_references_before_the_prompt() {
    let refs = [RefImage { mime_type: "image/png".into(), bytes: vec![1, 2, 3] }];
    let body = gemini_image_body(&GeminiImageRequest { model: "gemini-3-pro-image", prompt: "a fox", images: &refs, aspect_ratio: Some("16:9"), image_size: Some("2K") });
    assert_eq!(body["contents"][0]["parts"][0]["inlineData"]["data"], "AQID");
    assert_eq!(body["contents"][0]["parts"][1]["text"], "a fox");
    assert_eq!(body["generationConfig"]["imageConfig"], json!({ "aspectRatio": "16:9", "imageSize": "2K" }));
    let old = gemini_image_body(&GeminiImageRequest { model: "gemini-2.5-flash-image", prompt: "x", images: &[], aspect_ratio: None, image_size: Some("2K") });
    assert_eq!(old["generationConfig"]["imageConfig"], json!({}), "2.5 has no size setting");
  }

  #[test]
  fn thought_images_are_skipped() {
    let response = json!({ "candidates": [{ "finishReason": "STOP", "content": { "parts": [
      { "thought": true, "inlineData": { "mimeType": "image/png", "data": "AAAA" } },
      { "text": "here you go" },
      { "inlineData": { "mimeType": "image/jpeg", "data": "AQID" } }
    ] } }] });
    let image = first_image(&response).unwrap();
    assert_eq!((image.bytes, image.extension), (vec![1, 2, 3], "jpg"));
    let blocked = json!({ "candidates": [{ "finishReason": "IMAGE_SAFETY", "content": { "parts": [] } }] });
    assert!(first_image(&blocked).unwrap_err().to_string().contains("IMAGE_SAFETY"));
  }

  #[test]
  fn lyria_requests_put_the_count_in_the_instance() {
    assert_eq!(lyria_body("calm piano", Some("drums"), 2), json!({ "instances": [{ "prompt": "calm piano", "negative_prompt": "drums", "sample_count": 2 }], "parameters": {} }));
    assert_eq!(lyria_body("x", Some(" "), 1), json!({ "instances": [{ "prompt": "x" }], "parameters": {} }));
  }

  #[test]
  fn usage_breaks_out_image_tokens() {
    let response = json!({ "usageMetadata": { "promptTokenCount": 12, "candidatesTokenCount": 1130, "thoughtsTokenCount": 200, "candidatesTokensDetails": [{ "modality": "TEXT", "tokenCount": 10 }, { "modality": "IMAGE", "tokenCount": 1120 }] } });
    assert_eq!(usage(&response), Some(GeminiUsage { prompt_tokens: 12, candidates_tokens: 1130, thoughts_tokens: 200, image_tokens: Some(1120) }));
    assert_eq!(usage(&json!({})), None);
  }

  #[test]
  fn veo_requests_carry_frames_and_settings() {
    let frame = RefImage { mime_type: "image/png".into(), bytes: vec![1, 2, 3] };
    let body = veo_body(&VeoRequest { model: "veo-3.1-fast-generate-001", prompt: "waves", start_frame: Some(&frame), end_frame: None, aspect_ratio: Some("9:16"), resolution: Some("1080p"), duration_seconds: Some(6), sample_count: 2, generate_audio: Some(false), extend: None });
    assert_eq!(body["instances"][0]["image"], json!({ "bytesBase64Encoded": "AQID", "mimeType": "image/png" }));
    assert!(body["instances"][0].get("lastFrame").is_none());
    assert_eq!(body["parameters"], json!({ "sampleCount": 2, "aspectRatio": "9:16", "resolution": "1080p", "durationSeconds": 6, "generateAudio": false }));
  }
}
