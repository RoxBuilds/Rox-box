//! The two files the launcher writes: the `game_dir` line of `victory.toml`
//! (as `play.cmd -Setup` does: every other line stays, a timestamped copy is
//! kept) and its own `local\launcher.toml` with the title folders and the
//! toggles. Paths are written in TOML 'single quotes', as the example config
//! asks, so backslashes stay as they are.

use std::path::{Path, PathBuf};

/// A path as a TOML string: literal quotes where the path allows them.
pub fn toml_path(path: &str) -> String {
    if path.contains('\'') {
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("'{path}'")
    }
}

/// The value of a `key = <string>` line (literal or basic string), if `line` is one.
fn string_value(line: &str, key: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix(key)?.trim_start().strip_prefix('=')?.trim();
    if let Some(body) = rest.strip_prefix('\'') {
        return body.split('\'').next().map(str::to_string);
    }
    let body = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            c => out.push(c),
        }
    }
    None
}

/// `game_dir` under `[paths]` of a `victory.toml`'s text.
pub fn game_dir_of(text: &str) -> Option<String> {
    let mut in_paths = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_paths = t == "[paths]";
        } else if in_paths && let Some(v) = string_value(line, "game_dir") {
            return Some(v);
        }
    }
    None
}

/// `text` with `game_dir` under `[paths]` set to `folder`: the line is
/// changed where it is, or added under `[paths]`, or `[paths]` is added.
/// Every other line stays, with the file's own line endings.
pub fn with_game_dir(text: &str, folder: &str) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let new_line = format!("game_dir = {}", toml_path(folder));
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut in_paths = false;
    let mut paths_at = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with('[') {
            in_paths = t == "[paths]";
            if in_paths {
                paths_at = Some(i);
            }
        } else if in_paths && string_value(line, "game_dir").is_some() {
            lines[i] = new_line;
            return lines.join(nl) + nl;
        }
    }
    match paths_at {
        Some(i) => lines.insert(i + 1, new_line),
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[paths]".to_string());
            lines.push(new_line);
        }
    }
    lines.join(nl) + nl
}

/// Seconds since 1970 as `yyyyMMdd-HHmmss` in UTC (the stamp of a kept copy).
pub fn stamp(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3_600, rem % 3_600 / 60, rem % 60)
}

pub fn now_stamp() -> String {
    stamp(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

/// Writes `game_dir` into `victory.toml`. A file that is there keeps a copy
/// `victory.toml.bak-<stamp>` beside it first; a missing one is made with
/// only `game_dir` set. Returns what was done, in words for the status line.
pub fn write_game_dir(config: &Path, folder: &str) -> std::io::Result<String> {
    match std::fs::read_to_string(config) {
        Ok(text) => {
            if game_dir_of(&text).as_deref() == Some(folder) {
                return Ok("victory.toml already names this folder".to_string());
            }
            let copy = PathBuf::from(format!("{}.bak-{}", config.display(), now_stamp()));
            std::fs::copy(config, &copy)?;
            std::fs::write(config, with_game_dir(&text, folder))?;
            Ok(format!("victory.toml updated (copy kept: {})", copy.file_name().unwrap_or_default().to_string_lossy()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let text = format!(
                "# Rox-box configuration, written by the launcher. victory.example.toml explains every entry.\n\n[paths]\ngame_dir = {}\n",
                toml_path(folder)
            );
            std::fs::write(config, text)?;
            Ok("victory.toml written".to_string())
        }
        Err(e) => Err(e),
    }
}

/// What the launcher remembers between starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    /// The title folders, in the order they were added.
    pub titles: Vec<String>,
    /// The one launched and prepared for.
    pub active: Option<String>,
    pub fast: bool,
    pub log: bool,
    pub prepare: bool,
    pub safe: bool,
    /// `""` (the first built), or one of [`crate::paths::BUILDS`].
    pub build: String,
    /// `"SeriesX"` or `"SeriesS"`.
    pub console: String,
    /// Extra `NAME=value;NAME=value` for play.ps1's `-More`.
    pub more: String,
    /// What the launcher's window does when a title starts: `"stay"`,
    /// `"minimize"` (and come back when the title ends) or `"close"`.
    pub on_play: String,
}

impl Default for Library {
    fn default() -> Self {
        Library {
            titles: Vec::new(),
            active: None,
            fast: false,
            log: true,
            prepare: false,
            safe: false,
            build: String::new(),
            console: "SeriesX".to_string(),
            more: String::new(),
            on_play: "stay".to_string(),
        }
    }
}

impl Library {
    /// Reads the file's text; lines it does not know are left out.
    pub fn parse(text: &str) -> Library {
        let mut lib = Library::default();
        for line in text.lines() {
            if let Some(v) = string_value(line, "title") {
                if !lib.titles.contains(&v) {
                    lib.titles.push(v);
                }
            } else if let Some(v) = string_value(line, "active") {
                lib.active = Some(v);
            } else if let Some(v) = string_value(line, "build") {
                lib.build = v;
            } else if let Some(v) = string_value(line, "console") {
                lib.console = v;
            } else if let Some(v) = string_value(line, "more") {
                lib.more = v;
            } else if let Some(v) = string_value(line, "on_play") {
                lib.on_play = v;
            } else if let Some((key, value)) = line.split_once('=') {
                let on = value.trim() == "true";
                match key.trim() {
                    "fast" => lib.fast = on,
                    "log" => lib.log = on,
                    "prepare" => lib.prepare = on,
                    "safe" => lib.safe = on,
                    _ => {}
                }
            }
        }
        if !crate::paths::BUILDS.contains(&lib.build.as_str()) {
            lib.build.clear();
        }
        if !["minimize", "close"].contains(&lib.on_play.as_str()) {
            lib.on_play = "stay".to_string();
        }
        if lib.console != "SeriesS" {
            lib.console = "SeriesX".to_string();
        }
        if lib.active.as_ref().is_some_and(|a| !lib.titles.contains(a)) {
            lib.active = lib.titles.first().cloned();
        }
        lib
    }

    pub fn render(&self) -> String {
        let mut out = String::from(
            "# The Rox-box Launcher's own file: your title folders and toggles.\n# It holds paths of this PC: it is never committed (local\\ is git-ignored).\n\n",
        );
        for t in &self.titles {
            out += &format!("title = {}\n", toml_path(t));
        }
        if let Some(a) = &self.active {
            out += &format!("active = {}\n", toml_path(a));
        }
        out += &format!(
            "\nfast = {}\nlog = {}\nprepare = {}\nsafe = {}\nbuild = {}\nconsole = {}\nmore = {}\non_play = {}\n",
            self.fast,
            self.log,
            self.prepare,
            self.safe,
            toml_path(&self.build),
            toml_path(&self.console),
            toml_path(&self.more),
            toml_path(&self.on_play)
        );
        out
    }

    pub fn load(file: &Path) -> Library {
        std::fs::read_to_string(file).map(|t| Library::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, self.render())
    }

    /// Adds `folder` (once) and makes it the active one.
    pub fn add(&mut self, folder: &str) {
        let folder = folder.trim_end_matches(['\\', '/']).to_string();
        if folder.is_empty() {
            return;
        }
        if !self.titles.iter().any(|t| t.eq_ignore_ascii_case(&folder)) {
            self.titles.push(folder.clone());
        }
        self.active = self.titles.iter().find(|t| t.eq_ignore_ascii_case(&folder)).cloned();
    }

    pub fn remove(&mut self, folder: &str) {
        self.titles.retain(|t| t != folder);
        if self.active.as_deref() == Some(folder) {
            self.active = self.titles.first().cloned();
        }
    }
}

/// What a dropped or picked path means: a folder is a title folder, a file
/// stands for the folder it is in (the title's executable was dropped).
pub fn title_folder_of(path: &Path) -> Option<String> {
    if path.is_dir() {
        Some(path.to_string_lossy().into_owned())
    } else if path.is_file() {
        path.parent().map(|p| p.to_string_lossy().into_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_dir_is_changed_where_it_is_and_every_other_line_stays() {
        let text = "# top\r\n[paths]\r\n# REQUIRED\r\ngame_dir = 'C:\\Games\\Old'\r\n# cache_dir = 'D:\\c'\r\n\r\n[other]\r\ngame_dir = 'no'\r\n";
        let new = with_game_dir(text, "D:\\Titles\\New One");
        assert_eq!(game_dir_of(&new).as_deref(), Some("D:\\Titles\\New One"));
        assert_eq!(new.replace("D:\\Titles\\New One", "C:\\Games\\Old"), text);
        assert!(new.contains("[other]\r\ngame_dir = 'no'"), "another table's key is left alone");
    }

    #[test]
    fn game_dir_is_added_when_missing() {
        let under = with_game_dir("[paths]\ncache_dir = 'D:\\c'\n", "C:\\T");
        assert_eq!(under, "[paths]\ngame_dir = 'C:\\T'\ncache_dir = 'D:\\c'\n");
        let none = with_game_dir("# only a comment\n", "C:\\T");
        assert_eq!(none, "# only a comment\n\n[paths]\ngame_dir = 'C:\\T'\n");
        assert_eq!(game_dir_of(&with_game_dir("", "C:\\T")).as_deref(), Some("C:\\T"));
    }

    #[test]
    fn a_path_with_a_quote_is_written_as_a_basic_string_and_read_back() {
        let odd = "C:\\It's here";
        assert_eq!(toml_path(odd), "\"C:\\\\It's here\"");
        assert_eq!(game_dir_of(&with_game_dir("", odd)).as_deref(), Some(odd));
        assert_eq!(game_dir_of("[paths]\ngame_dir = \"C:\\\\a\\\\b\"\n").as_deref(), Some("C:\\a\\b"));
        assert_eq!(game_dir_of("[paths]\n# game_dir = 'C:\\x'\n"), None, "a commented line is no setting");
    }

    #[test]
    fn the_library_reads_back_what_it_wrote() {
        let mut lib = Library::default();
        lib.add("C:\\Titles\\One\\");
        lib.add("D:\\Two");
        lib.add("c:\\titles\\one");
        assert_eq!(lib.titles, ["C:\\Titles\\One", "D:\\Two"]);
        assert_eq!(lib.active.as_deref(), Some("C:\\Titles\\One"));
        lib.fast = true;
        lib.build = "release".to_string();
        lib.console = "SeriesS".to_string();
        lib.more = "VICTORY_AMBIENT=0.8;VICTORY_PIPELINE_BOOK=0".to_string();
        lib.on_play = "minimize".to_string();
        assert_eq!(Library::parse(&lib.render()), lib);
        lib.remove("C:\\Titles\\One");
        assert_eq!(lib.active.as_deref(), Some("D:\\Two"));
    }

    #[test]
    fn what_the_library_does_not_know_falls_back() {
        let lib = Library::parse("build = 'fastest'\nconsole = 'Other'\nactive = 'C:\\gone'\nfast = yes\non_play = 'explode'\n");
        assert_eq!(lib, Library::default());
    }

    #[test]
    fn stamps_are_utc_dates() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(951_782_400 + 3_661), "20000229-010101");
        assert_eq!(stamp(1_798_761_599), "20261231-235959");
    }

    #[test]
    fn writing_keeps_a_copy_and_makes_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("roxbox-launcher-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("victory.toml");
        assert_eq!(write_game_dir(&file, "C:\\A").unwrap(), "victory.toml written");
        assert!(write_game_dir(&file, "C:\\A").unwrap().contains("already"));
        assert!(write_game_dir(&file, "C:\\B").unwrap().contains("copy kept"));
        assert_eq!(game_dir_of(&std::fs::read_to_string(&file).unwrap()).as_deref(), Some("C:\\B"));
        let copies = std::fs::read_dir(&dir).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".bak-")).count();
        assert_eq!(copies, 1);
        assert_eq!(title_folder_of(&file).as_deref(), Some(dir.to_string_lossy().as_ref()));
        assert_eq!(title_folder_of(&dir).as_deref(), Some(dir.to_string_lossy().as_ref()));
    }
}
