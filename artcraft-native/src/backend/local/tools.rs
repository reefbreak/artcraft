//! The tool pages' work: frame extraction, background removal, image edits and video edits. Each
//! tool takes files from disk (the library or the user's own), saves its results into the local
//! library as one batch, records the charge, and leaves a record for the lightbox.
//!
//! Frame extraction, trimming, joining and local background removal run on the user's machine
//! (FFmpeg and ISNet) and cost nothing. Image edits run on Nano Banana (Vertex) or Stability's
//! image services (Bedrock); extending a clip runs on Veo (Vertex).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};

use super::GeneratedFile;
use super::RefImage;
use super::pricing::PriceBook;
use super::spend::Account;
use super::vertex::{GeminiImageRequest, VeoRequest};
use crate::backend::video;

/// How long a Veo extension adds.
pub const EXTEND_SECONDS: f64 = 7.0;
/// What Gemini is told when a mask comes with an edit.
const MASK_INSTRUCTION: &str = "The second image is a mask. Change only the areas that are white in the mask, blending the edit naturally into its surroundings, and keep everything in the black areas exactly as it is.";

/// A tool run, with the files it works on.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolRequest {
  /// Stills at these times (seconds).
  ExtractFrames {
    video: PathBuf,
    times: Vec<f64>,
  },
  RemoveBackground {
    image: PathBuf,
    engine: BackgroundEngine,
  },
  /// `mask` is a PNG the size of the image, white where the edit applies.
  EditImage {
    image: PathBuf,
    mask: Option<Vec<u8>>,
    op: EditOp,
  },
  TrimVideo {
    video: PathBuf,
    start: f64,
    end: f64,
  },
  /// Clips in order; later ones are scaled to the first one's frame size.
  JoinVideos {
    videos: Vec<PathBuf>,
  },
  ExtendVideo {
    video: PathBuf,
    prompt: String,
    tier: VeoTier,
  },
  /// Keep an image (a 3D Stage render) in the library as is.
  SaveImage {
    image: PathBuf,
  },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackgroundEngine {
  /// ISNet on this machine: free.
  #[default]
  Local,
  /// Stability's Remove Background on Bedrock.
  Bedrock,
}

/// What an image edit does.
#[derive(Clone, Debug, PartialEq)]
pub enum EditOp {
  /// Describe the change; Nano Banana makes it (inside the mask, when there is one).
  Instruct { pro: bool, prompt: String, image_size: Option<String> },
  /// Repaint the masked area from a prompt (Stability Inpaint).
  Inpaint { prompt: String },
  /// Remove what's under the mask (Stability Erase Object).
  Erase,
  /// Find something by description and replace it (Stability Search and Replace).
  SearchReplace { search: String, prompt: String },
  /// Extend the canvas by these pixels (Stability Outpaint).
  Outpaint { left: u32, right: u32, up: u32, down: u32, prompt: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VeoTier {
  #[default]
  Fast,
  Standard,
  Lite,
}

impl VeoTier {
  pub fn model(self) -> &'static str {
    match self {
      VeoTier::Fast => "veo-3.1-fast-generate-001",
      VeoTier::Standard => "veo-3.1-generate-001",
      VeoTier::Lite => "veo-3.1-lite-generate-001",
    }
  }

  pub fn label(self) -> &'static str {
    match self {
      VeoTier::Fast => "Veo 3.1 Fast",
      VeoTier::Standard => "Veo 3.1",
      VeoTier::Lite => "Veo 3.1 Lite",
    }
  }
}

/// Where a tool runs and what it's called, for records and the spend ledger.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolRoute {
  /// The record's model id (`tool_inpaint`).
  pub id: &'static str,
  pub name: &'static str,
  pub provider: &'static str,
  /// The API model, or the local program.
  pub upstream: &'static str,
  pub account: Option<Account>,
}

impl ToolRequest {
  pub fn route(&self) -> ToolRoute {
    let local = |id, name, upstream| ToolRoute { id, name, provider: "Your Mac", upstream, account: None };
    let bedrock = |id, name, upstream| ToolRoute { id, name, provider: "Bedrock", upstream, account: Some(Account::Bedrock) };
    match self {
      ToolRequest::ExtractFrames { .. } => local("tool_extract_frames", "Frame Extract", "ffmpeg"),
      ToolRequest::RemoveBackground { engine: BackgroundEngine::Local, .. } => local("tool_remove_background", "Remove Background (local)", "isnet-general-use"),
      ToolRequest::RemoveBackground { engine: BackgroundEngine::Bedrock, .. } => bedrock("tool_remove_background", "Remove Background", "stability.stable-image-remove-background-v1:0"),
      ToolRequest::EditImage { op: EditOp::Instruct { pro: true, .. }, .. } => ToolRoute { id: "tool_edit_image", name: "Edit with Nano Banana Pro", provider: "Vertex AI", upstream: "gemini-3-pro-image", account: Some(Account::Vertex) },
      ToolRequest::EditImage { op: EditOp::Instruct { pro: false, .. }, .. } => ToolRoute { id: "tool_edit_image", name: "Edit with Nano Banana 2", provider: "Vertex AI", upstream: "gemini-3.1-flash-image", account: Some(Account::Vertex) },
      ToolRequest::EditImage { op: EditOp::Inpaint { .. }, .. } => bedrock("tool_inpaint", "Inpaint", "stability.stable-image-inpaint-v1:0"),
      ToolRequest::EditImage { op: EditOp::Erase, .. } => bedrock("tool_erase", "Erase Object", "stability.stable-image-erase-object-v1:0"),
      ToolRequest::EditImage { op: EditOp::SearchReplace { .. }, .. } => bedrock("tool_search_replace", "Search and Replace", "stability.stable-image-search-replace-v1:0"),
      ToolRequest::EditImage { op: EditOp::Outpaint { .. }, .. } => bedrock("tool_outpaint", "Outpaint", "stability.stable-outpaint-v1:0"),
      ToolRequest::TrimVideo { .. } => local("tool_trim_video", "Trim", "ffmpeg"),
      ToolRequest::SaveImage { .. } => local("tool_stage_render", "3D Stage render", "stage"),
      ToolRequest::JoinVideos { .. } => local("tool_join_videos", "Join", "ffmpeg"),
      ToolRequest::ExtendVideo { tier, .. } => ToolRoute {
        id: "tool_extend_video",
        name: match tier {
          VeoTier::Fast => "Extend with Veo 3.1 Fast",
          VeoTier::Standard => "Extend with Veo 3.1",
          VeoTier::Lite => "Extend with Veo 3.1 Lite",
        },
        provider: "Vertex AI",
        upstream: tier.model(),
        account: Some(Account::Vertex),
      },
    }
  }

  /// What the run will cost at list prices (zero for the local tools).
  pub fn estimate(&self, prices: &PriceBook) -> f64 {
    let route = self.route();
    match self {
      ToolRequest::EditImage { op: EditOp::Instruct { image_size, .. }, mask, .. } => {
        let size = image_size.as_deref().and_then(super::catalog::image_size);
        let output = prices.gemini_call(route.upstream, None, size).unwrap_or(0.0);
        let inputs = 1 + usize::from(mask.is_some());
        let per_input = if route.upstream == "gemini-3-pro-image" { 560.0 } else { 1120.0 };
        output + prices.gemini_rates(route.upstream).map_or(0.0, |r| inputs as f64 * per_input * r.input / 1_000_000.0)
      },
      ToolRequest::EditImage { .. } | ToolRequest::RemoveBackground { engine: BackgroundEngine::Bedrock, .. } => prices.stability_service(route.upstream).unwrap_or(0.0),
      ToolRequest::ExtendVideo { tier, .. } => prices.veo_per_second(tier.model(), true, "720p").unwrap_or(0.0) * EXTEND_SECONDS,
      _ => 0.0,
    }
  }
}

/// Stills from a clip, as PNGs, in time order.
pub fn extract_frames(video: &Path, times: &[f64]) -> anyhow::Result<Vec<GeneratedFile>> {
  anyhow::ensure!(video::ffmpeg_available(), "Frame Extract needs FFmpeg: install it with `brew install ffmpeg`.");
  let dir = tempfile::tempdir()?;
  let mut times: Vec<f64> = times.iter().copied().filter(|t| t.is_finite() && *t >= 0.0).collect();
  times.sort_by(f64::total_cmp);
  times.dedup_by(|a, b| (*a - *b).abs() < 0.01);
  anyhow::ensure!(!times.is_empty(), "Pick at least one moment to extract");
  let mut out = Vec::new();
  for (i, t) in times.iter().enumerate() {
    let frame = dir.path().join(format!("frame_{i}.png"));
    run_ffmpeg(&["-y", "-loglevel", "error", "-ss", &format!("{t:.3}"), "-i"], video, &["-frames:v", "1"], &frame)?;
    out.push(GeneratedFile { bytes: std::fs::read(&frame)?, extension: "png", usage: None });
  }
  Ok(out)
}

/// A clip cut to `[start, end)` seconds, re-encoded so the cut lands on the exact frame.
pub fn trim_video(video: &Path, start: f64, end: f64) -> anyhow::Result<GeneratedFile> {
  anyhow::ensure!(end > start + 0.05, "The end must come after the start");
  let dir = tempfile::tempdir()?;
  let out = dir.path().join("trimmed.mp4");
  run_ffmpeg(&["-y", "-loglevel", "error", "-ss", &format!("{start:.3}"), "-i"], video, &["-t", &format!("{:.3}", end - start), "-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p", "-c:a", "aac", "-movflags", "+faststart"], &out)?;
  Ok(GeneratedFile { bytes: std::fs::read(&out)?, extension: "mp4", usage: None })
}

/// Clips joined end to end at the first clip's frame size (others letterboxed), 24 fps. Sound is
/// kept only when every clip has it.
pub fn join_videos(videos: &[PathBuf]) -> anyhow::Result<GeneratedFile> {
  anyhow::ensure!(videos.len() >= 2, "Add at least two clips to join");
  anyhow::ensure!(video::ffmpeg_available(), "Joining clips needs FFmpeg: install it with `brew install ffmpeg`.");
  let (w, h, _, _) = video::probe_file(&videos[0]).map_err(|e| anyhow::anyhow!("Couldn't read {}: {e}", videos[0].display()))?;
  let (w, h) = (w - w % 2, h - h % 2);
  let audio = videos.iter().all(|v| has_audio(v));
  let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
  for v in videos {
    args.push("-i".into());
    args.push(v.to_string_lossy().into_owned());
  }
  let mut filter = String::new();
  for i in 0..videos.len() {
    filter.push_str(&format!("[{i}:v]scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=24[v{i}];"));
  }
  for i in 0..videos.len() {
    filter.push_str(&format!("[v{i}]"));
    if audio {
      filter.push_str(&format!("[{i}:a]"));
    }
  }
  filter.push_str(&format!("concat=n={}:v=1:a={}[v]{}", videos.len(), u8::from(audio), if audio { "[a]" } else { "" }));
  let dir = tempfile::tempdir()?;
  let out = dir.path().join("joined.mp4");
  args.extend(["-filter_complex".into(), filter, "-map".into(), "[v]".into()]);
  if audio {
    args.extend(["-map".into(), "[a]".into(), "-c:a".into(), "aac".into()]);
  }
  args.extend(["-c:v".into(), "libx264".into(), "-preset".into(), "veryfast".into(), "-crf".into(), "18".into(), "-pix_fmt".into(), "yuv420p".into(), "-movflags".into(), "+faststart".into()]);
  args.push(out.to_string_lossy().into_owned());
  let status = video::command("ffmpeg").args(&args).stdout(Stdio::null()).stderr(Stdio::piped()).output()?;
  anyhow::ensure!(status.status.success(), "FFmpeg couldn't join the clips: {}", String::from_utf8_lossy(&status.stderr).lines().last().unwrap_or("unknown error"));
  Ok(GeneratedFile { bytes: std::fs::read(&out)?, extension: "mp4", usage: None })
}

/// A clip made ready for Veo extension: MP4, 24 fps, 720p (landscape or portrait), at most 30 s.
pub fn prepare_for_extension(video: &Path) -> anyhow::Result<Vec<u8>> {
  let (w, h, _, duration) = video::probe_file(video).map_err(|e| anyhow::anyhow!("Couldn't read the clip: {e}"))?;
  anyhow::ensure!(duration <= 30.5, "Veo can extend clips up to 30 seconds; trim this one first");
  let (tw, th) = if w >= h { (1280, 720) } else { (720, 1280) };
  let dir = tempfile::tempdir()?;
  let out = dir.path().join("for_veo.mp4");
  let scale = format!("scale={tw}:{th}:force_original_aspect_ratio=decrease,pad={tw}:{th}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=24");
  run_ffmpeg(&["-y", "-loglevel", "error", "-i"], video, &["-vf", &scale, "-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p", "-c:a", "aac"], &out)?;
  Ok(std::fs::read(&out)?)
}

/// Gemini's request for an instruction edit: the image, the mask (if any), and the instruction.
pub fn instruct_parts(image: RefImage, mask: Option<&[u8]>, prompt: &str) -> (Vec<RefImage>, String) {
  match mask {
    Some(mask) => (vec![image, RefImage { mime_type: "image/png".into(), bytes: mask.to_vec() }], format!("{prompt}\n\n{MASK_INSTRUCTION}")),
    None => (vec![image], format!("{prompt}\n\nKeep everything else in the image exactly as it is.")),
  }
}

/// The InvokeModel body for a Stability image service.
pub fn stability_body(op: &EditOp, image: &[u8], mask: Option<&[u8]>) -> Value {
  let image = BASE64.encode(image);
  let mut body = match op {
    EditOp::Inpaint { prompt } => json!({ "image": image, "prompt": prompt }),
    EditOp::Erase => json!({ "image": image }),
    EditOp::SearchReplace { search, prompt } => json!({ "image": image, "search_prompt": search, "prompt": prompt }),
    EditOp::Outpaint { left, right, up, down, prompt } => {
      let mut b = json!({ "image": image, "left": left, "right": right, "up": up, "down": down });
      if !prompt.trim().is_empty() {
        b["prompt"] = json!(prompt);
      }
      b
    },
    EditOp::Instruct { .. } => json!({ "image": image }),
  };
  if let (Some(mask), EditOp::Inpaint { .. } | EditOp::Erase) = (mask, op) {
    body["mask"] = json!(BASE64.encode(mask));
  }
  body["output_format"] = json!("png");
  body
}

/// The request to send Gemini for an instruction edit.
pub fn gemini_edit<'a>(upstream: &'a str, images: &'a [RefImage], prompt: &'a str, image_size: Option<&'a str>) -> GeminiImageRequest<'a> {
  GeminiImageRequest { model: upstream, prompt, images, aspect_ratio: None, image_size }
}

/// The request to send Veo to extend a prepared clip.
pub fn veo_extend<'a>(upstream: &'a str, prompt: &'a str, clip: &'a [u8]) -> VeoRequest<'a> {
  VeoRequest { model: upstream, prompt, start_frame: None, end_frame: None, aspect_ratio: None, resolution: None, duration_seconds: None, sample_count: 1, generate_audio: Some(true), extend: Some(clip) }
}

fn has_audio(video: &Path) -> bool {
  video::command("ffprobe").args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=index", "-of", "csv=p=0"]).arg(video).output().is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

fn run_ffmpeg(before_input: &[&str], input: &Path, after_input: &[&str], output: &Path) -> anyhow::Result<()> {
  let result = video::command("ffmpeg").args(before_input).arg(input).args(after_input).arg(output).stdout(Stdio::null()).stderr(Stdio::piped()).output()?;
  anyhow::ensure!(result.status.success() && output.exists(), "FFmpeg failed: {}", String::from_utf8_lossy(&result.stderr).lines().last().unwrap_or("unknown error"));
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tools_are_priced_by_where_they_run() {
    let prices = PriceBook::default();
    let image = PathBuf::from("a.png");
    assert_eq!(ToolRequest::RemoveBackground { image: image.clone(), engine: BackgroundEngine::Local }.estimate(&prices), 0.0);
    assert_eq!(ToolRequest::RemoveBackground { image: image.clone(), engine: BackgroundEngine::Bedrock }.estimate(&prices), 0.07);
    assert_eq!(ToolRequest::EditImage { image: image.clone(), mask: None, op: EditOp::Outpaint { left: 100, right: 0, up: 0, down: 0, prompt: String::new() } }.estimate(&prices), 0.06);
    let extend = ToolRequest::ExtendVideo { video: PathBuf::from("a.mp4"), prompt: "go on".into(), tier: VeoTier::Fast };
    assert!((extend.estimate(&prices) - 0.70).abs() < 1e-9);
    let edit = ToolRequest::EditImage { image, mask: Some(vec![0]), op: EditOp::Instruct { pro: true, prompt: "x".into(), image_size: Some("one_k".into()) } };
    assert!(edit.estimate(&prices) > 0.134 && edit.estimate(&prices) < 0.14);
  }

  #[test]
  fn stability_bodies_carry_the_mask_only_where_it_applies() {
    let inpaint = stability_body(&EditOp::Inpaint { prompt: "a red door".into() }, &[1, 2, 3], Some(&[9]));
    assert_eq!(inpaint, json!({ "image": "AQID", "prompt": "a red door", "mask": "CQ==", "output_format": "png" }));
    let replace = stability_body(&EditOp::SearchReplace { search: "cat".into(), prompt: "dog".into() }, &[1, 2, 3], Some(&[9]));
    assert!(replace.get("mask").is_none());
    let out = stability_body(&EditOp::Outpaint { left: 200, right: 0, up: 0, down: 50, prompt: " ".into() }, &[1], None);
    assert_eq!((out["left"].as_u64(), out["down"].as_u64(), out.get("prompt")), (Some(200), Some(50), None));
  }

  #[test]
  fn masked_instructions_send_the_mask_second() {
    let image = RefImage { mime_type: "image/png".into(), bytes: vec![1] };
    let (images, prompt) = instruct_parts(image.clone(), Some(&[2]), "make the sky stormy");
    assert_eq!(images.len(), 2);
    assert!(prompt.starts_with("make the sky stormy") && prompt.contains("white in the mask"));
    assert_eq!(instruct_parts(image, None, "x").0.len(), 1);
  }

  /// Needs FFmpeg on the PATH: `cargo test -p artcraft_native -- --ignored`.
  #[test]
  #[ignore]
  fn frames_trims_and_joins_with_ffmpeg() {
    let dir = tempfile::tempdir().unwrap();
    let clip = |name: &str, color: &str| {
      let path = dir.path().join(name);
      let status = video::command("ffmpeg").args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", &format!("color=c={color}:s=320x240:d=3:r=24"), "-f", "lavfi", "-i", "sine=duration=3", "-shortest", "-pix_fmt", "yuv420p"]).arg(&path).status().unwrap();
      assert!(status.success());
      path
    };
    let red = clip("red.mp4", "red");
    let blue = clip("blue.mp4", "blue");
    let frames = extract_frames(&red, &[2.0, 0.5, 0.5]).unwrap();
    assert_eq!(frames.len(), 2, "duplicate times collapse");
    assert!(image::load_from_memory(&frames[0].bytes).is_ok());
    let trimmed = trim_video(&red, 0.5, 2.0).unwrap();
    let path = dir.path().join("trimmed.mp4");
    std::fs::write(&path, &trimmed.bytes).unwrap();
    let (_, _, _, duration) = video::probe_file(&path).unwrap();
    assert!((duration - 1.5).abs() < 0.2, "{duration}");
    let joined = join_videos(&[red, blue]).unwrap();
    let path = dir.path().join("joined.mp4");
    std::fs::write(&path, &joined.bytes).unwrap();
    let (w, h, _, duration) = video::probe_file(&path).unwrap();
    assert_eq!((w, h), (320, 240));
    assert!((duration - 6.0).abs() < 0.3, "{duration}");
  }
}
