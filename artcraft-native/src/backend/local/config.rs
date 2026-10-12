//! Local mode's settings: which of the user's own cloud accounts to generate with.
//!
//! Read from `~/Artcraft/settings/local_providers.json`, then overridden by environment variables,
//! so either is enough on its own. Nothing here holds a secret except the optional Jev key and
//! Vertex express-mode key; GCP and AWS credentials come from their usual chains (`gcloud auth
//! application-default login`, `~/.aws`, SSO profiles, instance roles).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use log::warn;
use serde::{Deserialize, Serialize};

const FILENAME: &str = "local_providers.json";

const DEFAULT_GCP_LOCATION: &str = "global";
/// Veo's samples all run in us-central1; `global` isn't documented for it.
const DEFAULT_GCP_VIDEO_LOCATION: &str = "us-central1";
/// The Stability models on Bedrock are documented in us-west-2.
const DEFAULT_AWS_REGION: &str = "us-west-2";
const DEFAULT_JEV_MODEL: &str = "jev-1.13.0";
/// Below this, Jev's pick is ignored and the rules decide.
const DEFAULT_JEV_MIN_CONFIDENCE: f32 = 0.35;

/// Local mode's settings, file then environment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
  /// Off only when the user wants the stock ArtCraft account flow back (`ARTCRAFT_REMOTE=1`).
  pub local_mode: bool,

  // --- Google Cloud (Vertex AI) ---
  pub gcp_project: Option<String>,
  /// Gemini image models: `global` (or `us` / `eu`, or a region).
  pub gcp_location: String,
  /// Veo: a region.
  pub gcp_video_location: String,
  /// A Vertex express-mode API key; only Gemini's `generateContent` accepts it (not Veo).
  pub gcp_api_key: Option<String>,
  /// Replaces the Vertex host (a proxy, or a mock server for testing). `ARTCRAFT_VERTEX_BASE_URL`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub vertex_base_url: Option<String>,

  // --- AWS (Bedrock) ---
  /// `None`: on when an AWS credential source is found.
  pub bedrock_enabled: Option<bool>,
  pub aws_region: String,
  pub aws_profile: Option<String>,

  // --- Jev routing ---
  pub jev_api_key: Option<String>,
  pub jev_model: String,
  pub jev_min_confidence: f32,

  // --- Spend meter ---
  /// Price overrides in USD, keyed as documented on `PriceBook` (e.g. `stability.sd3-5-large-v1:0`).
  #[serde(skip_serializing_if = "HashMap::is_empty")]
  pub prices: HashMap<String, f64>,
}

impl Default for LocalConfig {
  fn default() -> Self {
    Self { local_mode: true, gcp_project: None, gcp_location: DEFAULT_GCP_LOCATION.to_owned(), gcp_video_location: DEFAULT_GCP_VIDEO_LOCATION.to_owned(), gcp_api_key: None, vertex_base_url: None, bedrock_enabled: None, aws_region: DEFAULT_AWS_REGION.to_owned(), aws_profile: None, jev_api_key: None, jev_model: DEFAULT_JEV_MODEL.to_owned(), jev_min_confidence: DEFAULT_JEV_MIN_CONFIDENCE, prices: HashMap::new() }
  }
}

impl LocalConfig {
  /// The file (if any), then the environment on top.
  pub fn load(data_root: &Path) -> Self {
    let path = config_path(data_root);
    let mut config = match fs::read_to_string(&path) {
      Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
        warn!("Ignoring unreadable {}: {err}", path.display());
        Self::default()
      }),
      Err(_) => Self::default(),
    };
    config.apply_env(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()));
    config
  }

  /// Writes a starter file the first time, so there's something to edit.
  pub fn write_template_if_missing(data_root: &Path) {
    let path = config_path(data_root);
    if path.exists() {
      return;
    }
    let result = path.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| fs::write(&path, serde_json::to_string_pretty(&Self::default()).unwrap_or_default()));
    if let Err(err) = result {
      warn!("Couldn't write {}: {err}", path.display());
    }
  }

  /// Vertex can be called: a project (for ADC) or an express-mode key.
  pub fn vertex_enabled(&self) -> bool {
    self.gcp_project.is_some() || self.gcp_api_key.is_some()
  }

  /// Veo needs a project: express-mode keys don't cover `predictLongRunning`.
  pub fn veo_enabled(&self) -> bool {
    self.gcp_project.is_some()
  }

  pub fn bedrock_enabled(&self) -> bool {
    self.bedrock_enabled.unwrap_or_else(aws_credentials_present)
  }

  pub fn jev_enabled(&self) -> bool {
    self.jev_api_key.is_some()
  }

  fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) {
    if var("ARTCRAFT_REMOTE").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
      self.local_mode = false;
    }
    if let Some(project) = var("ARTCRAFT_GCP_PROJECT").or_else(|| var("GOOGLE_CLOUD_PROJECT")) {
      self.gcp_project = Some(project);
    }
    if let Some(location) = var("ARTCRAFT_GCP_LOCATION") {
      self.gcp_location = location;
    }
    if let Some(location) = var("ARTCRAFT_GCP_VIDEO_LOCATION") {
      self.gcp_video_location = location;
    }
    if let Some(key) = var("ARTCRAFT_GCP_API_KEY") {
      self.gcp_api_key = Some(key);
    }
    if let Some(url) = var("ARTCRAFT_VERTEX_BASE_URL") {
      self.vertex_base_url = Some(if url.ends_with('/') { url } else { format!("{url}/") });
    }
    if let Some(region) = var("ARTCRAFT_AWS_REGION") {
      self.aws_region = region;
    }
    if let Some(profile) = var("AWS_PROFILE") {
      self.aws_profile.get_or_insert(profile);
    }
    if let Some(key) = var("ARTCRAFT_JEV_API_KEY").or_else(|| var("TYPESAFE_API_KEY")) {
      self.jev_api_key = Some(key);
    }
  }
}

pub fn config_path(data_root: &Path) -> PathBuf {
  data_root.join("settings").join(FILENAME)
}

/// Any of the AWS SDK's usual credential sources.
fn aws_credentials_present() -> bool {
  let env = ["AWS_ACCESS_KEY_ID", "AWS_PROFILE", "AWS_BEARER_TOKEN_BEDROCK", "AWS_CONTAINER_CREDENTIALS_FULL_URI", "AWS_WEB_IDENTITY_TOKEN_FILE"];
  if env.iter().any(|name| std::env::var_os(name).is_some()) {
    return true;
  }
  directories::UserDirs::new().is_some_and(|d| {
    let aws = d.home_dir().join(".aws");
    aws.join("credentials").exists() || aws.join("config").exists()
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn environment_overrides_the_file() {
    let mut config = LocalConfig { gcp_project: Some("from-file".into()), ..Default::default() };
    config.apply_env(|name| match name {
      "GOOGLE_CLOUD_PROJECT" => Some("from-env".into()),
      "TYPESAFE_API_KEY" => Some("ts_key".into()),
      _ => None,
    });
    assert_eq!(config.gcp_project.as_deref(), Some("from-env"));
    assert!(config.jev_enabled());
    assert!(config.local_mode);
  }

  #[test]
  fn remote_mode_is_opt_in() {
    let mut config = LocalConfig::default();
    config.apply_env(|name| (name == "ARTCRAFT_REMOTE").then(|| "1".to_owned()));
    assert!(!config.local_mode);
  }

  #[test]
  fn veo_needs_a_project_not_just_a_key() {
    let config = LocalConfig { gcp_api_key: Some("k".into()), ..Default::default() };
    assert!(config.vertex_enabled());
    assert!(!config.veo_enabled());
  }

  #[test]
  fn a_partial_file_keeps_the_defaults() {
    let config: LocalConfig = serde_json::from_str(r#"{ "gcp_project": "p" }"#).unwrap();
    assert_eq!((config.gcp_location.as_str(), config.aws_region.as_str()), ("global", "us-west-2"));
  }
}
