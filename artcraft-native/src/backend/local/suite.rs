//! Hand-offs between ArtCraft and the other Craft apps (FilmCraft, PhotoCraft, …).
//!
//! Out: "Open in <App>" sends library files to an app. When the app is already running on its
//! suite control port, the file goes over that channel (JSON lines on 127.0.0.1); otherwise the
//! app is started with the file on its command line and with that control port, so the next
//! hand-off reaches the same window.
//!
//! In: the apps' "Send to ArtCraft" commands write files into `~/Artcraft/inbox/`, which ArtCraft
//! polls. Each new file is added to the library and moved to `inbox/imported/`. Senders write
//! `<name>.part` and rename it when done, and name files `<App> - <name>.<ext>` so ArtCraft can
//! say where they came from.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use log::{info, warn};
use serde_json::{Value, json};

use super::library::MediaClass;

/// The inbox, under the data root (`~/Artcraft`).
pub const INBOX_DIRECTORY: &str = "inbox";
const IMPORTED_DIRECTORY: &str = "imported";
/// Where PhotoCraft's control token lives, under the data root.
const PHOTOCRAFT_TOKEN_FILE: &str = "settings/photocraft_control.token";
/// A file younger than this may still be being written.
const INBOX_SETTLE: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
/// Opening a large file can take a while before the app answers.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// Unfinished files in the inbox.
const PARTIAL_SUFFIXES: [&str; 3] = ["part", "tmp", "crdownload"];
/// How long a just-started app gets to open its control port.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(45);
const STARTUP_POLL: Duration = Duration::from_millis(500);

/// An app ArtCraft can hand files to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SuiteApp {
  FilmCraft,
  EffectCraft,
  PhotoCraft,
  LightCraft,
  VectorCraft,
  DesignCraft,
  DeckCraft,
  GridCraft,
  PrintCraft,
}

/// How ArtCraft finds the apps: settings, the suite checkout, installed apps, then `PATH`.
#[derive(Clone, Debug, Default)]
pub struct SuiteLocator {
  /// The craft-suite checkout (settings or `CRAFT_SUITE_DIR`, else found from ArtCraft's own
  /// location).
  pub suite_dir: Option<PathBuf>,
  /// Binaries named in settings, by binary name.
  pub overrides: HashMap<String, String>,
}

impl SuiteApp {
  pub const ALL: [SuiteApp; 9] = [
    SuiteApp::FilmCraft,
    SuiteApp::EffectCraft,
    SuiteApp::PhotoCraft,
    SuiteApp::LightCraft,
    SuiteApp::VectorCraft,
    SuiteApp::DesignCraft,
    SuiteApp::DeckCraft,
    SuiteApp::GridCraft,
    SuiteApp::PrintCraft,
  ];

  /// The apps that take a file of this class, most useful first.
  pub fn for_class(class: MediaClass) -> Vec<SuiteApp> {
    Self::ALL.into_iter().filter(|app| app.accepts(class)).collect()
  }

  pub fn name(self) -> &'static str {
    match self {
      SuiteApp::FilmCraft => "FilmCraft",
      SuiteApp::EffectCraft => "EffectCraft",
      SuiteApp::PhotoCraft => "PhotoCraft",
      SuiteApp::LightCraft => "LightCraft",
      SuiteApp::VectorCraft => "VectorCraft",
      SuiteApp::DesignCraft => "DesignCraft",
      SuiteApp::DeckCraft => "DeckCraft",
      SuiteApp::GridCraft => "GridCraft",
      SuiteApp::PrintCraft => "PrintCraft",
    }
  }

  /// What the app does with the file, for the button's hover text.
  pub fn purpose(self) -> &'static str {
    match self {
      SuiteApp::FilmCraft => "Import into FilmCraft's project to cut into a film",
      SuiteApp::EffectCraft => "Import into EffectCraft for compositing and motion graphics",
      SuiteApp::PhotoCraft => "Open in PhotoCraft to retouch with layers",
      SuiteApp::LightCraft => "Add to LightCraft's catalog to grade and organise",
      SuiteApp::VectorCraft => "Place in VectorCraft (trace, or build a layout around it)",
      SuiteApp::DesignCraft => "Place in a DesignCraft page layout",
      SuiteApp::DeckCraft => "Insert on the current slide in DeckCraft",
      SuiteApp::GridCraft => "Insert as a picture in the open GridCraft workbook",
      SuiteApp::PrintCraft => "Make a PDF from the image in PrintCraft",
    }
  }

  pub fn accepts(self, class: MediaClass) -> bool {
    match self {
      SuiteApp::FilmCraft | SuiteApp::EffectCraft => true,
      _ => class == MediaClass::Image,
    }
  }

  /// The executable's name (PrintCraft's is `pdfcraft`).
  pub fn binary(self) -> &'static str {
    match self {
      SuiteApp::FilmCraft => "filmcraft",
      SuiteApp::EffectCraft => "effectcraft",
      SuiteApp::PhotoCraft => "photocraft",
      SuiteApp::LightCraft => "lightcraft",
      SuiteApp::VectorCraft => "vectorcraft",
      SuiteApp::DesignCraft => "designcraft",
      SuiteApp::DeckCraft => "deckcraft",
      SuiteApp::GridCraft => "gridcraft",
      SuiteApp::PrintCraft => "pdfcraft",
    }
  }

  /// The app's folder in the suite checkout.
  fn folder(self) -> &'static str {
    match self {
      SuiteApp::PrintCraft => "printcraft",
      _ => self.binary(),
    }
  }

  /// The macOS bundle's name.
  fn bundle(self) -> &'static str {
    match self {
      SuiteApp::PrintCraft => "PdfCraft",
      _ => self.name(),
    }
  }

  /// The control port ArtCraft starts the app with; away from each app's documented default so
  /// a session the user started by hand for an agent isn't disturbed. PrintCraft's control
  /// channel works differently and isn't used: it is started afresh each time.
  fn port(self) -> Option<u16> {
    match self {
      SuiteApp::FilmCraft => Some(47_861),
      SuiteApp::EffectCraft => Some(47_862),
      SuiteApp::PhotoCraft => Some(47_863),
      SuiteApp::LightCraft => Some(47_864),
      SuiteApp::VectorCraft => Some(47_865),
      SuiteApp::DesignCraft => Some(47_866),
      SuiteApp::DeckCraft => Some(47_867),
      SuiteApp::GridCraft => Some(47_868),
      SuiteApp::PrintCraft => None,
    }
  }

  /// Whether the app opens the files named on its command line. DeckCraft and GridCraft open
  /// presentations and workbooks that way, not images, so they are started empty and sent the
  /// picture once their control port is up.
  fn opens_files_on_launch(self) -> bool {
    !matches!(self, SuiteApp::DeckCraft | SuiteApp::GridCraft)
  }
}

impl SuiteLocator {
  pub fn new(suite_dir: Option<&str>, overrides: &HashMap<String, String>) -> Self {
    let suite_dir = suite_dir.map(PathBuf::from).or_else(|| std::env::current_exe().ok().and_then(|exe| find_suite_root(&exe)));
    Self { suite_dir, overrides: overrides.clone() }
  }

  /// Every place the app's executable is looked for, in order.
  pub fn candidates(&self, app: SuiteApp) -> Vec<PathBuf> {
    let bin = app.binary();
    let exe = format!("{bin}{}", std::env::consts::EXE_SUFFIX);
    let mut out = Vec::new();
    if let Some(path) = self.overrides.get(bin) {
      out.push(PathBuf::from(path));
    }
    if let Some(root) = &self.suite_dir {
      for profile in ["release", "debug"] {
        out.push(root.join(app.folder()).join("target").join(profile).join(&exe));
      }
    }
    if cfg!(target_os = "macos") {
      let bundle = format!("{}.app/Contents/MacOS/{bin}", app.bundle());
      out.push(Path::new("/Applications").join(&bundle));
      if let Some(home) = directories::UserDirs::new() {
        out.push(home.home_dir().join("Applications").join(&bundle));
      }
    }
    if let Some(paths) = std::env::var_os("PATH") {
      out.extend(std::env::split_paths(&paths).map(|dir| dir.join(&exe)));
    }
    out
  }

  pub fn find(&self, app: SuiteApp) -> Option<PathBuf> {
    self.candidates(app).into_iter().find(|p| p.is_file())
  }

  /// What to tell the user when the app can't be found.
  fn not_found(&self, app: SuiteApp) -> String {
    match &self.suite_dir {
      Some(root) => format!("{} isn't built yet. Build it with: cd \"{}\" && cargo build --release -p {}", app.name(), root.join(app.folder()).display(), app.binary()),
      None => format!("Couldn't find {}. Set \"suite_dir\" (your craft-suite folder) in local_providers.json, or install the app.", app.name()),
    }
  }
}

/// Sends `files` to `app`: over its control channel when it's running, else by starting it.
/// Blocking (connects and waits for the reply); call from a blocking task. Returns the message
/// for the user.
pub fn open_in(app: SuiteApp, files: &[PathBuf], locator: &SuiteLocator, data_root: &Path) -> Result<String, String> {
  let files: Vec<PathBuf> = files.iter().filter(|f| f.is_file()).cloned().collect();
  if files.is_empty() {
    return Err("That file isn't in the library folder any more.".to_owned());
  }
  let token_file = data_root.join(PHOTOCRAFT_TOKEN_FILE);
  if let Some(port) = app.port() {
    if let Some(stream) = connect(port) {
      return send(app, stream, &files, &token_file).map(|()| format!("Sent to {}", app.name()));
    }
  }
  let binary = locator.find(app).ok_or_else(|| locator.not_found(app))?;
  if app == SuiteApp::PhotoCraft {
    if let Some(dir) = token_file.parent() {
      let _ = std::fs::create_dir_all(dir);
    }
  }
  let args = launch_args(app, &files, &token_file);
  info!("Starting {} {:?}", binary.display(), args);
  Command::new(&binary).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|e| format!("Couldn't start {}: {e}", app.name()))?;
  if app.opens_files_on_launch() {
    return Ok(format!("Opening in {}\u{2026}", app.name()));
  }
  let port = app.port().unwrap_or_default();
  let stream = wait_for(port, STARTUP_TIMEOUT).ok_or_else(|| format!("{} started but didn't open its control port. Try again once it's up.", app.name()))?;
  send(app, stream, &files, &token_file).map(|()| format!("Sent to {}", app.name()))
}

/// The command line that starts the app with the files open and its control channel on.
fn launch_args(app: SuiteApp, files: &[PathBuf], token_file: &Path) -> Vec<OsString> {
  let mut args: Vec<OsString> = Vec::new();
  if let Some(port) = app.port() {
    args.extend(["--control".into(), port.to_string().into()]);
  }
  // The control channel reads only inside its read root: the library's media folder.
  if matches!(app, SuiteApp::PhotoCraft | SuiteApp::VectorCraft) {
    if let Some(dir) = files[0].parent() {
      args.extend(["--automation-read-root".into(), dir.as_os_str().to_owned()]);
    }
  }
  match app {
    SuiteApp::PhotoCraft => args.extend(["--control-token-file".into(), token_file.as_os_str().to_owned()]),
    SuiteApp::PrintCraft => args.push("--create-images".into()),
    _ => {},
  }
  if app.opens_files_on_launch() {
    args.extend(files.iter().map(|f| f.as_os_str().to_owned()));
  }
  args
}

/// The requests that open the files in a running app. Several are tried in order for one file
/// when the first may not apply (placing needs an open document).
fn requests(app: SuiteApp, files: &[PathBuf]) -> Vec<Vec<Value>> {
  let paths: Vec<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
  let execute = |command: &str, params: Value| json!({"method": "engine.execute", "params": {"command": command, "params": params}});
  match app {
    SuiteApp::FilmCraft | SuiteApp::EffectCraft => vec![vec![execute("file.import", json!({"paths": paths}))]],
    SuiteApp::LightCraft => vec![vec![execute("library.import", json!({"paths": paths, "mode": "add"}))]],
    // Relative to the read root (the media folder).
    SuiteApp::PhotoCraft => files.iter().map(|f| vec![json!({"method": "app.open", "params": {"path": f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()}})]).collect(),
    SuiteApp::VectorCraft | SuiteApp::DesignCraft => paths.iter().map(|p| vec![execute("file.place", json!({"path": p})), json!({"method": "app.open", "params": {"path": p}})]).collect(),
    // A picture each, on the current slide / at the selected cell.
    SuiteApp::DeckCraft | SuiteApp::GridCraft => paths.iter().map(|p| vec![execute("insert.picture", json!({"path": p}))]).collect(),
    SuiteApp::PrintCraft => Vec::new(),
  }
}

fn connect(port: u16) -> Option<TcpStream> {
  let stream = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), CONNECT_TIMEOUT).ok()?;
  stream.set_read_timeout(Some(REPLY_TIMEOUT)).ok()?;
  stream.set_write_timeout(Some(REPLY_TIMEOUT)).ok()?;
  Some(stream)
}

/// Connects to a just-started app, retrying until its control port opens.
fn wait_for(port: u16, timeout: Duration) -> Option<TcpStream> {
  let deadline = std::time::Instant::now() + timeout;
  loop {
    if let Some(stream) = connect(port) {
      return Some(stream);
    }
    if std::time::Instant::now() >= deadline {
      return None;
    }
    std::thread::sleep(STARTUP_POLL);
  }
}

fn send(app: SuiteApp, stream: TcpStream, files: &[PathBuf], token_file: &Path) -> Result<(), String> {
  let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
  let mut reader = BufReader::new(stream);
  let mut next_id = 0u64;
  let mut call = |mut request: Value| -> Result<Value, String> {
    next_id += 1;
    request["id"] = json!(next_id);
    writeln!(writer, "{request}").map_err(|e| format!("{} stopped listening: {e}", app.name()))?;
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("{} didn't answer: {e}", app.name()))?;
    let reply: Value = serde_json::from_str(&line).map_err(|_| format!("{} gave an unexpected reply", app.name()))?;
    if reply.get("ok").and_then(Value::as_bool) == Some(true) {
      Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    } else {
      Err(reply.get("error").and_then(Value::as_str).unwrap_or("refused").to_owned())
    }
  };
  if app == SuiteApp::PhotoCraft {
    let token = std::fs::read_to_string(token_file).map_err(|_| "PhotoCraft is running without ArtCraft's control token. Quit it and try again.".to_owned())?;
    call(json!({"method": "auth", "params": {"token": token.trim()}})).map_err(|_| "PhotoCraft didn't accept ArtCraft's control token. Quit it and try again.".to_owned())?;
  }
  for attempts in requests(app, files) {
    let mut last = Ok(Value::Null);
    for request in attempts {
      last = call(request);
      if last.is_ok() {
        break;
      }
    }
    last.map_err(|e| format!("{}: {e}", app.name()))?;
  }
  Ok(())
}

/// The suite checkout containing `exe`: the nearest ancestor with ArtCraft and FilmCraft side by
/// side (`<suite>/artcraft/target/release/artcraft-native` → `<suite>`).
fn find_suite_root(exe: &Path) -> Option<PathBuf> {
  exe.ancestors().skip(1).find(|dir| dir.join("artcraft").is_dir() && dir.join("filmcraft").is_dir()).map(Path::to_path_buf)
}

// --- Inbox -------------------------------------------------------------------------------------

/// A file that arrived in the inbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxFile {
  pub path: PathBuf,
  pub extension: String,
  /// The sending app, from a `<App> - <name>` file name.
  pub from: Option<String>,
}

/// Files in the inbox that are complete: not hidden, not partial, not changed in the last moments.
pub fn inbox_ready(inbox: &Path, now: SystemTime) -> Vec<InboxFile> {
  let Ok(entries) = std::fs::read_dir(inbox) else {
    return Vec::new();
  };
  let mut files: Vec<InboxFile> = entries
    .flatten()
    .filter_map(|entry| {
      let path = entry.path();
      let meta = entry.metadata().ok().filter(|m| m.is_file())?;
      let name = path.file_name()?.to_str()?;
      let extension = path.extension()?.to_str()?.to_ascii_lowercase();
      if name.starts_with('.') || PARTIAL_SUFFIXES.contains(&extension.as_str()) {
        return None;
      }
      let settled = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age >= INBOX_SETTLE);
      let from = name.split_once(" - ").map(|(app, _)| app.trim().to_owned()).filter(|a| !a.is_empty());
      settled.then(|| InboxFile { from, extension, path })
    })
    .collect();
  files.sort_by(|a, b| a.path.cmp(&b.path));
  files
}

/// Moves an imported file out of the way (renaming when the name is taken).
pub fn mark_imported(inbox: &Path, file: &Path) {
  let done = inbox.join(IMPORTED_DIRECTORY);
  if let Err(err) = std::fs::create_dir_all(&done) {
    warn!("Couldn't create {}: {err}", done.display());
    return;
  }
  let Some(name) = file.file_name() else { return };
  let mut target = done.join(name);
  let mut n = 1;
  while target.exists() {
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = file.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    target = done.join(format!("{stem} ({n}){ext}"));
    n += 1;
  }
  if let Err(err) = std::fs::rename(file, &target) {
    warn!("Couldn't move {} out of the inbox: {err}", file.display());
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn apps_match_media_classes() {
    assert_eq!(SuiteApp::for_class(MediaClass::Video), vec![SuiteApp::FilmCraft, SuiteApp::EffectCraft]);
    assert_eq!(SuiteApp::for_class(MediaClass::Audio), vec![SuiteApp::FilmCraft, SuiteApp::EffectCraft]);
    assert_eq!(SuiteApp::for_class(MediaClass::Image).len(), 9);
  }

  #[test]
  fn photocraft_starts_confined_to_the_media_folder_with_a_token() {
    let file = PathBuf::from("/lib/media/a b.png");
    let args = launch_args(SuiteApp::PhotoCraft, std::slice::from_ref(&file), Path::new("/t/token"));
    let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(args, ["--control", "47863", "--automation-read-root", "/lib/media", "--control-token-file", "/t/token", "/lib/media/a b.png"]);
    let open = &requests(SuiteApp::PhotoCraft, &[file])[0][0];
    assert_eq!(open["params"]["path"], "a b.png", "PhotoCraft opens relative to its read root");
  }

  #[test]
  fn printcraft_makes_a_pdf_from_the_images() {
    let args = launch_args(SuiteApp::PrintCraft, &[PathBuf::from("/m/a.png")], Path::new("/t"));
    assert_eq!(args, [OsString::from("--create-images"), OsString::from("/m/a.png")]);
  }

  #[test]
  fn deckcraft_and_gridcraft_start_empty_and_get_the_picture_over_the_channel() {
    let file = PathBuf::from("/m/a.png");
    for app in [SuiteApp::DeckCraft, SuiteApp::GridCraft] {
      let args = launch_args(app, std::slice::from_ref(&file), Path::new("/t"));
      assert_eq!(args, [OsString::from("--control"), OsString::from(app.port().unwrap().to_string())], "{app:?}");
      let insert = &requests(app, std::slice::from_ref(&file))[0][0];
      assert_eq!(insert["params"]["command"], "insert.picture");
      assert_eq!(insert["params"]["params"]["path"], "/m/a.png");
    }
  }

  #[test]
  fn the_suite_root_is_found_from_artcraft_s_build() {
    let dir = tempfile::tempdir().unwrap();
    for app in ["artcraft/target/release", "filmcraft"] {
      std::fs::create_dir_all(dir.path().join(app)).unwrap();
    }
    let exe = dir.path().join("artcraft/target/release/artcraft-native");
    assert_eq!(find_suite_root(&exe).as_deref(), Some(dir.path()));
    let locator = SuiteLocator { suite_dir: Some(dir.path().into()), overrides: HashMap::new() };
    assert_eq!(locator.candidates(SuiteApp::PrintCraft)[0], dir.path().join("printcraft/target/release").join(format!("pdfcraft{}", std::env::consts::EXE_SUFFIX)));
  }

  #[test]
  fn a_running_app_gets_the_files_over_its_control_channel() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
      let (stream, _) = listener.accept().unwrap();
      let mut writer = stream.try_clone().unwrap();
      let mut line = String::new();
      BufReader::new(stream).read_line(&mut line).unwrap();
      let request: Value = serde_json::from_str(&line).unwrap();
      writeln!(writer, "{}", json!({"id": request["id"], "ok": true, "result": {}})).unwrap();
      request
    });
    let file = tempfile::NamedTempFile::new().unwrap();
    send(SuiteApp::FilmCraft, connect(port).unwrap(), &[file.path().to_path_buf()], Path::new("/none")).unwrap();
    let request = server.join().unwrap();
    assert_eq!(request["method"], "engine.execute");
    assert_eq!(request["params"]["command"], "file.import");
    assert_eq!(request["params"]["params"]["paths"][0], file.path().to_string_lossy().as_ref());
  }

  #[test]
  fn the_inbox_takes_settled_files_only() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["FilmCraft - shot 3.png", "plain.mp4", "busy.png.part", ".DS_Store"] {
      std::fs::write(dir.path().join(name), b"x").unwrap();
    }
    assert!(inbox_ready(dir.path(), SystemTime::now()).is_empty(), "just written");
    let later = SystemTime::now() + Duration::from_secs(5);
    let ready = inbox_ready(dir.path(), later);
    assert_eq!(ready.len(), 2);
    assert_eq!(ready[0].from.as_deref(), Some("FilmCraft"));
    assert_eq!(ready[1].from, None);
    assert_eq!(ready[1].extension, "mp4");
    mark_imported(dir.path(), &ready[0].path);
    std::fs::write(dir.path().join("FilmCraft - shot 3.png"), b"y").unwrap();
    mark_imported(dir.path(), &dir.path().join("FilmCraft - shot 3.png"));
    assert!(dir.path().join("imported/FilmCraft - shot 3 (1).png").exists());
  }
}
