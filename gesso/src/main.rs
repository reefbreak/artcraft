#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! ArtCraft, natively: an egui desktop app over the ArtCraft API (no Tauri, no webview).

mod app;
mod backend;
mod feed;
mod models;
mod overlays;
mod pages;
mod prompt_box;
mod shell;
mod theme;
mod ui;

const APP_ID: &str = "us.reefbreak.gesso";

fn main() -> eframe::Result {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
  let mut viewport = egui::ViewportBuilder::default().with_title("Gesso").with_app_id(APP_ID).with_inner_size([1440.0, 900.0]).with_min_inner_size([960.0, 640.0]);
  if let Some(icon) = app_icon() {
    viewport = viewport.with_icon(icon);
  }
  // The app draws its own title bar; macOS keeps its traffic lights over it.
  viewport = if cfg!(target_os = "macos") { viewport.with_fullsize_content_view(true).with_titlebar_shown(false).with_title_shown(false) } else { viewport.with_decorations(false) };
  let options = eframe::NativeOptions { viewport, persist_window: true, ..Default::default() };
  eframe::run_native("Gesso", options, Box::new(|cc| Ok(Box::new(app::ArtcraftApp::new(cc)))))
}

/// The window and taskbar icon.
fn app_icon() -> Option<egui::IconData> {
  const ICON: &[u8] = include_bytes!("../assets/brand/gesso.png");
  let rgba = image::load_from_memory(ICON).ok()?.into_rgba8();
  Some(egui::IconData { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}
