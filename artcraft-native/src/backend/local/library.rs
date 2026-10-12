//! The on-disk library that replaces ArtCraft's media service in local mode.
//!
//! Files live in `~/Artcraft/local/media/` and an index (`library.json`) keeps their records in
//! the server's [`MediaFile`] shape, so the feed, lightbox and pickers work unchanged. Links are
//! `file://` URLs; the media cache and video player read those straight from disk. Prompt records
//! (for Recreate) are kept beside them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use log::warn;
use serde::{Deserialize, Serialize};

use crate::backend::wire::{MediaFile, MediaLinks, Prompt, VideoPreviews};

const DIRECTORY: &str = "local";
const MEDIA_DIRECTORY: &str = "media";
const INDEX_FILENAME: &str = "library.json";
/// Token prefix for local files (ArtCraft's are `mf_…`).
const TOKEN_PREFIX: &str = "mf_local_";
const PROMPT_PREFIX: &str = "prompt_local_";
const BATCH_PREFIX: &str = "batch_local_";

/// What kind of file a record is (the server's `media_class`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaClass {
  Image,
  Video,
  Audio,
}

impl MediaClass {
  fn as_str(self) -> &'static str {
    match self {
      MediaClass::Image => "image",
      MediaClass::Video => "video",
      MediaClass::Audio => "audio",
    }
  }

  /// From a file extension (anything unknown is treated as an image).
  pub fn from_extension(ext: &str) -> Self {
    match ext.to_ascii_lowercase().as_str() {
      "mp4" | "mov" | "webm" | "m4v" => MediaClass::Video,
      "mp3" | "wav" | "m4a" | "aac" | "ogg" | "flac" => MediaClass::Audio,
      _ => MediaClass::Image,
    }
  }
}

/// Where a file came from (the server's `origin_category`; the feeds hide uploads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
  Generated,
  Upload,
}

/// A file to add: its bytes and how to label it.
pub struct NewFile<'a> {
  pub bytes: &'a [u8],
  pub extension: &'a str,
  pub class: MediaClass,
  pub origin: Origin,
  pub maybe_batch_token: Option<&'a str>,
  pub maybe_prompt_token: Option<&'a str>,
  pub maybe_model_id: Option<&'a str>,
}

/// The library, shared by the backend's tasks.
#[derive(Clone)]
pub struct LocalLibrary {
  dir: Arc<PathBuf>,
  index: Arc<Mutex<Index>>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Index {
  /// Newest first.
  files: Vec<MediaFile>,
  prompts: Vec<Prompt>,
  /// How each batch was made (routing and cost), newest first.
  generations: Vec<GenerationRecord>,
}

/// How a generation was routed and what it cost, shown beside its results in the lightbox.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GenerationRecord {
  pub batch_token: String,
  /// The model picked in the UI (`auto_image` for Auto).
  pub requested_model: String,
  pub chosen_model: String,
  pub chosen_name: String,
  /// `Vertex AI` or `Bedrock`.
  pub provider: String,
  pub upstream_model: String,
  /// `user`, `only_fit`, `jev`, `rules_jev_unsure`, `rules_jev_unavailable` or `rules`.
  pub decided_by: String,
  pub maybe_jev: Option<JevRecord>,
  /// Models that could honor the settings.
  pub candidates: Vec<String>,
  /// Models that couldn't, with the setting that ruled them out.
  pub ruled_out: Vec<(String, String)>,
  /// The provider's charge for the batch, at list prices.
  pub cost_usd: f64,
  /// Jev's charge for routing it.
  pub routing_cost_usd: f64,
  /// How the cost was worked out, e.g. `2 × 1,120 image tokens`.
  pub cost_basis: String,
  pub outputs: u32,
  /// Wall time from routing to saved results.
  pub seconds: f32,
}

/// Jev's answer for a generation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JevRecord {
  pub model: String,
  /// The model Jev picked, by name.
  pub choice: String,
  pub confidence: f32,
  /// The bar a pick had to clear to be used.
  pub min_confidence: f32,
  /// Every option, by name, highest first.
  pub probabilities: Vec<(String, f32)>,
  pub input_tokens: u64,
}

impl LocalLibrary {
  pub fn load(data_root: &Path) -> Self {
    let dir = data_root.join(DIRECTORY);
    let path = dir.join(INDEX_FILENAME);
    let index = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).map_err(|err| warn!("Ignoring unreadable {}: {err}", path.display())).ok()).unwrap_or_default();
    Self { dir: Arc::new(dir), index: Arc::new(Mutex::new(index)) }
  }

  pub fn new_batch_token() -> String {
    format!("{BATCH_PREFIX}{}", uuid::Uuid::new_v4().simple())
  }

  #[cfg_attr(not(test), allow(dead_code))]
  pub fn is_local_token(token: &str) -> bool {
    token.starts_with(TOKEN_PREFIX)
  }

  /// Writes the file into the library and records it.
  pub fn add(&self, file: NewFile<'_>) -> anyhow::Result<MediaFile> {
    let token = format!("{TOKEN_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let media_dir = self.dir.join(MEDIA_DIRECTORY);
    std::fs::create_dir_all(&media_dir)?;
    let extension = file.extension.trim_start_matches('.').to_ascii_lowercase();
    let path = media_dir.join(format!("{token}.{extension}"));
    std::fs::write(&path, file.bytes)?;
    let mut links = MediaLinks { cdn_url: file_url(&path), ..Default::default() };
    if file.class == MediaClass::Video {
      if let Some(still) = extract_still(&path, &media_dir.join(format!("{token}_still.jpg"))) {
        links.maybe_video_previews = Some(VideoPreviews { still: file_url(&still), ..Default::default() });
      }
    }
    let class = file.class.as_str().to_owned();
    let record = MediaFile {
      token,
      media_class: Some(class.clone()),
      media_type: class,
      origin_category: match file.origin {
        Origin::Generated => "inference".to_owned(),
        Origin::Upload => "upload".to_owned(),
      },
      maybe_batch_token: file.maybe_batch_token.map(str::to_owned),
      maybe_prompt_token: file.maybe_prompt_token.map(str::to_owned),
      maybe_model_type: file.maybe_model_id.map(str::to_owned),
      maybe_origin_model_type: file.maybe_model_id.map(str::to_owned),
      maybe_duration_millis: None,
      maybe_title: None,
      media_links: links,
      created_at: chrono::Utc::now().to_rfc3339(),
    };
    self.mutate(|index| index.files.insert(0, record.clone()))?;
    Ok(record)
  }

  /// Records a prompt (for Recreate) and returns its token.
  pub fn add_prompt(&self, mut prompt: Prompt) -> anyhow::Result<String> {
    let token = format!("{PROMPT_PREFIX}{}", uuid::Uuid::new_v4().simple());
    prompt.token = token.clone();
    self.mutate(|index| index.prompts.insert(0, prompt))?;
    Ok(token)
  }

  /// Records how a batch was made.
  pub fn add_generation(&self, record: GenerationRecord) -> anyhow::Result<()> {
    self.mutate(|index| index.generations.insert(0, record))
  }

  pub fn generation(&self, batch_token: &str) -> Option<GenerationRecord> {
    self.index.lock().ok()?.generations.iter().find(|g| g.batch_token == batch_token).cloned()
  }

  /// The directory the library lives in.
  pub fn dir(&self) -> &Path {
    &self.dir
  }

  pub fn prompt(&self, token: &str) -> Option<Prompt> {
    self.index.lock().ok()?.prompts.iter().find(|p| p.token == token).cloned()
  }

  pub fn get(&self, token: &str) -> Option<MediaFile> {
    self.index.lock().ok()?.files.iter().find(|f| f.token == token).cloned()
  }

  /// The file on disk behind a record.
  pub fn path_of(&self, token: &str) -> Option<PathBuf> {
    self.get(token).and_then(|f| file_path(&f.media_links.cdn_url))
  }

  /// One page of records whose class is in `classes` (comma-separated, like the server's
  /// `filter_media_classes`), with the page count. The feeds leave reference uploads out.
  pub fn list(&self, classes: &str, include_uploads: bool, page: u32, page_size: u32) -> (Vec<MediaFile>, u32) {
    let wanted: Vec<&str> = classes.split(',').map(str::trim).filter(|c| !c.is_empty()).collect();
    let Ok(index) = self.index.lock() else {
      return (Vec::new(), 0);
    };
    let matching: Vec<&MediaFile> = index.files.iter().filter(|f| include_uploads || f.origin_category != "upload").filter(|f| wanted.is_empty() || f.media_class.as_deref().is_some_and(|c| wanted.contains(&c))).collect();
    let size = page_size.max(1) as usize;
    let total_pages = matching.len().div_ceil(size) as u32;
    let items = matching.into_iter().skip(page as usize * size).take(size).cloned().collect();
    (items, total_pages)
  }

  /// Every record a generation produced, in order.
  pub fn batch(&self, batch_token: &str) -> Vec<MediaFile> {
    let Ok(index) = self.index.lock() else {
      return Vec::new();
    };
    let mut files: Vec<MediaFile> = index.files.iter().filter(|f| f.maybe_batch_token.as_deref() == Some(batch_token)).cloned().collect();
    files.reverse();
    files
  }

  /// Removes the record and its files.
  pub fn delete(&self, token: &str) -> anyhow::Result<()> {
    let Some(record) = self.get(token) else {
      anyhow::bail!("No such file in the local library");
    };
    for url in [Some(record.media_links.cdn_url.clone()), record.media_links.maybe_video_previews.map(|p| p.still)].into_iter().flatten() {
      if let Some(path) = file_path(&url) {
        if let Err(err) = std::fs::remove_file(&path) {
          warn!("Couldn't remove {}: {err}", path.display());
        }
      }
    }
    self.mutate(|index| index.files.retain(|f| f.token != token))
  }

  fn mutate(&self, change: impl FnOnce(&mut Index)) -> anyhow::Result<()> {
    let mut index = self.index.lock().map_err(|_| anyhow::anyhow!("library index lock poisoned"))?;
    change(&mut index);
    save(&self.dir.join(INDEX_FILENAME), &index)
  }
}

/// A `file://` URL for a path.
pub fn file_url(path: &Path) -> String {
  url::Url::from_file_path(path).map(String::from).unwrap_or_else(|()| format!("file://{}", path.display()))
}

/// The path behind a `file://` URL (`None` for anything else).
pub fn file_path(url: &str) -> Option<PathBuf> {
  url::Url::parse(url).ok().filter(|u| u.scheme() == "file").and_then(|u| u.to_file_path().ok())
}

/// Writes atomically, so a crash can't truncate the index.
fn save(path: &Path, index: &Index) -> anyhow::Result<()> {
  let dir = path.parent().ok_or_else(|| anyhow::anyhow!("no library directory"))?;
  std::fs::create_dir_all(dir)?;
  let mut file = tempfile::NamedTempFile::new_in(dir)?;
  file.write_all(serde_json::to_string(index)?.as_bytes())?;
  file.persist(path)?;
  Ok(())
}

/// A poster frame for a video, when FFmpeg is installed.
fn extract_still(video: &Path, still: &Path) -> Option<PathBuf> {
  let status = crate::backend::video::command("ffmpeg").args(["-y", "-loglevel", "error", "-i"]).arg(video).args(["-frames:v", "1", "-q:v", "3"]).arg(still).stdout(Stdio::null()).stderr(Stdio::null()).status().ok()?;
  (status.success() && still.exists()).then(|| still.to_path_buf())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn files_round_trip_through_the_index() {
    let dir = temp_root();
    let library = LocalLibrary::load(&dir);
    let batch = LocalLibrary::new_batch_token();
    let first = library.add(new_file(b"one", MediaClass::Image, Some(&batch))).unwrap();
    let second = library.add(new_file(b"two", MediaClass::Image, Some(&batch))).unwrap();
    library.add(new_file(b"vid", MediaClass::Video, None)).unwrap();
    assert!(LocalLibrary::is_local_token(&first.token));
    assert_eq!(std::fs::read(library.path_of(&first.token).unwrap()).unwrap(), b"one");

    let reloaded = LocalLibrary::load(&dir);
    let batch_tokens: Vec<String> = reloaded.batch(&batch).into_iter().map(|f| f.token).collect();
    assert_eq!(batch_tokens, [first.token.clone(), second.token.clone()], "a batch reads in creation order");
    let (images, pages) = reloaded.list("image", true, 0, 1);
    assert_eq!((images[0].token.as_str(), pages), (second.token.as_str(), 2), "newest first");
    assert_eq!(reloaded.list("image,video", true, 0, 40).0.len(), 3);

    reloaded.delete(&first.token).unwrap();
    assert!(reloaded.get(&first.token).is_none());
    assert!(!dir.join(DIRECTORY).join(MEDIA_DIRECTORY).join(format!("{}.png", first.token)).exists());
    let _ = std::fs::remove_dir_all(dir);
  }

  #[test]
  fn generation_records_survive_a_restart() {
    let dir = temp_root();
    let record = GenerationRecord { batch_token: "batch_local_1".into(), chosen_name: "Nano Banana Pro".into(), decided_by: "jev".into(), maybe_jev: Some(JevRecord { choice: "Nano Banana Pro".into(), confidence: 0.81, ..Default::default() }), cost_usd: 0.268, ..Default::default() };
    LocalLibrary::load(&dir).add_generation(record.clone()).unwrap();
    assert_eq!(LocalLibrary::load(&dir).generation("batch_local_1"), Some(record));
    let _ = std::fs::remove_dir_all(dir);
  }

  #[test]
  fn prompts_are_kept_for_recreate() {
    let dir = temp_root();
    let library = LocalLibrary::load(&dir);
    let token = library.add_prompt(Prompt { maybe_positive_prompt: Some("a fox".into()), ..Default::default() }).unwrap();
    assert_eq!(LocalLibrary::load(&dir).prompt(&token).and_then(|p| p.maybe_positive_prompt).as_deref(), Some("a fox"));
    let _ = std::fs::remove_dir_all(dir);
  }

  #[test]
  fn file_urls_round_trip() {
    let path = std::env::temp_dir().join("a b.png");
    assert_eq!(file_path(&file_url(&path)), Some(path));
    assert_eq!(file_path("https://cdn/x.png"), None);
  }

  fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("artcraft-local-library-{}", uuid::Uuid::new_v4()))
  }

  fn new_file<'a>(bytes: &'a [u8], class: MediaClass, batch: Option<&'a str>) -> NewFile<'a> {
    NewFile { bytes, extension: if class == MediaClass::Video { "mp4" } else { "png" }, class, origin: Origin::Generated, maybe_batch_token: batch, maybe_prompt_token: None, maybe_model_id: Some("nano_banana_2_vertex") }
  }
}
