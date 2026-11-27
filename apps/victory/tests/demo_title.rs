//! A real `victory run`: the open demo title (`tests/fixtures/demo-title`,
//! docs/demo-title.md), an executable of our own, is built, then prepared,
//! mapped, bound and started by `victory run <demo-title.exe> --execute` on
//! the software adapter, with temporary folders and no config file, the
//! way a player's run goes. The self-test covers the frame path in one
//! process of ours; this covers what it cannot: the loader, import binding,
//! the title's own thread, C runtime and command line, its threads and their
//! TLS, its two DLLs (one imported, one loaded by name), its exit (also with
//! threads waiting, a DLL loaded and a frame in flight), and the run report.
//! The oracle is the report, the exit code and the demo's own lines (its
//! checks, by its own means, of its memory, its picture, its threads' and
//! DLLs' notices, and at its very end each DLL's process detach).
//!
//! The binary runs in a process of its own (the host device is one per
//! process). Like `selftest.rs`, the test is skipped where there is no
//! Direct3D 12 device at all (the binary's exit code 3).
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Frames the demo title is asked for: a second or so at its 60 a second.
const FRAMES: u32 = 40;
/// Frames of the runs that check how the title ends.
const FEW: u32 = 10;
/// The longest the run may take (it takes a few seconds; a hang fails).
const PATIENCE: Duration = Duration::from_secs(120);

/// The game folder's files: the demo title and its two DLLs.
const FILES: [(&str, &str); 3] = [
    ("demo-title", "demo-title.exe"),
    ("demo-static-dll", "demo_static.dll"),
    ("demo-late-dll", "demo_late.dll"),
];

/// Builds the fixtures, once per test process, each into its own target
/// folder (the outer cargo holds the lock on the main one while tests run),
/// in the outer one's scratch folder for tests, as `victory-loader`'s
/// `load_probe.rs` builds `hle-probe`. The files of a game folder.
fn build_demo() -> &'static [PathBuf] {
    static BUILT: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| {
        FILES
            .iter()
            .map(|(fixture, file)| {
                let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("../../tests/fixtures/{fixture}/Cargo.toml"));
                let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join(fixture);
                let status = Command::new(env!("CARGO"))
                    .args(["build", "--locked", "--quiet", "--manifest-path"])
                    .arg(&manifest)
                    .arg("--target-dir")
                    .arg(&target)
                    .status()
                    .expect("running cargo");
                assert!(status.success(), "building {fixture} failed");
                target.join("debug").join(file)
            })
            .collect()
    })
}

/// A fresh folder under the system's temporary folder, outside any source
/// tree (the runtime refuses to write below a `.victory-root` marker).
fn scratch(what: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "rox-box-demo-title-{what}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One run of the demo title, done.
struct Ran {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    /// The run report's text and its table.
    text: String,
    report: toml::Table,
    root: PathBuf,
    cache: PathBuf,
    data: PathBuf,
}

/// Every file below `dir`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

impl Ran {
    /// The last lines of stdout and stderr, for a failure's message.
    fn tail(&self) -> String {
        let tail = |text: &str| {
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(40)..].join("\n")
        };
        format!("{}\n{}", tail(&self.stdout), tail(&self.stderr))
    }

    fn int(&self, section: &str, key: &str) -> i64 {
        self.value(section, key)
            .and_then(toml::Value::as_integer)
            .unwrap_or_else(|| panic!("no [{section}] {key}:\n{}", self.text))
    }

    fn float(&self, section: &str, key: &str) -> f64 {
        self.value(section, key)
            .and_then(toml::Value::as_float)
            .unwrap_or_else(|| panic!("no [{section}] {key}:\n{}", self.text))
    }

    fn value(&self, section: &str, key: &str) -> Option<&toml::Value> {
        let mut value = self.report.get(section.split('.').next().unwrap())?;
        for part in section.split('.').skip(1) {
            value = value.get(part)?;
        }
        value.get(key)
    }

    /// The demo's lines after its frames: they all matched, its picture
    /// too; its threads and DLLs were as a title's must be; and, the last
    /// thing it said, each DLL heard of the process's end once. Nothing
    /// failed, and nothing went into a crash log.
    fn finished(&self, frames: u32) {
        let done = format!(
            "demo title: {frames} frames rendered and presented; every value it read back from its own memory matched"
        );
        self.said(&done);
        self.said("demo title: its picture, copied into a buffer it maps (CopyTextureRegion), matched what it drew in every frame");
        self.said("demo title: 7 threads of its own (CreateThread, _beginthreadex, std::thread; ended by returning, ExitThread, _endthreadex, FreeLibraryAndExitThread)");
        self.said("demo title: demo_twin.dll, a copy of demo_late.dll under another name, is a module of its own, freed and unloaded");
        self.said("demo title: its DLLs demo_static.dll (imported) and demo_late.dll (LoadLibraryW, GetProcAddress) were each attached once");
        self.said("demo title: at its exit, each of its DLLs heard of the process's end once, and no thread's start or end came twice");
        assert!(!self.stdout.contains("demo title failed"), "{}", self.tail());
        // Both DLLs mapped as the title's, every import of theirs bound.
        for (dll, how) in [("demo_static.dll", " at "), ("demo_late.dll", " (loaded by the title)")] {
            assert!(
                self.stdout.lines().any(|l| l.starts_with(&format!("title dll      {dll} at ")) && l.contains(how)),
                "{dll} not mapped as the title's:\n{}",
                self.tail()
            );
            assert!(
                self.stdout.lines().any(|l| l.starts_with(&format!("bound          {dll}: ")) && l.ends_with(" 0 missing")),
                "{dll}'s imports not all bound:\n{}",
                self.tail()
            );
        }
        let logs: Vec<_> = walk(&self.data).into_iter().filter(|p| p.ends_with("crashes.log")).collect();
        assert!(logs.is_empty(), "{logs:?}:\n{}", self.tail());
    }

    /// A line of stdout starts with `start`.
    fn said(&self, start: &str) {
        assert!(
            self.stdout.lines().any(|l| l.starts_with(start)),
            "no line {start:?}:\n{}",
            self.tail()
        );
    }

    /// The report says the title exited with `code`, and the process ended
    /// with it.
    fn exited_with(&self, code: i32) {
        assert_eq!(self.status.code(), Some(code), "{:?}:\n{}", self.status, self.tail());
        assert_eq!(self.report["run"]["ended"].as_str(), Some("title_exited"), "{}", self.text);
        assert_eq!(self.int("run", "exit_code"), i64::from(code), "{}", self.text);
        assert_eq!(self.int("run", "rox_box_panics"), 0, "{}", self.text);
    }
}

/// Runs `victory run <demo> --execute -- <args>` on the software adapter
/// with temporary folders, no config file and no other `VICTORY_*` variable;
/// `None` where there is no Direct3D 12 device at all.
fn run_demo(what: &str, args: &[&str]) -> Option<Ran> {
    let built = build_demo();
    let root = scratch(what);
    let (game, cache, data) = (root.join("game"), root.join("cache"), root.join("data"));
    for dir in [&game, &cache, &data] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // The executable and its two DLLs are a whole game folder: the
    // manifest a run reads for the title's id and storage is optional.
    for (file, (_, name)) in built.iter().zip(FILES) {
        std::fs::copy(file, game.join(name)).unwrap();
    }
    // The late DLL once more, under another name: a title's DLL shipped
    // twice must be two modules.
    std::fs::copy(game.join("demo_late.dll"), game.join("demo_twin.dll")).unwrap();
    let exe = game.join("demo-title.exe");

    let mut run = Command::new(env!("CARGO_BIN_EXE_victory"));
    run.args(["run"]).arg(&exe).args(["--execute", "--"]).args(args);
    // Nothing of a developer's shell or configuration: no switches, and no
    // `victory.toml` in the current folder.
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("VICTORY_")
        {
            run.env_remove(&key);
        }
    }
    run.env("VICTORY_GAME_DIR", &game)
        .env("VICTORY_CACHE_DIR", &cache)
        .env("VICTORY_DATA_DIR", &data)
        .env("VICTORY_D3D12_WARP", "1")
        .current_dir(&root);
    let (out, err) = (root.join("stdout.txt"), root.join("stderr.txt"));
    run.stdout(Stdio::from(std::fs::File::create(&out).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&err).unwrap()));
    let mut child = run.spawn().expect("the victory binary runs");
    let began = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if began.elapsed() > PATIENCE {
            let _ = child.kill();
            panic!(
                "the run took longer than {PATIENCE:?}; its output is in {}",
                root.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = std::fs::read_to_string(&out).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err).unwrap_or_default();
    if status.code() == Some(3) {
        eprintln!("no Direct3D 12 device here; skipped\n{stdout}{stderr}");
        let _ = std::fs::remove_dir_all(&root);
        return None;
    }
    // The run report: the one file of the run, TOML.
    let reports: Vec<PathBuf> = std::fs::read_dir(data.join("reports"))
        .unwrap_or_else(|e| panic!("the run wrote no reports folder ({e}):\n{stdout}\n{stderr}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    assert_eq!(reports.len(), 1, "{reports:?}\n{stdout}\n{stderr}");
    let text = std::fs::read_to_string(&reports[0]).unwrap();
    let report: toml::Table =
        toml::from_str(&text).unwrap_or_else(|e| panic!("the report is not TOML: {e}\n{text}"));
    Some(Ran { status, stdout, stderr, text, report, root, cache, data })
}

#[test]
fn the_demo_title_runs_to_its_end_on_the_software_adapter() {
    let Some(ran) = run_demo("frames", &["--frames", &FRAMES.to_string()]) else {
        return;
    };
    assert!(ran.status.success(), "the run failed ({:?}):\n{}", ran.status, ran.tail());
    ran.finished(FRAMES);
    ran.exited_with(0);
    // The late DLL freed (after a thread gave back a reference of its own):
    // detached and unloaded at its last `FreeLibrary`, and loaded afresh
    // after (`VICTORY_TITLE_DLL_FREE`).
    ran.said("demo title: demo_late.dll: its last FreeLibrary ran its process detach and unloaded it");
    let text = &ran.text;
    assert_eq!(ran.report["kind"].as_str(), Some("run"), "{text}");
    assert_eq!(
        ran.report["host"]["gpu"]["software_adapter"].as_bool(),
        Some(true),
        "{text}"
    );
    assert_eq!(ran.int("frames", "presents"), i64::from(FRAMES), "{text}");
    // The frame meter's figures cover every present, its last part of a
    // second (here all of the run) included; the meter may leave out the
    // first, which only starts it.
    let metered = ran.int("frame_time", "frames");
    assert!(
        (i64::from(FRAMES) - 1..=i64::from(FRAMES)).contains(&metered),
        "[frame_time] frames {metered} for {FRAMES} presents:\n{text}"
    );
    assert!(ran.float("frame_time", "frame_ms") > 0.0, "{text}");
    // A run this short has no steady state: no figures, rather than zeros.
    let steady = ran.report["frame_time"]["steady"].as_table().expect("[frame_time.steady]");
    assert!(
        steady.get("frames").is_none_or(|n| n.as_integer().is_some_and(|n| n > 0)),
        "{text}"
    );
    // Each frame is one submission with one dispatch and two draws, all run
    // on the host (the first present's loading is left out of these).
    assert_eq!(ran.float("gpu_work", "submissions_avg"), 1.0, "{text}");
    assert_eq!(ran.float("gpu_work", "draws_avg"), 2.0, "{text}");
    assert_eq!(ran.int("gpu_work", "draws_max"), 2, "{text}");
    assert_eq!(ran.float("gpu_work", "dispatches_avg"), 1.0, "{text}");
    assert_eq!(ran.int("gpu_work", "dispatches_max"), 1, "{text}");
    assert_eq!(ran.int("gpu_work", "left_out_max"), 0, "{text}");
    assert_eq!(ran.int("gpu_work", "submissions_total"), i64::from(FRAMES), "{text}");
    for (reason, n) in ran.report["gpu_work"]["left_out"]
        .as_table()
        .expect("[gpu_work.left_out]")
    {
        assert_eq!(n.as_integer(), Some(0), "left out for {reason}:\n{text}");
    }
    // The compute program and the vertex and pixel programs, translated
    // into an empty cache; two pipelines, both made on the host.
    assert_eq!(ran.int("shaders", "translated_now"), 3, "{text}");
    assert_eq!(ran.int("shaders", "failed"), 0, "{text}");
    assert_eq!(ran.int("pipelines", "created_by_title"), 2, "{text}");
    assert_eq!(
        ran.int("pipelines", "host_made_now") + ran.int("pipelines", "host_from_library"),
        2,
        "{text}"
    );
    assert_eq!(ran.int("pipelines", "host_failed"), 0, "{text}");
    for key in [
        "methods_called",
        "methods_lost",
        "pipelines_gone_without",
        "imports_named",
        "imports_other",
        "imports_other_calls",
    ] {
        assert_eq!(ran.int("unimplemented", key), 0, "{key}:\n{text}");
    }
    assert!(
        ran.report["unimplemented"].get("method").is_none()
            && ran.report["unimplemented"].get("import").is_none(),
        "{text}"
    );
    // Everything the run wrote is in the temporary folders.
    assert!(
        ran.cache.join("images").is_dir() && ran.cache.join("shaders").is_dir(),
        "{}",
        ran.cache.display()
    );
    let _ = std::fs::remove_dir_all(&ran.root);
}

/// `ExitProcess` called by the title: the end is recorded with its code,
/// and `victory run` ends with it.
#[test]
fn a_title_that_calls_exit_process_is_reported_with_its_code() {
    let Some(ran) = run_demo("exit-process", &["--frames", &FEW.to_string(), "--exit-process", "5"]) else {
        return;
    };
    ran.finished(FEW);
    ran.exited_with(5);
    let _ = std::fs::remove_dir_all(&ran.root);
}

/// A code returned from `main` (which its C runtime passes to `exit`).
#[test]
fn a_title_that_returns_a_code_ends_the_run_with_it() {
    let Some(ran) = run_demo("return", &["--frames", &FEW.to_string(), "--return", "7"]) else {
        return;
    };
    ran.finished(FEW);
    ran.exited_with(7);
    let _ = std::fs::remove_dir_all(&ran.root);
}

/// The C runtime's `exit` called by the title with a code of its own.
#[test]
fn a_title_that_calls_exit_ends_the_run_with_its_code() {
    let Some(ran) = run_demo("exit", &["--frames", &FEW.to_string(), "--exit", "9"]) else {
        return;
    };
    ran.finished(FEW);
    ran.exited_with(9);
    let _ = std::fs::remove_dir_all(&ran.root);
}

/// The end with everything alive (`--alive`): two of the title's threads
/// waiting for ever, the late DLL loaded and a frame in flight. The run
/// ends with the title's code, said in the report, and no crash log; each
/// DLL heard of the process's end once (`finished`).
fn ends_alive(what: &str, end: &[&str], code: i32) {
    let mut args = vec!["--frames", "10", "--alive"];
    args.extend_from_slice(end);
    let Some(ran) = run_demo(what, &args) else {
        return;
    };
    ran.finished(FEW);
    ran.said("demo title: ends with 2 threads waiting, demo_late.dll loaded and a frame in flight");
    ran.exited_with(code);
    let _ = std::fs::remove_dir_all(&ran.root);
}

#[test]
fn a_title_that_returns_with_everything_alive_ends_with_its_code() {
    ends_alive("alive-return", &["--return", "7"], 7);
}

#[test]
fn a_title_that_calls_exit_with_everything_alive_ends_with_its_code() {
    ends_alive("alive-exit", &["--exit", "9"], 9);
}

#[test]
fn a_title_that_calls_exit_process_with_everything_alive_ends_with_its_code() {
    ends_alive("alive-exit-process", &["--exit-process", "5"], 5);
}
