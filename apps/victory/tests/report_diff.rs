//! `victory report --diff` as a user runs it: two paths need no config file,
//! indices read the reports folder, and bad input is exit code 1 with the
//! argument named.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const A: &str = "format = 2\n[run]\nended = \"title_exited\"\nduration_s = 10.0\n\
                 [frame_time.steady]\nframe_ms = 40.0\n";
const B: &str = "format = 2\n[run]\nended = \"title_exited\"\nduration_s = 10.0\n\
                 [frame_time.steady]\nframe_ms = 20.0\n";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("victory-report-diff-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs the binary against `config`, always a file of this test's own, never
/// the developer's: absent (a path that does not exist) or present with
/// `dir` as its data folder.
fn victory(dir: &Path, config: bool, args: &[&str]) -> Output {
    let file = dir.join("test-config.toml");
    if config {
        std::fs::write(&file, format!("[paths]\ndata_dir = '{}'\n", dir.display())).unwrap();
    }
    Command::new(env!("CARGO_BIN_EXE_victory"))
        .arg("report")
        .args(args)
        .current_dir(dir)
        .env("VICTORY_CONFIG", file)
        .env_remove("VICTORY_DATA_DIR")
        .output()
        .unwrap()
}

#[test]
fn two_paths_need_no_config_and_the_paths_stay_off_stdout() {
    let dir = scratch("paths");
    let (a, b) = (dir.join("old.txt"), dir.join("new.txt"));
    std::fs::write(&a, A).unwrap();
    std::fs::write(&b, B).unwrap();
    let out = victory(&dir, false, &["--diff", a.to_str().unwrap(), b.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stdout.contains("verdict: steady-state frame time 40.0 -> 20.0 ms (-50.0 %)"), "{stdout}");
    assert!(!stdout.contains(dir.to_str().unwrap()), "{stdout}");
    assert!(stderr.contains(dir.to_str().unwrap()), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn indices_and_the_no_argument_form_read_the_reports_folder() {
    let dir = scratch("index");
    let reports = dir.join("reports");
    std::fs::create_dir_all(&reports).unwrap();
    std::fs::write(reports.join("run-20250101-000000.txt"), A).unwrap();
    std::fs::write(reports.join("run-20250101-000100.txt"), B).unwrap();
    for args in [&["--diff"][..], &["--diff", "2", "1"]] {
        let out = victory(&dir, true, args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(stdout.contains("40.0 -> 20.0 ms"), "{args:?}: {stdout}");
    }
    let out = victory(&dir, true, &["--diff", "1", "3"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("argument 2 is 3"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The no-argument form compares the two newest of the newest one's kind:
/// a self-test's report beside run reports is not compared with them.
#[test]
fn the_newest_self_tests_compare_with_each_other() {
    let selftest = |rate: &str| {
        format!(
            "format = 3\nkind = \"selftest\"\n[selftest]\nended = \"completed\"\nstages_ok = 5\n\
             [selftest.rates]\n\"present_interval\" = {rate}\n[[selftest.stage]]\nname = \"Device\"\noutcome = \"ok\"\n"
        )
    };
    let dir = scratch("kinds");
    let reports = dir.join("reports");
    std::fs::create_dir_all(&reports).unwrap();
    std::fs::write(reports.join("run-20250101-000000.txt"), A).unwrap();
    std::fs::write(reports.join("run-20250101-000100.txt"), B).unwrap();
    std::fs::write(reports.join("selftest-20250101-000200.txt"), selftest("2.00")).unwrap();
    let out = victory(&dir, true, &["--diff"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs two self-test reports"));
    std::fs::write(reports.join("selftest-20250101-000300.txt"), selftest("4.00")).unwrap();
    let out = victory(&dir, true, &["--diff"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("stages: same outcomes (1 in each)"), "{stdout}");
    assert!(stdout.contains("verdict: present_interval 2.00 -> 4.00 (+100.0 %)"), "{stdout}");
    // `victory report --list` names both kinds.
    let out = victory(&dir, true, &["--list"]);
    let listed = String::from_utf8_lossy(&out.stdout);
    assert!(listed.contains("self-test, completed: 5 ok") && listed.contains("title_exited"), "{listed}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fewer_than_two_reports_and_bad_input_exit_with_code_1() {
    let dir = scratch("errors");
    let out = victory(&dir, true, &["--diff"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs two run reports"));

    let good = dir.join("good.txt");
    let bad = dir.join("bad.txt");
    std::fs::write(&good, A).unwrap();
    std::fs::write(&bad, "format = [").unwrap();
    let out = victory(&dir, true, &["--diff", good.to_str().unwrap(), bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("report argument 2"));
    let out = victory(&dir, true, &["--diff", good.to_str().unwrap(), "missing.txt"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("report argument 2"));
    let _ = std::fs::remove_dir_all(&dir);
}
