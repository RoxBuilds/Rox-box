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
    /// An update's checksum file, its package, and the unpacking of it.
    UpdateChecksum,
    UpdatePackage,
    UpdateUnpack,
    /// The save folder packed into a zip.
    Backup,
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
            Kind::UpdateChecksum => "update checksum",
            Kind::UpdatePackage => "update download",
            Kind::UpdateUnpack => "update unpack",
            Kind::Backup => "save backup",
        }
    }

    /// What a person started and watches: it turns PLAY into STOP.
    pub fn foreground(self) -> bool {
        matches!(self, Kind::Launch | Kind::Prepare | Kind::Shaders | Kind::Warm | Kind::SelfTest)
    }

    /// Jobs whose output is for the launcher, not for the console drawer.
    pub fn quiet(self) -> bool {
        matches!(self, Kind::Check | Kind::Report | Kind::Version | Kind::Latest | Kind::UpdateChecksum)
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
    /// Started with no pipes to the launcher, so that it goes on when the
    /// launcher closes (its output is then only in its own log file).
    pub detached: bool,
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
        detached: false,
    }
}

/// Whether a launcher script's text knows `-Shots` (it came with a later
/// engine: an older script stops at an option it does not know).
pub fn knows_shots(script: &str) -> bool {
    script.contains("]$Shots")
}

/// The options of play.ps1 the toggles stand for (not the title, not
/// `-Prepare`, not `-Shots`): the same words a person gives play.cmd.
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
    } else if lib.shots && std::fs::read_to_string(script).is_ok_and(|text| knows_shots(&text)) {
        // Pictures are of a run, not of its preparation.
        args.extend(["-Shots".to_string(), lib.shots_every.to_string()]);
    }
    Spec {
        kind: if prepare { Kind::Prepare } else { Kind::Launch },
        program: PathBuf::from("powershell.exe"),
        args,
        // play.cmd's own pause is not ours, but a script that looks for it finds it off.
        envs: vec![("ROXBOX_NO_PAUSE".to_string(), "1".to_string())],
        cwd: script.parent().map(Path::to_path_buf),
        detached: false,
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
    /// The Windows job the process and everything it starts belong to.
    tree: Option<Tree>,
}

/// Starts `spec`. `wake` is called whenever there is something to read.
pub fn start(spec: Spec, wake: impl Fn() + Send + Sync + Clone + 'static) -> Result<Job, String> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args).stdin(Stdio::null());
    if spec.detached {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    } else {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    command.creation_flags(CREATE_NO_WINDOW);
    for (k, v) in &spec.envs {
        command.env(k, v);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|e| format!("{}: {e}", spec.program.display()))?;
    let pid = child.id();
    // Before it has started anything of its own: what it starts is then in the job too.
    let tree = Tree::of(&child);
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
    Ok(Job { spec, pid, events, started: std::time::Instant::now(), output: Vec::new(), tree })
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

/// A Windows job object holding a started process and every process it
/// starts, however deep, also those whose parent has ended since. It is how
/// Stop ends all of them at once. Closing the handle ends nothing: a title
/// goes on when the launcher closes.
struct Tree(usize);

// SAFETY: a job object's handle may be used from any thread.
unsafe impl Send for Tree {}

impl Tree {
    fn of(child: &std::process::Child) -> Option<Tree> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        // SAFETY: no attributes and no name; the child's handle is alive.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            if AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
                CloseHandle(job);
                return None;
            }
            Some(Tree(job as usize))
        }
    }

    fn end(&self) -> bool {
        // SAFETY: the handle is the job's, open for as long as `self` lives.
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0 as _, 1) != 0 }
    }

    /// How many processes of the job are alive.
    #[cfg(test)]
    fn alive(&self) -> Option<u32> {
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation, QueryInformationJobObject,
        };
        // SAFETY: plain numbers; all zeros is a value of it.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is the structure the class asks for, of its size.
        let ok = unsafe {
            QueryInformationJobObject(
                self.0 as _,
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(info.ActiveProcesses)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // SAFETY: ours, closed once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0 as _) };
    }
}

impl Job {
    /// Ends the job and everything it started (play.ps1 starts victory.exe,
    /// which starts the title): the whole Windows job at once, so nothing
    /// stays behind, also not a process whose parent ended first. Where the
    /// job could not be made, `taskkill /PID <pid> /T /F`.
    pub fn stop(&self) -> Result<(), String> {
        if self.tree.as_ref().is_some_and(Tree::end) {
            return Ok(());
        }
        let status = Command::new("taskkill")
            .args(["/PID", &self.pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() { Ok(()) } else { Err(format!("taskkill ended with {status}")) }
    }

    /// Processes of the job still alive (none known: the job was not made).
    #[cfg(test)]
    pub fn alive(&self) -> Option<u32> {
        self.tree.as_ref().and_then(Tree::alive)
    }
}

/// The newest release's tag, asked of GitHub's public API with the curl.exe
/// Windows ships. Read-only, and only when the person presses the button.
pub fn latest_release() -> Spec {
    // `ROXBOX_RELEASE_JSON` names a file to read instead (the API's answer, saved):
    // the banner and the update's steps can then be looked at without the network.
    if let Some(file) = std::env::var_os("ROXBOX_RELEASE_JSON") {
        return Spec {
            kind: Kind::Latest,
            program: crate::update::system_tool("cmd.exe"),
            args: vec!["/c".into(), "type".into(), file.to_string_lossy().into_owned()],
            envs: Vec::new(),
            cwd: None,
            detached: false,
        };
    }
    Spec {
        kind: Kind::Latest,
        program: crate::update::system_tool("curl.exe"),
        args: ["-s", "-m", "15", "-H", "User-Agent: roxbox-launcher", "-H", "Accept: application/vnd.github+json", crate::RELEASE_API]
            .iter()
            .map(|a| a.to_string())
            .collect(),
        envs: Vec::new(),
        cwd: None,
        detached: false,
    }
}

/// A file of a release fetched to `to` with the curl.exe Windows ships:
/// redirects followed, nothing written on an error answer.
pub fn download(kind: Kind, url: &str, to: &Path) -> Spec {
    Spec {
        kind,
        program: crate::update::system_tool("curl.exe"),
        args: ["-s", "-S", "-L", "--fail", "-m", "900", "-H", "User-Agent: roxbox-launcher", "-o", &to.to_string_lossy(), url]
            .iter()
            .map(|a| a.to_string())
            .collect(),
        envs: Vec::new(),
        cwd: None,
        detached: false,
    }
}

/// A downloaded package unpacked into `to` with the tar.exe Windows ships.
pub fn unpack(zip: &Path, to: &Path) -> Spec {
    Spec {
        kind: Kind::UpdateUnpack,
        program: crate::update::system_tool("tar.exe"),
        args: vec!["-x".into(), "-f".into(), zip.to_string_lossy().into_owned(), "-C".into(), to.to_string_lossy().into_owned()],
        envs: Vec::new(),
        cwd: None,
        detached: false,
    }
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
    fn shots_are_asked_for_only_of_a_script_that_knows_them() {
        assert!(knows_shots("    [ValidateRange(1, 86400)][int]$Shots,\n"));
        assert!(!knows_shots("param([switch]$Fast, [switch]$Log)\n# -Shots is not here"));
        let dir = std::env::temp_dir().join(format!("roxbox-launcher-shots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (new, old) = (dir.join("new.ps1"), dir.join("old.ps1"));
        std::fs::write(&new, "param([int]$Shots)").unwrap();
        std::fs::write(&old, "param([switch]$Fast)").unwrap();
        let mut l = lib();
        l.shots = true;
        l.shots_every = 30;
        assert_eq!(play(&new, &l, false, false).args[play(&new, &l, false, false).args.len() - 2..], ["-Shots", "30"]);
        assert!(!play(&old, &l, false, false).args.contains(&"-Shots".to_string()), "an older script is not given it");
        assert!(!play(&new, &l, false, true).args.contains(&"-Shots".to_string()), "nor is a preparation");
        l.shots = false;
        assert!(!play(&new, &l, false, false).args.contains(&"-Shots".to_string()));
    }

    #[test]
    fn victory_commands_are_about_the_active_title() {
        let spec = victory(Kind::Warm, Path::new("C:\\rox\\victory.exe"), &["warm", "--used", "--settle"], Some("D:\\T"));
        assert_eq!(spec.args, ["warm", "--used", "--settle"]);
        assert_eq!(spec.envs, [("VICTORY_GAME_DIR".to_string(), "D:\\T".to_string())]);
        assert!(victory(Kind::SelfTest, Path::new("v.exe"), &["selftest"], None).envs.is_empty());
    }

    #[test]
    fn versions_are_read_and_compared() {
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
            detached: false,
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
        assert!(start(Spec { kind: Kind::Check, program: "no-such-program-here.exe".into(), args: vec![], envs: vec![], cwd: None, detached: false }, || {}).is_err());
    }

    #[test]
    fn stop_ends_the_whole_tree() {
        // A shell that starts a shell that starts a program that waits: three deep.
        let spec = Spec {
            kind: Kind::Launch,
            program: PathBuf::from("cmd.exe"),
            args: ["/c", "cmd.exe /c ping -n 60 127.0.0.1 >nul"].iter().map(|a| a.to_string()).collect(),
            envs: Vec::new(),
            cwd: None,
            detached: false,
        };
        let job = start(spec, || {}).unwrap();
        // Until the inner processes are there.
        for _ in 0..200 {
            if job.alive().is_some_and(|n| n >= 3) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(job.alive().is_some_and(|n| n >= 3), "the job holds what the shell started: {:?}", job.alive());
        job.stop().unwrap();
        let code = loop {
            match job.events.recv().unwrap() {
                Event::Exit(code) => break code,
                Event::Line(_) => {}
                Event::Failed(e) => panic!("{e}"),
            }
        };
        assert_eq!(code, Some(1));
        assert_eq!(job.alive(), Some(0), "nothing of the tree is left");
    }
}
