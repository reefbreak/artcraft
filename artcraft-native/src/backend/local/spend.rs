//! The spend meter's ledger: one entry per billed call, saved in `~/Artcraft/local/spend.json`.
//!
//! Each entry is what the app itself spent on the user's accounts, priced at list rates (see
//! [`super::pricing`]), so the totals update the moment a call returns. They're an estimate of
//! this app's usage, not the cloud bill, which also counts anything else on those accounts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Datelike, Local, Utc};
use log::warn;
use serde::{Deserialize, Serialize};

const FILENAME: &str = "spend.json";

/// Which account a charge lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Account {
  Jev,
  Vertex,
  Bedrock,
}

impl Account {
  pub const ALL: [Account; 3] = [Account::Vertex, Account::Bedrock, Account::Jev];

  pub fn label(self) -> &'static str {
    match self {
      Account::Jev => "Jev (TypeSafe)",
      Account::Vertex => "Google Cloud",
      Account::Bedrock => "AWS Bedrock",
    }
  }
}

/// One billed call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpendEntry {
  pub at: DateTime<Utc>,
  pub account: Account,
  pub usd: f64,
  /// What was billed: an upstream model id, or `jev`.
  pub item: String,
  #[serde(default)]
  pub maybe_batch_token: Option<String>,
}

/// Totals per account.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
  pub jev: f64,
  pub vertex: f64,
  pub bedrock: f64,
}

impl Totals {
  pub fn total(&self) -> f64 {
    self.jev + self.vertex + self.bedrock
  }

  pub fn of(&self, account: Account) -> f64 {
    match account {
      Account::Jev => self.jev,
      Account::Vertex => self.vertex,
      Account::Bedrock => self.bedrock,
    }
  }

  fn add(&mut self, account: Account, usd: f64) {
    match account {
      Account::Jev => self.jev += usd,
      Account::Vertex => self.vertex += usd,
      Account::Bedrock => self.bedrock += usd,
    }
  }
}

/// What the top bar shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpendSummary {
  pub today: Totals,
  pub month: Totals,
  pub all_time: Totals,
  pub generations_today: u32,
  /// The most recent charge: what and how much (a generation's provider and routing combined).
  pub last: Option<(String, f64)>,
}

#[derive(Clone)]
pub struct SpendLedger {
  path: Arc<PathBuf>,
  entries: Arc<Mutex<Vec<SpendEntry>>>,
}

impl SpendLedger {
  pub fn load(library_dir: &Path) -> Self {
    let path = library_dir.join(FILENAME);
    let entries = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).map_err(|err| warn!("Ignoring unreadable {}: {err}", path.display())).ok()).unwrap_or_default();
    Self { path: Arc::new(path), entries: Arc::new(Mutex::new(entries)) }
  }

  /// Adds charges (zero-dollar ones are skipped) and saves.
  pub fn record(&self, new: Vec<SpendEntry>) {
    let new: Vec<SpendEntry> = new.into_iter().filter(|e| e.usd > 0.0).collect();
    if new.is_empty() {
      return;
    }
    let Ok(mut entries) = self.entries.lock() else {
      return;
    };
    entries.extend(new);
    if let Err(err) = save(&self.path, &entries) {
      warn!("Couldn't save {}: {err}", self.path.display());
    }
  }

  pub fn summary(&self) -> SpendSummary {
    let entries = self.entries.lock().map(|e| e.clone()).unwrap_or_default();
    summarize(&entries, Local::now())
  }

  pub fn path(&self) -> &Path {
    &self.path
  }
}

/// Today and this month in the user's local time.
fn summarize(entries: &[SpendEntry], now: DateTime<Local>) -> SpendSummary {
  let mut summary = SpendSummary::default();
  let mut batches_today: Vec<&str> = Vec::new();
  for entry in entries {
    let local = entry.at.with_timezone(&Local);
    summary.all_time.add(entry.account, entry.usd);
    if local.year() == now.year() && local.month() == now.month() {
      summary.month.add(entry.account, entry.usd);
      if local.day() == now.day() {
        summary.today.add(entry.account, entry.usd);
        if let Some(batch) = entry.maybe_batch_token.as_deref().filter(|b| !batches_today.contains(b)) {
          batches_today.push(batch);
        }
      }
    }
  }
  summary.generations_today = batches_today.len() as u32;
  if let Some(last) = entries.last() {
    // A generation's charges share a batch: show them as one.
    let usd = match &last.maybe_batch_token {
      Some(batch) => entries.iter().filter(|e| e.maybe_batch_token.as_ref() == Some(batch)).map(|e| e.usd).sum(),
      None => last.usd,
    };
    summary.last = Some((last.item.clone(), usd));
  }
  summary
}

fn save(path: &Path, entries: &[SpendEntry]) -> anyhow::Result<()> {
  let dir = path.parent().ok_or_else(|| anyhow::anyhow!("no library directory"))?;
  std::fs::create_dir_all(dir)?;
  let mut file = tempfile::NamedTempFile::new_in(dir)?;
  file.write_all(serde_json::to_string(entries)?.as_bytes())?;
  file.persist(path)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use chrono::TimeZone;

  use super::*;

  #[test]
  fn totals_split_by_day_month_and_account() {
    let now = Local.with_ymd_and_hms(2026, 10, 12, 15, 0, 0).unwrap();
    let at = |d: u32, h: u32| Local.with_ymd_and_hms(2026, 10, d, h, 0, 0).unwrap().with_timezone(&Utc);
    let entries = vec![entry(Local.with_ymd_and_hms(2026, 9, 30, 9, 0, 0).unwrap().with_timezone(&Utc), Account::Vertex, 5.0, None), entry(at(1, 9), Account::Vertex, 1.00, Some("b0")), entry(at(12, 9), Account::Vertex, 0.134, Some("b1")), entry(at(12, 9), Account::Jev, 0.00002, Some("b1")), entry(at(12, 10), Account::Bedrock, 0.14, Some("b2"))];
    let s = summarize(&entries, now);
    assert!((s.today.total() - 0.27402).abs() < 1e-9);
    assert!((s.month.vertex - 1.134).abs() < 1e-9);
    assert!((s.all_time.total() - 6.27402).abs() < 1e-9);
    assert_eq!(s.generations_today, 2);
    assert_eq!(s.last.as_ref().map(|(item, _)| item.as_str()), Some("stability"));
  }

  #[test]
  fn charges_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("artcraft-spend-{}", uuid::Uuid::new_v4()));
    let ledger = SpendLedger::load(&dir);
    ledger.record(vec![entry(Utc::now(), Account::Vertex, 0.067, Some("b")), entry(Utc::now(), Account::Jev, 0.0, Some("b"))]);
    let reloaded = SpendLedger::load(&dir);
    assert!((reloaded.summary().today.vertex - 0.067).abs() < 1e-9);
    assert_eq!(reloaded.entries.lock().unwrap().len(), 1, "zero-dollar charges are skipped");
    let _ = std::fs::remove_dir_all(dir);
  }

  fn entry(at: DateTime<Utc>, account: Account, usd: f64, batch: Option<&str>) -> SpendEntry {
    let item = if account == Account::Bedrock { "stability" } else { "gemini" };
    SpendEntry { at, account, usd, item: item.to_owned(), maybe_batch_token: batch.map(str::to_owned) }
  }
}
