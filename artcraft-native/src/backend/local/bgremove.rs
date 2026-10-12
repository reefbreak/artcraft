//! Background removal on the user's own machine, free: the ISNet "general use" segmentation model
//! (from the DIS project, packaged by rembg) run with tract, a pure-Rust ONNX runtime, so there is
//! nothing to install. The model (179 MB) downloads once into `~/Artcraft/models` on first use.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use image::{DynamicImage, GenericImageView, ImageBuffer, Luma, Rgba, RgbaImage, imageops::FilterType};
use log::info;
use tokio::sync::OnceCell;
use tract_onnx::prelude::*;

const MODEL_URL: &str = "https://github.com/danielgatis/rembg/releases/download/v0.0.0/isnet-general-use.onnx";
const MODEL_FILE: &str = "isnet-general-use.onnx";
/// The download is ~179 MB; anything much smaller is a failed or truncated download.
const MODEL_MIN_BYTES: u64 = 150_000_000;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// ISNet's fixed input size.
const SIDE: u32 = 1024;

/// A loaded, optimized model (`into_runnable` hands back an `Arc`).
type Plan = Arc<TypedRunnableModel>;

#[derive(Clone)]
pub struct BackgroundRemover {
  dir: Arc<PathBuf>,
  http: reqwest::Client,
  plan: Arc<OnceCell<Plan>>,
}

impl BackgroundRemover {
  pub fn new(data_root: &Path, http: reqwest::Client) -> Self {
    Self { dir: Arc::new(data_root.join("models")), http, plan: Arc::new(OnceCell::new()) }
  }

  /// Whether the model still has to be downloaded (the first run says so before it starts).
  pub fn needs_download(&self) -> bool {
    !model_ready(&self.dir.join(MODEL_FILE))
  }

  /// The image with its background made transparent, as PNG bytes.
  pub async fn remove(&self, image: &[u8]) -> anyhow::Result<Vec<u8>> {
    let plan = self.plan.get_or_try_init(|| self.load()).await?.clone();
    let image = image.to_vec();
    tokio::task::spawn_blocking(move || cut_out(&plan, &image)).await?
  }

  async fn load(&self) -> anyhow::Result<Plan> {
    let path = self.dir.join(MODEL_FILE);
    if !model_ready(&path) {
      self.download(&path).await?;
    }
    tokio::task::spawn_blocking(move || -> anyhow::Result<Plan> {
      let plan = tract_onnx::onnx().model_for_path(&path)?.with_input_fact(0, f32::fact([1, 3, SIDE as usize, SIDE as usize]).into())?.into_optimized()?.into_runnable()?;
      Ok(plan)
    })
    .await?
  }

  async fn download(&self, path: &Path) -> anyhow::Result<()> {
    info!("Downloading the background-removal model to {}", path.display());
    std::fs::create_dir_all(self.dir.as_path())?;
    let response = self.http.get(MODEL_URL).timeout(DOWNLOAD_TIMEOUT).send().await?.error_for_status()?;
    let mut file = tempfile::NamedTempFile::new_in(self.dir.as_path())?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
      file.write_all(&chunk?)?;
    }
    file.persist(path)?;
    anyhow::ensure!(model_ready(path), "The background model download was incomplete. Try again.");
    Ok(())
  }
}

fn model_ready(path: &Path) -> bool {
  std::fs::metadata(path).is_ok_and(|m| m.len() >= MODEL_MIN_BYTES)
}

/// Runs ISNet and uses its mask as the alpha channel, at the original size.
fn cut_out(plan: &Plan, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
  let source = image::load_from_memory(bytes)?;
  let mask = predict_mask(plan, &source)?;
  let (w, h) = source.dimensions();
  let mask = image::imageops::resize(&mask, w, h, FilterType::Triangle);
  let mut out: RgbaImage = source.to_rgba8();
  for (pixel, alpha) in out.pixels_mut().zip(mask.pixels()) {
    let Rgba([r, g, b, a]) = *pixel;
    *pixel = Rgba([r, g, b, ((u16::from(a) * u16::from(alpha.0[0])) / 255) as u8]);
  }
  let mut png = Vec::new();
  DynamicImage::ImageRgba8(out).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
  Ok(png)
}

/// The foreground mask at 1024×1024: rembg's ISNet preprocessing (pixels over the image's max,
/// minus 0.5) and min-max scaling of the first output.
fn predict_mask(plan: &Plan, source: &DynamicImage) -> anyhow::Result<ImageBuffer<Luma<u8>, Vec<u8>>> {
  let rgb = source.resize_exact(SIDE, SIDE, FilterType::Triangle).to_rgb8();
  let max = f32::from(rgb.pixels().flat_map(|p| p.0).max().unwrap_or(255).max(1));
  let input = tract_ndarray::Array4::from_shape_fn((1, 3, SIDE as usize, SIDE as usize), |(_, c, y, x)| f32::from(rgb.get_pixel(x as u32, y as u32)[c]) / max - 0.5);
  let outputs = plan.run(tvec!(Tensor::from(input).into()))?;
  let mask_tensor = outputs[0].clone().into_tensor();
  let view = mask_tensor.to_plain_array_view::<f32>()?;
  let (lo, hi) = view.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
  let span = (hi - lo).max(1e-6);
  let pixels: Vec<u8> = view.iter().take((SIDE * SIDE) as usize).map(|&v| (((v - lo) / span) * 255.0).round().clamp(0.0, 255.0) as u8).collect();
  ImageBuffer::from_raw(SIDE, SIDE, pixels).ok_or_else(|| anyhow::anyhow!("Unexpected mask size from the model"))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Needs the model in `~/Artcraft/models` (or downloads it): `cargo test -- --ignored`.
  #[tokio::test(flavor = "multi_thread")]
  #[ignore]
  async fn cuts_a_subject_out_of_a_plain_background() {
    let root = std::env::var("ARTCRAFT_TEST_DATA_ROOT").map(PathBuf::from).unwrap_or_else(|_| crate::backend::session::data_root());
    let remover = BackgroundRemover::new(&root, reqwest::Client::new());
    // A dark disc on white.
    let mut img = RgbaImage::from_pixel(400, 300, Rgba([255, 255, 255, 255]));
    for (x, y, p) in img.enumerate_pixels_mut() {
      if (x as f32 - 200.0).powi(2) + (y as f32 - 150.0).powi(2) < 90.0f32.powi(2) {
        *p = Rgba([30, 60, 200, 255]);
      }
    }
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    let out = image::load_from_memory(&remover.remove(&png).await.unwrap()).unwrap().to_rgba8();
    assert_eq!(out.dimensions(), (400, 300));
    assert!(out.get_pixel(200, 150)[3] > 200, "the subject stays");
    assert!(out.get_pixel(10, 10)[3] < 60, "the background goes");
  }
}
