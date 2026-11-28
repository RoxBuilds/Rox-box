//! One file to attach when something goes wrong: the newest run report and
//! self-test report, the newest run log, and what `victory check` says,
//! packed into a zip in the logs folder, with this PC's personal details taken
//! out of every file first.
//!
//! Reports hold only numbers and Rox-box's own words by construction; a run
//! log does not: it names folders. So every file is passed through [`redact`]:
//! the title folder becomes `<game folder>`, the user's profile folder
//! `%USERPROFILE%`, and the user and computer names `<user>` and `<pc>`
//! wherever they appear. The person still decides whether to share the file.

use std::path::{Path, PathBuf};

/// The most of a log that is kept: its end, where a failure is.
const LOG_TAIL: usize = 8 * 1024 * 1024;

/// What is taken out of every file, and what stands for it.
#[derive(Debug, Clone, Default)]
pub struct Private {
    pub game_dir: Option<String>,
    pub profile: Option<String>,
    pub user: Option<String>,
    pub computer: Option<String>,
}

impl Private {
    /// This PC's: from the environment, and the active title folder.
    pub fn of_this_pc(game_dir: Option<&str>) -> Private {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Private { game_dir: game_dir.map(str::to_string), profile: var("USERPROFILE"), user: var("USERNAME"), computer: var("COMPUTERNAME") }
    }
}

/// `text` with every case-insensitive match of `what` replaced by `with`.
fn replace_folded(text: &str, what: &str, with: &str) -> String {
    if what.is_empty() {
        return text.to_string();
    }
    let lower = text.to_lowercase();
    let what_lower = what.to_lowercase();
    // Lowercasing can change byte lengths outside ASCII: fall back to exact matches then.
    if lower.len() != text.len() || what_lower.len() != what.len() {
        return text.replace(what, with);
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = lower[at..].find(&what_lower) {
        out.push_str(&text[at..at + found]);
        out.push_str(with);
        at += found + what.len();
    }
    out.push_str(&text[at..]);
    out
}

/// `text` without this PC's personal details (see the module's text). Paths
/// are matched with either kind of slash.
pub fn redact(text: &str, private: &Private) -> String {
    let mut out = text.to_string();
    let mut paths: Vec<(&str, &str)> = Vec::new();
    if let Some(g) = private.game_dir.as_deref() {
        paths.push((g.trim_end_matches(['\\', '/']), "<game folder>"));
    }
    if let Some(p) = private.profile.as_deref() {
        paths.push((p.trim_end_matches(['\\', '/']), "%USERPROFILE%"));
    }
    for (path, with) in paths {
        if path.len() < 3 {
            continue;
        }
        out = replace_folded(&out, path, with);
        out = replace_folded(&out, &path.replace('\\', "/"), with);
    }
    // Names last: what is left of them outside the folders above. Short names
    // would match ordinary words; they are left alone.
    // The longer first: a computer named after its user ("ALEX-PC") goes whole.
    let mut names: Vec<(&str, &str)> = [(private.user.as_deref(), "<user>"), (private.computer.as_deref(), "<pc>")]
        .into_iter()
        .filter_map(|(name, with)| name.filter(|n| n.chars().count() >= 3).map(|n| (n, with)))
        .collect();
    names.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    for (name, with) in names {
        out = replace_folded(&out, name, with);
    }
    out
}

/// The newest file in `dir` whose name starts with `prefix` and ends with `suffix`.
pub fn newest(dir: &Path, prefix: &str, suffix: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_lowercase();
            name.starts_with(prefix) && name.ends_with(suffix) && e.path().is_file()
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// A file's text for the export: lossy UTF-8, a log cut to its end.
fn read_for_export(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(LOG_TAIL);
    let mut text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if start > 0 {
        text.insert_str(0, &format!("[the first {start} bytes of this log are left out; its end follows]\n"));
    }
    Some(text)
}

/// Writes the export's files into `folder` (made if missing): each found
/// file, redacted, plus `check.txt` and `about.txt`. Says which were written.
pub fn gather(folder: &Path, reports: &Path, logs: &Path, check: &str, about: &str, private: &Private) -> std::io::Result<Vec<String>> {
    std::fs::create_dir_all(folder)?;
    let mut written = Vec::new();
    let found = [
        ("run-report.txt", newest(reports, "run-", ".txt")),
        ("selftest-report.txt", newest(reports, "selftest-", ".txt")),
        // A run's log, not an earlier export beside it.
        ("run-log.txt", newest(logs, "", ".log")),
    ];
    for (name, path) in found {
        if let Some(text) = path.as_deref().and_then(read_for_export) {
            std::fs::write(folder.join(name), redact(&text, private))?;
            written.push(name.to_string());
        }
    }
    for (name, text) in [("check.txt", check), ("about.txt", about)] {
        std::fs::write(folder.join(name), redact(text, private))?;
        written.push(name.to_string());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private() -> Private {
        Private {
            game_dir: Some(r"D:\Games\Some Title\".into()),
            profile: Some(r"E:\Profiles\Alex".into()),
            user: Some("Alex".into()),
            computer: Some("ALEX-PC".into()),
        }
    }

    #[test]
    fn personal_details_are_taken_out() {
        let log = "game_dir    : D:\\games\\some title\nlog e:\\profiles\\alex\\AppData\\Local\\victory\\logs\\x.log\n\
                   path=E:/Profiles/Alex/AppData/Local\nmachine ALEX-PC user alex\nshort ab\n";
        let p = private();
        let out = redact(log, &p);
        assert!(!out.to_lowercase().contains("alex"), "{out}");
        assert!(!out.to_lowercase().contains("some title"), "{out}");
        assert!(out.contains("game_dir    : <game folder>"), "{out}");
        assert!(out.contains("%USERPROFILE%\\AppData\\Local\\victory\\logs\\x.log"), "{out}");
        assert!(out.contains("path=%USERPROFILE%/AppData/Local"), "{out}");
        assert!(out.contains("machine <pc> user <user>"), "{out}");
        assert!(out.contains("short ab"));
    }

    #[test]
    fn short_names_are_left_alone() {
        let p = Private { user: Some("al".into()), ..Default::default() };
        assert_eq!(redact("final value", &p), "final value");
    }

    #[test]
    fn an_export_holds_what_was_found_and_nothing_private() {
        let root = std::env::temp_dir().join(format!("roxbox-launcher-export-{}", std::process::id()));
        let (reports, logs, out) = (root.join("reports"), root.join("logs"), root.join("out"));
        std::fs::create_dir_all(&reports).unwrap();
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(reports.join("run-20261128-000000.txt"), "frame_ms = 62\n").unwrap();
        std::fs::write(logs.join("20261128-000000-series-s.log"), "opened D:\\Games\\Some Title\\title.exe\n").unwrap();
        // An earlier export, newer than the log: not taken for it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(logs.join("rox-box-logs-20261128-000001.zip"), "an earlier export").unwrap();
        let written = gather(&out, &reports, &logs, "[  OK   ] Game folder D:\\Games\\Some Title", "launcher 0.1.0", &private()).unwrap();
        assert_eq!(written, ["run-report.txt", "run-log.txt", "check.txt", "about.txt"]);
        let log = std::fs::read_to_string(out.join("run-log.txt")).unwrap();
        assert_eq!(log, "opened <game folder>\\title.exe\n");
        assert!(std::fs::read_to_string(out.join("check.txt")).unwrap().ends_with("<game folder>"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
