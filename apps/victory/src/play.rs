//! `victory play`: a run as a player starts it, in one command and with no
//! script: the check, the preparation a run needs, then the run, its output
//! also written to a log file.
//!
//! 1. `victory check` (in this process): a run does not start when something
//!    it cannot do without is missing.
//! 2. Preparation, each step only when it is stale: the title's shaders
//!    (`victory shaders`, once per build of victory.exe and game folder,
//!    stamped in `<cache>\shaders\prepared-stamp.txt`), and, with
//!    `--warm yes`, the pipeline warm-up of earlier runs (`victory warm --used
//!    --settle`, stamped by itself in `<cache>\pipelines\warm-stamp.txt`). The
//!    stamps are the ones `scripts\play.ps1` reads and writes, so the two can
//!    be used in turn without preparing again.
//! 3. The run: `victory run <exe> --execute --console <model>`, in a process of
//!    its own, with this process's environment (the switches the launcher or
//!    the person set) and the run settings of the options below. With
//!    `--log`, everything it prints also goes to
//!    `%LOCALAPPDATA%\victory\logs\<UTC time>-<model>.log`, named as the run
//!    report of the same run.
//!
//! Each step is a process of this same executable, as `play.ps1` starts them:
//! a translator or a driver that crashes takes only its own step.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::warm::{self, ExeIdentity};

/// Where `victory shaders` having finished is stamped, under the cache's
/// `shaders` folder (the name `play.ps1` uses).
const PREPARED_STAMP: &str = "prepared-stamp.txt";
/// What earlier runs recorded for the warm-up to make, under `pipelines`.
const RECORDED: &str = "known-pipelines.bin";

/// The variables a preparation step keeps: paths and the checks' own
/// switches. Every other `VICTORY_*` (a run's behaviour) is taken out, as
/// `play.ps1` does, so that a run's switches do not change what is prepared.
/// Written after `VICTORY_` (so that `cargo xtask switches` does not take this
/// list for a place they are read).
const PATH_VARIABLES: &[&str] = &[
    "CONFIG",
    "GAME_DIR",
    "CACHE_DIR",
    "DATA_DIR",
    "PATCHES_FILE",
    "SHADER_PROFILE",
    "BCPACK_TABLES",
    "SKIP_PREFLIGHT",
    "SKIP_CPU_CHECK",
    "REPORT",
];

#[derive(clap::Args, Debug, Clone)]
pub struct Play {
    /// The title executable (default: the only .exe in the configured game
    /// folder).
    exe: Option<PathBuf>,
    /// The low-memory preset: the smaller console model at a 1920x1080
    /// display, each unless given.
    #[arg(long)]
    fast: bool,
    /// The console model the title sees: series-x or series-s (series-x,
    /// series-s with --fast).
    #[arg(long, value_parser = ["series-x", "series-s"])]
    console: Option<String>,
    /// The display size the title is told, WIDTHxHEIGHT.
    #[arg(long, value_parser = size)]
    display: Option<String>,
    /// The output size, WIDTHxHEIGHT (VICTORY_OUTPUT).
    #[arg(long, value_parser = size)]
    output: Option<String>,
    /// Only the long-standing defaults, without the newer optimisations
    /// (VICTORY_DEFAULTS=safe).
    #[arg(long)]
    safe: bool,
    /// Also write everything the run prints to a log file.
    #[arg(long)]
    log: bool,
    /// A half-size picture every this many seconds, into <data_dir>\shots.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    shots: Option<u32>,
    /// Run the pipeline warm-up when it is stale (10 to 40 minutes); without
    /// it a stale warm-up is only reported.
    #[arg(long)]
    warm: bool,
    /// Prepare only: start nothing.
    #[arg(long, conflicts_with = "no_prepare")]
    prepare_only: bool,
    /// Skip the preparation.
    #[arg(long)]
    no_prepare: bool,
}

fn size(text: &str) -> Result<String, String> {
    let ok = text.split_once('x').is_some_and(|(w, h)| {
        [w, h].iter().all(|n| !n.is_empty() && n.len() <= 5 && n.chars().all(|c| c.is_ascii_digit()))
    });
    if ok { Ok(text.to_owned()) } else { Err(format!("'{text}' is not WIDTHxHEIGHT, such as 1920x1080")) }
}

impl Play {
    fn console(&self) -> &str {
        self.console.as_deref().unwrap_or(if self.fast { "series-s" } else { "series-x" })
    }

    fn display(&self) -> Option<&str> {
        self.display.as_deref().or(self.fast.then_some("1920x1080"))
    }

    /// The run settings these options give, as variables for the run.
    fn settings(&self, data_dir: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut set = |k: &str, v: String| out.push((k.to_owned(), v));
        if self.safe {
            set("VICTORY_DEFAULTS", "safe".into());
        }
        if let Some(d) = self.display() {
            set("VICTORY_DISPLAY_MODE", d.into());
        }
        if let Some(o) = &self.output {
            // `VICTORY_OUTPUT=WxH`: the output size, as `play.cmd -Output` gives it; a group of the user's own
            // patch file (`VICTORY_PATCHES_FILE`) can be chosen by it.
            set("VICTORY_OUTPUT", o.clone());
        }
        if let Some(every) = self.shots {
            set("VICTORY_CAPTURE", data_dir.join("shots").display().to_string());
            set("VICTORY_CAPTURE_EVERY_S", every.to_string());
        }
        out
    }
}

/// A stamp's `name=value` lines (comments and other lines left out).
fn read_stamp(path: &Path) -> Option<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect(),
    )
}

/// A path as the stamps compare it: no `\\?\` prefix, no trailing
/// separator, case folded (Windows paths).
fn path_key(p: &str) -> String {
    p.trim_start_matches(r"\\?\").trim_end_matches(['\\', '/']).to_lowercase()
}

/// What differs between a stamp and this exe (and the game folder, for the
/// shaders' stamp); empty when it is current, `["no stamp"]` when there is
/// none.
fn differs(stamp: Option<&BTreeMap<String, String>>, exe: &Path, id: &ExeIdentity, game_dir: Option<&Path>) -> Vec<&'static str> {
    let Some(s) = stamp else { return vec!["no stamp"] };
    let get = |k: &str| s.get(k).map(String::as_str).unwrap_or("");
    let mut out = Vec::new();
    if path_key(get("exe")) != path_key(&exe.display().to_string()) {
        out.push("the exe");
    }
    if get("build") != warm::BUILD {
        out.push("the build");
    }
    if get("size") != id.size.to_string() {
        out.push("size");
    }
    if get("modified") != id.modified.to_string() {
        out.push("modified");
    }
    if get("head_sha256") != id.head_sha256 {
        out.push("head_sha256");
    }
    if let Some(dir) = game_dir
        && path_key(get("game_dir")) != path_key(&dir.display().to_string())
    {
        out.push("the game folder");
    }
    out
}

/// A process of this exe for a step, with `args`; with `paths_only`, the
/// run's behaviour switches are taken out of its environment.
fn step(me: &Path, args: &[&str], paths_only: bool) -> Command {
    let mut c = Command::new(me);
    c.args(args);
    if paths_only {
        for (key, _) in std::env::vars_os() {
            let k = key.to_string_lossy().to_ascii_uppercase();
            if k.strip_prefix("VICTORY_").is_some_and(|rest| !PATH_VARIABLES.contains(&rest)) {
                c.env_remove(&key);
            }
        }
    }
    c
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Translates the shaders when the stamp says they are stale. Whether a run
/// can go on (a failed translation is said; the run translates what it needs).
fn prepare_shaders(me: &Path, id: &ExeIdentity, shaders: &Path, game_dir: &Path) -> Result<bool> {
    let path = shaders.join(PREPARED_STAMP);
    let why = differs(read_stamp(&path).as_ref(), me, id, Some(game_dir));
    if why.is_empty() {
        println!("Shaders: prepared for this build and game folder.");
        return Ok(true);
    }
    println!();
    println!("Shaders: translating the title's shaders ({}).", why.join(", "));
    println!("This is done once per Rox-box version and game folder and takes a few minutes; the run is much smoother afterwards.");
    let status = step(me, &["shaders"], true).status().context("starting `victory shaders`")?;
    if !status.success() {
        println!("Translating the shaders failed (exit code {}); no stamp was written.", exit_code(status));
        return Ok(false);
    }
    let text = format!(
        "# victory play: victory shaders finished for this executable and game folder\n{}game_dir={}\n",
        id.lines(me),
        game_dir.display()
    );
    if let Err(e) = std::fs::write(&path, text) {
        println!("The shaders are translated, but the stamp ({}) could not be written: {e}", path.display());
    }
    Ok(true)
}

/// The pipeline warm-up, when its stamp is stale and earlier runs recorded
/// pipelines: run with `go`, else only reported. Whether it went well (or
/// was not needed).
fn prepare_pipelines(me: &Path, id: &ExeIdentity, pipelines: &Path, go: bool) -> Result<bool> {
    let why = differs(read_stamp(&pipelines.join(warm::STAMP)).as_ref(), me, id, None);
    if why.is_empty() {
        println!("Pipelines: warmed with this build.");
        return Ok(true);
    }
    if !pipelines.join(RECORDED).exists() {
        println!("Pipelines: none recorded yet. This run compiles them as the title draws (with stutter) and records them; the next preparation warms them up.");
        return Ok(true);
    }
    if !go {
        println!(
            "WARNING: Pipelines: the warm-up is stale ({}): this run makes them again as the title draws, with stutter. \
             Warm them up first with  victory play --warm  (or the launcher's Prepare first), 10 to 40 minutes.",
            why.join(", ")
        );
        return Ok(true);
    }
    println!();
    println!("Pipelines: warming up the pipelines of your earlier runs ({}); 10 to 40 minutes.", why.join(", "));
    let status = step(me, &["warm", "--used", "--settle"], true).status().context("starting `victory warm`")?;
    if !status.success() {
        println!("The warm-up did not finish (exit code {}).", exit_code(status));
        return Ok(false);
    }
    Ok(true)
}

/// The folder of log files: `%LOCALAPPDATA%\victory\logs` (where `play.ps1`
/// writes them), else beside the data folder.
fn logs_dir(data_dir: &Path) -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(base) => PathBuf::from(base).join("victory").join("logs"),
        None => data_dir.join("logs"),
    }
}

/// Starts the run and waits for it. With `log`, its output also goes to that
/// file (with a header naming the command and the switches set). Its exit
/// code.
fn run_title(mut run: Command, log: Option<(PathBuf, String)>) -> Result<i32> {
    let Some((file, header)) = log else {
        return Ok(exit_code(run.status().context("starting `victory run`")?));
    };
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    }
    let mut out = std::fs::File::create(&file).with_context(|| file.display().to_string())?;
    out.write_all(header.as_bytes())?;
    println!("Logging to {}", file.display());
    let out = Arc::new(Mutex::new(out));
    let mut child = run
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting `victory run`")?;
    // Each stream line by line into the file and on to ours, so that a
    // window over this command sees the run as it goes.
    let tee = |stream: Box<dyn std::io::Read + Send>, to_stderr: bool| {
        let out = Arc::clone(&out);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
                if let Ok(mut f) = out.lock() {
                    let _ = f.write_all(&line);
                }
                let _ = if to_stderr { std::io::stderr().write_all(&line) } else { std::io::stdout().write_all(&line) };
                line.clear();
            }
        })
    };
    let threads = [
        tee(Box::new(child.stdout.take().expect("piped")), false),
        tee(Box::new(child.stderr.take().expect("piped")), true),
    ];
    let status = child.wait()?;
    for t in threads {
        let _ = t.join();
    }
    let code = exit_code(status);
    println!("Exit code {code}. Log: {}", file.display());
    Ok(code)
}

pub fn run(p: Play) -> Result<()> {
    let me = std::env::current_exe().context("finding victory.exe")?;
    // 1. Everything a run needs.
    crate::preflight::check(p.exe.as_deref())?;
    let cfg = victory_core::Config::load()?;
    let Some(game_dir) = cfg.game_dir.clone() else { bail!("no game folder is set (victory check says how)") };

    // 2. Preparation.
    if !p.no_prepare {
        let id = ExeIdentity::of(&me)?;
        let shaders = cfg.cache_subdir("shaders")?;
        let pipelines = shaders.with_file_name("pipelines");
        let mut ok = prepare_shaders(&me, &id, &shaders, &game_dir)?;
        ok &= prepare_pipelines(&me, &id, &pipelines, p.warm)?;
        if p.prepare_only {
            if !ok {
                bail!("the preparation did not complete: see above");
            }
            println!("Prepared. Nothing was started.");
            return Ok(());
        }
    }

    // 3. The run.
    let settings = p.settings(&cfg.data_dir);
    let mut args: Vec<String> = vec!["run".into()];
    args.extend(p.exe.iter().map(|e| e.display().to_string()));
    args.extend(["--execute".into(), "--console".into(), p.console().into()]);
    let mut run = step(&me, &args.iter().map(String::as_str).collect::<Vec<_>>(), false);
    for (k, v) in &settings {
        run.env(k, v);
    }
    let log = p.log.then(|| {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let name = format!(
            "{}-{}{}{}.log",
            victory_core::report::utc_stamp(now),
            p.console(),
            p.display().map(|d| format!("-{d}")).unwrap_or_default(),
            if p.fast { "-fast" } else { "" }
        );
        (logs_dir(&cfg.data_dir), name)
    });
    let log = log.map(|(dir, name)| {
        let mut header = format!("# victory play{}\n# victory {}\n", if p.fast { " --fast" } else { "" }, args.join(" "));
        let mut set: BTreeMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.to_ascii_uppercase().starts_with("VICTORY_"))
            .collect();
        set.extend(settings.iter().cloned());
        for (k, v) in set {
            header += &format!("# {k}={v}\n");
        }
        (dir.join(name), header)
    });
    // A window over this command (the launcher) reads this line as the end of the preparation.
    println!("Starting the title.");
    let code = run_title(run, log)?;
    if code != 0 {
        // The run's own exit code, for whoever started this.
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> ExeIdentity {
        ExeIdentity { size: 10, modified: 20, head_sha256: "ab".into() }
    }

    fn stamp(lines: &str) -> BTreeMap<String, String> {
        lines.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
    }

    #[test]
    fn a_current_stamp_differs_in_nothing() {
        let exe = Path::new(r"C:\Rox Box\victory.exe");
        let s = stamp(&format!(
            "exe=\\\\?\\C:\\ROX BOX\\victory.exe\nbuild={}\nsize=10\nmodified=20\nhead_sha256=ab\ngame_dir=D:\\Títles\\One\\",
            warm::BUILD
        ));
        assert!(differs(Some(&s), exe, &id(), Some(Path::new(r"D:\Títles\One"))).is_empty());
        assert!(differs(Some(&s), exe, &id(), None).is_empty());
    }

    #[test]
    fn what_differs_is_named() {
        let exe = Path::new(r"C:\a\victory.exe");
        assert_eq!(differs(None, exe, &id(), None), ["no stamp"]);
        let s = stamp(&format!("exe=C:\\b\\victory.exe\nbuild={}\nsize=11\nmodified=20\nhead_sha256=cd\ngame_dir=D:\\x", warm::BUILD));
        assert_eq!(differs(Some(&s), exe, &id(), Some(Path::new(r"D:\y"))), ["the exe", "size", "head_sha256", "the game folder"]);
    }

    #[test]
    fn sizes_are_checked() {
        assert!(size("1920x1080").is_ok());
        assert!(size("1920").is_err() && size("x1080").is_err() && size("19a0x1080").is_err());
    }

    #[test]
    fn fast_chooses_what_was_not_given() {
        let mut p = Play {
            exe: None,
            fast: true,
            console: None,
            display: None,
            output: None,
            safe: true,
            log: false,
            shots: Some(10),
            warm: false,
            prepare_only: false,
            no_prepare: false,
        };
        assert_eq!((p.console(), p.display()), ("series-s", Some("1920x1080")));
        let s = p.settings(Path::new(r"C:\data"));
        assert!(s.contains(&("VICTORY_DEFAULTS".into(), "safe".into())));
        assert!(s.contains(&("VICTORY_CAPTURE_EVERY_S".into(), "10".into())));
        p.console = Some("series-x".into());
        p.display = Some("1280x720".into());
        assert_eq!((p.console(), p.display()), ("series-x", Some("1280x720")));
        p.fast = false;
        p.display = None;
        assert_eq!(p.display(), None);
    }
}
