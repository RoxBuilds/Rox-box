//! The Rox-box Launcher: one window over `victory.exe` and `play.ps1`.
//!
//! It starts what a person would type and shows what comes back: nothing it
//! does is its own way of running a title. It knows no title: a title is a
//! folder the person names. It is portable: it runs from the unzipped
//! release folder (or the source tree), writes `victory.toml`'s `game_dir`
//! line and its own `local\launcher.toml`, and nothing else.
//!
//! `roxbox-launcher.exe --probe` prints what it found and opens no window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod paths;
mod runner;
mod system;
mod theme;

use std::collections::VecDeque;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText};

use config::Library;
use paths::{Home, Layout};
use runner::{Event, Job, Kind, Spec};
use system::{CheckLine, Host, Level};

/// The releases page a person is sent to (never a download the launcher makes).
pub const RELEASES_PAGE: &str = "https://github.com/RoxBuilds/Rox-box/releases";
/// Where the newest release's tag is asked for, on the button's press only.
pub const RELEASE_API: &str = "https://api.github.com/repos/RoxBuilds/Rox-box/releases/latest";

/// The console drawer keeps this many lines.
const CONSOLE_LINES: usize = 5000;

/// What the status indicator says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Idle,
    Preparing,
    Running,
    /// A maintenance command is at work.
    Working(Kind),
    /// The last thing ended: what it was, its exit code (none: stopped).
    Exited(Kind, Option<i32>),
}

/// `cache_dir` out of `victory config`'s output (`cache_dir   : <path>`).
fn cache_dir_of(output: &str) -> Option<PathBuf> {
    output.lines().find_map(|l| {
        let (key, value) = l.split_once(':')?;
        (key.trim() == "cache_dir" && !value.trim().is_empty()).then(|| PathBuf::from(value.trim()))
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
    latest: Option<Result<String, String>>,
    note: String,
    picking: Option<Receiver<Option<PathBuf>>>,
    confirm_reset: bool,
}

impl App {
    fn new(ctx: &egui::Context) -> App {
        theme::apply(ctx);
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
        let mut app = App {
            saved: lib.clone(),
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
            latest: None,
            note: String::new(),
            picking: None,
            confirm_reset: false,
        };
        match &app.home {
            None => {
                app.note = "Rox-box was not found: put roxbox-launcher.exe in the folder with victory.exe and play.ps1.".to_string()
            }
            Some(_) => {
                app.enqueue_victory(Kind::Version, &["--version"]);
                app.enqueue_victory(Kind::Check, &["check"]);
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

    fn enqueue_victory(&mut self, kind: Kind, args: &[&str]) {
        match self.victory_exe() {
            Some(exe) => self.queue.push_back(runner::victory(kind, &exe, args, self.lib.active.as_deref())),
            None => self.note = "victory.exe is not built yet: build it first (scripts\\build.ps1 -Release).".to_string(),
        }
    }

    fn launch(&mut self) {
        let Some(home) = &self.home else { return };
        let script = home.play_script();
        let source = self.source_tree();
        if self.lib.prepare {
            self.queue.push_back(runner::play(&script, &self.lib, source, true));
        }
        self.queue.push_back(runner::play(&script, &self.lib, source, false));
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
                self.status = match kind {
                    Kind::Launch => Status::Running,
                    Kind::Prepare => Status::Preparing,
                    Kind::Check | Kind::Version | Kind::Report | Kind::Latest if self.status != Status::Idle => self.status.clone(),
                    kind => Status::Working(kind),
                };
                if kind == Kind::SelfTest {
                    self.selftest = None;
                }
                self.job = Some(job);
            }
            Err(e) => {
                self.say(format!("could not start: {e}"));
                self.note = format!("Could not start {}: {e}", kind.label());
                self.queue.clear();
                if kind == Kind::Latest {
                    self.latest = Some(Err("curl.exe could not be started".to_string()));
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
                    self.note = format!("Run report copied ({} lines). It holds numbers and Rox-box's own words only.", job.output.len());
                } else {
                    self.note = "No run report yet: every run of a title writes one.".to_string();
                }
            }
            Kind::Latest => {
                self.latest = Some(match runner::tag_of(&job.output.join("")) {
                    Some(tag) => Ok(tag),
                    None => Err("no answer (offline, or the API's limit)".to_string()),
                });
            }
            Kind::SelfTest => self.selftest = Some(ok),
            Kind::Prepare if !ok => {
                self.queue.retain(|s| s.kind != Kind::Launch);
                self.note = "Preparing did not finish: the title was not started. The console says why.".to_string();
            }
            Kind::Launch | Kind::Shaders | Kind::Warm => {
                // What a run or a preparation changed, the check should show.
                self.enqueue_victory(Kind::Check, &["check"]);
            }
            Kind::Prepare => {}
        }
    }

    fn add_title(&mut self, path: &Path) {
        let Some(folder) = config::title_folder_of(path) else {
            self.note = format!("Not a folder: {}", path.display());
            return;
        };
        let first = self.lib.titles.is_empty();
        self.lib.add(&folder);
        // The first title is also what victory.toml names, if it names none.
        if let Some(home) = &self.home
            && first
            && !home.config_file().exists()
        {
            self.note = match config::write_game_dir(&home.config_file(), &folder) {
                Ok(done) => done,
                Err(e) => format!("victory.toml could not be written: {e}"),
            };
        }
        self.enqueue_victory(Kind::Check, &["check"]);
    }

    fn reset_cache(&mut self) {
        let Some(exe) = self.victory_exe() else { return };
        let mut command = std::process::Command::new(&exe);
        command.arg("config").creation_flags(0x0800_0000);
        if let Some(title) = &self.lib.active {
            command.env("VICTORY_GAME_DIR", title);
        }
        let shown = command.output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        self.note = match cache_dir_of(&shown) {
            None => "The cache folder is not known: `victory config` did not name it.".to_string(),
            Some(dir) => match reset_pipeline_cache(&dir) {
                Ok((0, _)) => "The pipeline cache was already empty.".to_string(),
                Ok((files, bytes)) => {
                    format!("Pipeline cache reset: {files} file(s), {} MB. Warm Cache makes the pipelines again.", bytes >> 20)
                }
                Err(e) => format!("The pipeline cache could not be reset: {e}"),
            },
        };
        self.say(format!("[{}]", self.note));
        self.enqueue_victory(Kind::Check, &["check"]);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        if self.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
        // A folder (or a title's executable) dropped on the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for path in dropped {
            self.add_title(&path);
        }
        if let Some(rx) = &self.picking
            && let Ok(picked) = rx.try_recv()
        {
            self.picking = None;
            if let Some(path) = picked {
                self.add_title(&path);
            }
        }

        self.top_bar(ctx);
        self.console_drawer(ctx);
        self.library_panel(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.run_section(ui, ctx);
                self.system_section(ui);
                self.maintenance_section(ui);
                self.version_section(ui, ctx);
            });
        });

        if self.lib != self.saved
            && let Some(home) = &self.home
        {
            if let Err(e) = self.lib.save(&home.library_file()) {
                self.note = format!("The launcher's own file could not be written: {e}");
            }
            self.saved = self.lib.clone();
        }
    }
}

impl App {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").frame(theme::bar()).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("ROX-BOX").color(theme::ACCENT).strong().size(16.0));
                ui.label(RichText::new("// LAUNCHER").color(theme::DIM).size(16.0));
                if let Some(v) = &self.version {
                    ui.label(RichText::new(format!("v{v}")).color(theme::DIM));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (word, color) = match &self.status {
                        Status::Idle => ("IDLE".to_string(), theme::DIM),
                        Status::Preparing => ("PREPARING".to_string(), theme::WARN),
                        Status::Running => ("RUNNING".to_string(), theme::ACCENT),
                        Status::Working(kind) => (kind.label().to_uppercase(), theme::WARN),
                        Status::Exited(kind, Some(0)) => (format!("{} EXITED 0", kind.label().to_uppercase()), theme::TEXT),
                        Status::Exited(kind, Some(c)) => (format!("{} EXITED {c}", kind.label().to_uppercase()), theme::BAD),
                        Status::Exited(kind, None) => (format!("{} STOPPED", kind.label().to_uppercase()), theme::WARN),
                    };
                    theme::chip(ui, &word, color);
                    if let Some(job) = &self.job
                        && !job.spec.kind.quiet()
                    {
                        ui.label(RichText::new(format!("{} s", job.started.elapsed().as_secs())).color(theme::DIM));
                    }
                });
            });
        });
    }

    fn console_drawer(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("console-bar").frame(theme::bar()).show(ctx, |ui| {
            ui.horizontal(|ui| {
                let arrow = if self.console_open { "v" } else { ">" };
                if theme::flat(ui, &format!("{arrow} CONSOLE  {} lines", self.console.len())).clicked() {
                    self.console_open = !self.console_open;
                }
                if theme::flat(ui, "COPY LOG").clicked() {
                    ctx.copy_text(self.console.iter().cloned().collect::<Vec<_>>().join("\n"));
                    self.note = format!("Log copied ({} lines). It can hold paths of this PC: read it before posting.", self.console.len());
                }
                if theme::flat(ui, "CLEAR").clicked() {
                    self.console.clear();
                }
                ui.add(egui::Label::new(RichText::new(&self.note).color(theme::DIM)).truncate());
            });
        });
        egui::TopBottomPanel::bottom("console")
            .frame(theme::well())
            .resizable(true)
            .default_height(220.0)
            .min_height(80.0)
            .show_animated(ctx, self.console_open, |ui| {
                egui::ScrollArea::both().auto_shrink([false, false]).stick_to_bottom(true).show_rows(
                    ui,
                    ui.text_style_height(&egui::TextStyle::Monospace),
                    self.console.len(),
                    |ui, rows| {
                        for line in self.console.range(rows) {
                            let color = if line.starts_with("> ") || line.starts_with('[') { theme::ACCENT } else { theme::TEXT };
                            ui.add(egui::Label::new(RichText::new(line).color(color).monospace()).extend());
                        }
                    },
                );
            });
    }

    fn library_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("library").frame(theme::panel()).resizable(false).exact_width(280.0).show(ctx, |ui| {
            theme::heading(ui, "TITLES");
            if self.lib.titles.is_empty() {
                ui.label(RichText::new("No title folder yet.\n\nAdd the folder with your own, legally obtained title files, or drop it on this window.").color(theme::DIM));
            }
            let mut activate = None;
            for title in &self.lib.titles {
                let active = self.lib.active.as_deref() == Some(title.as_str());
                let name = Path::new(title).file_name().map_or_else(|| title.clone(), |n| n.to_string_lossy().into_owned());
                let text = RichText::new(format!("{} {name}", if active { ">" } else { " " }))
                    .color(if active { theme::ACCENT } else { theme::TEXT });
                if ui.add(egui::SelectableLabel::new(active, text)).on_hover_text(title).clicked() && !active {
                    activate = Some(title.clone());
                }
            }
            if let Some(title) = activate {
                self.lib.active = Some(title);
                self.enqueue_victory(Kind::Check, &["check"]);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if theme::button(ui, "+ ADD FOLDER", self.picking.is_none()).clicked() {
                    let (tx, rx) = channel();
                    let wake = ctx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(rfd::FileDialog::new().set_title("The folder with your title files").pick_folder());
                        wake.request_repaint();
                    });
                    self.picking = Some(rx);
                }
                if theme::button(ui, "REMOVE", self.lib.active.is_some() && !self.busy()).clicked()
                    && let Some(active) = self.lib.active.clone()
                {
                    self.lib.remove(&active);
                    self.note = "Removed from the list. No file was touched.".to_string();
                }
            });
            ui.add_space(8.0);
            let default = self
                .home
                .as_ref()
                .and_then(|h| std::fs::read_to_string(h.config_file()).ok())
                .and_then(|t| config::game_dir_of(&t));
            let is_default = default.is_some() && default.as_deref() == self.lib.active.as_deref();
            if theme::button(ui, "WRITE TO victory.toml", self.lib.active.is_some() && !is_default && self.home.is_some())
                .on_hover_text("Sets game_dir to this folder. Every other line stays; a timestamped copy of the file is kept.")
                .clicked()
                && let (Some(home), Some(active)) = (&self.home, &self.lib.active)
            {
                self.note = match config::write_game_dir(&home.config_file(), active) {
                    Ok(done) => done,
                    Err(e) => format!("victory.toml could not be written: {e}"),
                };
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(match (&default, is_default) {
                    (_, true) => "victory.toml names this folder.".to_string(),
                    (Some(_), false) => "victory.toml names another folder. Launching uses the one chosen here for that run.".to_string(),
                    (None, _) => "victory.toml names no folder yet.".to_string(),
                })
                .color(theme::DIM)
                .small(),
            );
        });
    }

    fn run_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        theme::heading(ui, "RUN");
        ui.horizontal(|ui| {
            let running = self.job.as_ref().filter(|j| !j.spec.kind.quiet()).map(|j| (j.pid, j.spec.kind));
            let ready = self.home.is_some() && self.lib.active.is_some() && !self.busy();
            if theme::primary(ui, "LAUNCH", ready).clicked() {
                self.launch();
                self.pump(ctx);
            }
            if theme::danger(ui, "STOP", running.is_some()).clicked()
                && let Some((pid, kind)) = running
            {
                self.queue.clear();
                match runner::stop(pid) {
                    Ok(()) => self.say(format!("[stopping {} and everything it started]", kind.label())),
                    Err(e) => self.note = format!("Could not stop it: {e}"),
                }
            }
            ui.add_space(12.0);
            ui.label(RichText::new(match &self.lib.active {
                Some(title) => title.clone(),
                None => "add a title folder first".to_string(),
            })
            .color(theme::DIM));
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.toggle_value(&mut self.lib.fast, "FAST").on_hover_text("play.cmd -Fast: the low-memory preset (smaller console model, 1920x1080, release build).");
            ui.toggle_value(&mut self.lib.log, "LOG").on_hover_text("play.cmd -Log: the run's output is also written under %LOCALAPPDATA%\\victory\\logs.");
            ui.toggle_value(&mut self.lib.prepare, "PREPARE FIRST")
                .on_hover_text("Before the run: play.cmd -Prepare (translate shaders, warm the pipelines). The title starts only if that succeeds.");
            ui.toggle_value(&mut self.lib.safe, "SAFE").on_hover_text("play.cmd -Safe: newer experimental optimizations off.");
            ui.add_space(12.0);
            ui.label(RichText::new("CONSOLE").color(theme::DIM));
            egui::ComboBox::from_id_salt("console-model").selected_text(self.lib.console.clone()).show_ui(ui, |ui| {
                for model in ["SeriesX", "SeriesS"] {
                    ui.selectable_value(&mut self.lib.console, model.to_string(), model);
                }
            });
            if self.source_tree() {
                ui.label(RichText::new("BUILD").color(theme::DIM));
                let shown = if self.lib.build.is_empty() { "auto".to_string() } else { self.lib.build.clone() };
                egui::ComboBox::from_id_salt("build").selected_text(shown).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.lib.build, String::new(), "auto (first built)");
                    for build in self.home.as_ref().map(|h| h.builds()).unwrap_or_default() {
                        ui.selectable_value(&mut self.lib.build, build.to_string(), build);
                    }
                });
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("MORE").color(theme::DIM)).on_hover_text("play.cmd -More: NAME=value pairs separated by ';'");
            ui.add(egui::TextEdit::singleline(&mut self.lib.more).hint_text("VICTORY_NAME=value;VICTORY_OTHER=value").desired_width(f32::INFINITY));
        });
        if self.lib.fast && self.lib.console == "SeriesX" {
            ui.label(RichText::new("FAST chooses the smaller console model unless one is given: CONSOLE stays as -Fast sets it.").color(theme::DIM).small());
        }
    }

    fn system_section(&mut self, ui: &mut egui::Ui) {
        theme::heading(ui, "SYSTEM");
        let gpu = system::gpu_of(&self.checks);
        let vram = gpu.as_ref().and_then(|g| g.1);
        egui::Grid::new("facts").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            let mut row = |name: &str, value: String| {
                ui.label(RichText::new(name).color(theme::DIM));
                ui.label(value);
                ui.end_row();
            };
            row("GPU", gpu.as_ref().map_or_else(|| "not read yet (victory check names it)".to_string(), |g| g.0.clone()));
            row("VRAM", vram.map_or_else(|| "-".to_string(), system::gb));
            row("RAM", self.host.ram_mb.map_or_else(|| "-".to_string(), system::gb));
            row("WINDOWS", self.host.windows.clone());
            row(
                "PAGE FILE",
                match &self.host.page_file {
                    system::PageFile::None => "none".to_string(),
                    system::PageFile::SystemManaged => "system-managed".to_string(),
                    system::PageFile::Fixed { initial_mb, maximum_mb } => {
                        format!("fixed, {} to {}", system::gb(*initial_mb), system::gb(*maximum_mb))
                    }
                    system::PageFile::Unknown => "not read".to_string(),
                },
            );
            row("COMMIT LIMIT", self.host.commit_limit_mb.map_or_else(|| "-".to_string(), system::gb));
        });
        for (level, text) in system::warnings(&self.host, vram) {
            let (tag, color) = if level == Level::Warn { ("WARN", theme::WARN) } else { ("NOTE", theme::DIM) };
            ui.horizontal_wrapped(|ui| {
                theme::chip(ui, tag, color);
                ui.label(text);
            });
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if theme::button(ui, "RE-CHECK", self.home.is_some() && !self.busy()).on_hover_text("victory check: everything a run needs, and how to fix what is missing. Starts nothing.").clicked() {
                self.host = Host::read();
                self.enqueue_victory(Kind::Check, &["check"]);
            }
            if let Some(summary) = &self.check_summary {
                ui.label(RichText::new(summary).color(theme::DIM));
            }
        });
        for check in &self.checks {
            let color = match check.status.as_str() {
                "OK" => theme::DIM,
                "WARN" => theme::WARN,
                _ => theme::BAD,
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{:<7}", check.status)).color(color));
                ui.label(RichText::new(format!("{:<17}", check.name)).color(theme::TEXT));
                ui.label(RichText::new(&check.detail).color(if check.status == "OK" { theme::DIM } else { theme::TEXT }));
            });
        }
    }

    fn maintenance_section(&mut self, ui: &mut egui::Ui) {
        theme::heading(ui, "MAINTENANCE");
        let idle = self.home.is_some() && !self.busy();
        ui.horizontal_wrapped(|ui| {
            if theme::button(ui, "PREPARE SHADERS", idle).on_hover_text("victory shaders: translate every shader of the title into the cache. A few minutes, once per version.").clicked() {
                self.enqueue_victory(Kind::Shaders, &["shaders"]);
            }
            if theme::button(ui, "WARM CACHE", idle).on_hover_text("victory warm --used --settle: make the pipelines earlier runs drew with, before a run needs them.").clicked() {
                self.enqueue_victory(Kind::Warm, &["warm", "--used", "--settle"]);
            }
            if theme::button(ui, "RUN SELF-TEST", idle).on_hover_text("victory selftest: the frame path with a stand-in of our own. Needs no title.").clicked() {
                self.enqueue_victory(Kind::SelfTest, &["selftest"]);
            }
            match self.selftest {
                Some(true) => theme::chip(ui, "PASS", theme::ACCENT),
                Some(false) => theme::chip(ui, "FAIL", theme::BAD),
                None => {}
            }
        });
        ui.horizontal_wrapped(|ui| {
            if theme::button(ui, "COPY RUN REPORT", idle)
                .on_hover_text("victory report: the newest run's report, to the clipboard. Numbers and Rox-box's own words only: safe for a public issue.")
                .clicked()
            {
                self.enqueue_victory(Kind::Report, &["report"]);
            }
            if !self.confirm_reset {
                if theme::button(ui, "RESET PIPELINE CACHE", idle)
                    .on_hover_text("For a stale or damaged cache (pipelines refused after a driver or version change). Removes the pipeline library and the warm-up's marks; what runs recorded stays.")
                    .clicked()
                {
                    self.confirm_reset = true;
                }
            } else {
                ui.label(RichText::new("Remove the pipeline library? Warm Cache makes it again.").color(theme::WARN));
                if theme::danger(ui, "YES, RESET", idle).clicked() {
                    self.confirm_reset = false;
                    self.reset_cache();
                }
                if theme::button(ui, "NO", true).clicked() {
                    self.confirm_reset = false;
                }
            }
        });
    }

    fn version_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        theme::heading(ui, "VERSION");
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("ROX-BOX").color(theme::DIM));
            ui.label(self.version.clone().unwrap_or_else(|| "-".to_string()));
            ui.label(RichText::new("LAUNCHER").color(theme::DIM));
            ui.label(env!("CARGO_PKG_VERSION"));
            if theme::button(ui, "CHECK LATEST", !self.busy())
                .on_hover_text("Asks GitHub for the newest release's name. Nothing is downloaded or installed.")
                .clicked()
            {
                self.latest = None;
                self.queue.push_back(runner::latest_release());
            }
            match (&self.latest, &self.version) {
                (Some(Ok(tag)), Some(local)) => {
                    let latest = runner::version_of(tag).unwrap_or_default();
                    if runner::newer(&latest, local) {
                        theme::chip(ui, &format!("{tag} IS OUT"), theme::WARN);
                    } else {
                        theme::chip(ui, &format!("UP TO DATE ({tag})"), theme::ACCENT);
                    }
                }
                (Some(Ok(tag)), None) => theme::chip(ui, tag, theme::DIM),
                (Some(Err(e)), _) => {
                    ui.label(RichText::new(e).color(theme::DIM));
                }
                (None, _) => {}
            }
            if theme::flat(ui, "RELEASES PAGE").clicked() {
                ctx.open_url(egui::OpenUrl::new_tab(RELEASES_PAGE));
            }
        });
        ui.add_space(12.0);
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
        }
    }
    let host = Host::read();
    println!("ram_mb: {:?}  commit_limit_mb: {:?}", host.ram_mb, host.commit_limit_mb);
    println!("page file: {:?}", host.page_file);
    println!("windows: {}", host.windows);
    for (level, text) in system::warnings(&host, None) {
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
            .with_title("Rox-box Launcher")
            .with_inner_size([1040.0, 760.0])
            .with_min_inner_size([820.0, 520.0])
            .with_drag_and_drop(true)
            .with_icon(theme::icon()),
        ..Default::default()
    };
    eframe::run_native("Rox-box Launcher", options, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_folder_is_read_from_victory_config() {
        let shown = "config file : C:\\r\\victory.toml\ngame_dir    : D:\\T\ncache_dir   : C:\\Users\\example\\AppData\\Local\\victory\\cache\ndata_dir    : C:\\d\n";
        assert_eq!(cache_dir_of(shown), Some(PathBuf::from("C:\\Users\\example\\AppData\\Local\\victory\\cache")));
        assert_eq!(cache_dir_of("cache_dir   :\n"), None);
        assert_eq!(cache_dir_of("error: no config"), None);
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
