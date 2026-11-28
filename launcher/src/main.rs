//! Rox-Box: one window over `victory.exe` and `play.ps1`.
//!
//! It starts what a person would type and shows what comes back: nothing it
//! does is its own way of running a title. It knows no title: a title is a
//! folder the person names. It is portable: it runs from the unzipped
//! release folder (or the source tree) with nothing to set up, and writes
//! only `victory.toml`'s `game_dir` line and its own files under `local\`.
//!
//! `roxbox-launcher.exe --probe` prints what it found and opens no window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backup;
mod config;
mod paths;
mod runner;
mod switches;
mod system;
mod theme;
mod update;

use std::collections::VecDeque;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use config::Library;
use paths::{Home, Layout};
use runner::{Event, Job, Kind, Spec};
use switches::Book;
use system::{CheckLine, Host, Level};
use theme::Mark;
use update::Release;

/// The releases page a person is sent to when an update cannot be done here.
pub const RELEASES_PAGE: &str = "https://github.com/RoxBuilds/Rox-box/releases";
/// Where the newest release is asked for.
pub const RELEASE_API: &str = "https://api.github.com/repos/RoxBuilds/Rox-box/releases/latest";

/// The console drawer keeps this many lines.
const CONSOLE_LINES: usize = 5000;
/// How long a toast stays.
const TOAST: Duration = Duration::from_millis(4500);
/// No console window for a child of a windowed program.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How the last thing a person started ended (what runs now is the job itself).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Idle,
    /// What it was, and its exit code (none: it was stopped).
    Exited(Kind, Option<i32>),
}

/// The pages of the rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Play,
    Diagnostics,
    Switches,
    Settings,
}

/// Where an update is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Updating {
    No,
    Downloading,
    Unpacking,
    Failed(String),
}

/// What a folder dialog was opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    Title,
    BackupDir,
    SaveDir,
}

/// A notice that comes and goes.
struct Toast {
    born: Instant,
    mark: Mark,
    words: String,
}

/// The value of `key` in `victory config`'s output (`cache_dir   : <path>`).
fn config_value(output: &str, key: &str) -> Option<PathBuf> {
    output.lines().find_map(|l| {
        let (k, value) = l.split_once(':')?;
        (k.trim() == key && !value.trim().is_empty()).then(|| PathBuf::from(value.trim()))
    })
}

/// The files of the pipeline cache that a stale or damaged cache is made of:
/// the host pipeline library and the warm-up's marks. What earlier runs
/// recorded (the lists `victory warm --used` works from) stays, so the next
/// warm-up makes everything again.
const PIPELINE_CACHE_FILES: [&str; 4] = ["host-pipelines.bin", "warm-stamp.txt", "warm-done.txt", "warm-bad.txt"];

/// Removes [`PIPELINE_CACHE_FILES`] from `<cache_dir>\pipelines`. Says how
/// many files and bytes went.
fn reset_pipeline_cache(cache_dir: &Path) -> std::io::Result<(usize, u64)> {
    let dir = cache_dir.join("pipelines");
    let (mut files, mut bytes) = (0, 0);
    for name in PIPELINE_CACHE_FILES {
        let file = dir.join(name);
        match std::fs::metadata(&file) {
            Ok(meta) if meta.is_file() => {
                std::fs::remove_file(&file)?;
                files += 1;
                bytes += meta.len();
            }
            _ => {}
        }
    }
    Ok((files, bytes))
}

/// Whether `victory.toml` is written now: by itself in a release folder, and
/// only when it was asked for in a source tree.
fn writes_config(layout: Option<Layout>, asked: bool) -> bool {
    match layout {
        Some(Layout::Release) => true,
        Some(Layout::Source) => asked,
        None => false,
    }
}

/// `m:ss` of a number of seconds.
fn clock(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// `path` cut in its middle to at most `most` characters, both ends kept.
fn elide(path: &str, most: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    let most = most.max(12);
    if chars.len() <= most {
        return path.to_string();
    }
    let head = most / 3;
    let tail = most - head - 1;
    chars[..head].iter().chain(std::iter::once(&'\u{2026}')).chain(&chars[chars.len() - tail..]).collect()
}

/// The last part of a folder's path: what a title is called here.
fn title_name(folder: &str) -> String {
    Path::new(folder).file_name().map_or_else(|| folder.to_string(), |n| n.to_string_lossy().into_owned())
}

/// A size in bytes as megabytes with one decimal.
fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

struct App {
    home: Option<Home>,
    lib: Library,
    saved: Library,
    host: Host,
    job: Option<Job>,
    queue: VecDeque<Spec>,
    status: Status,
    console: VecDeque<String>,
    console_open: bool,
    checks: Vec<CheckLine>,
    check_summary: Option<String>,
    selftest: Option<bool>,
    version: Option<String>,
    /// The newest release, once asked for, or why there is no answer.
    release: Option<Result<Release, String>>,
    /// A check a person asked for says so when there is nothing newer.
    asked_for_release: bool,
    banner_hidden: bool,
    updating: Updating,
    /// The zip a backup is being written to.
    backing_up: Option<PathBuf>,
    toasts: Vec<Toast>,
    note: String,
    picking: Option<(Pick, Receiver<Option<PathBuf>>)>,
    confirm_reset: bool,
    page: Page,
    /// The look that is applied (high contrast, reduced motion).
    look: (bool, bool),
    /// The scale the slider shows while it is dragged, in percent.
    scale_shown: f32,
    /// The saved switches, and what of them is on disk.
    book: Book,
    book_saved: Book,
    /// What the Switches page's fields hold while they are typed in.
    paste: String,
    new_switch: (String, String),
    profile_name: String,
    confirm_delete: bool,
}

impl App {
    fn new(ctx: &egui::Context) -> App {
        let home = Home::find();
        let mut lib = home.as_ref().map(|h| Library::load(&h.library_file())).unwrap_or_default();
        // A victory.toml that already names a folder: it is the first title.
        if lib.titles.is_empty()
            && let Some(home) = &home
            && let Ok(text) = std::fs::read_to_string(home.config_file())
            && let Some(dir) = config::game_dir_of(&text)
        {
            lib.add(&dir);
        }
        theme::apply(ctx, lib.high_contrast, lib.reduced_motion);
        ctx.set_zoom_factor(lib.scale as f32 / 100.0);
        let book = home.as_ref().map(|h| Book::load(&h.switches_file())).unwrap_or_default();
        let mut app = App {
            book_saved: book.clone(),
            profile_name: book.active.clone().unwrap_or_default(),
            book,
            paste: String::new(),
            new_switch: Default::default(),
            confirm_delete: false,
            saved: lib.clone(),
            look: (lib.high_contrast, lib.reduced_motion),
            scale_shown: lib.scale as f32,
            lib,
            home,
            host: Host::read(),
            job: None,
            queue: VecDeque::new(),
            status: Status::Idle,
            console: VecDeque::new(),
            console_open: false,
            checks: Vec::new(),
            check_summary: None,
            selftest: None,
            version: None,
            release: None,
            asked_for_release: false,
            banner_hidden: false,
            updating: Updating::No,
            backing_up: None,
            toasts: Vec::new(),
            note: String::new(),
            picking: None,
            confirm_reset: false,
            // `ROXBOX_PAGE` opens on another page (for pictures of the launcher).
            page: match std::env::var("ROXBOX_PAGE").as_deref() {
                Ok("diagnostics") => Page::Diagnostics,
                Ok("switches") => Page::Switches,
                Ok("settings") => Page::Settings,
                _ => Page::Play,
            },
        };
        match &app.home {
            None => app.toast(Mark::Err, "Rox-Box's engine was not found: put roxbox-launcher.exe in the folder with victory.exe and play.ps1."),
            Some(home) => {
                // What an update renamed aside while the old program still ran.
                update::clean_up(&home.root);
                app.enqueue_victory(Kind::Version, &["--version"]);
                app.enqueue_victory(Kind::Check, &["check"]);
                if app.lib.auto_update {
                    app.queue.push_back(runner::latest_release());
                }
                app.pump(ctx);
            }
        }
        app
    }

    fn source_tree(&self) -> bool {
        self.home.as_ref().is_some_and(|h| h.layout == Layout::Source)
    }

    fn victory_exe(&self) -> Option<PathBuf> {
        self.home.as_ref()?.victory(&self.lib.build)
    }

    fn busy(&self) -> bool {
        self.job.is_some() || !self.queue.is_empty()
    }

    fn say(&mut self, line: impl Into<String>) {
        if self.console.len() >= CONSOLE_LINES {
            self.console.pop_front();
        }
        self.console.push_back(line.into());
    }

    /// A notice: shown for a few seconds, and kept as the footer's line.
    fn toast(&mut self, mark: Mark, words: impl Into<String>) {
        let words = words.into();
        self.note = words.clone();
        self.toasts.push(Toast { born: Instant::now(), mark, words });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    fn enqueue_victory(&mut self, kind: Kind, args: &[&str]) {
        match self.victory_exe() {
            Some(exe) => self.queue.push_back(runner::victory(kind, &exe, args, self.lib.active.as_deref())),
            None => self.note = "The engine is not built yet: build it first (scripts\\build.ps1 -Release).".to_string(),
        }
    }

    /// What `victory config` prints (it is quick: a file is read, nothing is started).
    fn engine_config(&self) -> String {
        let Some(exe) = self.victory_exe() else { return String::new() };
        let mut command = std::process::Command::new(&exe);
        command.arg("config").creation_flags(CREATE_NO_WINDOW);
        if let Some(title) = &self.lib.active {
            command.env("VICTORY_GAME_DIR", title);
        }
        command.output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    }

    /// Keeps `victory.toml` naming the active title. In a release folder that
    /// happens by itself, when a title is added, chosen or played. In a source
    /// tree the file is a developer's own: it is written only when asked for
    /// (`asked`: PLAY, or the button in Settings), never by choosing a title.
    /// Every other line of the file stays, and a copy is kept when it changes.
    fn sync_config(&mut self, asked: bool) {
        if !writes_config(self.home.as_ref().map(|h| h.layout), asked) {
            return;
        }
        let (Some(home), Some(active)) = (&self.home, &self.lib.active) else { return };
        match config::write_game_dir(&home.config_file(), active) {
            Ok(done) if done.contains("already") => {}
            Ok(done) => self.note = done,
            Err(e) => self.toast(Mark::Err, format!("victory.toml could not be written: {e}")),
        }
    }

    fn launch(&mut self) {
        let Some(home) = &self.home else { return };
        let script = home.play_script();
        let source = self.source_tree();
        self.sync_config(true);
        // What the run is given: the toggles, with the saved switches before the extra text.
        let mut given = self.lib.clone();
        given.more = switches::more_for_run(self.book.active_profile(), &self.lib.more);
        if self.lib.prepare {
            self.queue.push_back(runner::play(&script, &given, source, true));
        }
        let mut run = runner::play(&script, &given, source, false);
        if self.lib.on_play == "close" {
            // The launcher will be gone: no pipes to it, and the log file keeps the output.
            let mut logged = given.clone();
            logged.log = true;
            run = runner::play(&script, &logged, source, false);
            run.detached = true;
        }
        self.queue.push_back(run);
    }

    /// Starts the next queued command when nothing runs.
    fn pump(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        let Some(spec) = self.queue.pop_front() else { return };
        let kind = spec.kind;
        if !kind.quiet() {
            self.say(format!("> {}", spec.shown()));
        }
        let wake = ctx.clone();
        match runner::start(spec, move || wake.request_repaint()) {
            Ok(job) => {
                if kind == Kind::SelfTest {
                    self.selftest = None;
                }
                self.job = Some(job);
                if kind == Kind::Launch {
                    // A window that is not shown draws nothing and holds no frame.
                    match self.lib.on_play.as_str() {
                        "minimize" => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                        "close" => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        _ => {}
                    }
                }
            }
            Err(e) => {
                self.say(format!("could not start: {e}"));
                self.queue.clear();
                match kind {
                    Kind::Latest => self.release = Some(Err("curl.exe could not be started".to_string())),
                    Kind::UpdateChecksum | Kind::UpdatePackage | Kind::UpdateUnpack => self.update_failed(ctx, format!("a step could not be started: {e}")),
                    _ => self.toast(Mark::Err, format!("Could not start {}: {e}", kind.label())),
                }
            }
        }
    }

    /// Reads what the running command said since the last frame.
    fn poll(&mut self, ctx: &egui::Context) {
        let mut ended = None;
        if let Some(job) = &mut self.job {
            let quiet = job.spec.kind.quiet();
            let mut lines = Vec::new();
            while let Ok(event) = job.events.try_recv() {
                match event {
                    Event::Line(line) => {
                        if !quiet {
                            lines.push(line.clone());
                        }
                        job.output.push(line);
                    }
                    Event::Exit(code) => ended = Some(Ok(code)),
                    Event::Failed(e) => ended = Some(Err(e)),
                }
            }
            for line in lines {
                self.say(line);
            }
        }
        if let Some(how) = ended
            && let Some(job) = self.job.take()
        {
            self.finished(job, how, ctx);
        }
        self.pump(ctx);
    }

    fn finished(&mut self, job: Job, how: Result<Option<i32>, String>, ctx: &egui::Context) {
        let kind = job.spec.kind;
        let code = how.clone().unwrap_or(None);
        let ok = code == Some(0);
        if !kind.quiet() {
            let secs = job.started.elapsed().as_secs();
            self.say(match (&how, code) {
                (Err(e), _) => format!("[{}: {e}]", kind.label()),
                (_, Some(c)) => format!("[{} ended with exit code {c} after {secs} s]", kind.label()),
                (_, None) => format!("[{} was stopped after {secs} s]", kind.label()),
            });
        }
        if kind.foreground() {
            self.status = Status::Exited(kind, code);
        }
        match kind {
            Kind::Version => self.version = runner::version_of(&job.output.join(" ")),
            Kind::Check => {
                let (checks, summary) = system::parse_check(&job.output);
                self.checks = checks;
                self.check_summary = summary;
            }
            Kind::Report => {
                if ok && !job.output.is_empty() {
                    ctx.copy_text(job.output.join("\n") + "\n");
                    self.toast(Mark::Ok, format!("Run report copied to the clipboard ({} lines). It holds numbers and Rox-Box's own words only.", job.output.len()));
                } else {
                    self.toast(Mark::Warn, "No run report yet: every run of a title writes one.");
                }
            }
            Kind::Latest => {
                let asked = std::mem::take(&mut self.asked_for_release);
                self.release = Some(update::parse_release(&job.output.join("")).ok_or_else(|| "no answer (offline, or the API's limit)".to_string()));
                self.banner_hidden = false;
                match (&self.release, self.update_available(), asked) {
                    (Some(Ok(release)), true, _) => {
                        let tag = release.tag.clone();
                        self.toast(Mark::Ok, format!("Rox-Box {tag} is available."));
                    }
                    (Some(Ok(_)), false, true) => self.toast(Mark::Ok, "Rox-Box is up to date."),
                    (Some(Err(e)), _, true) => {
                        let e = e.clone();
                        self.toast(Mark::Warn, format!("The newest release could not be asked for: {e}"));
                    }
                    _ => {}
                }
            }
            Kind::SelfTest => {
                self.selftest = Some(ok);
                self.toast(if ok { Mark::Ok } else { Mark::Err }, if ok { "Self-test passed." } else { "Self-test failed: the console says where." });
            }
            Kind::Prepare if !ok => {
                self.queue.retain(|s| s.kind != Kind::Launch);
                self.toast(Mark::Err, "Preparing did not finish: the title was not started. The console says why.");
            }
            Kind::Prepare => {}
            Kind::Launch | Kind::Shaders | Kind::Warm => {
                if kind == Kind::Launch {
                    if self.lib.on_play == "minimize" {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    if self.lib.backup {
                        self.backup_now();
                    }
                } else if ok {
                    self.toast(Mark::Ok, if kind == Kind::Shaders { "Shaders prepared." } else { "Pipelines warmed." });
                }
                // What a run or a preparation changed, the check should show.
                self.enqueue_victory(Kind::Check, &["check"]);
            }
            Kind::Backup => self.backup_finished(ok),
            Kind::UpdateChecksum if ok => {}
            Kind::UpdatePackage if ok => self.update_downloaded(ctx),
            Kind::UpdateUnpack if ok => self.update_unpacked(ctx),
            Kind::UpdateChecksum | Kind::UpdatePackage | Kind::UpdateUnpack => {
                self.update_failed(ctx, format!("{} did not finish", kind.label()));
            }
        }
    }

    fn add_title(&mut self, path: &Path) {
        let Some(folder) = config::title_folder_of(path) else {
            self.toast(Mark::Warn, format!("Not a folder: {}", path.display()));
            return;
        };
        self.lib.add(&folder);
        self.sync_config(false);
        self.toast(Mark::Ok, format!("{} added to the library.", title_name(&folder)));
        self.enqueue_victory(Kind::Check, &["check"]);
    }

    fn activate(&mut self, title: String) {
        // A profile tied to this title is chosen with it.
        if let Some(profile) = self.book.for_title(&title).map(str::to_string) {
            self.set_profile(Some(profile));
        }
        self.lib.active = Some(title);
        self.sync_config(false);
        self.enqueue_victory(Kind::Check, &["check"]);
    }

    fn reset_cache(&mut self) {
        let shown = self.engine_config();
        match config_value(&shown, "cache_dir") {
            None => self.toast(Mark::Err, "The cache folder is not known: `victory config` did not name it."),
            Some(dir) => match reset_pipeline_cache(&dir) {
                Ok((0, _)) => self.toast(Mark::Ok, "The pipeline cache was already empty."),
                Ok((files, bytes)) => self.toast(Mark::Ok, format!("Pipeline cache reset: {files} file(s), {}. Warm cache makes the pipelines again.", megabytes(bytes))),
                Err(e) => self.toast(Mark::Err, format!("The pipeline cache could not be reset: {e}")),
            },
        }
        self.enqueue_victory(Kind::Check, &["check"]);
    }

    // ---- save backups ----------------------------------------------------

    /// Where backups go: the folder named in Settings, else `local\backups`.
    fn backup_dir(&self) -> Option<PathBuf> {
        if !self.lib.backup_dir.trim().is_empty() {
            return Some(PathBuf::from(self.lib.backup_dir.trim()));
        }
        self.home.as_ref().map(|h| h.root.join("local").join("backups"))
    }

    /// What is packed, and whether the person named it: the folder named in
    /// Settings, else what [`backup::default_source`] finds in the engine's data folder.
    fn save_dir(&self) -> Option<(PathBuf, bool)> {
        if !self.lib.save_dir.trim().is_empty() {
            return Some((PathBuf::from(self.lib.save_dir.trim()), true));
        }
        config_value(&self.engine_config(), "data_dir").map(|data| (backup::default_source(&data), false))
    }

    fn backup_now(&mut self) {
        if self.backing_up.is_some() {
            return;
        }
        let (Some((saves, named)), Some(dir)) = (self.save_dir(), self.backup_dir()) else {
            self.toast(Mark::Warn, "No backup: the save folder is not known. Name it in Settings.");
            return;
        };
        if !saves.is_dir() {
            self.toast(Mark::Warn, "No backup: the save folder does not exist yet.");
            return;
        }
        // A folder nobody named is not packed when it is large: the engine's
        // data folder can hold far more than saves (logs, reports, pictures).
        if !named && backup::folder_size(&saves, backup::UNASKED_LIMIT) > backup::UNASKED_LIMIT {
            self.toast(
                Mark::Warn,
                format!("No backup: the engine's data folder holds more than {}, so it is not packed whole. Name your save folder in Settings, Auto-backups.", megabytes(backup::UNASKED_LIMIT)),
            );
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.toast(Mark::Err, format!("The backup folder could not be made: {e}"));
            return;
        }
        let zip = backup::file_for(&dir, &config::now_stamp());
        match backup::pack(&saves, &zip) {
            Some(spec) => {
                self.backing_up = Some(zip);
                self.queue.push_back(spec);
            }
            None => self.toast(Mark::Warn, "No backup: a whole drive cannot be the save folder."),
        }
    }

    fn backup_finished(&mut self, ok: bool) {
        let Some(zip) = self.backing_up.take() else { return };
        if !ok {
            let _ = std::fs::remove_file(&zip);
            self.toast(Mark::Err, "The save backup did not finish. The console says why.");
            return;
        }
        let size = std::fs::metadata(&zip).map_or(0, |m| m.len());
        let removed = zip.parent().map_or(0, |dir| backup::prune(dir, self.lib.backup_keep as usize));
        let name = zip.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let tail = if removed > 0 { format!(" {removed} older removed.") } else { String::new() };
        self.toast(Mark::Ok, format!("Backup created: {name} ({}).{tail}", megabytes(size)));
    }

    // ---- updates ---------------------------------------------------------

    /// Whether the newest release is newer than the engine here.
    fn update_available(&self) -> bool {
        match (&self.release, &self.version) {
            (Some(Ok(release)), Some(local)) => runner::newer(&runner::version_of(&release.tag).unwrap_or_default(), local),
            _ => false,
        }
    }

    /// Whether an update can be done here: a release folder, and a package to take.
    fn can_install(&self) -> bool {
        self.home.as_ref().is_some_and(|h| h.layout == Layout::Release) && matches!(&self.release, Some(Ok(r)) if r.package.is_some())
    }

    fn start_update(&mut self, ctx: &egui::Context) {
        let (Some(home), Some(Ok(release))) = (&self.home, &self.release) else { return };
        let Some((package, _)) = &release.package else { return };
        let stage = update::staging(&home.root, &release.tag);
        let _ = std::fs::remove_dir_all(&stage);
        if let Err(e) = std::fs::create_dir_all(stage.join("unpacked")) {
            self.update_failed(ctx, format!("the staging folder could not be made: {e}"));
            return;
        }
        if let Some(checksum) = &release.checksum {
            self.queue.push_back(runner::download(Kind::UpdateChecksum, checksum, &stage.join("package.zip.sha256")));
        }
        self.queue.push_back(runner::download(Kind::UpdatePackage, package, &stage.join("package.zip")));
        self.updating = Updating::Downloading;
        self.pump(ctx);
    }

    fn stage(&self) -> Option<PathBuf> {
        match (&self.home, &self.release) {
            (Some(home), Some(Ok(release))) => Some(update::staging(&home.root, &release.tag)),
            _ => None,
        }
    }

    /// The package is here: it is checked before anything of it is used.
    fn update_downloaded(&mut self, ctx: &egui::Context) {
        let Some(stage) = self.stage() else { return };
        let checksum = stage.join("package.zip.sha256");
        match update::verify(&stage.join("package.zip"), checksum.is_file().then_some(checksum.as_path())) {
            Ok(checked) => {
                self.say(match checked {
                    Some(sum) => format!("[update: the download matches the release's checksum {}\u{2026}]", &sum[..12]),
                    None => "[update: the release publishes no checksum; nothing to compare with]".to_string(),
                });
                self.updating = Updating::Unpacking;
                self.queue.push_back(runner::unpack(&stage.join("package.zip"), &stage.join("unpacked")));
            }
            Err(e) => self.update_failed(ctx, e),
        }
    }

    /// The package is unpacked: its files are swapped in and the launcher starts again.
    fn update_unpacked(&mut self, ctx: &egui::Context) {
        let (Some(stage), Some(home)) = (self.stage(), self.home.clone()) else { return };
        let Some(root) = update::package_root(&stage.join("unpacked")) else {
            self.update_failed(ctx, "the package does not hold the engine".to_string());
            return;
        };
        match update::install(&root, &home.root) {
            Ok(files) => {
                self.say(format!("[update: {} file(s) replaced; victory.toml and local\\ were not touched]", files.len()));
                let _ = std::fs::remove_dir_all(stage.join("unpacked"));
                let _ = std::fs::remove_file(stage.join("package.zip"));
                // The new launcher, if the package brought one; else this one again.
                let next = home.root.join("roxbox-launcher.exe");
                let next = if next.is_file() { Some(next) } else { std::env::current_exe().ok() };
                if let Some(next) = next {
                    let _ = std::process::Command::new(next).current_dir(&home.root).spawn();
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(e) => self.update_failed(ctx, e),
        }
    }

    /// Nothing is left half done: the page with the release is opened instead.
    fn update_failed(&mut self, ctx: &egui::Context, why: String) {
        self.queue.retain(|s| !matches!(s.kind, Kind::UpdateChecksum | Kind::UpdatePackage | Kind::UpdateUnpack));
        self.say(format!("[update: {why}]"));
        self.toast(Mark::Err, format!("The update was not installed: {why}. The releases page is open instead."));
        self.updating = Updating::Failed(why);
        ctx.open_url(egui::OpenUrl::new_tab(RELEASES_PAGE));
    }

    // ---- small things ----------------------------------------------------

    /// The job a person watches (it turns PLAY into STOP), and its seconds.
    fn visible_job(&self) -> Option<(Kind, u64)> {
        self.job.as_ref().filter(|j| j.spec.kind.foreground()).map(|j| (j.spec.kind, j.started.elapsed().as_secs()))
    }

    fn stop(&mut self) {
        self.queue.clear();
        let Some(job) = &self.job else { return };
        let label = job.spec.kind.label();
        match job.stop() {
            Ok(()) => self.say(format!("[stopping {label} and everything it started]")),
            Err(e) => self.toast(Mark::Err, format!("Could not stop it: {e}")),
        }
    }

    fn pick_folder(&mut self, ctx: &egui::Context, what: Pick) {
        if self.picking.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let wake = ctx.clone();
        let title = match what {
            Pick::Title => "The folder with your title files",
            Pick::BackupDir => "Where save backups go",
            Pick::SaveDir => "The save folder to back up",
        };
        std::thread::spawn(move || {
            let _ = tx.send(rfd::FileDialog::new().set_title(title).pick_folder());
            wake.request_repaint();
        });
        self.picking = Some((what, rx));
    }

    /// Opens a folder or a file with what Windows opens it with.
    fn open(&mut self, path: &Path) {
        if !path.exists() {
            self.toast(Mark::Warn, format!("Not there yet: {}", path.display()));
            return;
        }
        if std::process::Command::new(update::system_tool("..\\explorer.exe")).arg(path).spawn().is_err() {
            self.toast(Mark::Err, "Explorer could not be started.");
        }
    }

    fn set_profile(&mut self, name: Option<String>) {
        self.profile_name = name.clone().unwrap_or_default();
        self.book.active = name;
        self.confirm_delete = false;
    }

    fn missing(&self) -> usize {
        self.checks.iter().filter(|c| c.status != "OK" && c.status != "WARN").count()
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        // Nothing is drawn unless something happened. The timers: the run's
        // clock once a second, a download's bar, and a toast's end; none
        // while the window is minimized.
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if !minimized {
            if self.updating == Updating::Downloading {
                ctx.request_repaint_after(Duration::from_millis(250));
            } else if self.visible_job().is_some() {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
        }
        self.keys(ctx);
        // A folder (or a title's executable) dropped on the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for path in dropped {
            self.add_title(&path);
            self.page = Page::Play;
        }
        if let Some((what, rx)) = &self.picking
            && let Ok(picked) = rx.try_recv()
        {
            let what = *what;
            self.picking = None;
            if let Some(path) = picked {
                match what {
                    Pick::Title => self.add_title(&path),
                    Pick::BackupDir => self.lib.backup_dir = path.to_string_lossy().into_owned(),
                    Pick::SaveDir => self.lib.save_dir = path.to_string_lossy().into_owned(),
                }
            }
        }
        self.resize_edges(ctx);
        self.title_bar(ctx);
        self.rail(ctx);
        self.footer(ctx);
        self.console_drawer(ctx);
        egui::CentralPanel::default().frame(theme::surface(theme::BG, egui::vec2(0.0, 0.0))).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Frame::none().inner_margin(egui::Margin { left: 38.0, right: 38.0, top: 30.0, bottom: 34.0 }).show(ui, |ui| {
                    ui.set_max_width(ui.available_width().min(940.0));
                    match self.page {
                        Page::Play => self.play_page(ui, ctx),
                        Page::Diagnostics => self.diagnostics_page(ui),
                        Page::Switches => self.switches_page(ui, ctx),
                        Page::Settings => self.settings_page(ui, ctx),
                    }
                });
            });
        });
        self.show_toasts(ctx);
        // The window's own edge, over everything.
        let edge = ctx.screen_rect().shrink(0.5);
        ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("edge"))).rect_stroke(edge, egui::Rounding::ZERO, egui::Stroke::new(1.0_f32, theme::line()));

        // Everything is kept as it is changed: there is no Save.
        if (self.lib.high_contrast, self.lib.reduced_motion) != self.look {
            self.look = (self.lib.high_contrast, self.lib.reduced_motion);
            theme::apply(ctx, self.look.0, self.look.1);
        }
        // Ctrl with + or - or 0 scales the window (egui's own keys); what it came to is kept.
        let scale = ((ctx.zoom_factor() * 100.0).round() as u32).clamp(config::SCALE.0, config::SCALE.1);
        if (ctx.zoom_factor() * 100.0).round() as u32 != scale {
            ctx.set_zoom_factor(scale as f32 / 100.0);
        }
        if scale != self.lib.scale {
            self.lib.scale = scale;
            self.scale_shown = scale as f32;
        }
        if self.lib != self.saved
            && let Some(home) = &self.home
        {
            match self.lib.save(&home.library_file()) {
                Ok(()) => self.note = "Saved.".to_string(),
                Err(e) => self.toast(Mark::Err, format!("The launcher's own file could not be written: {e}")),
            }
            self.saved = self.lib.clone();
        }
        if self.book != self.book_saved
            && let Some(home) = &self.home
        {
            match self.book.save(&home.switches_file()) {
                Ok(()) => self.note = "Saved.".to_string(),
                Err(e) => self.toast(Mark::Err, format!("The saved switches could not be written: {e}")),
            }
            self.book_saved = self.book.clone();
        }
    }
}

impl App {
    /// Ctrl+Enter plays or stops, Ctrl+L shows or hides the console, Esc closes what is open.
    fn keys(&mut self, ctx: &egui::Context) {
        let (play, console, escape) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::L),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if play {
            if self.visible_job().is_some() {
                self.stop();
            } else if self.home.is_some() && self.lib.active.is_some() && !self.busy() {
                self.launch();
                self.pump(ctx);
            }
        }
        if console {
            self.console_open = !self.console_open;
        }
        if escape {
            if self.confirm_reset || self.confirm_delete {
                self.confirm_reset = false;
                self.confirm_delete = false;
            } else if self.console_open {
                self.console_open = false;
            } else {
                self.toasts.clear();
            }
        }
    }

    fn show_toasts(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.born.elapsed() < TOAST);
        let Some(oldest) = self.toasts.first() else { return };
        ctx.request_repaint_after(TOAST.saturating_sub(oldest.born.elapsed()) + Duration::from_millis(30));
        egui::Area::new(egui::Id::new("toasts")).anchor(egui::Align2::RIGHT_TOP, egui::vec2(-20.0, 48.0)).order(egui::Order::Foreground).interactable(false).show(ctx, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::RIGHT), |ui| {
                for toast in &self.toasts {
                    theme::toast(ui, toast.mark, &toast.words);
                    ui.add_space(4.0);
                }
            });
        });
    }

    /// The window has no frame of Windows' own: its edges resize it here.
    fn resize_edges(&self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
            return;
        }
        let (at, pressed) = ctx.input(|i| (i.pointer.hover_pos(), i.pointer.primary_pressed()));
        let Some(direction) = at.and_then(|at| theme::resize_direction(ctx.screen_rect(), at, 5.0)) else { return };
        use egui::{CursorIcon as C, ResizeDirection as D};
        ctx.set_cursor_icon(match direction {
            D::North | D::South => C::ResizeVertical,
            D::East | D::West => C::ResizeHorizontal,
            D::NorthWest | D::SouthEast => C::ResizeNwSe,
            D::NorthEast | D::SouthWest => C::ResizeNeSw,
        });
        if pressed {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }

    fn title_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("title-bar").exact_height(34.0).frame(theme::surface(theme::RAIL, egui::vec2(0.0, 0.0))).show(ctx, |ui| {
            let bar = ui.max_rect();
            let drag = ui.interact(bar, egui::Id::new("title-bar-drag"), egui::Sense::click_and_drag());
            if drag.drag_started_by(egui::PointerButton::Primary) {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            if drag.double_clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
            ui.painter().text(egui::pos2(bar.left() + 16.0, bar.center().y), egui::Align2::LEFT_CENTER, "Rox-Box", egui::FontId::proportional(12.5), theme::dim());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if theme::window_button(ui, theme::Sign::Close).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if theme::window_button(ui, theme::Sign::Maximize).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                if theme::window_button(ui, theme::Sign::Minimize).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
            });
        });
    }

    fn rail(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("rail").exact_width(236.0).resizable(false).frame(theme::surface(theme::RAIL, egui::vec2(14.0, 0.0))).show(ctx, |ui| {
            // The brand: the emblem and the wordmark, both drawn.
            let (brand, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 86.0), egui::Sense::hover());
            theme::emblem(ui, egui::pos2(brand.left() + 30.0, brand.center().y), 20.0);
            theme::wordmark(ui, egui::pos2(brand.left() + 62.0, brand.center().y), 17.0);
            ui.add_space(6.0);
            for (page, sign, label) in [
                (Page::Play, theme::Sign::Play, "Library / Play"),
                (Page::Diagnostics, theme::Sign::Pulse, "Diagnostics"),
                (Page::Switches, theme::Sign::Toggles, "Switches"),
                (Page::Settings, theme::Sign::Sliders, "Settings"),
            ] {
                if theme::nav(ui, sign, label, self.page == page).clicked() {
                    self.page = page;
                }
                ui.add_space(2.0);
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    match &self.release {
                        Some(Ok(release)) if self.update_available() => theme::badge(ui, &format!("{} available", release.tag), theme::WARN),
                        Some(Ok(_)) => theme::badge(ui, "up to date", theme::GOOD),
                        _ => {}
                    }
                });
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    let version = self.version.as_ref().map_or_else(|| "engine not found".to_string(), |v| format!("Rox-Box {v}"));
                    ui.label(RichText::new(version).color(theme::dim()).small());
                });
            });
        });
    }

    fn footer(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("footer").exact_height(34.0).frame(theme::surface(theme::RAIL, egui::vec2(16.0, 0.0))).show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                let label = if self.console_open { "Hide console" } else { "Console" };
                if theme::link(ui, label).on_hover_text("Ctrl+L").clicked() {
                    self.console_open = !self.console_open;
                }
                ui.add(egui::Label::new(RichText::new(&self.note).color(theme::faint()).small()).truncate());
            });
        });
    }

    fn console_drawer(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("console")
            .frame(theme::surface(theme::WELL, egui::vec2(16.0, 10.0)))
            .resizable(true)
            .default_height(240.0)
            .min_height(100.0)
            .show_animated(ctx, self.console_open, |ui| {
                ui.horizontal(|ui| {
                    theme::eyebrow(ui, "CONSOLE");
                    ui.label(RichText::new(format!("{} lines", self.console.len())).color(theme::faint()).small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::link(ui, "Clear").clicked() {
                            self.console.clear();
                        }
                        if theme::link(ui, "Copy log").clicked() {
                            ctx.copy_text(self.console.iter().cloned().collect::<Vec<_>>().join("\n"));
                            let lines = self.console.len();
                            self.toast(Mark::Ok, format!("Log copied ({lines} lines). It can hold paths of this PC: read it before posting."));
                        }
                    });
                });
                egui::ScrollArea::both().auto_shrink([false, false]).stick_to_bottom(true).show_rows(
                    ui,
                    ui.text_style_height(&egui::TextStyle::Monospace),
                    self.console.len(),
                    |ui, rows| {
                        for line in self.console.range(rows) {
                            let color = if line.starts_with("> ") || line.starts_with('[') { theme::ACCENT } else { theme::dim() };
                            ui.add(egui::Label::new(RichText::new(line).color(color).monospace()).extend());
                        }
                    },
                );
            });
    }

    /// The banner of a newer release: what it is, and one button.
    fn update_banner(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(Ok(release)) = self.release.clone() else { return };
        if !self.update_available() || (self.banner_hidden && self.updating == Updating::No) {
            return;
        }
        theme::banner().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_width((ui.available_width() - 330.0).max(220.0));
                    ui.label(RichText::new(format!("Rox-Box {} is available", release.tag)).size(16.5).strong().color(theme::text()));
                    let preview = update::notes_preview(&release.notes, 170);
                    if !preview.is_empty() {
                        ui.add(egui::Label::new(RichText::new(preview).color(theme::dim()).small()).wrap());
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.updating == Updating::No || matches!(self.updating, Updating::Failed(_)) {
                        if theme::link(ui, "Later").clicked() {
                            self.banner_hidden = true;
                        }
                        if theme::button(ui, "Release notes", true).clicked() {
                            ctx.open_url(egui::OpenUrl::new_tab(format!("{RELEASES_PAGE}/tag/{}", release.tag)));
                        }
                        if self.can_install() {
                            if theme::primary(ui, "Download & install", !self.busy()).on_hover_text("Downloads the release, checks it, replaces the engine and the launcher, and starts again. Your victory.toml, local\\ folder, caches and saves are not touched.").clicked() {
                                self.start_update(ctx);
                            }
                        } else if theme::primary(ui, "Open release page", true).on_hover_text("In a source tree the launcher does not replace files: the page is opened.").clicked() {
                            ctx.open_url(egui::OpenUrl::new_tab(RELEASES_PAGE));
                        }
                    }
                });
            });
            match &self.updating {
                Updating::Downloading => {
                    let size = release.package.as_ref().map_or(0, |p| p.1);
                    let got = self.stage().and_then(|s| std::fs::metadata(s.join("package.zip")).ok()).map_or(0, |m| m.len());
                    let fraction = if size > 0 { got as f32 / size as f32 } else { 0.0 };
                    ui.add_space(6.0);
                    theme::progress(ui, fraction, &if size > 0 { format!("Downloading  {} of {}", megabytes(got), megabytes(size)) } else { format!("Downloading  {}", megabytes(got)) });
                }
                Updating::Unpacking => {
                    ui.add_space(6.0);
                    theme::progress(ui, 1.0, "Checked. Installing\u{2026}");
                }
                Updating::Failed(why) => {
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("Not installed: {why}. Nothing was changed.")).color(theme::WARN).small());
                }
                Updating::No => {}
            }
        });
        ui.add_space(18.0);
    }

    fn play_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.update_banner(ui, ctx);
        if self.lib.titles.is_empty() {
            self.onboarding(ui, ctx);
            return;
        }
        let running = self.visible_job();
        let alive = matches!(running, Some((Kind::Launch | Kind::Prepare, _)));
        let active = self.lib.active.clone().unwrap_or_default();
        theme::hero(alive).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::eyebrow(ui, "ACTIVE TITLE");
            ui.label(RichText::new(title_name(&active)).size(38.0).strong().color(theme::text()));
            ui.add(egui::Label::new(RichText::new(&active).font(theme::mono(12.5)).color(theme::dim())).truncate()).on_hover_text(&active);
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                let ready = self.home.is_some() && !self.busy();
                match running {
                    Some(_) => {
                        if theme::big_button(ui, "STOP", theme::Big::Stop).on_hover_text("Ends the title and everything started with it (Ctrl+Enter)").clicked() {
                            self.stop();
                        }
                    }
                    None => {
                        if theme::big_button(ui, "PLAY", if ready { theme::Big::Play } else { theme::Big::Off }).on_hover_text("Ctrl+Enter").clicked() {
                            self.launch();
                            self.pump(ctx);
                        }
                    }
                }
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.add_space(8.0);
                    let missing = self.missing();
                    let (state, mark, detail) = match (&running, &self.status) {
                        (Some((Kind::Launch, secs)), _) => ("Running".to_string(), Mark::Ok, clock(*secs)),
                        (Some((Kind::Prepare, secs)), _) => ("Preparing shaders".to_string(), Mark::Warn, format!("{}  \u{00b7}  the title starts when this is done", clock(*secs))),
                        (Some((kind, secs)), _) => (format!("Working: {}", kind.label()), Mark::Warn, clock(*secs)),
                        (None, _) if missing > 0 => ("Not ready".to_string(), Mark::Err, format!("the preflight check misses {missing} thing(s): Diagnostics says which")),
                        (None, Status::Exited(kind, Some(0))) => ("Ready".to_string(), Mark::Ok, format!("last {} ended normally", kind.label())),
                        (None, Status::Exited(kind, Some(code))) => ("Ready".to_string(), Mark::Err, format!("last {} ended with exit code {code}", kind.label())),
                        (None, Status::Exited(kind, None)) => ("Ready".to_string(), Mark::Warn, format!("last {} was stopped", kind.label())),
                        (None, Status::Idle) => ("Ready".to_string(), Mark::Ok, String::new()),
                    };
                    ui.horizontal(|ui| {
                        let (spot, _) = ui.allocate_exact_size(egui::vec2(16.0, 22.0), egui::Sense::hover());
                        mark.draw(ui, spot.center());
                        ui.label(RichText::new(state).size(17.5).strong().color(theme::text()));
                    });
                    ui.label(RichText::new(detail).color(theme::dim()).small());
                });
            });
            ui.add_space(20.0);
            ui.horizontal_wrapped(|ui| {
                self.launch_pills(ui);
            });
            ui.add_space(6.0);
            let saved = match self.book.active_profile() {
                Some(profile) => format!("Switches: {}  \u{00b7}  {} on", profile.name, profile.on_count()),
                None if self.book.profiles.is_empty() => "Switches: none saved yet".to_string(),
                None => "Switches: no profile chosen".to_string(),
            };
            if theme::link(ui, &saved).on_hover_text("The saved switches this run is given. Opens the Switches page.").clicked() {
                self.page = Page::Switches;
            }
        });

        ui.add_space(24.0);
        self.readiness_strip(ui);

        ui.add_space(28.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Library").size(20.0).strong().color(theme::text()));
            ui.label(RichText::new(format!("{} title folder(s)", self.lib.titles.len())).color(theme::dim()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::button(ui, "+  Add folder", self.picking.is_none()).clicked() {
                    self.pick_folder(ctx, Pick::Title);
                }
            });
        });
        ui.add_space(4.0);
        let mut activate = None;
        let mut remove = None;
        for title in self.lib.titles.clone() {
            let active = self.lib.active.as_deref() == Some(title.as_str());
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 64.0), egui::Sense::click());
            let t = if self.lib.reduced_motion { f32::from(u8::from(resp.hovered())) } else { ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered()) };
            ui.painter().rect(
                rect,
                egui::Rounding::same(theme::RADIUS),
                theme::mix(theme::CARD, theme::CARD_HI, t),
                egui::Stroke::new(1.0_f32, if active { theme::mix(theme::line(), theme::ACCENT, 0.6) } else { theme::mix(theme::line(), theme::dim(), t) }),
            );
            if resp.has_focus() {
                ui.painter().rect_stroke(rect.expand(3.0), egui::Rounding::same(theme::RADIUS + 3.0), egui::Stroke::new(2.0_f32, theme::FOCUS));
            }
            let name = title_name(&title);
            resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, active, format!("{name}, {title}")));
            theme::emblem(ui, egui::pos2(rect.left() + 30.0, rect.center().y), if active { 13.0 } else { 10.0 });
            ui.painter().text(egui::pos2(rect.left() + 58.0, rect.top() + 23.0), egui::Align2::LEFT_CENTER, &name, egui::FontId::proportional(16.0), theme::text());
            ui.painter().text(egui::pos2(rect.left() + 58.0, rect.top() + 44.0), egui::Align2::LEFT_CENTER, elide(&title, ((rect.width() - 250.0) / 7.2) as usize), theme::mono(11.5), theme::dim());
            let side = egui::Rect::from_min_max(egui::pos2(rect.right() - 170.0, rect.top()), rect.right_bottom()).shrink2(egui::vec2(14.0, 14.0));
            let mut side_ui = ui.new_child(egui::UiBuilder::new().max_rect(side).layout(egui::Layout::right_to_left(egui::Align::Center)));
            if active {
                theme::badge(&mut side_ui, "ACTIVE", theme::ACCENT);
            } else if theme::link(&mut side_ui, "Remove").on_hover_text("Takes it off this list. No file is touched.").clicked() {
                remove = Some(title.clone());
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !active && remove.is_none() {
                activate = Some(title.clone());
            }
            ui.add_space(2.0);
        }
        if let Some(title) = remove {
            self.lib.remove(&title);
            self.toast(Mark::Ok, format!("{} removed from the library. No file was touched.", title_name(&title)));
        } else if let Some(title) = activate
            && !self.busy()
        {
            self.activate(title);
        }
    }

    /// The quick toggles of a launch (the hero has them, and Settings).
    fn launch_pills(&mut self, ui: &mut egui::Ui) {
        if theme::pill(ui, "Fast", self.lib.fast).on_hover_text("The low-memory preset: smaller console model, 1920x1080, release build (play.cmd -Fast).").clicked() {
            self.lib.fast = !self.lib.fast;
        }
        if theme::pill(ui, "Log", self.lib.log).on_hover_text("Also write the run's output under %LOCALAPPDATA%\\victory\\logs (play.cmd -Log).").clicked() {
            self.lib.log = !self.lib.log;
        }
        if theme::pill(ui, "Safe", self.lib.safe).on_hover_text("Newer experimental optimizations off (play.cmd -Safe).").clicked() {
            self.lib.safe = !self.lib.safe;
        }
        if theme::pill(ui, "Prepare first", self.lib.prepare)
            .on_hover_text("Translate shaders and warm the pipelines before the run (play.cmd -Prepare). The title starts only if that succeeds.")
            .clicked()
        {
            self.lib.prepare = !self.lib.prepare;
        }
        ui.add_space(8.0);
        if let Some(model) = theme::segmented(ui, &[("SeriesX", "Series X"), ("SeriesS", "Series S")], &self.lib.console) {
            self.lib.console = model.to_string();
        }
    }

    /// Nothing is set up yet: one place to drop a folder, one button.
    fn onboarding(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let hot = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let zone = theme::drop_zone(ui, 380.0, hot);
        let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(zone.shrink(30.0)).layout(egui::Layout::top_down(egui::Align::Center)));
        inner.add_space(26.0);
        let (spot, _) = inner.allocate_exact_size(egui::vec2(80.0, 80.0), egui::Sense::hover());
        theme::emblem(&inner, spot.center(), 36.0);
        inner.add_space(14.0);
        inner.label(RichText::new("Welcome to Rox-Box").size(30.0).strong().color(theme::text()));
        inner.label(RichText::new(if hot { "Let go to add it" } else { "Drop your title folder here" }).size(18.0).color(if hot { theme::ACCENT } else { theme::text() }));
        inner.label(RichText::new("the folder with your own, legally obtained title files (or the title's .exe)").color(theme::dim()));
        inner.add_space(16.0);
        if theme::primary(&mut inner, "   Browse\u{2026}   ", self.picking.is_none()).clicked() {
            self.pick_folder(ctx, Pick::Title);
        }
        inner.add_space(10.0);
        inner.label(RichText::new("That is all the setup there is. Rox-Box knows no title by name and ships none.").color(theme::faint()).small());
        ui.add_space(24.0);
        self.readiness_strip(ui);
    }

    /// The PC's facts as pills: a mark and its word each; why is on hover.
    fn readiness_strip(&mut self, ui: &mut egui::Ui) {
        let gpu = system::gpu_of(&self.checks);
        let vram = gpu.as_ref().and_then(|g| g.1);
        let warnings = system::warnings(&self.host, vram);
        let about = |subject: system::Subject| -> (Mark, String) {
            match warnings.iter().find(|w| w.0 == subject) {
                Some((_, Level::Warn, text)) => (Mark::Err, text.clone()),
                Some((_, Level::Note, text)) => (Mark::Warn, text.clone()),
                None => (Mark::Ok, String::new()),
            }
        };
        theme::eyebrow(ui, "SYSTEM READINESS");
        ui.horizontal_wrapped(|ui| {
            let card = self.checks.iter().find(|c| c.name == "Graphics card");
            let (gpu_mark, gpu_tip) = match card {
                Some(c) if c.status == "OK" => (Mark::Ok, c.detail.clone()),
                Some(c) => (Mark::Err, c.detail.clone()),
                None => (Mark::Off, "Not read yet: the engine's own check names it.".to_string()),
            };
            theme::fact(ui, "GPU", &gpu.as_ref().map_or_else(|| "not read".to_string(), |g| g.0.clone()), gpu_mark, &gpu_tip);
            let (mark, tip) = about(system::Subject::Vram);
            theme::fact(ui, "VRAM", &vram.map_or_else(|| "\u{2013}".to_string(), system::gb), if vram.is_some() { mark } else { Mark::Off }, &tip);
            let (mark, tip) = about(system::Subject::Ram);
            theme::fact(ui, "RAM", &self.host.ram_mb.map_or_else(|| "\u{2013}".to_string(), system::gb), mark, &tip);
            let (mark, tip) = about(system::Subject::PageFile);
            let page = match &self.host.page_file {
                system::PageFile::None => "none".to_string(),
                system::PageFile::SystemManaged => "system-managed".to_string(),
                system::PageFile::Fixed { maximum_mb, .. } => format!("fixed, {}", system::gb(*maximum_mb)),
                system::PageFile::Unknown => "not read".to_string(),
            };
            theme::fact(ui, "PAGE FILE", &page, mark, &tip);
            let missing = self.missing();
            let warned = self.checks.iter().filter(|c| c.status == "WARN").count();
            let (value, mark) = match (self.checks.is_empty(), missing, warned) {
                (true, ..) => ("not run".to_string(), Mark::Off),
                (_, 0, 0) => ("ready".to_string(), Mark::Ok),
                (_, 0, w) => (format!("{w} warning(s)"), Mark::Warn),
                (_, m, _) => (format!("{m} missing"), Mark::Err),
            };
            let tip: Vec<String> = self.checks.iter().filter(|c| c.status != "OK").map(|c| format!("{}: {}", c.name, c.detail)).collect();
            if theme::fact(ui, "PREFLIGHT", &value, mark, &if tip.is_empty() { "Everything a run needs is there.".to_string() } else { tip.join("\n") }).clicked() {
                self.page = Page::Diagnostics;
            }
        });
    }

    fn diagnostics_page(&mut self, ui: &mut egui::Ui) {
        theme::page_title(ui, "Diagnostics", "What a run needs, and the tools that keep it ready. Nothing here starts a title.");
        let idle = self.home.is_some() && !self.busy();
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::eyebrow(ui, "PREFLIGHT");
                if let Some(summary) = &self.check_summary {
                    ui.label(RichText::new(summary).color(theme::dim()).small());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::button(ui, "Check again", idle).on_hover_text("victory check: everything a run needs, and how to fix what is missing.").clicked() {
                        self.host = Host::read();
                        self.enqueue_victory(Kind::Check, &["check"]);
                    }
                });
            });
            ui.add_space(4.0);
            if self.checks.is_empty() {
                ui.label(RichText::new("Not checked yet.").color(theme::dim()));
            }
            egui::Grid::new("checks").num_columns(3).spacing([16.0, 10.0]).show(ui, |ui| {
                for check in &self.checks {
                    let mark = match check.status.as_str() {
                        "OK" => Mark::Ok,
                        "WARN" => Mark::Warn,
                        _ => Mark::Err,
                    };
                    theme::mark_word(ui, mark);
                    ui.label(RichText::new(&check.name).color(theme::text()));
                    ui.add(egui::Label::new(RichText::new(&check.detail).font(theme::mono(12.0)).color(if mark == Mark::Ok { theme::dim() } else { theme::text() })).wrap());
                    ui.end_row();
                }
            });
        });
        ui.add_space(18.0);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::eyebrow(ui, "MAINTENANCE");
            ui.add_space(4.0);
            let tool = |ui: &mut egui::Ui, name: &str, verdict: Option<Mark>, about: &str, action: &str, enabled: bool| -> bool {
                let mut clicked = false;
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width((ui.available_width() - 190.0).max(200.0));
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(name).size(15.5).strong().color(theme::text()));
                            if let Some(mark) = verdict {
                                theme::mark_word(ui, mark);
                            }
                        });
                        ui.add(egui::Label::new(RichText::new(about).color(theme::dim()).small()).wrap());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        clicked = theme::button(ui, action, enabled).clicked();
                    });
                });
                ui.add_space(10.0);
                clicked
            };
            if tool(ui, "Prepare shaders", None, "Translate every shader of the title into the cache, so no run has to. A few minutes, once per version.  victory shaders", "Prepare", idle) {
                self.enqueue_victory(Kind::Shaders, &["shaders"]);
            }
            if tool(ui, "Warm cache", None, "Make the pipelines earlier runs drew with before a run needs them.  victory warm --used --settle", "Warm", idle) {
                self.enqueue_victory(Kind::Warm, &["warm", "--used", "--settle"]);
            }
            let verdict = self.selftest.map(|ok| if ok { Mark::Ok } else { Mark::Err });
            if tool(ui, "Self-test", verdict, "The frame path end to end with a stand-in of our own. Needs no title.  victory selftest", "Run", idle) {
                self.enqueue_victory(Kind::SelfTest, &["selftest"]);
            }
            if tool(ui, "Run report", None, "The newest run's report, to the clipboard. Numbers and Rox-Box's own words only: safe for a public issue.  victory report", "Copy", idle) {
                self.enqueue_victory(Kind::Report, &["report"]);
            }
            if !self.confirm_reset {
                if tool(ui, "Reset pipeline cache", None, "For a cache gone stale after a driver or version change. Removes the pipeline library and the warm-up's marks; what runs recorded stays.", "Reset", idle) {
                    self.confirm_reset = true;
                }
            } else {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Remove the pipeline library? Warm cache makes it again.").color(theme::WARN));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::button(ui, "Keep it", true).on_hover_text("Esc").clicked() {
                            self.confirm_reset = false;
                        }
                        if theme::danger(ui, "Yes, reset", idle).clicked() {
                            self.confirm_reset = false;
                            self.reset_cache();
                        }
                    });
                });
            }
        });
    }

    /// The saved switches: profiles of toggles that stand for what would be pasted into `-More`.
    fn switches_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        theme::page_title(ui, "Switches", "The VICTORY_* settings you would paste into -More for every run, kept as toggles. They stay in local\\switches.toml on this PC; nothing of a title enters the launcher.");

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::eyebrow(ui, "PROFILE");
            let mut choose = None;
            ui.horizontal_wrapped(|ui| {
                if theme::pill(ui, "None", self.book.active.is_none()).on_hover_text("No saved switches are given to a run.").clicked() {
                    choose = Some(None);
                }
                for profile in &self.book.profiles {
                    let on = self.book.active.as_deref() == Some(profile.name.as_str());
                    if theme::pill(ui, &profile.name, on).clicked() && !on {
                        choose = Some(Some(profile.name.clone()));
                    }
                }
                if theme::button(ui, "+  New profile", true).clicked() {
                    let name = self.book.add("Profile");
                    choose = Some(Some(name));
                }
            });
            if let Some(name) = choose {
                self.set_profile(name);
            }
            let Some(active) = self.book.active.clone() else {
                ui.add_space(4.0);
                ui.label(RichText::new(if self.book.profiles.is_empty() { "No profile yet. Paste your -More text below, or make a new profile." } else { "No profile is chosen: a run is given only the toggles of the Play page." }).color(theme::dim()).small());
                return;
            };
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.profile_name).desired_width(220.0).margin(egui::vec2(10.0, 7.0)));
                let can_rename = !self.profile_name.trim().is_empty() && self.profile_name.trim() != active && self.book.profile(self.profile_name.trim()).is_none();
                if theme::button(ui, "Rename", can_rename).clicked() {
                    let to = self.profile_name.trim().to_string();
                    if self.book.rename(&active, &to) {
                        self.set_profile(Some(to));
                    }
                }
                if theme::button(ui, "Duplicate", true).clicked()
                    && let Some(copy) = self.book.profile(&active).cloned()
                {
                    let name = self.book.add(&format!("{active} copy"));
                    if let Some(new) = self.book.profile_mut(&name) {
                        new.switches = copy.switches;
                    }
                    self.set_profile(Some(name));
                }
                if !self.confirm_delete {
                    if theme::danger(ui, "Delete", true).clicked() {
                        self.confirm_delete = true;
                    }
                } else {
                    ui.label(RichText::new("Delete this profile and its switches?").color(theme::WARN));
                    if theme::danger(ui, "Yes, delete", true).clicked() {
                        self.book.remove(&active);
                        self.set_profile(None);
                        self.toast(Mark::Ok, format!("Profile {active} deleted."));
                    }
                    if theme::button(ui, "Keep it", true).on_hover_text("Esc").clicked() {
                        self.confirm_delete = false;
                    }
                }
            });
            if let Some(title) = self.lib.active.clone() {
                ui.add_space(6.0);
                let tied = self.book.for_title(&title) == Some(active.as_str());
                if theme::pill(ui, &format!("Choose this profile whenever {} is chosen", title_name(&title)), tied).clicked() {
                    self.book.tie(&title, if tied { None } else { Some(active.as_str()) });
                }
            }
        });

        ui.add_space(18.0);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::eyebrow(ui, "ADD");
            ui.label(RichText::new("Paste what you give -More: NAME=value;NAME=value").color(theme::text()));
            ui.add(egui::TextEdit::multiline(&mut self.paste).font(theme::mono(12.5)).desired_rows(2).desired_width(f32::INFINITY).hint_text(RichText::new("VICTORY_NAME=value;VICTORY_OTHER=value").color(theme::faint())).margin(egui::vec2(10.0, 8.0)));
            ui.horizontal_wrapped(|ui| {
                if theme::primary(ui, "Turn into toggles", !self.paste.trim().is_empty()).clicked() {
                    match switches::import(&self.paste) {
                        Ok(found) => {
                            let name = match self.book.active.clone() {
                                Some(name) => name,
                                None => self.book.add("Imported"),
                            };
                            let count = found.len();
                            if let Some(profile) = self.book.profile_mut(&name) {
                                for switch in found {
                                    profile.put(switch);
                                }
                            }
                            self.set_profile(Some(name.clone()));
                            self.paste.clear();
                            self.toast(Mark::Ok, format!("{count} switch(es) put into {name}."));
                        }
                        Err(why) => self.toast(Mark::Warn, format!("Not taken: {why}.")),
                    }
                }
                ui.label(RichText::new("A name the profile already has gets the new value. With no profile chosen, one is made.").color(theme::dim()).small());
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("or one:").color(theme::dim()));
                ui.add(egui::TextEdit::singleline(&mut self.new_switch.0).font(theme::mono(12.5)).desired_width(280.0).hint_text(RichText::new("VICTORY_NAME").color(theme::faint())).margin(egui::vec2(10.0, 7.0)));
                ui.label("=");
                ui.add(egui::TextEdit::singleline(&mut self.new_switch.1).font(theme::mono(12.5)).desired_width(160.0).hint_text(RichText::new("value").color(theme::faint())).margin(egui::vec2(10.0, 7.0)));
                let (name, value) = (self.new_switch.0.trim().to_string(), self.new_switch.1.trim().to_string());
                let fits = switches::valid_name(&name) && switches::valid_value(&value);
                if theme::button(ui, "Add", fits && self.book.active.is_some()).on_hover_text("A name is capitals, digits and _. A value holds no ; and no quote.").clicked()
                    && let Some(profile) = self.book.active.clone().and_then(|a| self.book.profile_mut(&a))
                {
                    profile.put(switches::Switch { name, value, on: true, note: String::new() });
                    self.new_switch = Default::default();
                }
            });
        });

        let Some(at) = self.book.active.as_ref().and_then(|a| self.book.profiles.iter().position(|p| &p.name == a)) else { return };
        ui.add_space(18.0);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let profile = &mut self.book.profiles[at];
            ui.horizontal(|ui| {
                theme::eyebrow(ui, "SWITCHES");
                ui.label(RichText::new(format!("{} on of {}", profile.on_count(), profile.switches.len())).color(theme::dim()).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::link(ui, "All off").clicked() {
                        profile.switches.iter_mut().for_each(|s| s.on = false);
                    }
                    if theme::link(ui, "All on").clicked() {
                        profile.switches.iter_mut().for_each(|s| s.on = true);
                    }
                });
            });
            if profile.switches.is_empty() {
                ui.label(RichText::new("This profile is empty.").color(theme::dim()));
            }
            let mut remove = None;
            egui::Grid::new("switches").num_columns(4).spacing([12.0, 8.0]).show(ui, |ui| {
                for (i, switch) in profile.switches.iter_mut().enumerate() {
                    if theme::pill(ui, &switch.name, switch.on).clicked() {
                        switch.on = !switch.on;
                    }
                    let value = ui.add_sized([200.0, 30.0], egui::TextEdit::singleline(&mut switch.value).font(theme::mono(12.5)).margin(egui::vec2(8.0, 6.0)));
                    if !switches::valid_value(&switch.value) {
                        ui.painter().rect_stroke(value.rect, egui::Rounding::same(8.0), egui::Stroke::new(1.5_f32, theme::WARN));
                        value.on_hover_text("A value holds no ; and no quote: this switch is not given to a run as it is.");
                    }
                    ui.add_sized([260.0, 30.0], egui::TextEdit::singleline(&mut switch.note).hint_text(RichText::new("a note of your own").color(theme::faint())).margin(egui::vec2(8.0, 6.0)));
                    if theme::link(ui, "Remove").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
            if let Some(i) = remove {
                profile.switches.remove(i);
            }
        });

        ui.add_space(18.0);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let given = switches::more_for_run(self.book.active_profile(), &self.lib.more);
            ui.horizontal(|ui| {
                theme::eyebrow(ui, "WHAT A RUN IS GIVEN AS -MORE");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::link(ui, "Copy").clicked() {
                        ctx.copy_text(given.clone());
                        self.toast(Mark::Ok, "The -More text is copied.");
                    }
                });
            });
            ui.add(egui::Label::new(RichText::new(if given.is_empty() { "(nothing)" } else { given.as_str() }).font(theme::mono(12.5)).color(theme::text())).wrap());
            if !self.lib.more.trim().is_empty() {
                ui.label(RichText::new("The end of it is the extra text of Settings, Launch defaults.").color(theme::dim()).small());
            }
        });
    }

    /// A folder setting: its name, what it is now (or what stands in), Browse and a way back to the default.
    fn folder_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, name: &str, value: &str, default: &str, what: Pick) -> bool {
        let mut cleared = false;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width((ui.available_width() - 210.0).max(200.0));
                ui.label(RichText::new(name).color(theme::text()));
                let shown = if value.trim().is_empty() { default.to_string() } else { value.to_string() };
                ui.add(egui::Label::new(RichText::new(shown).font(theme::mono(12.0)).color(theme::dim())).truncate());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !value.trim().is_empty() && theme::link(ui, "Default").clicked() {
                    cleared = true;
                }
                if theme::button(ui, "Browse\u{2026}", self.picking.is_none()).clicked() {
                    self.pick_folder(ctx, what);
                }
            });
        });
        cleared
    }

    fn settings_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        theme::page_title(ui, "Settings", "Everything is kept the moment you change it, in local\\launcher.toml beside the engine. Nothing leaves this PC.");

        theme::section(ui, "General & behavior");
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::eyebrow(ui, "WHEN A TITLE STARTS");
            if let Some(choice) = theme::segmented(ui, &[("stay", "Keep the launcher open"), ("minimize", "Minimize it"), ("close", "Close it")], &self.lib.on_play) {
                self.lib.on_play = choice.to_string();
            }
            ui.label(
                RichText::new(match self.lib.on_play.as_str() {
                    "minimize" => "Minimized, the launcher draws nothing and comes back when the title ends.",
                    "close" => "The launcher exits and uses no memory at all. The run's output is then only in its log file (Log is switched on for it), and no backup is made after it.",
                    _ => "The launcher stays and draws only when something changes: once a second for the run's clock.",
                })
                .color(theme::dim())
                .small(),
            );
        });

        theme::section(ui, "Launch defaults");
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                self.launch_pills(ui);
            });
            if self.source_tree() {
                ui.add_space(12.0);
                theme::eyebrow(ui, "ENGINE BUILD");
                let builds = self.home.as_ref().map(|h| h.builds()).unwrap_or_default();
                let mut choices = vec![("", "First built")];
                choices.extend(builds.iter().map(|b| (*b, *b)));
                if let Some(choice) = theme::segmented(ui, &choices, &self.lib.build) {
                    self.lib.build = choice.to_string();
                }
            }
            let knows_shots = self.home.as_ref().and_then(|h| std::fs::read_to_string(h.play_script()).ok()).is_some_and(|text| runner::knows_shots(&text));
            if knows_shots {
                ui.add_space(12.0);
                theme::eyebrow(ui, "PICTURES OF A RUN");
                ui.horizontal_wrapped(|ui| {
                    if theme::pill(ui, "Save a picture while playing", self.lib.shots).on_hover_text("play.cmd -Shots: a half-size picture of the run, at most 120 a run, into the shots folder of the engine's data folder.").clicked() {
                        self.lib.shots = !self.lib.shots;
                    }
                    ui.label(RichText::new("every").color(theme::dim()));
                    ui.add_enabled(self.lib.shots, egui::DragValue::new(&mut self.lib.shots_every).range(1..=86_400).speed(0.2).suffix(" s"));
                });
                ui.label(RichText::new("The pictures are your own data: they stay in that folder and are in no report.").color(theme::dim()).small());
            }
            ui.add_space(12.0);
            theme::eyebrow(ui, "EXTRA SWITCHES");
            ui.add(egui::TextEdit::singleline(&mut self.lib.more).font(theme::mono(12.5)).hint_text(RichText::new("VICTORY_NAME=value;VICTORY_OTHER=value").color(theme::faint())).desired_width(f32::INFINITY).margin(egui::vec2(10.0, 8.0)));
            ui.label(RichText::new("Given to every run after the saved switches of the Switches page (play.cmd -More), so a pair here stands over the same name there.").color(theme::dim()).small());
        });

        theme::section(ui, "Updates");
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if theme::pill(ui, "Check for updates when Rox-Box starts", self.lib.auto_update).clicked() {
                    self.lib.auto_update = !self.lib.auto_update;
                }
                if theme::button(ui, "Check now", !self.busy()).on_hover_text("Asks GitHub for the newest release of RoxBuilds/Rox-box.").clicked() {
                    self.asked_for_release = true;
                    self.queue.push_back(runner::latest_release());
                }
                match &self.release {
                    Some(Ok(release)) if self.update_available() => theme::badge(ui, &format!("{} available", release.tag), theme::WARN),
                    Some(Ok(release)) => theme::badge(ui, &format!("up to date ({})", release.tag), theme::GOOD),
                    Some(Err(why)) => {
                        ui.label(RichText::new(why).color(theme::dim()).small());
                    }
                    None => {}
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(if self.source_tree() {
                    "This is a source tree: a newer release is shown, and its page is opened. Files here are never replaced."
                } else {
                    "A newer release is downloaded to local\\update, checked against the release's checksum, and swapped in; the launcher then starts again. victory.toml, local\\, caches and saves are never touched. If anything fails, everything is put back and the release page is opened."
                })
                .color(theme::dim())
                .small(),
            );
            if self.update_available() {
                ui.add_space(10.0);
                self.banner_hidden = false;
                self.update_banner(ui, ctx);
            }
        });

        theme::section(ui, "Auto-backups");
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if theme::pill(ui, "Back up saves after each session", self.lib.backup).clicked() {
                    self.lib.backup = !self.lib.backup;
                }
                if theme::button(ui, "Back up now", self.home.is_some() && !self.busy()).clicked() {
                    self.backup_now();
                }
            });
            ui.add_space(8.0);
            let (save, backup) = (self.lib.save_dir.clone(), self.lib.backup_dir.clone());
            if self.folder_row(ui, ctx, "Save folder", &save, "not named: a saves folder in the engine's data folder, else that folder if it is under 250 MB", Pick::SaveDir) {
                self.lib.save_dir.clear();
            }
            ui.add_space(6.0);
            if self.folder_row(ui, ctx, "Backup folder", &backup, "local\\backups, beside the engine", Pick::BackupDir) {
                self.lib.backup_dir.clear();
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Keep the newest").color(theme::text()));
                ui.add(egui::DragValue::new(&mut self.lib.backup_keep).range(0..=99).speed(0.1));
                ui.label(RichText::new(if self.lib.backup_keep == 0 { "(0: every backup is kept)" } else { "zip(s); older ones of the launcher's own are removed" }).color(theme::dim()).small());
            });
            ui.label(RichText::new("Each backup is one saves-<date>-<time>.zip of the whole save folder. Only files with that name are ever removed. A folder you did not name is not packed when it holds more than 250 MB: name your save folder then.").color(theme::dim()).small());
        });

        theme::section(ui, "Accessibility");
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::eyebrow(ui, "SIZE OF EVERYTHING");
            ui.horizontal(|ui| {
                let slider = ui.add(egui::Slider::new(&mut self.scale_shown, config::SCALE.0 as f32..=config::SCALE.1 as f32).step_by(5.0).suffix(" %"));
                // Applied when the handle is let go: a window that resizes under the pointer cannot be dragged.
                if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                    ctx.set_zoom_factor(self.scale_shown / 100.0);
                }
                if theme::link(ui, "100 %").clicked() {
                    ctx.set_zoom_factor(1.0);
                }
                ui.label(RichText::new("Ctrl +   Ctrl -   Ctrl 0").font(theme::mono(11.5)).color(theme::faint()));
            });
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                if theme::pill(ui, "High contrast", self.lib.high_contrast).on_hover_text("Brighter text and edges: body text over 15:1, secondary text over 7:1, edges over 4.5:1.").clicked() {
                    self.lib.high_contrast = !self.lib.high_contrast;
                }
                if theme::pill(ui, "Reduced motion", self.lib.reduced_motion).on_hover_text("Nothing fades or slides: hover, the console drawer.").clicked() {
                    self.lib.reduced_motion = !self.lib.reduced_motion;
                }
            });
            ui.add_space(10.0);
            theme::eyebrow(ui, "KEYBOARD");
            egui::Grid::new("keys").num_columns(2).spacing([18.0, 5.0]).show(ui, |ui| {
                for (key, what) in [
                    ("Tab  /  Shift+Tab", "to the next or the previous control"),
                    ("Enter  /  Space", "presses the control that has the ring"),
                    ("Ctrl+Enter", "Play, or Stop while a title runs"),
                    ("Ctrl+L", "shows or hides the console"),
                    ("Esc", "closes the console, a question, the notices"),
                ] {
                    ui.label(RichText::new(key).font(theme::mono(12.0)).color(theme::text()));
                    ui.label(RichText::new(what).color(theme::dim()).small());
                    ui.end_row();
                }
            });
            ui.label(RichText::new("Every state has a shape and a word besides its colour (disc OK, triangle WARN, square ERR). Screen readers get each control's role and name.").color(theme::dim()).small());
        });

        theme::section(ui, "About & folders");
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Engine {}", self.version.clone().unwrap_or_else(|| "\u{2013}".to_string()))).size(15.5).strong().color(theme::text()));
                ui.label(RichText::new(format!("Launcher {}", env!("CARGO_PKG_VERSION"))).color(theme::dim()));
                ui.label(RichText::new("GPL-3.0-or-later").color(theme::faint()).small());
            });
            let default = self.home.as_ref().and_then(|h| std::fs::read_to_string(h.config_file()).ok()).and_then(|t| config::game_dir_of(&t));
            let in_sync = default.is_some() && default.as_deref() == self.lib.active.as_deref();
            let source = self.source_tree();
            ui.label(
                RichText::new(match (&default, in_sync, source) {
                    (_, true, false) => "victory.toml names the active title: it is kept so by itself (game_dir only; a copy of the file is kept when it changes).",
                    (_, true, true) => "victory.toml names the active title.",
                    (Some(_), false, false) => "victory.toml names another folder; it follows the next title you choose or play.",
                    (None, _, false) => "victory.toml names no folder yet; it will, with the first title.",
                    (_, false, true) => "This is a source tree: victory.toml is yours, and choosing a title does not change it. PLAY writes it (game_dir only, a copy kept), and so does the button here. Every command is about the active title all the same.",
                })
                .color(theme::dim())
                .small(),
            );
            if source && !in_sync && theme::button(ui, "Write active title to victory.toml", self.lib.active.is_some()).clicked() {
                self.sync_config(true);
                self.toast(Mark::Ok, "victory.toml names the active title now.");
            }
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                let home = self.home.clone();
                if theme::button(ui, "Open Rox-Box folder", home.is_some()).clicked()
                    && let Some(home) = &home
                {
                    self.open(&home.root);
                }
                if theme::button(ui, "Open logs folder", true).clicked() {
                    let logs = std::env::var_os("LOCALAPPDATA").map(|l| PathBuf::from(l).join("victory").join("logs")).unwrap_or_default();
                    self.open(&logs);
                }
                if theme::button(ui, "Open victory.toml", home.is_some()).clicked()
                    && let Some(home) = &home
                {
                    self.open(&home.config_file());
                }
                if theme::button(ui, "Open backups folder", home.is_some()).clicked()
                    && let Some(dir) = self.backup_dir()
                {
                    self.open(&dir);
                }
                if theme::button(ui, "Releases page", true).clicked() {
                    ctx.open_url(egui::OpenUrl::new_tab(RELEASES_PAGE));
                }
            });
        });
    }
}

/// `--probe`: what the launcher found, as text, and no window.
fn probe() {
    let home = Home::find();
    println!("launcher {}", env!("CARGO_PKG_VERSION"));
    match &home {
        None => println!("home: not found"),
        Some(h) => {
            println!("home: {:?} ({} builds: {:?})", h.layout, h.builds().len(), h.builds());
            println!("play script: {}", h.play_script().is_file());
            println!("victory.exe: {}", h.victory("").is_some_and(|v| v.is_file()));
            println!("victory.toml: {}", h.config_file().is_file());
            let lib = Library::load(&h.library_file());
            println!("titles: {} (active: {})", lib.titles.len(), lib.active.is_some());
            println!("auto update: {}  backups: {}  scale: {} %", lib.auto_update, lib.backup, lib.scale);
        }
    }
    let host = Host::read();
    println!("ram_mb: {:?}  commit_limit_mb: {:?}", host.ram_mb, host.commit_limit_mb);
    println!("page file: {:?}", host.page_file);
    println!("windows: {}", host.windows);
    println!("tools: curl {}  tar {}", update::system_tool("curl.exe").is_file(), update::system_tool("tar.exe").is_file());
    for (_, level, text) in system::warnings(&host, None) {
        println!("{level:?}: {text}");
    }
}

fn main() -> eframe::Result {
    if std::env::args().any(|a| a == "--probe") {
        probe();
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rox-Box")
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([940.0, 600.0])
            // The title bar is the launcher's own (drawn, with its three buttons).
            .with_decorations(false)
            .with_drag_and_drop(true)
            .with_icon(theme::icon()),
        ..Default::default()
    };
    eframe::run_native("Rox-Box", options, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_paths_are_cut_in_the_middle_and_times_are_clocks() {
        assert_eq!(elide("C:\\short", 40), "C:\\short");
        let cut = elide("C:\\a very long folder\\with more folders\\and the title", 24);
        assert_eq!(cut.chars().count(), 24);
        assert!(cut.starts_with("C:\\a ver") && cut.ends_with("and the title") && cut.contains('\u{2026}'), "{cut}");
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(754), "12:34");
        assert_eq!(title_name("D:\\Titles\\My Title"), "My Title");
        assert_eq!(megabytes(4_515_465), "4.3 MB");
    }

    #[test]
    fn victory_toml_is_written_unasked_only_in_a_release_folder() {
        assert!(writes_config(Some(Layout::Release), false));
        assert!(writes_config(Some(Layout::Release), true));
        assert!(!writes_config(Some(Layout::Source), false), "choosing a title in a checkout leaves the developer's file alone");
        assert!(writes_config(Some(Layout::Source), true), "PLAY and the button write it");
        assert!(!writes_config(None, true));
    }

    #[test]
    fn folders_are_read_from_victory_config() {
        let shown = "config file : C:\\r\\victory.toml\ngame_dir    : D:\\T\ncache_dir   : C:\\Users\\example\\AppData\\Local\\victory\\cache\ndata_dir    : C:\\d\n";
        assert_eq!(config_value(shown, "cache_dir"), Some(PathBuf::from("C:\\Users\\example\\AppData\\Local\\victory\\cache")));
        assert_eq!(config_value(shown, "data_dir"), Some(PathBuf::from("C:\\d")));
        assert_eq!(config_value("cache_dir   :\n", "cache_dir"), None);
        assert_eq!(config_value("error: no config", "cache_dir"), None);
    }

    #[test]
    fn a_reset_removes_the_library_and_keeps_what_runs_recorded() {
        let cache = std::env::temp_dir().join(format!("roxbox-launcher-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let dir = cache.join("pipelines");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["host-pipelines.bin", "warm-stamp.txt", "known-pipelines.bin", "known-shaders.bin", "early-pipelines.txt"] {
            std::fs::write(dir.join(name), b"12345").unwrap();
        }
        std::fs::create_dir_all(cache.join("shaders")).unwrap();
        assert_eq!(reset_pipeline_cache(&cache).unwrap(), (2, 10));
        assert!(!dir.join("host-pipelines.bin").exists());
        assert!(dir.join("known-pipelines.bin").exists() && dir.join("known-shaders.bin").exists() && dir.join("early-pipelines.txt").exists());
        assert!(cache.join("shaders").exists());
        assert_eq!(reset_pipeline_cache(&cache).unwrap(), (0, 0));
        assert_eq!(reset_pipeline_cache(&cache.join("nowhere")).unwrap(), (0, 0));
    }
}
