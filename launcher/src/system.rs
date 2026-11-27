//! What the launcher reads of this PC, and never changes: memory and the
//! page file from Windows itself, the graphics card and everything a run
//! needs from `victory check` (the same lines a person gets from
//! `play.cmd -Check`). The warnings are the launcher's own words, said
//! before a run instead of after a bad one.

use winreg::RegKey;
use winreg::enums::HKEY_LOCAL_MACHINE;

/// One line of `victory check`: `[  OK   ] Name   detail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckLine {
    /// `OK`, `WARN`, `MISSING`, or whatever a later version says.
    pub status: String,
    pub name: String,
    pub detail: String,
}

/// The lines of `victory check`'s output that are checks, and its last
/// line (the summary), if any.
pub fn parse_check(output: &[String]) -> (Vec<CheckLine>, Option<String>) {
    let mut lines = Vec::new();
    for line in output {
        let Some(rest) = line.trim_start().strip_prefix('[') else { continue };
        let Some((status, rest)) = rest.split_once(']') else { continue };
        let rest = rest.trim();
        let (name, detail) = match rest.find("  ") {
            Some(at) => (rest[..at].trim(), rest[at..].trim()),
            None => (rest, ""),
        };
        if !status.trim().is_empty() && !name.is_empty() {
            lines.push(CheckLine { status: status.trim().to_string(), name: name.to_string(), detail: detail.to_string() });
        }
    }
    let summary = output.iter().rev().find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('[')).cloned();
    (lines, summary)
}

/// The graphics card's name and video memory in MB, out of the check's
/// "Graphics card" line (`<name>, <n> MB video memory, driver ...`).
pub fn gpu_of(lines: &[CheckLine]) -> Option<(String, Option<u64>)> {
    let line = lines.iter().find(|l| l.name == "Graphics card")?;
    let mut parts = line.detail.split(", ");
    let name = parts.next()?.to_string();
    let vram = parts.find_map(|p| p.strip_suffix(" MB video memory")?.trim().parse().ok());
    Some((name, vram))
}

/// How Windows is set to keep its page file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageFile {
    /// No page file at all.
    None,
    /// Windows sizes it as it goes.
    SystemManaged,
    /// A fixed range, in MB (the sum over drives).
    Fixed { initial_mb: u64, maximum_mb: u64 },
    /// The setting could not be read.
    Unknown,
}

/// The `PagingFiles` setting's lines (`<path> <initial MB> <maximum MB>`;
/// `?:\pagefile.sys` or sizes of 0: Windows manages it).
pub fn page_file_of(entries: &[String]) -> PageFile {
    let entries: Vec<&str> = entries.iter().map(|e| e.trim()).filter(|e| !e.is_empty()).collect();
    if entries.is_empty() {
        return PageFile::None;
    }
    let (mut initial, mut maximum) = (0, 0);
    for entry in entries {
        let mut words = entry.rsplitn(3, ' ');
        let max: Option<u64> = words.next().and_then(|w| w.parse().ok());
        let init: Option<u64> = words.next().and_then(|w| w.parse().ok());
        match (init, max) {
            (Some(i), Some(m)) if i > 0 || m > 0 => {
                initial += i;
                maximum += m.max(i);
            }
            _ => return PageFile::SystemManaged,
        }
    }
    PageFile::Fixed { initial_mb: initial, maximum_mb: maximum }
}

/// The facts read from Windows.
#[derive(Debug, Clone)]
pub struct Host {
    /// Installed memory, in MB.
    pub ram_mb: Option<u64>,
    /// The commit limit (memory and page file together), in MB.
    pub commit_limit_mb: Option<u64>,
    pub page_file: PageFile,
    /// For example `Windows 11 24H2 (build 26100)`.
    pub windows: String,
}

impl Host {
    pub fn read() -> Host {
        let (ram_mb, commit_limit_mb) = memory();
        Host { ram_mb, commit_limit_mb, page_file: page_file(), windows: windows() }
    }
}

fn memory() -> (Option<u64>, Option<u64>) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is a MEMORYSTATUSEX of the length it states.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return (None, None);
    }
    (Some(status.ullTotalPhys >> 20), Some(status.ullTotalPageFile >> 20))
}

fn page_file() -> PageFile {
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Memory Management");
    let Ok(key) = key else { return PageFile::Unknown };
    // A list of lines on most PCs; a single line on some.
    if let Ok(entries) = key.get_value::<Vec<String>, _>("PagingFiles") {
        return page_file_of(&entries);
    }
    match key.get_value::<String, _>("PagingFiles") {
        Ok(entry) => page_file_of(&[entry]),
        Err(_) => PageFile::Unknown,
    }
}

/// The name people know a build by: builds from 22000 on are Windows 11,
/// whatever the product name in the registry still says.
pub fn windows_name(build: u32, display_version: &str) -> String {
    let name = if build >= 22_000 { "Windows 11" } else { "Windows 10" };
    if display_version.is_empty() { format!("{name} (build {build})") } else { format!("{name} {display_version} (build {build})") }
}

fn windows() -> String {
    let key = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let Ok(key) = key else { return "Windows (version not read)".to_string() };
    let build: u32 = key.get_value::<String, _>("CurrentBuildNumber").ok().and_then(|b| b.parse().ok()).unwrap_or(0);
    let display: String = key.get_value("DisplayVersion").unwrap_or_default();
    windows_name(build, &display)
}

/// What a warning is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Vram,
    Ram,
    PageFile,
}

/// How much a warning matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Note,
    Warn,
}

/// The launcher's own warnings from the facts. Plain statements of what to
/// expect; nothing here stops a run.
pub fn warnings(host: &Host, vram_mb: Option<u64>) -> Vec<(Subject, Level, String)> {
    let mut out = Vec::new();
    match vram_mb {
        Some(v) if v <= 4_608 => out.push((
            Subject::Vram,
            Level::Warn,
            format!("{:.0} GB of video memory: below what the layer is written for. Expect missing or low-detail textures and long stalls; use Fast.", v as f64 / 1024.0),
        )),
        Some(v) if v <= 8_704 => out.push((
            Subject::Vram,
            Level::Note,
            format!("{:.0} GB of video memory: textures are cut to fit (docs/low-memory-gpus.md). Fast is the preset for this card.", v as f64 / 1024.0),
        )),
        _ => {}
    }
    match host.ram_mb {
        Some(r) if r <= 16_896 => out.push((
            Subject::Ram,
            Level::Warn,
            format!("{:.0} GB of RAM: a large title can use more than this PC has, and then runs from the page file, slowly. Close other programs first.", r as f64 / 1024.0),
        )),
        Some(r) if r <= 24_576 => out.push((Subject::Ram, Level::Note, format!("{:.0} GB of RAM: enough to start; a large title may still fill it.", r as f64 / 1024.0))),
        _ => {}
    }
    match &host.page_file {
        PageFile::None => out.push((Subject::PageFile, Level::Warn, "No page file: a run that needs more memory than is installed stops instead of slowing down.".to_string())),
        PageFile::SystemManaged => out.push((
            Subject::PageFile,
            Level::Note,
            "Page file is system-managed: Windows grows it only when memory runs short, and a run can stall while it grows. A fixed size avoids that.".to_string(),
        )),
        PageFile::Fixed { maximum_mb, .. } if *maximum_mb < 16_384 => out.push((
            Subject::PageFile,
            Level::Warn,
            format!("Page file is fixed at {:.0} GB at most: small for a large title. 16 GB or more leaves room.", *maximum_mb as f64 / 1024.0),
        )),
        _ => {}
    }
    out
}

/// A size in MB as people write it.
pub fn gb(mb: u64) -> String {
    format!("{:.1} GB", mb as f64 / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn check_lines_are_read_as_the_program_prints_them() {
        let out = lines(
            "victory 0.2.0 preflight\n\n[  OK   ] Config file       C:\\x\\victory.toml\n[ WARN  ] Memory            3.0 GB of commit memory free of 16.0 GB\n[MISSING] Game folder       not set: set game_dir\n[  OK   ] Graphics card     Some Vendor Card 4060, 7956 MB video memory, driver 1.2.3\n[  OK   ] Mesh shaders      supported\n\nReady to run (1 warning(s)).\n",
        );
        let (checks, summary) = parse_check(&out);
        assert_eq!(checks.len(), 5);
        assert_eq!(checks[0], CheckLine { status: "OK".into(), name: "Config file".into(), detail: "C:\\x\\victory.toml".into() });
        assert_eq!(checks[1].status, "WARN");
        assert_eq!(checks[2].status, "MISSING");
        assert_eq!(checks[2].detail, "not set: set game_dir");
        assert_eq!(summary.as_deref(), Some("Ready to run (1 warning(s))."));
        assert_eq!(gpu_of(&checks), Some(("Some Vendor Card 4060".to_string(), Some(7956))));
        assert_eq!(gpu_of(&checks[..2]), None);
        assert_eq!(parse_check(&lines("error: no such thing")).0, []);
    }

    #[test]
    fn the_page_file_setting_is_told_apart() {
        let entries = |e: &[&str]| e.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(page_file_of(&entries(&[])), PageFile::None);
        assert_eq!(page_file_of(&entries(&[""])), PageFile::None);
        assert_eq!(page_file_of(&entries(&["?:\\pagefile.sys"])), PageFile::SystemManaged);
        assert_eq!(page_file_of(&entries(&["c:\\pagefile.sys 0 0"])), PageFile::SystemManaged);
        assert_eq!(page_file_of(&entries(&["c:\\pagefile.sys 4096 8192"])), PageFile::Fixed { initial_mb: 4096, maximum_mb: 8192 });
        assert_eq!(
            page_file_of(&entries(&["c:\\page file.sys 1024 2048", "d:\\pagefile.sys 16384 16384"])),
            PageFile::Fixed { initial_mb: 17408, maximum_mb: 18432 }
        );
    }

    #[test]
    fn warnings_follow_the_facts() {
        let host = |ram, page_file| Host { ram_mb: Some(ram), commit_limit_mb: None, page_file, windows: String::new() };
        let roomy = host(65_536, PageFile::Fixed { initial_mb: 32_768, maximum_mb: 32_768 });
        assert!(warnings(&roomy, Some(24_000)).is_empty());
        let small = warnings(&host(16_200, PageFile::SystemManaged), Some(4_096));
        assert_eq!(small.iter().filter(|w| w.1 == Level::Warn).count(), 2, "{small:?}");
        assert_eq!(small.iter().filter(|w| w.1 == Level::Note).count(), 1);
        let eight = warnings(&host(32_768, PageFile::None), Some(7_956));
        assert_eq!((eight[0].0, eight[0].1), (Subject::Vram, Level::Note));
        assert!(eight[0].2.starts_with("8 GB"));
        assert_eq!((eight.last().unwrap().0, eight.last().unwrap().1), (Subject::PageFile, Level::Warn));
        assert!(warnings(&host(65_536, PageFile::Fixed { initial_mb: 2048, maximum_mb: 4096 }), None)[0].2.contains("4 GB"));
    }

    #[test]
    fn windows_is_named_by_its_build() {
        assert_eq!(windows_name(26_100, "24H2"), "Windows 11 24H2 (build 26100)");
        assert_eq!(windows_name(19_045, ""), "Windows 10 (build 19045)");
    }

    #[test]
    fn this_pc_can_be_read() {
        let host = Host::read();
        assert!(host.ram_mb.is_some_and(|r| r > 512));
        assert!(host.windows.starts_with("Windows"));
        assert_eq!(gb(8192), "8.0 GB");
    }
}
