//! `victory selftest` on the software adapter: the whole frame path (device,
//! command lists through the function tables, submission with title memory
//! kept in step, a translated shader, present) on every `cargo test` and in
//! CI, with no title files.
//!
//! The binary runs in a process of its own: the host device is one per
//! process, and the debug layer must not come on beside the other GPU
//! tests. Like those, it is skipped where there is no Direct3D 12 device at
//! all (the binary's exit code 3).
#![cfg(windows)]

use std::process::Command;

#[test]
fn the_self_test_passes_on_the_software_adapter() {
    let output = Command::new(env!("CARGO_BIN_EXE_victory"))
        .args(["selftest", "--warp", "--frames", "8", "--bench"])
        // The switches a developer's shell may hold change what is tested.
        .env_remove("VICTORY_D3D12_DEBUG")
        .env_remove("VICTORY_LOG")
        .output()
        .expect("the victory binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(3) {
        eprintln!("no Direct3D 12 device here; skipped\n{stdout}{stderr}");
        return;
    }
    assert!(output.status.success(), "the self-test failed ({:?}):\n{stdout}\n{stderr}", output.status);
    for stage in ["Device", "Clear + present", "Unified memory", "Translated shader"] {
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
}

#[test]
fn a_bad_frame_count_is_refused() {
    let output = Command::new(env!("CARGO_BIN_EXE_victory"))
        .args(["selftest", "--frames", "many"])
        .output()
        .expect("the victory binary runs");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}
