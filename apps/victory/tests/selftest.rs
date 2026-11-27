//! `victory selftest` on the software adapter: the whole frame path (device,
//! command lists through the function tables, submission with title memory
//! kept in step, a translated compute shader and a translated draw, present)
//! on every `cargo test` and in CI, with no title files.
//!
//! The binary runs in a process of its own: the host device is one per
//! process, and the debug layer must not come on beside the other GPU
//! tests. Like those, it is skipped where there is no Direct3D 12 device at
//! all (the binary's exit code 3).
#![cfg(windows)]

use std::process::Command;

/// A fresh folder under the system's temporary folder, outside any source
/// tree, with no `victory.toml`.
fn scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("rox-box-selftest-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_self_test_passes_on_the_software_adapter() {
    // Its report goes to the data folder: a temporary one, found with no
    // config file (from a folder that holds none).
    let root = scratch();
    let data = root.join("data");
    let output = Command::new(env!("CARGO_BIN_EXE_victory"))
        .args(["selftest", "--warp", "--frames", "8", "--bench"])
        // The switches a developer's shell may hold change what is tested.
        .env_remove("VICTORY_D3D12_DEBUG")
        .env_remove("VICTORY_LOG")
        .env_remove("VICTORY_CONFIG")
        .env_remove("VICTORY_REPORT")
        .env("VICTORY_DATA_DIR", &data)
        .env("VICTORY_CACHE_DIR", root.join("cache"))
        .current_dir(&root)
        .output()
        .expect("the victory binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(3) {
        eprintln!("no Direct3D 12 device here; skipped\n{stdout}{stderr}");
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    assert!(output.status.success(), "the self-test failed ({:?}):\n{stdout}\n{stderr}", output.status);
    for stage in ["Device", "Clear + present", "Unified memory", "Translated shader", "Translated draw"] {
        assert!(
            stdout.lines().any(|l| l.contains(stage) && (l.starts_with("[  OK   ]") || l.starts_with("[ SKIP  ]"))),
            "no OK or SKIP line for {stage}:\n{stdout}"
        );
    }
    assert!(stdout.contains("Self-test passed"), "{stdout}");
    // Every rate is a positive number with a unit.
    let rates: Vec<&str> = stdout.lines().skip_while(|l| !l.starts_with("Benchmark")).skip(1).take_while(|l| !l.is_empty()).collect();
    assert!(rates.len() >= 4, "{stdout}");
    for line in rates {
        let words: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(words.len(), 3, "{line}");
        assert!(words[1].parse::<f64>().is_ok_and(|v| v > 0.0), "{line}");
    }

    // The report: one file in the data folder's reports, TOML, holding
    // every stage and the rates; its path said on stderr, never stdout.
    let reports: Vec<_> = std::fs::read_dir(data.join("reports"))
        .unwrap_or_else(|e| panic!("no reports folder ({e}):\n{stderr}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    assert_eq!(reports.len(), 1, "{reports:?}");
    let name = reports[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("selftest-") && name.ends_with(".txt"), "{name}");
    let text = std::fs::read_to_string(&reports[0]).unwrap();
    let report: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("not TOML: {e}\n{text}"));
    assert_eq!(report["kind"].as_str(), Some("selftest"), "{text}");
    assert_eq!(report["host"]["gpu"]["software_adapter"].as_bool(), Some(true), "{text}");
    let test = &report["selftest"];
    assert_eq!(test["ended"].as_str(), Some("completed"), "{text}");
    assert_eq!(test["passed"].as_bool(), Some(true), "{text}");
    assert_eq!(test["software_adapter"].as_bool(), Some(true), "{text}");
    let stages = test["stage"].as_array().expect("[[selftest.stage]]");
    let names: Vec<&str> = stages.iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, ["Device", "Clear + present", "Unified memory", "Translated shader", "Translated draw"], "{text}");
    for stage in stages {
        assert!(matches!(stage["outcome"].as_str(), Some("ok" | "skip")), "{text}");
    }
    let rates = test["rates"].as_table().expect("[selftest.rates]");
    assert!(rates.len() >= 4 && rates.values().all(|v| v.as_float().is_some_and(|v| v > 0.0)), "{text}");
    // Nothing of what the stages printed (the adapter's figures, the
    // frames' words) and no path.
    assert!(!text.contains('\\') && !text.contains("cleared and presented"), "{text}");
    let shown = reports[0].display().to_string();
    assert!(stderr.contains(&shown) && !stdout.contains(&shown), "{stdout}\n{stderr}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_bad_frame_count_is_refused() {
    let output = Command::new(env!("CARGO_BIN_EXE_victory"))
        .args(["selftest", "--frames", "many"])
        .output()
        .expect("the victory binary runs");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}
