//! Google Vertex AI, with the user's own project: Gemini image models (`generateContent`), Veo
//! (`predictLongRunning` + `fetchPredictOperation`), and Gemini Omni Flash video edits (the
//! Interactions API, `v1beta1/.../interactions`).
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
use super::pricing::{GeminiUsage, OmniUsage};

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const IMAGE_TIMEOUT: Duration = Duration::from_secs(180);
const VIDEO_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Veo jobs not done by then are given up on.
const VIDEO_GIVE_UP: Duration = Duration::from_secs(20 * 60);
/// Omni Flash's id on Vertex (the preview the Vertex samples use), then the Gemini API's GA id.
pub const OMNI_MODELS: [&str; 2] = ["gemini-omni-1.1-flash-preview", "gemini-omni-1.1-flash"];
/// Omni Flash is served from the global endpoint.
const OMNI_LOCATION: &str = "global";

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

/// One Omni Flash edit of a clip (at most 10 s).
pub struct OmniEditRequest<'a> {
  /// Tried in order while Vertex says a model isn't found.
  pub models: &'a [&'a str],
  pub prompt: &'a str,
  /// MP4 bytes.
  pub clip: &'a [u8],
  /// `360p`, `720p`, `1080p`, `4k`.
  pub resolution: &'a str,
  /// Results go to `gs://…` and are downloaded, for clips too big to return inline.
  pub output_gcs_uri: Option<&'a str>,
}

/// An Omni Flash result: the video, the model that made it, and the tokens it reported.
pub struct OmniEdit {
  pub file: GeneratedFile,
  pub model: String,
  pub usage: Option<OmniUsage>,
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

  /// Edits a clip with Gemini Omni Flash and waits for the result.
  pub async fn edit_video(&self, request: OmniEditRequest<'_>) -> anyhow::Result<OmniEdit> {
    let project = self.config.gcp_project.as_deref().ok_or_else(|| anyhow::anyhow!("Gemini Omni Flash needs a Google Cloud project in local_providers.json"))?;
    let base = format!("{}v1beta1/projects/{project}/locations/{OMNI_LOCATION}/interactions", self.host(OMNI_LOCATION));
    let mut last_error = None;
    for model in request.models {
      let mut body = omni_body(model, &request);
      let mut sent = self.post_with_token(&base, &body, IMAGE_TIMEOUT).await;
      // Vertex refuses some output settings for edits; drop the one it names and send again.
      while let Err(err) = &sent {
        let Some(field) = refused_format_field(err, &body) else {
          break;
        };
        warn!("Omni Flash won't take `{field}` for an edit; sending without it");
        if let Some(format) = body.get_mut("response_format").and_then(Value::as_object_mut) {
          format.remove(field);
        }
        sent = self.post_with_token(&base, &body, IMAGE_TIMEOUT).await;
      }
      match sent {
        Ok(started) => {
          let id = started.get("id").and_then(Value::as_str).map(str::to_owned);
          let result = async {
            let finished = self.wait_for_interaction(&base, started).await?;
            let file = self.omni_video(&finished).await?;
            Ok(OmniEdit { file, model: (*model).to_owned(), usage: omni_usage(&finished) })
          }
          .await;
          if let Some(id) = id {
            self.delete_interaction(&base, &id).await;
          }
          return result;
        },
        Err(err) if is_not_found(&err) => {
          info!("Omni model {model} not found on Vertex; trying the next id");
          last_error = Some(err);
        },
        Err(err) => return Err(err),
      }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("No Omni Flash model configured")))
  }

  /// Removes a stored interaction (the uploaded clip and the result) from Google's side.
  async fn delete_interaction(&self, base: &str, id: &str) {
    let Ok(token) = self.token().await else {
      return;
    };
    match self.http.delete(format!("{base}/{id}")).bearer_auth(token).timeout(IMAGE_TIMEOUT).send().await {
      Ok(response) if response.status().is_success() => {},
      Ok(response) => warn!("Couldn't delete Omni interaction {id}: {}", response.status()),
      Err(err) => warn!("Couldn't delete Omni interaction {id}: {err}"),
    }
  }

  /// Polls a background interaction until it has finished.
  async fn wait_for_interaction(&self, base: &str, mut interaction: Value) -> anyhow::Result<Value> {
    let deadline = tokio::time::Instant::now() + VIDEO_GIVE_UP;
    loop {
      match interaction.get("status").and_then(Value::as_str).unwrap_or("completed") {
        "completed" => return Ok(interaction),
        "in_progress" | "queued" => {},
        other => anyhow::bail!("Omni Flash {}: {}", other.replace('_', " "), interaction_error(&interaction).unwrap_or_else(|| "no reason given".to_owned())),
      }
      anyhow::ensure!(tokio::time::Instant::now() < deadline, "Omni Flash didn't finish in time");
      let id = interaction.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Omni Flash didn't return an interaction id"))?.to_owned();
      tokio::time::sleep(VIDEO_POLL_INTERVAL).await;
      let token = self.token().await?;
      match read_json(self.http.get(format!("{base}/{id}")).bearer_auth(token).timeout(IMAGE_TIMEOUT).send().await?).await {
        Ok(next) => interaction = next,
        Err(err) if tokio::time::Instant::now() < deadline => warn!("Polling Omni Flash failed (will retry): {err:#}"),
        Err(err) => return Err(err),
      }
    }
  }

  /// The video in a finished interaction: inline bytes or a `gs://` file.
  async fn omni_video(&self, interaction: &Value) -> anyhow::Result<GeneratedFile> {
    match find_video(interaction) {
      Some(VideoOut::Inline(data)) => Ok(GeneratedFile { bytes: BASE64.decode(data)?, extension: "mp4", usage: None }),
      Some(VideoOut::Uri(uri)) if uri.starts_with("gs://") => Ok(GeneratedFile { bytes: self.download_gcs(&uri).await?, extension: "mp4", usage: None }),
      Some(VideoOut::Uri(uri)) => anyhow::bail!("Omni Flash returned a video at {uri}, which ArtCraft can't download"),
      None => anyhow::bail!("Omni Flash returned no video{}", interaction_error(interaction).map(|e| format!(": {e}")).unwrap_or_default()),
    }
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

/// The Interactions API request for an Omni Flash edit.
fn omni_body(model: &str, request: &OmniEditRequest<'_>) -> Value {
  // No aspect ratio: an edit keeps the clip's ("cannot be set in response format for edit task").
  let mut format = json!({ "type": "video", "resolution": request.resolution, "delivery": "inline" });
  if let Some(uri) = request.output_gcs_uri {
    format["delivery"] = json!("uri");
    format["gcs_uri"] = json!(uri);
  }
  json!({
    "model": model,
    "input": [
      { "type": "video", "mime_type": "video/mp4", "data": BASE64.encode(request.clip) },
      { "type": "text", "text": request.prompt },
    ],
    "generation_config": { "video_config": { "task": "edit" } },
    "response_format": format,
    // Background interactions must be stored to be polled; ArtCraft deletes it once it has the video.
    "background": true,
    "store": true,
  })
}

enum VideoOut {
  Inline(String),
  Uri(String),
}

/// The first video in an interaction's model output (or the SDK-style `output_video`).
fn find_video(interaction: &Value) -> Option<VideoOut> {
  let from = |item: &Value| -> Option<VideoOut> {
    if let Some(data) = item.get("data").and_then(Value::as_str) {
      return Some(VideoOut::Inline(data.to_owned()));
    }
    item.get("uri").and_then(Value::as_str).map(|u| VideoOut::Uri(u.to_owned()))
  };
  let steps = interaction.get("steps").and_then(Value::as_array).into_iter().flatten();
  let in_steps = steps.filter(|s| s.get("type").and_then(Value::as_str).is_none_or(|t| t == "model_output")).flat_map(|s| s.get("content").and_then(Value::as_array).cloned().unwrap_or_default()).filter(|c| c.get("type").and_then(Value::as_str) == Some("video")).find_map(|c| from(&c));
  in_steps.or_else(|| interaction.get("output_video").and_then(from))
}

/// The tokens an interaction reports, for the spend meter.
fn omni_usage(interaction: &Value) -> Option<OmniUsage> {
  let usage = interaction.get("usage")?;
  let count = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
  let by_modality: Vec<&Value> = usage.get("output_tokens_by_modality").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
  let video_tokens: u64 = by_modality.iter().filter(|m| m.get("modality").and_then(Value::as_str).is_some_and(|m| m.eq_ignore_ascii_case("video"))).map(|m| count(m.get("tokens"))).sum();
  let total_out = count(usage.get("total_output_tokens"));
  let input_tokens = count(usage.get("total_input_tokens"));
  (input_tokens > 0 || total_out > 0).then(|| {
    let video_tokens = if video_tokens == 0 { total_out } else { video_tokens };
    OmniUsage { input_tokens, video_tokens, other_output_tokens: total_out.saturating_sub(video_tokens) }
  })
}

/// Why an interaction failed, when it says.
fn interaction_error(interaction: &Value) -> Option<String> {
  let message = |e: &Value| e.get("message").and_then(Value::as_str).map(str::to_owned);
  interaction.get("error").and_then(message).or_else(|| interaction.get("errors").and_then(Value::as_array).and_then(|a| a.iter().find_map(message))).or_else(|| interaction.get("steps").and_then(Value::as_array).and_then(|s| s.iter().find_map(|s| s.get("error").and_then(message))))
}

/// The `response_format` field a "… cannot be set in response format" error names, when the body
/// still has it.
fn refused_format_field(err: &anyhow::Error, body: &Value) -> Option<&'static str> {
  let text = err.to_string().to_ascii_lowercase();
  if !text.contains("cannot be set in response format") {
    return None;
  }
  let format = body.get("response_format")?;
  [("aspect ratio", "aspect_ratio"), ("aspect_ratio", "aspect_ratio"), ("resolution", "resolution"), ("duration", "duration")].into_iter().find(|(words, key)| text.contains(words) && format.get(*key).is_some()).map(|(_, key)| key)
}

/// Whether Vertex said the model (or endpoint) doesn't exist.
fn is_not_found(err: &anyhow::Error) -> bool {
  let text = err.to_string();
  text.contains("error 404") || text.contains("NOT_FOUND")
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
  fn omni_requests_send_the_clip_and_ask_for_an_edit() {
    let request = OmniEditRequest { models: &OMNI_MODELS, prompt: "remove the tail", clip: &[1, 2, 3], resolution: "720p", output_gcs_uri: None };
    let body = omni_body("gemini-omni-1.1-flash-preview", &request);
    assert_eq!(body["input"][0], json!({ "type": "video", "mime_type": "video/mp4", "data": "AQID" }));
    assert_eq!(body["input"][1]["text"], "remove the tail");
    assert_eq!(body["generation_config"]["video_config"]["task"], "edit");
    assert_eq!(body["response_format"]["delivery"], "inline");
    assert!(body["response_format"].get("aspect_ratio").is_none(), "edits keep the clip's shape");
    let refused = anyhow::anyhow!("Vertex AI error 400 Bad Request: Resolution cannot be set in response format for edit task.");
    assert_eq!(refused_format_field(&refused, &body), Some("resolution"));
    assert_eq!(refused_format_field(&anyhow::anyhow!("Vertex AI error 400: quota"), &body), None);
    assert_eq!((body["background"].clone(), body["store"].clone()), (json!(true), json!(true)), "Vertex polls only stored background interactions");
    let to_gcs = omni_body("m", &OmniEditRequest { output_gcs_uri: Some("gs://b/out/"), ..request });
    assert_eq!((to_gcs["response_format"]["delivery"].clone(), to_gcs["response_format"]["gcs_uri"].clone()), (json!("uri"), json!("gs://b/out/")));
  }

  #[test]
  fn omni_results_are_found_in_the_model_output() {
    let done = json!({ "id": "i1", "status": "completed",
      "steps": [{ "type": "user_input", "content": [{ "type": "video", "data": "AAAA" }] },
                { "type": "model_output", "content": [{ "type": "text", "text": "ok" }, { "type": "video", "mime_type": "video/mp4", "data": "AQID" }] }],
      "usage": { "total_input_tokens": 57920, "total_output_tokens": 58000, "output_tokens_by_modality": [{ "modality": "video", "tokens": 57920 }] } });
    assert!(matches!(find_video(&done), Some(VideoOut::Inline(d)) if d == "AQID"), "the user's own clip is skipped");
    assert_eq!(omni_usage(&done), Some(OmniUsage { input_tokens: 57920, video_tokens: 57920, other_output_tokens: 80 }));
    let by_uri = json!({ "steps": [{ "type": "model_output", "content": [{ "type": "video", "uri": "gs://b/o.mp4" }] }] });
    assert!(matches!(find_video(&by_uri), Some(VideoOut::Uri(u)) if u == "gs://b/o.mp4"));
    let failed = json!({ "status": "failed", "steps": [{ "type": "model_output", "error": { "code": 3, "message": "blocked by safety filters" } }] });
    assert_eq!(interaction_error(&failed).as_deref(), Some("blocked by safety filters"));
    assert!(find_video(&failed).is_none());
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
