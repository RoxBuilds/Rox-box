//! Where Rox-box is, seen from the launcher: a release folder (victory.exe and
//! play.ps1 beside the launcher) or the source tree (scripts\play.ps1 and
//! target\<build>\victory.exe above it). Nothing is installed and nothing is
//! looked for outside that folder.

use std::path::{Path, PathBuf};

/// The builds `scripts\play.ps1 -Build` knows, in the order it tries them.
pub const BUILDS: [&str; 4] = ["diag", "release", "release-dist", "debug"];

/// What kind of folder the launcher runs from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// An unzipped release: victory.exe, play.cmd and play.ps1 side by side.
    Release,
    /// A checkout: scripts\play.ps1, target\<build>\victory.exe.
    Source,
}

/// The Rox-box folder and what is in it.
#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
    pub layout: Layout,
}

impl Home {
    /// Looks from the launcher's own folder upwards; `ROXBOX_HOME` names the
    /// folder outright (for running a development build of the launcher).
    pub fn find() -> Option<Home> {
        if let Some(named) = std::env::var_os("ROXBOX_HOME") {
            return Home::at(Path::new(&named));
        }
        let exe = std::env::current_exe().ok()?;
        exe.ancestors().skip(1).find_map(Home::at)
    }

    /// `dir` as a Rox-box folder, if it is one.
    pub fn at(dir: &Path) -> Option<Home> {
        if dir.join("victory.exe").is_file() && dir.join("play.ps1").is_file() {
            return Some(Home { root: dir.to_path_buf(), layout: Layout::Release });
        }
        if dir.join("scripts").join("play.ps1").is_file() && dir.join("Cargo.toml").is_file() {
            return Some(Home { root: dir.to_path_buf(), layout: Layout::Source });
        }
        None
    }

    /// `play.ps1`, the script `play.cmd` starts.
    pub fn play_script(&self) -> PathBuf {
        match self.layout {
            Layout::Release => self.root.join("play.ps1"),
            Layout::Source => self.root.join("scripts").join("play.ps1"),
        }
    }

    /// `victory.toml`, where both victory.exe and play.ps1 look for it.
    pub fn config_file(&self) -> PathBuf {
        self.root.join("victory.toml")
    }

    /// The launcher's own file: the title folders and the toggles. Under
    /// `local\`, which the source tree keeps out of git, beside play.ps1's
    /// own local file.
    pub fn library_file(&self) -> PathBuf {
        self.root.join("local").join("launcher.toml")
    }

    /// The builds that exist. A release folder has the one beside it.
    pub fn builds(&self) -> Vec<&'static str> {
        match self.layout {
            Layout::Release => Vec::new(),
            Layout::Source => BUILDS.into_iter().filter(|b| self.victory_of(b).is_file()).collect(),
        }
    }

    fn victory_of(&self, build: &str) -> PathBuf {
        self.root.join("target").join(build).join("victory.exe")
    }

    /// victory.exe: beside the launcher, or of `build` (empty: the first
    /// that is built, as play.ps1 chooses).
    pub fn victory(&self, build: &str) -> Option<PathBuf> {
        match self.layout {
            Layout::Release => Some(self.root.join("victory.exe")),
            Layout::Source if build.is_empty() => self.builds().first().map(|b| self.victory_of(b)),
            Layout::Source => Some(self.victory_of(build)).filter(|p| p.is_file()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("roxbox-launcher-paths-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_release_folder_and_a_checkout_are_told_apart() {
        let release = scratch("release");
        std::fs::write(release.join("victory.exe"), b"").unwrap();
        std::fs::write(release.join("play.ps1"), b"").unwrap();
        let home = Home::at(&release).unwrap();
        assert_eq!(home.layout, Layout::Release);
        assert_eq!(home.victory("").unwrap(), release.join("victory.exe"));
        assert_eq!(home.play_script(), release.join("play.ps1"));
        assert!(home.builds().is_empty());

        let source = scratch("source");
        std::fs::create_dir_all(source.join("scripts")).unwrap();
        std::fs::write(source.join("scripts").join("play.ps1"), b"").unwrap();
        std::fs::write(source.join("Cargo.toml"), b"").unwrap();
        let home = Home::at(&source).unwrap();
        assert_eq!(home.layout, Layout::Source);
        assert!(home.victory("").is_none(), "nothing is built yet");
        std::fs::create_dir_all(source.join("target").join("release")).unwrap();
        std::fs::write(source.join("target").join("release").join("victory.exe"), b"").unwrap();
        assert_eq!(home.builds(), ["release"]);
        assert!(home.victory("").is_some());
        assert!(home.victory("diag").is_none());

        assert!(Home::at(&scratch("empty")).is_none());
    }
}
