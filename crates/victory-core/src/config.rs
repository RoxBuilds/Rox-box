//! Where the user's files live.
//!
//! Resolution order for each value: environment variable, then the config
//! file, then the default. The config file is the one `VICTORY_CONFIG`
//! names, else `victory.toml` in the current directory, else `victory.toml`
//! next to the executable (so a release folder works from anywhere).
//! A relative path written in the config file is relative to that file's
//! folder; one given in an environment variable is used as it is.
//! See `victory.example.toml` at the repository root.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::policy::ensure_outside_repo;

#[derive(Debug, Default, Clone)]
pub struct Config {
    /// The user's dump of the title.
    pub game_dir: Option<PathBuf>,
    /// Root for all derived data. Always outside the repository.
    pub cache_dir: PathBuf,
    /// What titles write and must keep (persistent local storage). Never
    /// inside the cache, which may be deleted at any time.
    pub data_dir: PathBuf,
    /// Other titles used to exercise platform-level code.
    pub test_titles: Vec<PathBuf>,
    /// Optional user-supplied TOML file defining byte-checked binary patches.
    pub patches_file: Option<PathBuf>,
    /// Optional user-supplied TOML file defining shader profile patterns and hashes.
    pub shader_profile: Option<PathBuf>,
    /// Optional user-supplied path to an external `bcpack-tables.bin` file.
    pub bcpack_tables: Option<PathBuf>,
    /// The config file that was read, if any.
    pub source: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    paths: FilePaths,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePaths {
    game_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    test_titles: Vec<PathBuf>,
    patches_file: Option<PathBuf>,
    shader_profile: Option<PathBuf>,
    bcpack_tables: Option<PathBuf>,
}

impl Config {
    /// Loads configuration from the environment and the config file.
    pub fn load() -> Result<Self> {
        let explicit = explicit_config_path(|k| std::env::var_os(k));
        let cwd = std::env::current_dir().ok();
        let beside = exe_dir();
        let (source, parsed) = match locate_config(explicit, cwd.as_deref(), beside.as_deref())? {
            Some(path) => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading the config file {}", path.display()))?;
                let parsed: FileConfig = toml::from_str(&text).with_context(|| {
                    format!(
                        "the config file {} is not valid (compare it with victory.example.toml; \
                         Windows paths go in 'single quotes')",
                        path.display()
                    )
                })?;
                (Some(path), parsed)
            }
            None => (None, FileConfig::default()),
        };
        Self::resolve(parsed.paths, source, |k| std::env::var_os(k))
    }

    /// Where [`Config::load`] looks for `victory.toml` when `VICTORY_CONFIG`
    /// is not set, in order (for messages).
    pub fn search_locations() -> Vec<PathBuf> {
        let mut all: Vec<PathBuf> = Vec::new();
        for dir in std::env::current_dir().ok().into_iter().chain(exe_dir()) {
            let file = dir.join(CONFIG_NAME);
            if !all.contains(&file) {
                all.push(file);
            }
        }
        all
    }

    fn resolve(
        file: FilePaths,
        source: Option<PathBuf>,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self> {
        let env_path = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        // What the file says is relative to the file, wherever it is run from.
        let base = source.as_deref().and_then(Path::parent).map(Path::to_path_buf);
        let in_file = |p: PathBuf| match &base {
            Some(base) if p.is_relative() && !base.as_os_str().is_empty() => base.join(p),
            _ => p,
        };
        let file = FilePaths {
            game_dir: file.game_dir.map(in_file),
            cache_dir: file.cache_dir.map(in_file),
            data_dir: file.data_dir.map(in_file),
            test_titles: file.test_titles.into_iter().map(in_file).collect(),
            patches_file: file.patches_file.map(in_file),
            shader_profile: file.shader_profile.map(in_file),
            bcpack_tables: file.bcpack_tables.map(in_file),
        };
        let test_titles = match env("VICTORY_TEST_TITLES") {
            Some(v) if !v.is_empty() => std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect(),
            _ => file.test_titles,
        };
        let cache_dir = env_path("VICTORY_CACHE_DIR")
            .or(file.cache_dir)
            .map_or_else(|| default_dir("cache"), Ok)?;
        let data_dir = env_path("VICTORY_DATA_DIR")
            .or(file.data_dir)
            .map_or_else(|| default_dir("data"), Ok)?;
        Ok(Config {
            game_dir: env_path("VICTORY_GAME_DIR").or(file.game_dir),
            cache_dir,
            data_dir,
            test_titles,
            patches_file: env_path("VICTORY_PATCHES_FILE").or(file.patches_file),
            shader_profile: env_path("VICTORY_SHADER_PROFILE").or(file.shader_profile),
            bcpack_tables: env_path("VICTORY_BCPACK_TABLES").or(file.bcpack_tables),
            source,
        })
    }

    /// Where to tell the user to put a new `victory.toml`: next to the
    /// executable for a distribution build (found from any current
    /// directory), in the current directory for a build in a source tree
    /// (whose executable is deep in the build folder).
    pub fn suggested_location() -> PathBuf {
        let all = Self::search_locations();
        let pick = if cfg!(victory_dist) { all.last() } else { all.first() };
        pick.cloned().unwrap_or_else(|| PathBuf::from(CONFIG_NAME))
    }

    /// The game directory, or a helpful error if it is not configured.
    pub fn require_game_dir(&self) -> Result<&Path> {
        self.game_dir.as_deref().with_context(|| match &self.source {
            Some(file) => format!(
                "no game folder is set: add  game_dir = '<folder with your title's files>'  \
                 under [paths] in {} (or set VICTORY_GAME_DIR)",
                file.display()
            ),
            None => format!(
                "no game folder is set and no config file was found: copy victory.example.toml \
                 to {} and set game_dir in it (or set VICTORY_GAME_DIR)",
                Self::suggested_location().display()
            ),
        })
    }

    /// Returns `<cache>/<sub>`, creating it, after checking the asset policy.
    pub fn cache_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.cache_dir, sub)
    }

    /// Returns `<data>/<sub>`, creating it, after checking the asset policy.
    pub fn data_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.data_dir, sub)
    }
}

/// The config file explicitly named by the environment, if any. An empty
/// `VICTORY_CONFIG` means unset, like the other path variables.
fn explicit_config_path(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    env("VICTORY_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The config file's name where it is looked for.
pub const CONFIG_NAME: &str = "victory.toml";

/// The folder this executable is in.
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// The config file to read: `explicit` (`VICTORY_CONFIG`, which must exist),
/// else `victory.toml` in `cwd`, else `victory.toml` in `exe_dir`; none if
/// neither is there.
fn locate_config(
    explicit: Option<PathBuf>,
    cwd: Option<&Path>,
    exe_dir: Option<&Path>,
) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        // Made absolute so that what it says stays relative to it.
        let path = match cwd {
            Some(cwd) if path.is_relative() => cwd.join(path),
            _ => path,
        };
        anyhow::ensure!(
            path.is_file(),
            "VICTORY_CONFIG points at {}, which does not exist (or is not a file)",
            path.display()
        );
        return Ok(Some(path));
    }
    Ok(cwd
        .into_iter()
        .chain(exe_dir)
        .map(|dir| dir.join(CONFIG_NAME))
        .find(|file| file.is_file()))
}

/// The top-level keys of the TOML file at `path`, or why it cannot be read
/// as TOML (for checking a user's optional files before a run).
pub fn toml_top_level_keys(path: &Path) -> Result<Vec<String>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let table: toml::Table =
        toml::from_str(&text).with_context(|| format!("{} is not valid TOML", path.display()))?;
    Ok(table.keys().cloned().collect())
}

fn subdir(root: &Path, sub: &str) -> Result<PathBuf> {
    let dir = root.join(sub);
    ensure_outside_repo(&dir)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// `%LOCALAPPDATA%/victory/<name>` (elsewhere the XDG or home cache).
fn default_dir(name: &str) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("cannot determine a user directory; set VICTORY_CACHE_DIR and VICTORY_DATA_DIR")?;
    Ok(base.join("victory").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn environment_overrides_file() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            cache_dir: Some("cache-from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_GAME_DIR").then(|| OsString::from("from-env"));
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-env"));
        assert_eq!(cfg.cache_dir, PathBuf::from("cache-from-file"));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_GAME_DIR").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-file"));
    }

    #[test]
    fn empty_test_titles_env_is_ignored() {
        let file = FilePaths {
            test_titles: vec!["from-file".into()],
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_TEST_TITLES").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.test_titles, vec![PathBuf::from("from-file")]);
    }

    #[test]
    fn test_titles_env_splits_paths() {
        let value = std::env::join_paths(["first", "second"]).unwrap();
        let file = FilePaths::default();
        let env = |k: &str| (k == "VICTORY_TEST_TITLES").then(|| value.clone());
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(
            cfg.test_titles,
            vec![PathBuf::from("first"), PathBuf::from("second")]
        );
    }

    #[test]
    fn empty_config_env_is_ignored() {
        let empty = |k: &str| (k == "VICTORY_CONFIG").then(OsString::new);
        assert_eq!(explicit_config_path(empty), None);
        let set = |k: &str| (k == "VICTORY_CONFIG").then(|| OsString::from("custom.toml"));
        assert_eq!(
            explicit_config_path(set),
            Some(PathBuf::from("custom.toml"))
        );
    }

    #[test]
    fn config_file_lookup_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (cwd, exe) = (tmp.path().join("cwd"), tmp.path().join("exe"));
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&exe).unwrap();
        let find = |explicit: Option<PathBuf>| locate_config(explicit, Some(&cwd), Some(&exe));
        // Nothing anywhere: no file, and no error.
        assert_eq!(find(None).unwrap(), None);
        // Next to the executable only.
        std::fs::write(exe.join(CONFIG_NAME), "").unwrap();
        assert_eq!(find(None).unwrap(), Some(exe.join(CONFIG_NAME)));
        // The current directory comes before the executable's.
        std::fs::write(cwd.join(CONFIG_NAME), "").unwrap();
        assert_eq!(find(None).unwrap(), Some(cwd.join(CONFIG_NAME)));
        // VICTORY_CONFIG comes before both, and must exist.
        let other = tmp.path().join("other.toml");
        assert!(find(Some(other.clone())).is_err());
        std::fs::write(&other, "").unwrap();
        assert_eq!(find(Some(other.clone())).unwrap(), Some(other));
        // A relative VICTORY_CONFIG is relative to the current directory.
        std::fs::write(cwd.join("mine.toml"), "").unwrap();
        assert_eq!(
            find(Some(PathBuf::from("mine.toml"))).unwrap(),
            Some(cwd.join("mine.toml"))
        );
        // Without a current directory, the executable's folder still works.
        assert_eq!(
            locate_config(None, None, Some(&exe)).unwrap(),
            Some(exe.join(CONFIG_NAME))
        );
    }

    #[test]
    fn paths_in_the_file_are_relative_to_the_file() {
        let dir = std::env::temp_dir().join("victory-config-test-folder");
        let absolute = dir.join("elsewhere").join("game");
        let file = FilePaths {
            game_dir: Some(absolute.clone()),
            cache_dir: Some("cache".into()),
            patches_file: Some("patches.toml".into()),
            test_titles: vec!["other".into()],
            ..Default::default()
        };
        // The environment's values are used as they are.
        let env = |k: &str| (k == "VICTORY_DATA_DIR").then(|| OsString::from("data"));
        let cfg = Config::resolve(file, Some(dir.join(CONFIG_NAME)), env).unwrap();
        assert_eq!(cfg.game_dir, Some(absolute));
        assert_eq!(cfg.cache_dir, dir.join("cache"));
        assert_eq!(cfg.patches_file, Some(dir.join("patches.toml")));
        assert_eq!(cfg.test_titles, vec![dir.join("other")]);
        assert_eq!(cfg.data_dir, PathBuf::from("data"));
    }

    #[test]
    fn a_missing_game_folder_says_what_to_do() {
        let cfg = Config {
            source: Some("somewhere/victory.toml".into()),
            ..Default::default()
        };
        let message = cfg.require_game_dir().unwrap_err().to_string();
        assert!(message.contains("game_dir") && message.contains("victory.toml"), "{message}");
        let message = Config::default().require_game_dir().unwrap_err().to_string();
        assert!(message.contains("victory.example.toml"), "{message}");
    }

    #[test]
    fn toml_keys_of_a_user_file() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("good.toml");
        std::fs::write(&good, "[[patch]]\nname = 'a'\n").unwrap();
        assert_eq!(toml_top_level_keys(&good).unwrap(), ["patch"]);
        let bad = tmp.path().join("bad.toml");
        std::fs::write(&bad, "[[patch\n").unwrap();
        assert!(toml_top_level_keys(&bad).is_err());
        assert!(toml_top_level_keys(&tmp.path().join("none.toml")).is_err());
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../victory.example.toml");
        let parsed: FileConfig = toml::from_str(text).unwrap();
        assert!(parsed.paths.game_dir.is_some());
    }

    #[test]
    fn external_asset_paths_override_from_env() {
        let file = FilePaths {
            patches_file: Some("file-patches.toml".into()),
            shader_profile: Some("file-shaders.toml".into()),
            bcpack_tables: Some("file-bcpack.bin".into()),
            ..Default::default()
        };
        let env = |k: &str| match k {
            "VICTORY_PATCHES_FILE" => Some(OsString::from("env-patches.toml")),
            "VICTORY_BCPACK_TABLES" => Some(OsString::new()),
            _ => None,
        };
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.patches_file, Some(PathBuf::from("env-patches.toml")));
        assert_eq!(cfg.shader_profile, Some(PathBuf::from("file-shaders.toml")));
        assert_eq!(cfg.bcpack_tables, Some(PathBuf::from("file-bcpack.bin")));
    }

    #[test]
    fn cache_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        let cfg = Config {
            cache_dir: inside,
            ..Default::default()
        };
        assert!(cfg.cache_subdir("x").is_err());
    }

    #[test]
    fn data_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
        let cfg = Config {
            data_dir: inside,
            ..Default::default()
        };
        assert!(cfg.data_subdir("x").is_err());
    }
}
