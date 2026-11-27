//! What a run needs, checked before it starts and said in plain words:
//! `victory check` prints every prerequisite as OK, WARN or MISSING with
//! what to do about it, and `victory run --execute` refuses to start
//! without the ones it cannot do without (instead of stopping later with
//! nothing saying why).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use victory_core::Config;

/// Free commit memory under which a title may be refused memory, in MiB.
const COMMIT_WANTED_MB: u64 = 12 * 1024;
/// `VICTORY_SKIP_PREFLIGHT=1`: `run --execute` starts whatever the host lacks.
pub const SKIP_PREFLIGHT_ENV: &str = "VICTORY_SKIP_PREFLIGHT";
pub const SKIP_CPU_CHECK_ENV: &str = "VICTORY_SKIP_CPU_CHECK";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ok,
    /// A run starts, and may run worse or stop later.
    Warn,
    /// A run cannot start.
    Missing,
}

/// One prerequisite: what it is, how it stands and how to put it right.
#[derive(Debug, Clone)]
pub struct Item {
    pub name: &'static str,
    pub state: State,
    pub detail: String,
    pub fix: Option<String>,
}

impl Item {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Item { name, state: State::Ok, detail: detail.into(), fix: None }
    }

    fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Item { name, state: State::Warn, detail: detail.into(), fix: Some(fix.into()) }
    }

    fn missing(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Item { name, state: State::Missing, detail: detail.into(), fix: Some(fix.into()) }
    }

    /// As an error for `run`: what is wrong, then what to do.
    fn message(&self) -> String {
        match &self.fix {
            Some(fix) => format!("{}: {}. {fix}", self.name, self.detail),
            None => format!("{}: {}", self.name, self.detail),
        }
    }
}

/// An error whose text was printed already: only the exit code is left.
#[derive(Debug)]
pub struct Reported;

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the checks above did not all pass")
    }
}

impl std::error::Error for Reported {}

/// The `.exe` files directly in `dir`, sorted.
fn exes_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("the game folder {} cannot be read", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        })
        .collect();
    found.sort();
    Ok(found)
}

/// The title executable to run: `given`, or the only `.exe` in the
/// configured game folder.
pub fn title_exe(cfg: &Config, given: Option<&Path>) -> Result<PathBuf> {
    if let Some(exe) = given {
        return Ok(exe.to_path_buf());
    }
    let dir = cfg.require_game_dir()?;
    anyhow::ensure!(
        dir.is_dir(),
        "the game folder {} does not exist: correct game_dir in {} (or VICTORY_GAME_DIR)",
        dir.display(),
        config_name(cfg)
    );
    let found = exes_in(dir)?;
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => anyhow::bail!(
            "no .exe file in the game folder {}: point game_dir at the folder that holds the \
             title's executable, or name the executable on the command line",
            dir.display()
        ),
        many => anyhow::bail!(
            "{} .exe files in the game folder {} ({}): name the one to run on the command line \
             (play.cmd -Exe <file>)",
            many.len(),
            dir.display(),
            many.iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn config_name(cfg: &Config) -> String {
    cfg.source.as_ref().map_or_else(|| "victory.toml".to_owned(), |p| p.display().to_string())
}

/// What the start of a file says it is, as far as a run cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageKind {
    /// A 64-bit x86 Windows image: what a run maps.
    Pe64Amd64,
    NotPe,
    Pe32,
    OtherMachine(u16),
}

/// Reads the headers in `head` (the first bytes of a file).
fn image_kind(head: &[u8]) -> ImageKind {
    let u16_at = |at: usize| head.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |at: usize| {
        head.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    if u16_at(0) != Some(0x5a4d) {
        return ImageKind::NotPe;
    }
    let Some(pe) = u32_at(0x3c).map(|at| at as usize) else {
        return ImageKind::NotPe;
    };
    if u32_at(pe) != Some(0x0000_4550) {
        return ImageKind::NotPe;
    }
    let (Some(machine), Some(magic)) = (u16_at(pe + 4), u16_at(pe + 24)) else {
        return ImageKind::NotPe;
    };
    match (machine, magic) {
        (0x8664, 0x20b) => ImageKind::Pe64Amd64,
        (0x8664, _) | (0x014c, _) => ImageKind::Pe32,
        (other, _) => ImageKind::OtherMachine(other),
    }
}

/// `Ok` if the file at `exe` is there and is a 64-bit x86 Windows image.
pub fn validate_title_exe(exe: &Path) -> Result<()> {
    use std::io::Read;
    anyhow::ensure!(
        exe.exists(),
        "the title executable {} does not exist: check the path (and game_dir in victory.toml)",
        exe.display()
    );
    anyhow::ensure!(
        exe.is_file(),
        "{} is a folder, not a title executable: name the .exe file inside it",
        exe.display()
    );
    let mut head = Vec::new();
    std::fs::File::open(exe)
        .and_then(|file| file.take(4096).read_to_end(&mut head))
        .with_context(|| format!("the title executable {} cannot be read", exe.display()))?;
    match image_kind(&head) {
        ImageKind::Pe64Amd64 => Ok(()),
        ImageKind::NotPe => anyhow::bail!(
            "{} is not a Windows executable image (no PE header): it may be encrypted, \
             compressed, incomplete or another kind of file. A run needs the title's decrypted \
             64-bit executable",
            exe.display()
        ),
        ImageKind::Pe32 => anyhow::bail!(
            "{} is a 32-bit image: only 64-bit (PE32+, x64) title executables can be run",
            exe.display()
        ),
        ImageKind::OtherMachine(machine) => anyhow::bail!(
            "{} is built for another processor (machine {machine:#06x}): only x64 title \
             executables can be run",
            exe.display()
        ),
    }
}

/// The user's optional files: each one that is configured must be there and
/// well formed (one that is not is otherwise left out without a word).
pub fn optional_files(cfg: &Config) -> Vec<Item> {
    let toml_file = |name: &'static str, key: &str, path: &Option<PathBuf>, known: &[&str]| {
        let Some(path) = path else {
            return Item::ok(name, "not set (optional)");
        };
        let remove = format!("correct or remove {key} in {}", config_name(cfg));
        if !path.is_file() {
            return Item::missing(name, format!("{} does not exist", path.display()), remove);
        }
        match victory_core::config::toml_top_level_keys(path) {
            Err(e) => Item::missing(name, format!("{e:#}"), remove),
            Ok(keys) => match keys.iter().find(|k| !known.contains(&k.as_str())) {
                Some(unknown) => Item::missing(
                    name,
                    format!(
                        "{} has an unknown entry `{unknown}` (known: {})",
                        path.display(),
                        known.join(", ")
                    ),
                    "see the schema in victory.example.toml",
                ),
                None => Item::ok(name, path.display().to_string()),
            },
        }
    };
    let tables = match &cfg.bcpack_tables {
        None => Item::ok("Format tables", "not set (optional)"),
        Some(path) => match victory_formats::bcpack::Tables::from_file(path) {
            Ok(_) => Item::ok("Format tables", path.display().to_string()),
            Err(e) => Item::missing(
                "Format tables",
                e.to_string(),
                format!("correct or remove bcpack_tables in {}", config_name(cfg)),
            ),
        },
    };
    vec![
        toml_file("Patch file", "patches_file", &cfg.patches_file, &["patch"]),
        toml_file(
            "Shader profile",
            "shader_profile",
            &cfg.shader_profile,
            &["ambient", "bake", "cull_shaders"],
        ),
        tables,
    ]
}

fn cpu_item(has_avx2: bool, skip: Option<&str>) -> Item {
    if has_avx2 {
        Item::ok("Processor", "x64 with AVX2")
    } else if skip == Some("1") {
        Item::warn(
            "Processor",
            format!("no AVX2 ({SKIP_CPU_CHECK_ENV}=1: not checked)"),
            "titles use AVX2 instructions and stop where they meet one",
        )
    } else {
        Item::missing(
            "Processor",
            "this processor has no AVX2 instructions, which titles require",
            format!(
                "a processor from 2013 (Intel) or 2015 (AMD) on has them; \
                 {SKIP_CPU_CHECK_ENV}=1 skips this check (under an instruction emulator, say)"
            ),
        )
    }
}

/// Windows 11 (build 22000) is the first Windows whose own Direct3D 12 runtime
/// gives Shader Model 6.6; Windows 10's stops at 6.5, whatever the driver.
const WINDOWS_11_BUILD: u32 = 22000;
const NEEDS_WINDOWS_11: &str = "Windows 11 (64-bit) is required: Windows 10's Direct3D 12                                 runtime stops at Shader Model 6.5, and titles need 6.6";

fn windows_item(version: Option<(u32, u32, u32)>) -> Item {
    match version {
        None => Item::warn("Windows", "version unknown", NEEDS_WINDOWS_11),
        Some((major, minor, build)) if major < 10 || build < WINDOWS_11_BUILD => Item::missing(
            "Windows",
            format!("version {major}.{minor} build {build} is older than Windows 11"),
            NEEDS_WINDOWS_11,
        ),
        Some((major, minor, build)) => Item::ok("Windows", format!("{major}.{minor} build {build}")),
    }
}

fn memory_item(commit: Option<(u64, u64)>) -> Item {
    match commit {
        None => Item::warn("Memory", "free commit memory unknown", "close other programs before a run"),
        Some((free, total)) if free < COMMIT_WANTED_MB => Item::warn(
            "Memory",
            format!(
                "{:.1} GB of commit memory free of {:.1} GB (under {} GB)",
                free as f64 / 1024.0,
                total as f64 / 1024.0,
                COMMIT_WANTED_MB / 1024
            ),
            "a title can be refused memory: close other programs, or enlarge the page file \
             (Advanced system settings > Performance > Virtual memory)",
        ),
        Some((free, total)) => Item::ok(
            "Memory",
            format!("{:.1} GB of commit memory free of {:.1} GB", free as f64 / 1024.0, total as f64 / 1024.0),
        ),
    }
}

/// What the graphics adapter gives, as items: the device, the shader model,
/// mesh shaders.
#[cfg(windows)]
fn gpu_items(gpu: Result<victory_gpu::preflight::Gpu, victory_gpu::preflight::NoDevice>) -> Vec<Item> {
    const UPDATE: &str = "update the graphics driver (from the card maker's site) and Windows; \
                          if it stays, this graphics card is too old";
    let gpu = match gpu {
        Ok(gpu) => gpu,
        Err(none) => {
            let detail = match &none.adapter {
                Some(name) => format!(
                    "no Direct3D 12 device (feature level 12_0) on {name}: {}",
                    none.reason
                ),
                None => format!("Windows lists no graphics adapter: {}", none.reason),
            };
            return vec![Item::missing(
                "Graphics card",
                detail,
                "a graphics card with Direct3D 12 feature level 12_0 and a current driver is \
                 needed: install or update the graphics driver; over Remote Desktop or in a \
                 virtual machine there may be no such device",
            )];
        }
    };
    let described = format!(
        "{}, {} MB video memory, driver {}",
        gpu.name,
        gpu.dedicated_mb,
        gpu.driver.as_deref().unwrap_or("unknown")
    );
    let card = if gpu.software {
        Item::warn(
            "Graphics card",
            format!("{described}: a software adapter, not a graphics card"),
            "install the graphics driver: titles run at seconds per frame on this adapter",
        )
    } else {
        Item::ok("Graphics card", described)
    };
    let (major, minor) = gpu.shader_model;
    let model = if (major, minor) >= (6, 6) {
        Item::ok("Shader Model 6.6", format!("supported (highest asked for: {major}.{minor})"))
    } else {
        Item::missing(
            "Shader Model 6.6",
            format!("not supported: the driver and Windows give {major}.{minor} at most"),
            format!("{UPDATE} (Windows 11 is required)"),
        )
    };
    let mesh = if gpu.mesh_shaders {
        Item::ok("Mesh shaders", "supported")
    } else {
        Item::warn(
            "Mesh shaders",
            "not supported: what titles draw with mesh shaders is left out",
            UPDATE,
        )
    };
    vec![card, model, mesh]
}

/// The host's side: processor, Windows, memory, graphics.
fn host_items() -> Vec<Item> {
    #[cfg(target_arch = "x86_64")]
    let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    let has_avx2 = true;
    let skip = std::env::var(SKIP_CPU_CHECK_ENV).ok();
    let mut items = vec![cpu_item(has_avx2, skip.as_deref())];
    #[cfg(windows)]
    {
        items.push(windows_item(victory_gpu::preflight::windows_version()));
        items.push(memory_item(victory_gpu::preflight::commit_memory_mb()));
        items.extend(gpu_items(victory_gpu::preflight::gpu()));
    }
    #[cfg(not(windows))]
    items.push(Item::missing("Windows", "this is not Windows", "titles run on Windows 11 (x64)"));
    items
}

/// What to say of a folder of game files (`game_dir`, a `test_titles`
/// entry) that lies inside a copy of the source tree: allowed, and warned
/// about by `victory check` and `victory config` alike, since files there
/// are one `git add` away from being published. `None` outside one.
pub fn game_files_in_source_tree(dir: &Path) -> Option<String> {
    victory_core::ensure_outside_repo(dir).err().map(|_| {
        format!(
            "{} is inside a copy of the Rox-box source tree: game files are safer outside it",
            dir.display()
        )
    })
}

/// The fix for [`game_files_in_source_tree`].
pub const MOVE_GAME_FILES: &str =
    "move the folder outside the source tree (and correct victory.toml), so that game files can never be committed";

/// Where derived data and saves go: named, and refused inside a source tree.
fn folder_item(name: &'static str, key: &str, dir: &Path) -> Item {
    match victory_core::ensure_outside_repo(dir) {
        Ok(()) => Item::ok(name, dir.display().to_string()),
        Err(e) => Item::missing(name, e.to_string(), format!("set {key} to a folder outside the source tree")),
    }
}

/// The user's side: the config file, the game folder, the executable, the
/// optional files, the folders written to.
fn user_items(cfg: &Config, exe: Option<&Path>) -> Vec<Item> {
    let mut items = Vec::new();
    items.push(match &cfg.source {
        Some(file) => Item::ok("Config file", file.display().to_string()),
        None if cfg.game_dir.is_some() => Item::ok("Config file", "none (the environment is used)"),
        None => Item::missing(
            "Config file",
            "no victory.toml found",
            format!(
                "copy victory.example.toml to {} and set game_dir in it",
                Config::suggested_location().display()
            ),
        ),
    });
    let game_dir_ok = match &cfg.game_dir {
        None => {
            items.push(Item::missing(
                "Game folder",
                "not set",
                "set  game_dir = '<folder with your title's files>'  under [paths] in victory.toml",
            ));
            false
        }
        Some(dir) if !dir.is_dir() => {
            items.push(Item::missing(
                "Game folder",
                format!("{} does not exist", dir.display()),
                format!("correct game_dir in {}", config_name(cfg)),
            ));
            false
        }
        Some(dir) => {
            items.push(match game_files_in_source_tree(dir) {
                Some(inside) => Item::warn("Game folder", inside, MOVE_GAME_FILES),
                None => Item::ok("Game folder", dir.display().to_string()),
            });
            true
        }
    };
    if exe.is_some() || game_dir_ok {
        items.push(match title_exe(cfg, exe).and_then(|exe| validate_title_exe(&exe).map(|()| exe)) {
            Ok(exe) => Item::ok("Title executable", format!("{} (64-bit x64 image)", exe.display())),
            Err(e) => Item::missing(
                "Title executable",
                format!("{e:#}"),
                "game_dir must hold the title's complete, decrypted files; name the executable if there are several (play.cmd -Exe <file>)",
            ),
        });
    }
    items.extend(optional_files(cfg));
    for dir in &cfg.test_titles {
        if let Some(inside) = game_files_in_source_tree(dir) {
            items.push(Item::warn("Test title folder", inside, MOVE_GAME_FILES));
        }
    }
    items.push(folder_item("Cache folder", "cache_dir", &cfg.cache_dir));
    items.push(folder_item("Data folder", "data_dir", &cfg.data_dir));
    items
}

fn print(items: &[Item]) {
    for item in items {
        let tag = match item.state {
            State::Ok => "[  OK   ]",
            State::Warn => "[ WARN  ]",
            State::Missing => "[MISSING]",
        };
        println!("{tag} {:<17} {}", item.name, item.detail);
        if let Some(fix) = item.fix.as_deref().filter(|_| item.state != State::Ok) {
            println!("          {:<17} fix: {fix}", "");
        }
    }
}

/// `victory check`: every prerequisite, one a line, and an error if a run
/// cannot start. Starts nothing and writes nothing.
pub fn check(exe: Option<&Path>) -> Result<()> {
    println!("victory {} preflight\n", env!("CARGO_PKG_VERSION"));
    let mut items = Vec::new();
    match Config::load() {
        Ok(cfg) => items.extend(user_items(&cfg, exe)),
        Err(e) => items.push(Item::missing(
            "Config file",
            format!("{e:#}"),
            "correct the file (victory.example.toml shows every entry)",
        )),
    }
    items.extend(host_items());
    print(&items);
    let count = |state: State| items.iter().filter(|i| i.state == state).count();
    let (missing, warnings) = (count(State::Missing), count(State::Warn));
    println!();
    if missing == 0 {
        println!("Ready to run ({warnings} warning(s)).");
        Ok(())
    } else {
        println!("Not ready: {missing} thing(s) to fix above ({warnings} warning(s)).");
        Err(Reported.into())
    }
}

/// Before `run --execute` starts a title: an error naming everything the
/// host lacks that a run cannot do without, warnings for the rest.
pub fn before_execute(cfg: &Config) -> Result<()> {
    let skip = std::env::var(SKIP_PREFLIGHT_ENV).is_ok_and(|v| v == "1");
    let mut items = optional_files(cfg);
    // A title that relaunched itself was checked when it first started, and
    // the software adapter of a diagnostics build is not the default one.
    let relaunched = std::env::var_os("VICTORY_RELAUNCHED_FROM").is_some();
    let warp = std::env::var("VICTORY_D3D12_WARP").is_ok_and(|v| !v.is_empty() && v != "0");
    if !relaunched && !warp {
        items.extend(host_items());
    }
    for item in items.iter().filter(|i| i.state == State::Warn) {
        eprintln!("warning: {}", item.message());
    }
    let missing: Vec<String> = items
        .iter()
        .filter(|i| i.state == State::Missing)
        .map(Item::message)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    if skip {
        for message in &missing {
            eprintln!("warning ({SKIP_PREFLIGHT_ENV}=1, starting anyway): {message}");
        }
        return Ok(());
    }
    anyhow::bail!(
        "this PC or its configuration cannot run a title yet:\n  - {}\n\
         (`victory check` shows every prerequisite; {SKIP_PREFLIGHT_ENV}=1 starts anyway)",
        missing.join("\n  - ")
    )
}

/// What to add to an error Windows gave for lack of memory, if it is one.
pub fn memory_hint(error: &anyhow::Error) -> Option<&'static str> {
    // ERROR_NOT_ENOUGH_MEMORY, ERROR_OUTOFMEMORY, ERROR_COMMITMENT_LIMIT,
    // ERROR_NO_SYSTEM_RESOURCES, ERROR_WORKING_SET_QUOTA.
    const CODES: [i32; 5] = [8, 14, 1455, 1450, 1453];
    let by_code = error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| CODES.contains(&code))
    });
    let text = format!("{error:#}");
    let by_text = CODES.iter().any(|code| text.contains(&format!("os error {code})")));
    (by_code || by_text).then_some(
        "Windows ran out of memory to give (commit memory: RAM plus page file). Close other \
         programs, or enlarge the page file (Advanced system settings > Performance > Virtual \
         memory), and start again.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pe_head(machine: u16, magic: u16) -> Vec<u8> {
        let mut head = vec![0u8; 0x200];
        head[..2].copy_from_slice(b"MZ");
        head[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        head[0x80..0x84].copy_from_slice(b"PE\0\0");
        head[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        head[0x98..0x9a].copy_from_slice(&magic.to_le_bytes());
        head
    }

    #[test]
    fn image_kinds() {
        assert_eq!(image_kind(&pe_head(0x8664, 0x20b)), ImageKind::Pe64Amd64);
        assert_eq!(image_kind(&pe_head(0x014c, 0x10b)), ImageKind::Pe32);
        assert_eq!(image_kind(&pe_head(0xaa64, 0x20b)), ImageKind::OtherMachine(0xaa64));
        assert_eq!(image_kind(b"not an image"), ImageKind::NotPe);
        assert_eq!(image_kind(b""), ImageKind::NotPe);
        // A header that points past the end of what was read.
        let mut cut = pe_head(0x8664, 0x20b);
        cut[0x3c..0x40].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        assert_eq!(image_kind(&cut), ImageKind::NotPe);
    }

    #[test]
    fn this_test_binary_is_a_title_shaped_image() {
        if cfg!(all(windows, target_arch = "x86_64")) {
            validate_title_exe(&std::env::current_exe().unwrap()).unwrap();
        }
    }

    #[test]
    fn a_wrong_title_executable_is_explained() {
        let tmp = std::env::temp_dir().join(format!("victory-preflight-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let missing = validate_title_exe(&tmp.join("none.exe")).unwrap_err().to_string();
        assert!(missing.contains("does not exist"), "{missing}");
        let folder = validate_title_exe(&tmp).unwrap_err().to_string();
        assert!(folder.contains("is a folder"), "{folder}");
        let text = tmp.join("text.exe");
        std::fs::write(&text, "hello").unwrap();
        let not_pe = validate_title_exe(&text).unwrap_err().to_string();
        assert!(not_pe.contains("not a Windows executable image"), "{not_pe}");
        let small = tmp.join("small.exe");
        std::fs::write(&small, pe_head(0x014c, 0x10b)).unwrap();
        let pe32 = validate_title_exe(&small).unwrap_err().to_string();
        assert!(pe32.contains("32-bit"), "{pe32}");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn the_only_exe_of_the_game_folder_is_the_title() {
        let tmp = std::env::temp_dir().join(format!("victory-preflight-exe-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("sub.exe")).unwrap();
        let cfg = Config { game_dir: Some(tmp.clone()), ..Default::default() };
        let none = title_exe(&cfg, None).unwrap_err().to_string();
        assert!(none.contains("no .exe file"), "{none}");
        std::fs::write(tmp.join("Title.EXE"), "").unwrap();
        std::fs::write(tmp.join("notes.txt"), "").unwrap();
        assert_eq!(title_exe(&cfg, None).unwrap(), tmp.join("Title.EXE"));
        std::fs::write(tmp.join("other.exe"), "").unwrap();
        let two = title_exe(&cfg, None).unwrap_err().to_string();
        assert!(two.contains("2 .exe files") && two.contains("other.exe"), "{two}");
        // One named on the command line is taken as it is.
        assert_eq!(title_exe(&cfg, Some(Path::new("x.exe"))).unwrap(), PathBuf::from("x.exe"));
        let unset = title_exe(&Config::default(), None).unwrap_err().to_string();
        assert!(unset.contains("no game folder"), "{unset}");
        let gone = Config { game_dir: Some(tmp.join("gone")), ..Default::default() };
        let gone = title_exe(&gone, None).unwrap_err().to_string();
        assert!(gone.contains("does not exist"), "{gone}");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// One rule for `check` and `config`: game files inside a copy of the
    /// source tree are warned about, the folders written to are refused.
    #[test]
    fn folders_inside_a_source_tree() {
        let tmp = std::env::temp_dir().join(format!("victory-preflight-tree-{}", std::process::id()));
        let (tree, outside) = (tmp.join("tree"), tmp.join("outside"));
        std::fs::create_dir_all(tree.join("game")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(tree.join(victory_core::policy::ROOT_MARKER), "").unwrap();
        assert!(game_files_in_source_tree(&outside).is_none());
        let cfg = Config {
            game_dir: Some(tree.join("game")),
            test_titles: vec![outside.clone(), tree.join("other")],
            cache_dir: tree.join("cache"),
            data_dir: outside.join("data"),
            ..Default::default()
        };
        let items = user_items(&cfg, None);
        let state = |name: &str| -> Vec<State> {
            items.iter().filter(|i| i.name == name).map(|i| i.state).collect()
        };
        assert_eq!(state("Game folder"), [State::Warn]);
        assert_eq!(state("Test title folder"), [State::Warn]);
        assert_eq!(state("Cache folder"), [State::Missing]);
        assert_eq!(state("Data folder"), [State::Ok]);
        let game = items.iter().find(|i| i.name == "Game folder").unwrap();
        assert!(game.detail.contains("Rox-box source tree"), "{}", game.detail);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn optional_files_are_checked_only_when_configured() {
        let none = optional_files(&Config::default());
        assert!(none.iter().all(|i| i.state == State::Ok));
        let tmp = std::env::temp_dir().join(format!("victory-preflight-opt-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let patches = tmp.join("patches.toml");
        std::fs::write(&patches, "[[patch]]\nname = 'a'\n").unwrap();
        let profile = tmp.join("profile.toml");
        std::fs::write(&profile, "[ambiant]\n").unwrap();
        let tables = tmp.join("tables.bin");
        std::fs::write(&tables, [0u8; 16]).unwrap();
        let cfg = Config {
            patches_file: Some(patches.clone()),
            shader_profile: Some(profile),
            bcpack_tables: Some(tables),
            ..Default::default()
        };
        let items = optional_files(&cfg);
        assert_eq!(items[0].state, State::Ok);
        assert_eq!(items[1].state, State::Missing);
        assert!(items[1].detail.contains("ambiant"), "{}", items[1].detail);
        assert_eq!(items[2].state, State::Missing);
        std::fs::write(&patches, "[[patch\n").unwrap();
        assert_eq!(optional_files(&cfg)[0].state, State::Missing);
        std::fs::remove_file(&patches).unwrap();
        let gone = &optional_files(&cfg)[0];
        assert!(gone.state == State::Missing && gone.detail.contains("does not exist"));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn host_items_say_what_is_wrong() {
        assert_eq!(cpu_item(true, None).state, State::Ok);
        assert_eq!(cpu_item(false, Some("1")).state, State::Warn);
        for skip in [None, Some(""), Some("0"), Some("true")] {
            let item = cpu_item(false, skip);
            assert_eq!(item.state, State::Missing);
            assert!(item.message().contains("AVX2") && item.message().contains(SKIP_CPU_CHECK_ENV));
        }
        assert_eq!(windows_item(Some((10, 0, 26100))).state, State::Ok);
        assert_eq!(windows_item(Some((10, 0, 22000))).state, State::Ok);
        for old in [(10, 0, 19045), (10, 0, 18363), (6, 1, 7601)] {
            let item = windows_item(Some(old));
            assert_eq!(item.state, State::Missing);
            assert!(item.message().contains("Windows 11") && item.message().contains("6.5"), "{}", item.message());
        }
        assert_eq!(windows_item(None).state, State::Warn);
        assert_eq!(memory_item(Some((20_000, 40_000))).state, State::Ok);
        let low = memory_item(Some((4096, 16_384)));
        assert!(low.state == State::Warn && low.detail.contains("4.0 GB"), "{}", low.detail);
    }

    #[cfg(windows)]
    #[test]
    fn graphics_items_say_what_is_wrong() {
        use victory_gpu::preflight::{Gpu, NoDevice};
        let gpu = Gpu {
            name: "Some Adapter".into(),
            driver: Some("1.2.3.4".into()),
            dedicated_mb: 8192,
            software: false,
            shader_model: (6, 6),
            mesh_shaders: true,
        };
        assert!(gpu_items(Ok(gpu.clone())).iter().all(|i| i.state == State::Ok));
        let old = gpu_items(Ok(Gpu { shader_model: (6, 5), mesh_shaders: false, ..gpu.clone() }));
        assert_eq!(old[1].state, State::Missing);
        assert!(old[1].message().contains("6.5") && old[1].message().contains("driver"));
        assert_eq!(old[2].state, State::Warn);
        assert_eq!(gpu_items(Ok(Gpu { software: true, ..gpu }))[0].state, State::Warn);
        let none = gpu_items(Err(NoDevice { adapter: None, reason: "refused".into() }));
        assert!(none.len() == 1 && none[0].state == State::Missing);
        assert!(none[0].message().contains("Direct3D 12"));
    }

    #[test]
    fn out_of_memory_is_explained() {
        let commit = anyhow::Error::from(std::io::Error::from_raw_os_error(1455)).context("mapping");
        assert!(memory_hint(&commit).is_some());
        let text = anyhow::anyhow!("LoadLibraryExW(x) failed: no room (os error 8)");
        assert!(memory_hint(&text).is_some());
        let other = anyhow::Error::from(std::io::Error::from_raw_os_error(2)).context("reading");
        assert!(memory_hint(&other).is_none());
    }
}
