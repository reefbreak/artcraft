//! Local mode: the app generates with the user's own Google Cloud and AWS accounts, keeps its
//! library on disk, and never talks to ArtCraft's servers (no sign-in, no credits).
//!
//! The rest of the backend calls into [`LocalEngine`] wherever it would have called the ArtCraft
//! API, and the engine answers in the same shapes (jobs, media files, prompts), so the UI is
//! unchanged.

pub mod bedrock;
pub mod catalog;
pub mod config;
pub mod jev;
pub mod library;
pub mod router;
pub mod vertex;

use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use futures::future::join_all;
use log::{info, warn};
use serde_json::{Map, Value};

use crate::backend::wire::{ContextImage, Job, JobRequest, JobResult, JobStatus, MediaFile, Prompt, SessionUser};
use crate::backend::{Event, Modality, UploadedMedia};
use crate::models::ModelInfo;
use bedrock::{BedrockClient, StabilityRequest};
use catalog::Route;
use config::LocalConfig;
use jev::JevClient;
use library::{LocalLibrary, MediaClass, NewFile, Origin};
use router::{Decision, Request};
use vertex::{GeminiImageRequest, VeoRequest, VertexClient};

pub const LOCAL_USERNAME: &str = "local";
const JOB_PREFIX: &str = "job_local_";
/// Finished jobs stay listed this long, so the feed sees them complete.
const FINISHED_JOB_TTL_SECS: i64 = 10 * 60;
/// Veo's only clip length at 1080p and 4K.
const VEO_HD_SECONDS: u16 = 8;
/// Image types the providers accept as-is; others are converted to PNG on upload.
const PROVIDER_IMAGE_TYPES: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
/// Thumbnails in the feed are this wide (the media cache resizes; local files have one size).
const THUMBNAIL_WIDTH: u32 = 256;

/// An image handed to a provider.
#[derive(Clone, Debug)]
pub struct RefImage {
  pub mime_type: String,
  pub bytes: Vec<u8>,
}

/// One result from a provider.
#[derive(Debug)]
pub struct GeneratedFile {
  pub bytes: Vec<u8>,
  pub extension: &'static str,
}

pub struct LocalEngine {
  config: LocalConfig,
  library: LocalLibrary,
  vertex: VertexClient,
  bedrock: BedrockClient,
  jev: Option<JevClient>,
  jobs: Arc<Mutex<Vec<Job>>>,
}

impl LocalEngine {
  pub fn new(data_root: &Path, config: LocalConfig, http: reqwest::Client) -> Self {
    LocalConfig::write_template_if_missing(data_root);
    let jev = config.jev_api_key.clone().map(|key| JevClient::new(http.clone(), key, config.jev_model.clone()));
    info!("Local mode: Vertex {}, Veo {}, Bedrock {}, Jev {}", config.vertex_enabled(), config.veo_enabled(), config.bedrock_enabled(), jev.is_some());
    Self { library: LocalLibrary::load(data_root), vertex: VertexClient::new(http, config.clone()), bedrock: BedrockClient::new(config.clone()), jev, jobs: Arc::new(Mutex::new(Vec::new())), config }
  }

  /// Who the app shows as signed in.
  pub fn user() -> SessionUser {
    SessionUser { username: LOCAL_USERNAME.to_owned(), display_name: "Local".to_owned(), email_gravatar_hash: String::new() }
  }

  pub fn models(&self, modality: Modality) -> Result<Vec<ModelInfo>, String> {
    let models = match modality {
      Modality::Image => catalog::image_models(&self.config),
      Modality::Video => catalog::video_models(&self.config),
      Modality::Audio => Vec::new(),
    };
    if models.is_empty() && modality != Modality::Audio {
      warn!("No {modality:?} models: add your accounts to ~/Artcraft/settings/local_providers.json");
    }
    Ok(models)
  }

  pub fn library(&self) -> &LocalLibrary {
    &self.library
  }

  /// One line per provider for the settings dialog: name, what's configured, and whether it's on.
  pub fn status(&self) -> Vec<(&'static str, String, bool)> {
    let c = &self.config;
    let vertex = match (&c.gcp_project, &c.gcp_api_key) {
      (Some(project), _) => format!("Project {project}, images in {}, Veo in {}", c.gcp_location, c.gcp_video_location),
      (None, Some(_)) => "Express-mode API key (images only, no Veo)".to_owned(),
      (None, None) => "Not set up: add gcp_project".to_owned(),
    };
    let bedrock = if c.bedrock_enabled() { format!("{}{}", c.aws_region, c.aws_profile.as_ref().map(|p| format!(", profile {p}")).unwrap_or_default()) } else { "Not set up: no AWS credentials found".to_owned() };
    let jev = if c.jev_enabled() { format!("{} routes Auto (min confidence {:.0}%)", c.jev_model, c.jev_min_confidence * 100.0) } else { "Off: Auto uses fixed rules (add jev_api_key)".to_owned() };
    vec![("Vertex AI", vertex, c.vertex_enabled()), ("AWS Bedrock", bedrock, c.bedrock_enabled()), ("Jev routing", jev, c.jev_enabled())]
  }

  /// Records a pending job, then runs it in the background. Returns the job token.
  pub fn start(self: &Arc<Self>, modality: Modality, fields: &Map<String, Value>, tx: Sender<Event>, ctx: egui::Context) -> String {
    let request = Request::from_fields(modality, fields);
    let token = format!("{JOB_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let now = chrono::Utc::now().to_rfc3339();
    let job = Job { job_token: token.clone(), request: JobRequest { inference_category: category(modality).to_owned(), maybe_prompt_token: None, maybe_model_type: Some(request.model_id.clone()), maybe_raw_inference_text: Some(request.prompt.clone()) }, status: JobStatus { status: "started".to_owned(), ..Default::default() }, maybe_result: None, created_at: now.clone(), updated_at: now };
    if let Ok(mut jobs) = self.jobs.lock() {
      jobs.push(job);
    }
    let engine = self.clone();
    let job_token = token.clone();
    tokio::spawn(async move {
      let result = engine.run(modality, &request, &job_token, &tx).await;
      engine.finish(&job_token, result);
      ctx.request_repaint();
    });
    token
  }

  /// Every job still worth listing.
  pub fn jobs(&self) -> Vec<Job> {
    let now = chrono::Utc::now().timestamp();
    let Ok(mut jobs) = self.jobs.lock() else {
      return Vec::new();
    };
    jobs.retain(|j| j.status.status == "started" || chrono::DateTime::parse_from_rfc3339(&j.updated_at).is_ok_and(|t| now - t.timestamp() < FINISHED_JOB_TTL_SECS));
    jobs.clone()
  }

  /// Copies a reference file into the library.
  pub fn upload(&self, bytes: &[u8], extension: &str) -> Result<UploadedMedia, String> {
    let class = MediaClass::from_extension(extension);
    // GIF, BMP, TIFF and the like become PNG, so every provider can read the reference.
    let converted;
    let (bytes, extension) = if class == MediaClass::Image && !PROVIDER_IMAGE_TYPES.contains(&extension.to_ascii_lowercase().as_str()) {
      let image = image::load_from_memory(bytes).map_err(|e| format!("Unsupported image ({extension}): {e}"))?;
      let mut png = Vec::new();
      image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| e.to_string())?;
      converted = png;
      (converted.as_slice(), "png")
    } else {
      (bytes, extension)
    };
    let file = self.library.add(NewFile { bytes, extension, class, origin: Origin::Upload, maybe_batch_token: None, maybe_prompt_token: None, maybe_model_id: None }).map_err(|e| e.to_string())?;
    let thumbnail = file.thumbnail(THUMBNAIL_WIDTH);
    Ok(UploadedMedia { token: file.token, duration_secs: 0.0, thumbnail, full_url: Some(file.media_links.cdn_url) })
  }

  async fn run(&self, modality: Modality, request: &Request, job_token: &str, tx: &Sender<Event>) -> anyhow::Result<Vec<MediaFile>> {
    let Decision { model, reason } = router::decide(&self.config, self.jev.as_ref(), modality, request).await?;
    let _ = tx.send(Event::Toast { error: false, message: reason });
    let prompt_token = self.record_prompt(request).map_err(|err| warn!("Couldn't save the prompt record: {err}")).ok();
    self.set_prompt_token(job_token, prompt_token.clone());
    let images = self.load_images(&request.image_tokens)?;
    let start = request.start_frame.as_deref().map(|t| self.load_image(t)).transpose()?;
    let end = request.end_frame.as_deref().map(|t| self.load_image(t)).transpose()?;
    if end.is_some() && start.is_none() {
      anyhow::bail!("An ending frame needs a starting frame too");
    }
    let aspect = request.aspect_ratio.as_deref().filter(|a| model.info.aspect_ratios.iter().any(|m| m == a)).and_then(catalog::aspect_ratio);
    let resolution = request.resolution.as_deref().filter(|r| model.info.resolutions.iter().any(|m| m == r));
    let files: Vec<GeneratedFile> = match model.route {
      Route::GeminiImage { model: upstream } => {
        let calls = (0..request.batch_count).map(|_| self.vertex.generate_image(GeminiImageRequest { model: upstream, prompt: &request.prompt, images: &images, aspect_ratio: aspect, image_size: resolution.and_then(catalog::image_size) }));
        collect_batch(join_all(calls).await)?
      },
      Route::Stability { model: upstream } => {
        let calls = (0..request.batch_count).map(|_| self.bedrock.generate_image(StabilityRequest { model: upstream, prompt: &request.prompt, aspect_ratio: aspect, image: images.first() }));
        collect_batch(join_all(calls).await)?
      },
      Route::Veo { model: upstream } => {
        let resolution = resolution.and_then(catalog::video_resolution);
        // Veo makes 1080p and 4K only as 8-second clips.
        let duration = match resolution {
          Some("1080p" | "4k") => Some(VEO_HD_SECONDS),
          _ => request.duration_seconds.filter(|d| model.info.duration_options.contains(d)).or(model.info.duration_default),
        };
        self.vertex.generate_video(VeoRequest { model: upstream, prompt: &request.prompt, start_frame: start.as_ref(), end_frame: end.as_ref(), aspect_ratio: aspect, resolution, duration_seconds: duration, sample_count: request.batch_count, generate_audio: request.generate_audio }).await?
      },
    };
    let batch = LocalLibrary::new_batch_token();
    let class = if modality == Modality::Video { MediaClass::Video } else { MediaClass::Image };
    // Saving writes files (and runs FFmpeg for video posters): keep it off the async workers.
    tokio::task::block_in_place(|| files.iter().map(|f| self.library.add(NewFile { bytes: &f.bytes, extension: f.extension, class, origin: Origin::Generated, maybe_batch_token: Some(&batch), maybe_prompt_token: prompt_token.as_deref(), maybe_model_id: Some(&model.info.id) })).collect())
  }

  /// Marks the job done (with its first result, which points the feed at the batch) or failed.
  fn finish(&self, job_token: &str, result: anyhow::Result<Vec<MediaFile>>) {
    let Ok(mut jobs) = self.jobs.lock() else {
      return;
    };
    let Some(job) = jobs.iter_mut().find(|j| j.job_token == job_token) else {
      return;
    };
    job.updated_at = chrono::Utc::now().to_rfc3339();
    match result {
      Ok(files) if !files.is_empty() => {
        let first = &files[0];
        job.status.status = "complete_success".to_owned();
        job.status.progress_percentage = 100;
        job.maybe_result = Some(JobResult { entity_token: first.token.clone(), maybe_batch_token: first.maybe_batch_token.clone(), media_links: first.media_links.clone() });
      },
      Ok(_) => fail(job, "The provider returned nothing".to_owned()),
      Err(err) => {
        warn!("Local job {job_token} failed: {err:#}");
        fail(job, format!("{err:#}"));
      },
    }
  }

  fn set_prompt_token(&self, job_token: &str, prompt_token: Option<String>) {
    if let Ok(mut jobs) = self.jobs.lock() {
      if let Some(job) = jobs.iter_mut().find(|j| j.job_token == job_token) {
        job.request.maybe_prompt_token = prompt_token;
      }
    }
  }

  /// Saves what Recreate needs.
  fn record_prompt(&self, request: &Request) -> anyhow::Result<String> {
    let context = |token: &str, semantic: &str| self.library.get(token).map(|f| ContextImage { media_token: f.token, semantic: semantic.to_owned(), media_links: f.media_links });
    let mut images: Vec<ContextImage> = request.image_tokens.iter().filter_map(|t| context(t, "imgref")).collect();
    images.extend(request.start_frame.as_deref().and_then(|t| context(t, "vid_start_frame")));
    images.extend(request.end_frame.as_deref().and_then(|t| context(t, "vid_end_frame")));
    self.library.add_prompt(Prompt { token: String::new(), maybe_positive_prompt: Some(request.prompt.clone()), maybe_negative_prompt: None, maybe_model_type: Some(request.model_id.clone()), maybe_aspect_ratio: request.aspect_ratio.clone(), maybe_resolution: request.resolution.clone(), maybe_duration_seconds: request.duration_seconds, maybe_generate_audio: request.generate_audio, maybe_batch_count: Some(request.batch_count), maybe_context_images: Some(images) })
  }

  fn load_images(&self, tokens: &[String]) -> anyhow::Result<Vec<RefImage>> {
    tokens.iter().map(|t| self.load_image(t)).collect()
  }

  fn load_image(&self, token: &str) -> anyhow::Result<RefImage> {
    let path = self.library.path_of(token).ok_or_else(|| anyhow::anyhow!("A reference image is missing from the local library ({token}). Add it again."))?;
    let bytes = std::fs::read(&path)?;
    let mime_type = match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
      Some("jpg" | "jpeg") => "image/jpeg",
      Some("webp") => "image/webp",
      _ => "image/png",
    };
    Ok(RefImage { mime_type: mime_type.to_owned(), bytes })
  }
}

/// Every result of a batch; one failure is tolerated when others succeeded.
fn collect_batch(results: Vec<anyhow::Result<GeneratedFile>>) -> anyhow::Result<Vec<GeneratedFile>> {
  let mut files = Vec::new();
  let mut last_err = None;
  for result in results {
    match result {
      Ok(file) => files.push(file),
      Err(err) => {
        warn!("One image in the batch failed: {err:#}");
        last_err = Some(err);
      },
    }
  }
  match (files.is_empty(), last_err) {
    (true, Some(err)) => Err(err),
    _ => Ok(files),
  }
}

fn fail(job: &mut Job, message: String) {
  job.status.status = "complete_failure".to_owned();
  job.status.maybe_failure_category_updated = Some("local_provider_error".to_owned());
  job.status.maybe_failure_message = Some(message);
}

fn category(modality: Modality) -> &'static str {
  match modality {
    Modality::Image => "image_generation",
    Modality::Video => "video_generation",
    Modality::Audio => "audio_generation",
  }
}

#[cfg(test)]
mod tests {
  use std::io::{BufRead, BufReader, Read, Write};
  use std::net::TcpListener;
  use std::time::Duration;

  use super::*;

  /// The whole path with a fake Vertex: Auto (rules) picks a Gemini model, the batch's two calls
  /// go out with the key, and the images land in the library as one batch the feed can load.
  #[tokio::test(flavor = "multi_thread")]
  async fn a_generation_runs_end_to_end_against_a_fake_vertex() {
    let (base_url, requests) = fake_vertex(2);
    let dir = std::env::temp_dir().join(format!("artcraft-local-e2e-{}", uuid::Uuid::new_v4()));
    let config = LocalConfig { gcp_api_key: Some("test-key".into()), vertex_base_url: Some(base_url), bedrock_enabled: Some(false), ..Default::default() };
    let engine = Arc::new(LocalEngine::new(&dir, config, reqwest::Client::builder().no_proxy().build().unwrap()));
    let reference = engine.upload(&tiny_png(), "png").unwrap();
    let fields = serde_json::json!({ "model": "auto_image", "prompt": "a fox in snow", "aspect_ratio": "wide_sixteen_by_nine", "resolution": "two_k", "image_batch_count": 2, "image_media_tokens": [reference.token] });
    let (tx, rx) = std::sync::mpsc::channel();
    let token = engine.start(Modality::Image, fields.as_object().unwrap(), tx, egui::Context::default());

    let job = wait_for_job(&engine, &token).await;
    assert_eq!(job.status.status, "complete_success", "{:?}", job.status.maybe_failure_message);
    let result = job.maybe_result.unwrap();
    let batch = engine.library().batch(result.maybe_batch_token.as_deref().unwrap());
    assert_eq!(batch.len(), 2);
    assert!(batch.iter().all(|f| f.maybe_model_type.as_deref() == Some("nano_banana_2_vertex")));
    assert!(image::load_from_memory(&std::fs::read(engine.library().path_of(&batch[0].token).unwrap()).unwrap()).is_ok());
    let prompt = engine.library().prompt(job.request.maybe_prompt_token.as_deref().unwrap()).unwrap();
    assert_eq!(prompt.maybe_model_type.as_deref(), Some("auto_image"), "Recreate goes back through Auto");

    let toast = rx.try_iter().find_map(|e| match e {
      Event::Toast { message, .. } => Some(message),
      _ => None,
    });
    assert_eq!(toast.as_deref(), Some("Auto \u{2192} Nano Banana 2 (rules)"));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for (request_line, headers, body) in requests.iter() {
      assert_eq!(request_line, "POST /v1beta1/publishers/google/models/gemini-3.1-flash-image:generateContent HTTP/1.1");
      assert!(headers.contains("x-goog-api-key: test-key"));
      let body: Value = serde_json::from_str(body).unwrap();
      assert_eq!(body["generationConfig"]["imageConfig"], serde_json::json!({ "aspectRatio": "16:9", "imageSize": "2K" }));
      assert_eq!(body["contents"][0]["parts"][1]["text"], "a fox in snow");
    }
    let _ = std::fs::remove_dir_all(dir);
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn provider_errors_fail_the_job_with_their_message() {
    let dir = std::env::temp_dir().join(format!("artcraft-local-e2e-{}", uuid::Uuid::new_v4()));
    let engine = Arc::new(LocalEngine::new(&dir, LocalConfig { bedrock_enabled: Some(false), ..Default::default() }, reqwest::Client::new()));
    let fields = serde_json::json!({ "model": "nano_banana_2_vertex", "prompt": "x" });
    let (tx, _rx) = std::sync::mpsc::channel();
    let token = engine.start(Modality::Image, fields.as_object().unwrap(), tx, egui::Context::default());
    let job = wait_for_job(&engine, &token).await;
    assert_eq!(job.status.phase(), crate::backend::wire::JobPhase::Failed);
    assert!(job.status.maybe_failure_message.unwrap().contains("isn't available"), "no Vertex account configured");
    let _ = std::fs::remove_dir_all(dir);
  }

  #[test]
  fn a_partly_failed_batch_keeps_its_images() {
    let ok = || Ok(GeneratedFile { bytes: vec![1], extension: "png" });
    assert_eq!(collect_batch(vec![ok(), Err(anyhow::anyhow!("blocked"))]).unwrap().len(), 1);
    assert!(collect_batch(vec![Err(anyhow::anyhow!("blocked"))]).is_err());
  }

  #[test]
  fn failures_carry_the_providers_message() {
    let mut job = Job::default();
    fail(&mut job, "Vertex AI error 403: permission denied".into());
    assert_eq!(job.status.phase(), crate::backend::wire::JobPhase::Failed);
    assert_eq!(job.status.maybe_failure_message.as_deref(), Some("Vertex AI error 403: permission denied"));
  }

  #[tokio::test]
  async fn uploads_land_in_the_library_as_references() {
    let dir = std::env::temp_dir().join(format!("artcraft-local-engine-{}", uuid::Uuid::new_v4()));
    let engine = LocalEngine::new(&dir, LocalConfig { bedrock_enabled: Some(false), ..Default::default() }, reqwest::Client::new());
    let uploaded = engine.upload(&[1, 2, 3], "png").unwrap();
    assert!(uploaded.full_url.as_deref().is_some_and(|u| u.starts_with("file://")));
    assert_eq!(engine.load_image(&uploaded.token).unwrap().bytes, vec![1, 2, 3]);
    assert!(engine.library().list("image", true, 0, 40).0.len() == 1 && engine.library().list("image", false, 0, 40).0.is_empty(), "the feeds leave uploads out");
    assert!(config::config_path(&dir).exists(), "a starter settings file is written");
    let _ = std::fs::remove_dir_all(dir);
  }

  async fn wait_for_job(engine: &LocalEngine, token: &str) -> Job {
    for _ in 0..200 {
      if let Some(job) = engine.jobs().into_iter().find(|j| j.job_token == token && j.status.status != "started") {
        return job;
      }
      tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("job {token} never finished");
  }

  /// A one-thread HTTP server that answers `count` requests with a Gemini image reply, recording
  /// each request line, headers and body.
  fn fake_vertex(count: usize) -> (String, Arc<Mutex<Vec<(String, String, String)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let reply = serde_json::json!({ "candidates": [{ "finishReason": "STOP", "content": { "parts": [{ "inlineData": { "mimeType": "image/png", "data": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, tiny_png()) } }] } }] }).to_string();
    std::thread::spawn(move || {
      for stream in listener.incoming().take(count) {
        let mut stream = stream.unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let (mut headers, mut length) = (String::new(), 0);
        loop {
          let mut line = String::new();
          reader.read_line(&mut line).unwrap();
          if line == "\r\n" {
            break;
          }
          if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
          }
          headers.push_str(&line.to_ascii_lowercase());
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        record.lock().unwrap().push((request_line.trim().to_owned(), headers, String::from_utf8(body).unwrap()));
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
      }
    });
    (url, seen)
  }

  fn tiny_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 80, 40, 255])).write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
    bytes
  }
}
