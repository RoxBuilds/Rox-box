//! Where the PC upscaler DLL the user supplies is looked for. Nothing of it
//! is in the repository or a release (docs/upscaling.md).

use std::path::{Path, PathBuf};

/// The PC FidelityFX API DLL of the FidelityFX SDK v1.1.x release.
pub const DLL_NAME: &str = "amd_fidelityfx_dx12.dll";

/// The places, as the front end resolved them.
#[derive(Debug, Clone, Default)]
pub struct Places {
    /// `[paths] upscaler_dll` of `victory.toml`, or `VICTORY_UPSCALER_DLL`
    /// (which wins, as for every path): the DLL itself or the folder it is in.
    pub configured: Option<PathBuf>,
    /// The folder `victory.exe` is in (its `vendor` folder is looked in).
    pub exe_dir: Option<PathBuf>,
    /// The data folder (its `vendor` folder is looked in).
    pub data_dir: Option<PathBuf>,
}

/// Where the DLL was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Configured,
    BesideExe,
    DataDir,
}

/// What a look found: the DLL and where, and what was looked at and not
/// there, in order (for the one line a run logs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Found {
    pub dll: Option<(PathBuf, Source)>,
    pub missing: Vec<PathBuf>,
}

/// Looks in order: the configured path (a file, or a folder holding
/// [`DLL_NAME`]), `vendor\` beside the executable, `<data_dir>\vendor\`.
/// A configured path that does not exist is noted and the others are still
/// looked in. `is_file` and `is_dir` are the file system's (tests give their
/// own).
pub fn discover(places: &Places, is_file: &dyn Fn(&Path) -> bool, is_dir: &dyn Fn(&Path) -> bool) -> Found {
    let mut found = Found::default();
    let mut candidates: Vec<(PathBuf, Source)> = Vec::new();
    if let Some(configured) = &places.configured {
        let file = if is_dir(configured) { configured.join(DLL_NAME) } else { configured.clone() };
        candidates.push((file, Source::Configured));
    }
    if let Some(exe) = &places.exe_dir {
        candidates.push((exe.join("vendor").join(DLL_NAME), Source::BesideExe));
    }
    if let Some(data) = &places.data_dir {
        candidates.push((data.join("vendor").join(DLL_NAME), Source::DataDir));
    }
    for (file, source) in candidates {
        if is_file(&file) {
            found.dll = Some((file, source));
            return found;
        }
        found.missing.push(file);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn look(places: &Places, files: &[PathBuf], dirs: &[PathBuf]) -> Found {
        let files: HashSet<PathBuf> = files.iter().cloned().collect();
        let dirs: HashSet<PathBuf> = dirs.iter().cloned().collect();
        discover(places, &|p| files.contains(p), &|p| dirs.contains(p))
    }

    fn places() -> Places {
        Places {
            configured: Some(PathBuf::from("set").join("my.dll")),
            exe_dir: Some(PathBuf::from("exe")),
            data_dir: Some(PathBuf::from("data")),
        }
    }

    #[test]
    fn the_configured_path_comes_first() {
        let p = places();
        let all = [
            PathBuf::from("set").join("my.dll"),
            PathBuf::from("exe").join("vendor").join(DLL_NAME),
            PathBuf::from("data").join("vendor").join(DLL_NAME),
        ];
        let found = look(&p, &all, &[]);
        assert_eq!(found.dll, Some((all[0].clone(), Source::Configured)));
        assert!(found.missing.is_empty());
    }

    #[test]
    fn then_beside_the_executable_then_the_data_folder() {
        let p = places();
        let beside = PathBuf::from("exe").join("vendor").join(DLL_NAME);
        let data = PathBuf::from("data").join("vendor").join(DLL_NAME);
        let found = look(&p, &[beside.clone(), data.clone()], &[]);
        assert_eq!(found.dll, Some((beside.clone(), Source::BesideExe)));
        assert_eq!(found.missing, vec![PathBuf::from("set").join("my.dll")]);
        let found = look(&p, std::slice::from_ref(&data), &[]);
        assert_eq!(found.dll, Some((data, Source::DataDir)));
        assert_eq!(found.missing.len(), 2);
    }

    #[test]
    fn a_configured_folder_holds_the_dll_by_its_name() {
        let p = Places { configured: Some(PathBuf::from("folder")), ..Places::default() };
        let inside = PathBuf::from("folder").join(DLL_NAME);
        let found = look(&p, std::slice::from_ref(&inside), &[PathBuf::from("folder")]);
        assert_eq!(found.dll, Some((inside, Source::Configured)));
    }

    #[test]
    fn nothing_anywhere_names_every_place_looked_at() {
        let found = look(&places(), &[], &[]);
        assert_eq!(found.dll, None);
        assert_eq!(found.missing.len(), 3);
        assert_eq!(discover(&Places::default(), &|_| true, &|_| false), Found::default());
    }

    /// With a real (fake) file on disk: only its name and place matter here,
    /// nothing is loaded.
    #[test]
    fn a_file_on_disk_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = tmp.path().join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(vendor.join(DLL_NAME), b"not a real library").unwrap();
        let p = Places { data_dir: Some(tmp.path().to_path_buf()), ..Places::default() };
        let found = discover(&p, &|p| p.is_file(), &|p| p.is_dir());
        assert_eq!(found.dll, Some((vendor.join(DLL_NAME), Source::DataDir)));
    }
}
