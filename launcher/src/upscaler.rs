//! The Upscaler choice of the Play page, and what it means for a run.
//!
//! The launcher knows no title. "Host FSR" is the engine's own upscaler (it
//! needs AMD's `amd_fidelityfx_dx12.dll`, which the person supplies: see
//! docs/upscaling.md). "Title's own" leaves the title's upscaler in charge.
//! "Title's simple" asks for a faster, grainier one that exists only if the
//! person's own local patch file defines it: its switch (name fixed here,
//! value the person's, default 6) is given only when they turned that on in
//! Settings. Every choice is a plain `NAME=value` text, put first in a run's
//! switches (see [`crate::switches::more_for_run`]).

use std::path::{Path, PathBuf};

/// The file the host upscaler needs, under `vendor\`.
pub const FSR_DLL: &str = "amd_fidelityfx_dx12.dll";

/// Where the steps of installing it are.
pub const DOCS_URL: &str = "https://github.com/RoxBuilds/Rox-box/blob/main/docs/upscaling.md";

/// The choices, as stored in `launcher.toml`, and their words.
pub const CHOICES: [(&str, &str); 3] = [("host", "Host FSR (if installed)"), ("title", "Title's own"), ("simple", "Title's simple (faster, grainier)")];

/// The value `local patch group` has until the person changes it.
pub const DEFAULT_GROUP: &str = "6";

/// What the choice stands for, as far as it is known here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice<'a> {
    /// One of [`CHOICES`]' keys.
    pub which: &'a str,
    /// Whether the host's DLL was found.
    pub installed: bool,
    /// The person turned the title's simple upscaler's group on in Settings.
    pub simple_on: bool,
    /// Its value.
    pub group: &'a str,
}

/// The first `vendor\amd_fidelityfx_dx12.dll` that exists: beside victory.exe,
/// else under the engine's data folder.
pub fn find_dll(exe_dir: Option<&Path>, data_dir: Option<&Path>) -> Option<PathBuf> {
    [exe_dir, data_dir].into_iter().flatten().map(|dir| dir.join("vendor").join(FSR_DLL)).find(|file| file.is_file())
}

/// The `NAME=value;...` text of a choice (empty when it gives nothing).
/// Host FSR is switched on only when the DLL is there, so that a PC without
/// it runs as it did before; the title's own and the simple one switch it off.
pub fn switches(c: Choice<'_>) -> String {
    match c.which {
        "title" => "VICTORY_HOST_UPSCALER=0".to_string(),
        "simple" => {
            let group = c.group.trim();
            if c.simple_on && !group.is_empty() && crate::switches::valid_value(group) {
                format!("VICTORY_HOST_UPSCALER=0;VICTORY_UPSCALER={group}")
            } else {
                "VICTORY_HOST_UPSCALER=0".to_string()
            }
        }
        _ if c.installed => "VICTORY_HOST_UPSCALER=1".to_string(),
        _ => String::new(),
    }
}

/// A line for the card: what the choice does now.
pub fn explain(c: Choice<'_>) -> String {
    match c.which {
        "title" => "The title's own upscaler is used; the host's is off.".to_string(),
        "simple" if c.simple_on => format!("The host's is off and the title's simple upscaler (group {}) is asked for.", c.group.trim()),
        "simple" => "The host's is off. The simple upscaler is asked for only if you turn on its group in Settings, Launch defaults.".to_string(),
        _ if c.installed => "The host's FSR is used.".to_string(),
        _ => "The host's FSR is used once its DLL is installed; until then the title's own runs.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(which: &str, installed: bool, simple_on: bool) -> String {
        switches(Choice { which, installed, simple_on, group: "6" })
    }

    #[test]
    fn each_choice_gives_its_own_text() {
        assert_eq!(choice("host", true, false), "VICTORY_HOST_UPSCALER=1");
        assert_eq!(choice("host", false, false), "");
        assert_eq!(choice("title", true, true), "VICTORY_HOST_UPSCALER=0");
        assert_eq!(choice("simple", true, false), "VICTORY_HOST_UPSCALER=0");
        assert_eq!(choice("simple", true, true), "VICTORY_HOST_UPSCALER=0;VICTORY_UPSCALER=6");
        assert_eq!(choice("anything else", true, false), "VICTORY_HOST_UPSCALER=1");
    }

    #[test]
    fn the_simple_group_is_the_persons_and_must_fit() {
        let with = |group| switches(Choice { which: "simple", installed: false, simple_on: true, group });
        assert_eq!(with(" 9 "), "VICTORY_HOST_UPSCALER=0;VICTORY_UPSCALER=9");
        assert_eq!(with(""), "VICTORY_HOST_UPSCALER=0");
        assert_eq!(with("1;VICTORY_X=2"), "VICTORY_HOST_UPSCALER=0", "a value that would add a pair is not given");
    }

    #[test]
    fn the_dll_is_found_beside_the_engine_or_in_its_data_folder() {
        let root = std::env::temp_dir().join(format!("roxbox-launcher-fsr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (exe, data) = (root.join("exe"), root.join("data"));
        std::fs::create_dir_all(exe.join("vendor")).unwrap();
        std::fs::create_dir_all(data.join("vendor")).unwrap();
        assert_eq!(find_dll(Some(&exe), Some(&data)), None);
        assert_eq!(find_dll(None, None), None);
        std::fs::write(data.join("vendor").join(FSR_DLL), b"x").unwrap();
        assert_eq!(find_dll(Some(&exe), Some(&data)), Some(data.join("vendor").join(FSR_DLL)));
        assert_eq!(find_dll(Some(&exe), None), None);
        std::fs::write(exe.join("vendor").join(FSR_DLL), b"x").unwrap();
        assert_eq!(find_dll(Some(&exe), Some(&data)), Some(exe.join("vendor").join(FSR_DLL)), "beside the engine comes first");
        // A folder of that name is no DLL.
        std::fs::remove_file(exe.join("vendor").join(FSR_DLL)).unwrap();
        std::fs::create_dir_all(exe.join("vendor").join(FSR_DLL)).unwrap();
        assert_eq!(find_dll(Some(&exe), None), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
