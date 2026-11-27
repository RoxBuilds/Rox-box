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

/// How the last thing a person started ended (what runs now is the job itself).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Idle,
    /// What it was, and its exit code (none: it was stopped).
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
    page: Page,
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
            // `ROXBOX_PAGE` opens on another page (for pictures of the launcher).
            page: match std::env::var("ROXBOX_PAGE").as_deref() {
                Ok("diagnostics") => Page::Diagnostics,
                Ok("settings") => Page::Settings,
                _ => Page::Play,
            },
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
        let mut run = runner::play(&script, &self.lib, source, false);
        if self.lib.on_play == "close" {
            // The launcher will be gone: no pipes to it, and the log file keeps the output.
            let mut logged = self.lib.clone();
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
                if kind == Kind::Launch && self.lib.on_play == "minimize" {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
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

/// The pages of the rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Play,
    Diagnostics,
    Settings,
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

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        // Nothing is drawn unless something happened: the only timer is the
        // run's clock, once a second, and not while the window is minimized.
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if self.job.as_ref().is_some_and(|j| !j.spec.kind.quiet()) && !minimized {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
        // A folder (or a title's executable) dropped on the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for path in dropped {
            self.add_title(&path);
            self.page = Page::Play;
        }
        if let Some(rx) = &self.picking
            && let Ok(picked) = rx.try_recv()
        {
            self.picking = None;
            if let Some(path) = picked {
                self.add_title(&path);
            }
        }
        self.resize_edges(ctx);
        self.title_bar(ctx);
        self.rail(ctx);
        self.footer(ctx);
        self.console_drawer(ctx);
        egui::CentralPanel::default().frame(theme::surface(theme::BG, egui::vec2(0.0, 0.0))).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Frame::none().inner_margin(egui::Margin { left: 36.0, right: 36.0, top: 28.0, bottom: 28.0 }).show(ui, |ui| {
                    ui.set_max_width(ui.available_width().min(940.0));
                    match self.page {
                        Page::Play => self.play_page(ui, ctx),
                        Page::Diagnostics => self.diagnostics_page(ui),
                        Page::Settings => self.settings_page(ui, ctx),
                    }
                });
            });
        });
        // The window's own edge, over everything.
        let edge = ctx.screen_rect().shrink(0.5);
        ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("edge"))).rect_stroke(edge, egui::Rounding::ZERO, egui::Stroke::new(1.0_f32, theme::LINE));

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
            ui.painter().text(egui::pos2(bar.left() + 16.0, bar.center().y), egui::Align2::LEFT_CENTER, "Rox-Box", egui::FontId::proportional(12.5), theme::DIM);
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
                (Page::Settings, theme::Sign::Sliders, "Settings"),
            ] {
                if theme::nav(ui, sign, label, self.page == page).clicked() {
                    self.page = page;
                }
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    match (&self.latest, &self.version) {
                        (Some(Ok(tag)), Some(local)) if runner::newer(&runner::version_of(tag).unwrap_or_default(), local) => {
                            theme::badge(ui, &format!("{tag} is out"), theme::WARN);
                        }
                        (Some(Ok(_)), Some(_)) => theme::badge(ui, "up to date", theme::GOOD),
                        _ => {}
                    }
                });
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    let version = self.version.as_ref().map_or_else(|| "engine not found".to_string(), |v| format!("Rox-Box {v}"));
                    ui.label(RichText::new(version).color(theme::DIM).small());
                });
            });
        });
    }

    fn footer(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("footer").exact_height(34.0).frame(theme::surface(theme::RAIL, egui::vec2(16.0, 0.0))).show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                let label = if self.console_open { "Hide console" } else { "Console" };
                if theme::link(ui, label).clicked() {
                    self.console_open = !self.console_open;
                }
                ui.add(egui::Label::new(RichText::new(&self.note).color(theme::FAINT).small()).truncate());
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
                    ui.label(RichText::new(format!("{} lines", self.console.len())).color(theme::FAINT).small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::link(ui, "Clear").clicked() {
                            self.console.clear();
                        }
                        if theme::link(ui, "Copy log").clicked() {
                            ctx.copy_text(self.console.iter().cloned().collect::<Vec<_>>().join("\n"));
                            self.note = format!("Log copied ({} lines). It can hold paths of this PC: read it before posting.", self.console.len());
                        }
                    });
                });
                egui::ScrollArea::both().auto_shrink([false, false]).stick_to_bottom(true).show_rows(
                    ui,
                    ui.text_style_height(&egui::TextStyle::Monospace),
                    self.console.len(),
                    |ui, rows| {
                        for line in self.console.range(rows) {
                            let color = if line.starts_with("> ") || line.starts_with('[') { theme::ACCENT } else { theme::DIM };
                            ui.add(egui::Label::new(RichText::new(line).color(color).monospace()).extend());
                        }
                    },
                );
            });
    }

    /// The job a person sees as running (not the quiet ones behind the scenes).
    fn visible_job(&self) -> Option<(Kind, u64)> {
        self.job.as_ref().filter(|j| !j.spec.kind.quiet()).map(|j| (j.spec.kind, j.started.elapsed().as_secs()))
    }

    fn stop(&mut self) {
        self.queue.clear();
        let Some(job) = &self.job else { return };
        let label = job.spec.kind.label();
        match job.stop() {
            Ok(()) => self.say(format!("[stopping {label} and everything it started]")),
            Err(e) => self.note = format!("Could not stop it: {e}"),
        }
    }

    fn pick_folder(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let wake = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(rfd::FileDialog::new().set_title("The folder with your title files").pick_folder());
            wake.request_repaint();
        });
        self.picking = Some(rx);
    }

    fn play_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let running = self.visible_job();
        let alive = matches!(running, Some((Kind::Launch | Kind::Prepare, _)));
        theme::hero(alive).show(ui, |ui| {
            ui.set_width(ui.available_width());
            match self.lib.active.clone() {
                None => {
                    theme::eyebrow(ui, "WELCOME");
                    ui.label(RichText::new("Add your first title").size(34.0).strong());
                    ui.label(RichText::new("Choose the folder that holds your own, legally obtained title files, or drop it anywhere on this window.").color(theme::DIM));
                    ui.add_space(14.0);
                    if theme::big_button(ui, "ADD FOLDER", if self.picking.is_none() { theme::Big::Play } else { theme::Big::Off }).clicked() {
                        self.pick_folder(ctx);
                    }
                }
                Some(active) => {
                    theme::eyebrow(ui, "ACTIVE TITLE");
                    ui.label(RichText::new(title_name(&active)).size(38.0).strong());
                    ui.add(egui::Label::new(RichText::new(&active).font(theme::mono(12.5)).color(theme::DIM)).truncate()).on_hover_text(&active);
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        let ready = self.home.is_some() && !self.busy();
                        match running {
                            Some(_) => {
                                if theme::big_button(ui, "STOP", theme::Big::Stop).clicked() {
                                    self.stop();
                                }
                            }
                            None => {
                                if theme::big_button(ui, "PLAY", if ready { theme::Big::Play } else { theme::Big::Off }).clicked() {
                                    self.launch();
                                    self.pump(ctx);
                                }
                            }
                        }
                        ui.add_space(14.0);
                        ui.vertical(|ui| {
                            ui.add_space(8.0);
                            let missing = self.checks.iter().filter(|c| c.status != "OK" && c.status != "WARN").count();
                            let (state, color, detail) = match (&running, &self.status) {
                                (Some((Kind::Launch, secs)), _) => ("Running".to_string(), theme::ACCENT, clock(*secs)),
                                (Some((Kind::Prepare, secs)), _) => ("Preparing shaders".to_string(), theme::WARN, format!("{}  ·  the title starts when this is done", clock(*secs))),
                                (Some((kind, secs)), _) => (format!("Working: {}", kind.label()), theme::WARN, clock(*secs)),
                                (None, _) if missing > 0 => {
                                    ("Not ready".to_string(), theme::BAD, format!("the preflight check misses {missing} thing(s): Diagnostics says which"))
                                }
                                (None, Status::Exited(kind, Some(0))) => ("Ready".to_string(), theme::GOOD, format!("last {} ended normally", kind.label())),
                                (None, Status::Exited(kind, Some(code))) => ("Ready".to_string(), theme::BAD, format!("last {} ended with exit code {code}", kind.label())),
                                (None, Status::Exited(kind, None)) => ("Ready".to_string(), theme::WARN, format!("last {} was stopped", kind.label())),
                                (None, _) => ("Ready".to_string(), theme::GOOD, String::new()),
                            };
                            ui.horizontal(|ui| {
                                let (spot, _) = ui.allocate_exact_size(egui::vec2(14.0, 20.0), egui::Sense::hover());
                                theme::dot(ui, spot.center(), color);
                                ui.label(RichText::new(state).size(17.0).strong());
                            });
                            ui.label(RichText::new(detail).color(theme::DIM).small());
                        });
                    });
                    ui.add_space(18.0);
                    ui.horizontal_wrapped(|ui| {
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
                    });
                }
            }
        });

        ui.add_space(22.0);
        self.readiness_strip(ui);

        ui.add_space(26.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Library").size(20.0).strong());
            ui.label(RichText::new(format!("{} title folder(s)", self.lib.titles.len())).color(theme::DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::button(ui, "+  Add folder", self.picking.is_none()).clicked() {
                    self.pick_folder(ctx);
                }
            });
        });
        ui.add_space(4.0);
        let mut activate = None;
        let mut remove = None;
        for title in self.lib.titles.clone() {
            let active = self.lib.active.as_deref() == Some(title.as_str());
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 62.0), egui::Sense::click());
            let t = ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered());
            ui.painter().rect(
                rect,
                egui::Rounding::same(theme::RADIUS),
                theme::mix(theme::CARD, theme::CARD_HI, t),
                egui::Stroke::new(1.0_f32, if active { theme::mix(theme::LINE, theme::ACCENT, 0.6) } else { theme::mix(theme::LINE, theme::DIM, t) }),
            );
            theme::emblem(ui, egui::pos2(rect.left() + 30.0, rect.center().y), if active { 13.0 } else { 10.0 });
            ui.painter().text(egui::pos2(rect.left() + 58.0, rect.top() + 22.0), egui::Align2::LEFT_CENTER, title_name(&title), egui::FontId::proportional(16.0), theme::TEXT);
            ui.painter().text(egui::pos2(rect.left() + 58.0, rect.top() + 43.0), egui::Align2::LEFT_CENTER, elide(&title, ((rect.width() - 250.0) / 7.2) as usize), theme::mono(11.5), theme::DIM);
            let side = egui::Rect::from_min_max(egui::pos2(rect.right() - 170.0, rect.top()), rect.right_bottom()).shrink2(egui::vec2(14.0, 14.0));
            let mut side_ui = ui.new_child(egui::UiBuilder::new().max_rect(side).layout(egui::Layout::right_to_left(egui::Align::Center)));
            if active {
                theme::badge(&mut side_ui, "ACTIVE", theme::ACCENT);
            } else if theme::link(&mut side_ui, "Remove").clicked() {
                remove = Some(title.clone());
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !active && remove.is_none() {
                activate = Some(title.clone());
            }
            ui.add_space(2.0);
        }
        if let Some(title) = remove {
            self.lib.remove(&title);
            self.note = "Removed from the library. No file was touched.".to_string();
        } else if let Some(title) = activate
            && !self.busy()
        {
            self.lib.active = Some(title);
            self.enqueue_victory(Kind::Check, &["check"]);
        }
        if self.lib.titles.is_empty() {
            ui.label(RichText::new("Nothing here yet. A title is a folder on your PC: the launcher knows no title by name and ships none.").color(theme::DIM));
        }
    }

    /// The PC's facts as pills with a dot each; what a dot means is on hover.
    fn readiness_strip(&mut self, ui: &mut egui::Ui) {
        let gpu = system::gpu_of(&self.checks);
        let vram = gpu.as_ref().and_then(|g| g.1);
        let warnings = system::warnings(&self.host, vram);
        let about = |subject: system::Subject| -> (egui::Color32, String) {
            match warnings.iter().find(|w| w.0 == subject) {
                Some((_, Level::Warn, text)) => (theme::BAD, text.clone()),
                Some((_, Level::Note, text)) => (theme::WARN, text.clone()),
                None => (theme::GOOD, String::new()),
            }
        };
        theme::eyebrow(ui, "SYSTEM READINESS");
        ui.horizontal_wrapped(|ui| {
            let card = self.checks.iter().find(|c| c.name == "Graphics card");
            let (gpu_color, gpu_tip) = match card {
                Some(c) if c.status == "OK" => (theme::GOOD, c.detail.clone()),
                Some(c) => (theme::BAD, c.detail.clone()),
                None => (theme::FAINT, "Not read yet: the engine's own check names it.".to_string()),
            };
            theme::fact(ui, "GPU", &gpu.as_ref().map_or_else(|| "not read".to_string(), |g| g.0.clone()), gpu_color, &gpu_tip);
            let (c, tip) = about(system::Subject::Vram);
            theme::fact(ui, "VRAM", &vram.map_or_else(|| "-".to_string(), system::gb), if vram.is_some() { c } else { theme::FAINT }, &tip);
            let (c, tip) = about(system::Subject::Ram);
            theme::fact(ui, "RAM", &self.host.ram_mb.map_or_else(|| "-".to_string(), system::gb), c, &tip);
            let (c, tip) = about(system::Subject::PageFile);
            let page = match &self.host.page_file {
                system::PageFile::None => "none".to_string(),
                system::PageFile::SystemManaged => "system-managed".to_string(),
                system::PageFile::Fixed { maximum_mb, .. } => format!("fixed, {}", system::gb(*maximum_mb)),
                system::PageFile::Unknown => "not read".to_string(),
            };
            theme::fact(ui, "PAGE FILE", &page, c, &tip);
            let missing = self.checks.iter().filter(|c| c.status != "OK" && c.status != "WARN").count();
            let warned = self.checks.iter().filter(|c| c.status == "WARN").count();
            let (value, color) = match (self.checks.is_empty(), missing, warned) {
                (true, ..) => ("not run".to_string(), theme::FAINT),
                (_, 0, 0) => ("ready".to_string(), theme::GOOD),
                (_, 0, w) => (format!("{w} warning(s)"), theme::WARN),
                (_, m, _) => (format!("{m} missing"), theme::BAD),
            };
            let tip: Vec<String> = self.checks.iter().filter(|c| c.status != "OK").map(|c| format!("{}: {}", c.name, c.detail)).collect();
            if theme::fact(ui, "PREFLIGHT", &value, color, &if tip.is_empty() { "Everything a run needs is there.".to_string() } else { tip.join("\n") })
                .interact(egui::Sense::click())
                .clicked()
            {
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
                    ui.label(RichText::new(summary).color(theme::DIM).small());
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
                ui.label(RichText::new("Not checked yet.").color(theme::DIM));
            }
            egui::Grid::new("checks").num_columns(3).spacing([14.0, 9.0]).show(ui, |ui| {
                for check in &self.checks {
                    let color = match check.status.as_str() {
                        "OK" => theme::GOOD,
                        "WARN" => theme::WARN,
                        _ => theme::BAD,
                    };
                    let (spot, _) = ui.allocate_exact_size(egui::vec2(14.0, 18.0), egui::Sense::hover());
                    theme::dot(ui, spot.center(), color);
                    ui.label(&check.name);
                    ui.add(egui::Label::new(RichText::new(&check.detail).font(theme::mono(12.0)).color(if check.status == "OK" { theme::DIM } else { theme::TEXT })).wrap());
                    ui.end_row();
                }
            });
        });
        ui.add_space(18.0);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::eyebrow(ui, "MAINTENANCE");
            ui.add_space(4.0);
            let tool = |ui: &mut egui::Ui, name: &str, about: &str, action: &str, enabled: bool| -> bool {
                let mut clicked = false;
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width((ui.available_width() - 190.0).max(200.0));
                        ui.label(RichText::new(name).size(15.5).strong());
                        ui.add(egui::Label::new(RichText::new(about).color(theme::DIM).small()).wrap());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        clicked = theme::button(ui, action, enabled).clicked();
                    });
                });
                ui.add_space(8.0);
                clicked
            };
            if tool(ui, "Prepare shaders", "Translate every shader of the title into the cache, so no run has to. A few minutes, once per version.  victory shaders", "Prepare", idle) {
                self.enqueue_victory(Kind::Shaders, &["shaders"]);
            }
            if tool(ui, "Warm cache", "Make the pipelines earlier runs drew with before a run needs them.  victory warm --used --settle", "Warm", idle) {
                self.enqueue_victory(Kind::Warm, &["warm", "--used", "--settle"]);
            }
            let verdict = match self.selftest {
                Some(true) => "Self-test  ·  PASS",
                Some(false) => "Self-test  ·  FAIL",
                None => "Self-test",
            };
            if tool(ui, verdict, "The frame path end to end with a stand-in of our own. Needs no title.  victory selftest", "Run", idle) {
                self.enqueue_victory(Kind::SelfTest, &["selftest"]);
            }
            if tool(ui, "Run report", "The newest run's report, to the clipboard. Numbers and Rox-Box's own words only: safe for a public issue.  victory report", "Copy", idle) {
                self.enqueue_victory(Kind::Report, &["report"]);
            }
            if !self.confirm_reset {
                if tool(ui, "Reset pipeline cache", "For a cache gone stale after a driver or version change. Removes the pipeline library and the warm-up's marks; what runs recorded stays.", "Reset", idle) {
                    self.confirm_reset = true;
                }
            } else {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Remove the pipeline library? Warm cache makes it again.").color(theme::WARN));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::button(ui, "Keep it", true).clicked() {
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

    fn settings_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        theme::page_title(ui, "Settings", "Kept in local\\launcher.toml beside the engine. Nothing here leaves this PC.");
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::eyebrow(ui, "WHEN A TITLE STARTS");
            if let Some(choice) = theme::segmented(ui, &[("stay", "Keep the launcher open"), ("minimize", "Minimize it"), ("close", "Close it")], &self.lib.on_play) {
                self.lib.on_play = choice.to_string();
            }
            ui.label(
                RichText::new(match self.lib.on_play.as_str() {
                    "minimize" => "Minimized, the launcher draws nothing and comes back when the title ends.",
                    "close" => "The launcher exits and uses no memory at all. The run's output is then only in its log file (Log is switched on for it).",
                    _ => "The launcher stays and draws only when something changes: once a second for the run's clock.",
                })
                .color(theme::DIM)
                .small(),
            );
            if self.source_tree() {
                ui.add_space(14.0);
                theme::eyebrow(ui, "ENGINE BUILD");
                let builds = self.home.as_ref().map(|h| h.builds()).unwrap_or_default();
                let mut choices = vec![("", "First built")];
                choices.extend(builds.iter().map(|b| (*b, *b)));
                if let Some(choice) = theme::segmented(ui, &choices, &self.lib.build) {
                    self.lib.build = choice.to_string();
                }
            }
            ui.add_space(14.0);
            theme::eyebrow(ui, "EXTRA SWITCHES");
            ui.add(egui::TextEdit::singleline(&mut self.lib.more).font(theme::mono(12.5)).hint_text("VICTORY_NAME=value;VICTORY_OTHER=value").desired_width(f32::INFINITY).margin(egui::vec2(10.0, 8.0)));
            ui.label(RichText::new("Passed to every run as play.cmd -More. docs/switches.md lists them.").color(theme::DIM).small());
        });
        ui.add_space(18.0);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::eyebrow(ui, "CONFIG FILE  ·  VICTORY.TOML");
            let default = self.home.as_ref().and_then(|h| std::fs::read_to_string(h.config_file()).ok()).and_then(|t| config::game_dir_of(&t));
            let is_default = default.is_some() && default.as_deref() == self.lib.active.as_deref();
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_width((ui.available_width() - 230.0).max(200.0));
                    ui.label(match (&default, is_default) {
                        (_, true) => "It names the active title's folder.".to_string(),
                        (Some(_), false) => "It names another folder. Playing uses the active title for that run all the same.".to_string(),
                        (None, _) => "It names no folder yet.".to_string(),
                    });
                    ui.label(RichText::new("Writing sets game_dir only. Every other line stays, and a timestamped copy of the file is kept.").color(theme::DIM).small());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::button(ui, "Write active title", self.lib.active.is_some() && !is_default && self.home.is_some()).clicked()
                        && let (Some(home), Some(active)) = (&self.home, &self.lib.active)
                    {
                        self.note = match config::write_game_dir(&home.config_file(), active) {
                            Ok(done) => done,
                            Err(e) => format!("victory.toml could not be written: {e}"),
                        };
                    }
                });
            });
        });
        ui.add_space(18.0);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::eyebrow(ui, "VERSION");
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Engine {}", self.version.clone().unwrap_or_else(|| "-".to_string()))).size(15.5).strong());
                ui.label(RichText::new(format!("Launcher {}", env!("CARGO_PKG_VERSION"))).color(theme::DIM));
                match (&self.latest, &self.version) {
                    (Some(Ok(tag)), Some(local)) if runner::newer(&runner::version_of(tag).unwrap_or_default(), local) => theme::badge(ui, &format!("{tag} is out"), theme::WARN),
                    (Some(Ok(tag)), _) => theme::badge(ui, &format!("newest: {tag}"), theme::GOOD),
                    (Some(Err(e)), _) => {
                        ui.label(RichText::new(e).color(theme::DIM).small());
                    }
                    (None, _) => {}
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::button(ui, "Releases page", true).clicked() {
                        ctx.open_url(egui::OpenUrl::new_tab(RELEASES_PAGE));
                    }
                    if theme::button(ui, "Check for a newer release", !self.busy()).on_hover_text("Asks GitHub for the newest release's name. Nothing is downloaded or installed.").clicked() {
                        self.latest = None;
                        self.queue.push_back(runner::latest_release());
                    }
                });
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
        }
    }
    let host = Host::read();
    println!("ram_mb: {:?}  commit_limit_mb: {:?}", host.ram_mb, host.commit_limit_mb);
    println!("page file: {:?}", host.page_file);
    println!("windows: {}", host.windows);
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
            .with_inner_size([1180.0, 800.0])
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
    }

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
