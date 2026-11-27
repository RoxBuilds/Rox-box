//! Child processes: what the launcher starts (victory.exe, or play.ps1 the
//! way play.cmd starts it), their output line by line, and how they ended.
//! One at a time. No window of their own; nothing is typed into them.

use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::config::Library;

/// No console window for a child of a windowed program.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What a job is for: decides the status word and what happens when it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Launch,
    Prepare,
    Shaders,
    Warm,
    SelfTest,
    Check,
    Report,
    Version,
    Latest,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Launch => "title",
            Kind::Prepare => "prepare",
            Kind::Shaders => "shaders",
            Kind::Warm => "warm",
            Kind::SelfTest => "selftest",
            Kind::Check => "check",
            Kind::Report => "report",
            Kind::Version => "version",
            Kind::Latest => "latest release",
        }
    }

    /// Jobs whose output is for the launcher, not for the console drawer.
    pub fn quiet(self) -> bool {
        matches!(self, Kind::Check | Kind::Report | Kind::Version | Kind::Latest)
    }
}

/// A command line, ready to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub kind: Kind,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

impl Spec {
    /// The command as a person would type it (for the console drawer).
    pub fn shown(&self) -> String {
        let quote = |s: &str| if s.contains(' ') || s.contains(';') { format!("\"{s}\"") } else { s.to_string() };
        let name = self.program.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        std::iter::once(name).chain(self.args.iter().map(|a| quote(a))).collect::<Vec<_>>().join(" ")
    }
}

/// victory.exe with `args`. `title` (the active title folder) is given as
/// `VICTORY_GAME_DIR`, which wins over the file, so that every command is
/// about the title chosen in the launcher.
pub fn victory(kind: Kind, exe: &Path, args: &[&str], title: Option<&str>) -> Spec {
    Spec {
        kind,
        program: exe.to_path_buf(),
        args: args.iter().map(|a| a.to_string()).collect(),
        envs: title.map(|t| ("VICTORY_GAME_DIR".to_string(), t.to_string())).into_iter().collect(),
        cwd: exe.parent().map(Path::to_path_buf),
    }
}

/// The options of play.ps1 the toggles stand for (not the title, not
/// `-Prepare`): the same words a person gives play.cmd.
pub fn play_options(lib: &Library, source_tree: bool) -> Vec<String> {
    let mut args = Vec::new();
    if lib.fast {
        args.push("-Fast".to_string());
    }
    if lib.safe {
        args.push("-Safe".to_string());
    }
    if lib.log {
        args.push("-Log".to_string());
    }
    if source_tree && !lib.build.is_empty() {
        args.extend(["-Build".to_string(), lib.build.clone()]);
    }
    if lib.console == "SeriesS" {
        args.extend(["-Console".to_string(), "SeriesS".to_string()]);
    }
    let more = lib.more.trim();
    if !more.is_empty() {
        args.extend(["-More".to_string(), more.to_string()]);
    }
    args
}

/// play.ps1 started as play.cmd starts it (`powershell.exe -NoLogo
/// -NoProfile -ExecutionPolicy Bypass -File play.ps1 ...`: the execution
/// policy is bypassed for that one process only). The title folder is the
/// script's first argument, "the game folder for this run". With `prepare`
/// it is `-Prepare`: shaders and the warm-up, and nothing is started.
pub fn play(script: &Path, lib: &Library, source_tree: bool, prepare: bool) -> Spec {
    let mut args: Vec<String> =
        ["-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File"].iter().map(|a| a.to_string()).collect();
    args.push(script.to_string_lossy().into_owned());
    if let Some(title) = &lib.active {
        args.push(title.clone());
    }
    args.extend(play_options(lib, source_tree));
    if prepare {
        args.push("-Prepare".to_string());
    }
    Spec {
        kind: if prepare { Kind::Prepare } else { Kind::Launch },
        program: PathBuf::from("powershell.exe"),
        args,
        // play.cmd's own pause is not ours, but a script that looks for it finds it off.
        envs: vec![("ROXBOX_NO_PAUSE".to_string(), "1".to_string())],
        cwd: script.parent().map(Path::to_path_buf),
    }
}

/// What a running job says.
#[derive(Debug)]
pub enum Event {
    Line(String),
    /// The exit code; `None` when the process was ended from outside.
    Exit(Option<i32>),
    /// It could not be started.
    Failed(String),
}

/// A job that was started.
pub struct Job {
    pub spec: Spec,
    pub pid: u32,
    pub events: Receiver<Event>,
    pub started: std::time::Instant,
    /// Everything it printed (quiet jobs are read from here when they end).
    pub output: Vec<String>,
}

/// Starts `spec`. `wake` is called whenever there is something to read.
pub fn start(spec: Spec, wake: impl Fn() + Send + Sync + Clone + 'static) -> Result<Job, String> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    command.creation_flags(CREATE_NO_WINDOW);
    for (k, v) in &spec.envs {
        command.env(k, v);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|e| format!("{}: {e}", spec.program.display()))?;
    let pid = child.id();
    let (tx, events) = channel();
    let mut readers = Vec::new();
    let out = child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>);
    let err = child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>);
    for stream in [out, err].into_iter().flatten() {
        readers.push(pump(stream, tx.clone(), wake.clone()));
    }
    std::thread::spawn(move || {
        let status = child.wait();
        for r in readers {
            let _ = r.join();
        }
        let _ = tx.send(match status {
            Ok(s) => Event::Exit(s.code()),
            Err(e) => Event::Failed(e.to_string()),
        });
        wake();
    });
    Ok(Job { spec, pid, events, started: std::time::Instant::now(), output: Vec::new() })
}

fn pump(
    stream: Box<dyn std::io::Read + Send>,
    tx: Sender<Event>,
    wake: impl Fn() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&bytes).trim_end_matches(['\r', '\n']).to_string();
                    if tx.send(Event::Line(line)).is_err() {
                        break;
                    }
                    wake();
                }
            }
        }
    })
}

/// Ends a job and everything it started (play.ps1 starts victory.exe, which
/// starts the title): `taskkill /PID <pid> /T /F`.
pub fn stop(pid: u32) -> Result<(), String> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("taskkill ended with {status}")) }
}

/// The newest release's tag, asked of GitHub's public API with the curl.exe
/// Windows ships. Read-only, and only when the person presses the button.
pub fn latest_release() -> Spec {
    Spec {
        kind: Kind::Latest,
        program: PathBuf::from("curl.exe"),
        args: ["-s", "-m", "15", "-H", "User-Agent: roxbox-launcher", "-H", "Accept: application/vnd.github+json", crate::RELEASE_API]
            .iter()
            .map(|a| a.to_string())
            .collect(),
        envs: Vec::new(),
        cwd: None,
    }
}

/// `"tag_name": "<tag>"` out of the API's answer.
pub fn tag_of(json: &str) -> Option<String> {
    let rest = &json[json.find("\"tag_name\"")? + 10..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let tag = &rest[..rest.find('"')?];
    (!tag.is_empty() && tag.chars().all(|c| c.is_ascii_alphanumeric() || ".-_+".contains(c))).then(|| tag.to_string())
}

/// `0.2.0` out of `victory 0.2.0` or `v0.2.0`.
pub fn version_of(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.split('.').count() >= 2 && w.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())))
        .map(str::to_string)
}

/// Whether `latest` is a later version than `local` (both `a.b.c`).
pub fn newer(latest: &str, local: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(latest) > parts(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> Library {
        Library { titles: vec!["D:\\My Titles\\One".into()], active: Some("D:\\My Titles\\One".into()), ..Library::default() }
    }

    #[test]
    fn the_toggles_become_play_options() {
        let mut l = lib();
        l.log = false;
        assert!(play_options(&l, true).is_empty());
        l.fast = true;
        l.log = true;
        l.safe = true;
        l.build = "release".into();
        l.console = "SeriesS".into();
        l.more = " VICTORY_AMBIENT=0.8;VICTORY_PIPELINE_BOOK=0 ".into();
        assert_eq!(
            play_options(&l, true),
            ["-Fast", "-Safe", "-Log", "-Build", "release", "-Console", "SeriesS", "-More", "VICTORY_AMBIENT=0.8;VICTORY_PIPELINE_BOOK=0"]
        );
        assert!(!play_options(&l, false).contains(&"-Build".to_string()), "-Build is for the source tree only");
    }

    #[test]
    fn play_is_started_as_play_cmd_starts_it() {
        let script = Path::new("C:\\rox\\scripts\\play.ps1");
        let spec = play(script, &lib(), true, false);
        assert_eq!(spec.kind, Kind::Launch);
        assert_eq!(spec.program, PathBuf::from("powershell.exe"));
        assert_eq!(
            spec.args,
            ["-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "C:\\rox\\scripts\\play.ps1", "D:\\My Titles\\One", "-Log"]
        );
        assert_eq!(spec.shown(), "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File C:\\rox\\scripts\\play.ps1 \"D:\\My Titles\\One\" -Log");
        let prepare = play(script, &lib(), true, true);
        assert_eq!(prepare.kind, Kind::Prepare);
        assert_eq!(prepare.args.last().map(String::as_str), Some("-Prepare"));
    }

    #[test]
    fn victory_commands_are_about_the_active_title() {
        let spec = victory(Kind::Warm, Path::new("C:\\rox\\victory.exe"), &["warm", "--used", "--settle"], Some("D:\\T"));
        assert_eq!(spec.args, ["warm", "--used", "--settle"]);
        assert_eq!(spec.envs, [("VICTORY_GAME_DIR".to_string(), "D:\\T".to_string())]);
        assert!(victory(Kind::SelfTest, Path::new("v.exe"), &["selftest"], None).envs.is_empty());
    }

    #[test]
    fn versions_and_tags_are_read() {
        assert_eq!(tag_of("{\"url\":\"x\",\"tag_name\": \"v0.2.0\",\"name\":\"n\"}").as_deref(), Some("v0.2.0"));
        assert_eq!(tag_of("{\"message\":\"Not Found\"}"), None);
        assert_eq!(tag_of("{\"tag_name\":\"<script>\"}"), None);
        assert_eq!(version_of("victory 0.2.0\n").as_deref(), Some("0.2.0"));
        assert_eq!(version_of("v0.10.1").as_deref(), Some("0.10.1"));
        assert_eq!(version_of("no version here"), None);
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.1.9", "0.2.0"));
    }

    #[test]
    fn a_job_gives_its_lines_and_its_exit_code() {
        let spec = Spec {
            kind: Kind::Check,
            program: PathBuf::from("cmd.exe"),
            args: ["/c", "echo one& echo two 1>&2& exit /b 7"].iter().map(|a| a.to_string()).collect(),
            envs: Vec::new(),
            cwd: None,
        };
        let job = start(spec, || {}).unwrap();
        let mut lines = Vec::new();
        let code = loop {
            match job.events.recv().unwrap() {
                Event::Line(l) => lines.push(l.trim().to_string()),
                Event::Exit(code) => break code,
                Event::Failed(e) => panic!("{e}"),
            }
        };
        lines.sort();
        assert_eq!(lines, ["one", "two"]);
        assert_eq!(code, Some(7));
        assert!(start(Spec { kind: Kind::Check, program: "no-such-program-here.exe".into(), args: vec![], envs: vec![], cwd: None }, || {}).is_err());
    }
}
