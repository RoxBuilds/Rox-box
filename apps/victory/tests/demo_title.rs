//! A real `victory run`: the open demo title (`tests/fixtures/demo-title`,
//! docs/demo-title.md), an executable of our own, is built, then prepared,
//! mapped, bound and started by `victory run <demo-title.exe> --execute` on
//! the software adapter, with temporary folders and no config file, the
//! way a player's run goes. The self-test covers the frame path in one
//! process of ours; this covers what it cannot: the loader, import binding,
//! the title's own thread and C runtime, its exit, and the run report, which
//! is the oracle here.
//!
//! The binary runs in a process of its own (the host device is one per
//! process). Like `selftest.rs`, the test is skipped where there is no
//! Direct3D 12 device at all (the binary's exit code 3).
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Frames the demo title is asked for: a second or so at its 60 a second.
const FRAMES: u32 = 40;
/// The longest the run may take (it takes a few seconds; a hang fails).
const PATIENCE: Duration = Duration::from_secs(120);

/// Builds the fixture into its own target folder (the outer cargo holds the
/// lock on the main one while tests run), in the outer one's scratch folder
/// for tests, as `victory-loader`'s `load_probe.rs` builds `hle-probe`.
fn build_demo() -> PathBuf {
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/demo-title/Cargo.toml");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("demo-title");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--locked", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .status()
        .expect("running cargo");
    assert!(status.success(), "building the demo title failed");
    target.join("debug").join("demo-title.exe")
}

/// A fresh folder under the system's temporary folder, outside any source
/// tree (the runtime refuses to write below a `.victory-root` marker).
fn scratch() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir =
        std::env::temp_dir().join(format!("rox-box-demo-title-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_demo_title_runs_to_its_end_on_the_software_adapter() {
    let demo = build_demo();
    let root = scratch();
    let (game, cache, data) = (root.join("game"), root.join("cache"), root.join("data"));
    for dir in [&game, &cache, &data] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // The executable alone is a whole game folder: it ships no DLLs, and
    // the manifest a run reads for the title's id and storage is optional.
    let exe = game.join("demo-title.exe");
    std::fs::copy(&demo, &exe).unwrap();

    let mut run = Command::new(env!("CARGO_BIN_EXE_victory"));
    run.args(["run"])
        .arg(&exe)
        .args(["--execute", "--", "--frames", &FRAMES.to_string()]);
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
        return;
    }
    let tail = |text: &str| {
        text.lines()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        status.success(),
        "the run failed ({status:?}):\n{}\n{}",
        tail(&stdout),
        tail(&stderr)
    );
    let done = format!(
        "demo title: {FRAMES} frames rendered and presented; every value it read back from its own memory matched"
    );
    assert!(
        stdout.lines().any(|l| l == done),
        "no line {done:?}:\n{}",
        tail(&stdout)
    );

    // The run report: the one file of the run, TOML.
    let reports: Vec<PathBuf> = std::fs::read_dir(data.join("reports"))
        .expect("the run wrote a reports folder")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    assert_eq!(reports.len(), 1, "{reports:?}");
    let text = std::fs::read_to_string(&reports[0]).unwrap();
    let report: toml::Table =
        toml::from_str(&text).unwrap_or_else(|e| panic!("the report is not TOML: {e}\n{text}"));
    let int = |section: &str, key: &str| -> i64 {
        let mut value = &report[section.split('.').next().unwrap()];
        for part in section.split('.').skip(1) {
            value = &value[part];
        }
        value
            .get(key)
            .and_then(toml::Value::as_integer)
            .unwrap_or_else(|| panic!("no [{section}] {key}:\n{text}"))
    };
    let float = |section: &str, key: &str| -> f64 {
        report[section]
            .get(key)
            .and_then(toml::Value::as_float)
            .unwrap_or_else(|| panic!("no [{section}] {key}:\n{text}"))
    };
    let run = &report["run"];
    assert_eq!(run["ended"].as_str(), Some("title_exited"), "{text}");
    assert_eq!(int("run", "exit_code"), 0, "{text}");
    assert_eq!(int("run", "rox_box_panics"), 0, "{text}");
    assert_eq!(
        report["host"]["gpu"]["software_adapter"].as_bool(),
        Some(true),
        "{text}"
    );
    assert_eq!(int("frames", "presents"), i64::from(FRAMES), "{text}");
    // Each frame is one submission with one dispatch and two draws, all run
    // on the host (the first present's loading is left out of these).
    assert_eq!(float("gpu_work", "submissions_avg"), 1.0, "{text}");
    assert_eq!(float("gpu_work", "draws_avg"), 2.0, "{text}");
    assert_eq!(int("gpu_work", "draws_max"), 2, "{text}");
    assert_eq!(float("gpu_work", "dispatches_avg"), 1.0, "{text}");
    assert_eq!(int("gpu_work", "dispatches_max"), 1, "{text}");
    assert_eq!(int("gpu_work", "left_out_max"), 0, "{text}");
    assert_eq!(
        int("gpu_work", "submissions_total"),
        i64::from(FRAMES),
        "{text}"
    );
    for (reason, n) in report["gpu_work"]["left_out"]
        .as_table()
        .expect("[gpu_work.left_out]")
    {
        assert_eq!(n.as_integer(), Some(0), "left out for {reason}:\n{text}");
    }
    // The compute program and the vertex and pixel programs, translated
    // into an empty cache; two pipelines, both made on the host.
    assert_eq!(int("shaders", "translated_now"), 3, "{text}");
    assert_eq!(int("shaders", "failed"), 0, "{text}");
    assert_eq!(int("pipelines", "created_by_title"), 2, "{text}");
    assert_eq!(
        int("pipelines", "host_made_now") + int("pipelines", "host_from_library"),
        2,
        "{text}"
    );
    assert_eq!(int("pipelines", "host_failed"), 0, "{text}");
    for key in [
        "methods_called",
        "methods_lost",
        "pipelines_gone_without",
        "imports_named",
        "imports_other",
        "imports_other_calls",
    ] {
        assert_eq!(int("unimplemented", key), 0, "{key}:\n{text}");
    }
    assert!(
        report["unimplemented"].get("method").is_none()
            && report["unimplemented"].get("import").is_none(),
        "{text}"
    );
    // Everything the run wrote is in the temporary folders.
    assert!(
        cache.join("images").is_dir() && cache.join("shaders").is_dir(),
        "{}",
        cache.display()
    );
    let _ = std::fs::remove_dir_all(&root);
}
