//! Repository maintenance. Run with `cargo xtask <command>`.

mod policy;
mod switches;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce the asset policy: the repository holds only small text files
    /// we wrote. Checks tracked and untracked-but-not-ignored files.
    Policy {
        /// Check only what is staged for commit (used by the pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
    /// Point git at .githooks/ so every commit runs the policy check.
    InstallHooks,
    /// Generate `docs/switches.md`, a reference of every `VICTORY_*` switch
    /// collected from the source (the comments where each is read).
    Switches {
        /// Write nothing; fail if the file on disk is not what would be generated.
        #[arg(long)]
        check: bool,
    },
    /// Build the hermetic distribution binary (`target/release-dist/victory.exe`):
    /// `[profile.release-dist]`, the C runtime linked in (no redistributable
    /// to install), no timestamp, no build-machine paths; then check it
    /// (`verify-dist`).
    Dist,
    /// Build the launcher as it is shipped (`launcher/target/release/roxbox-launcher.exe`):
    /// its own workspace and release profile, with the distribution build's
    /// flags (the C runtime linked in, no timestamp, no build-machine paths);
    /// then check it (`verify-dist`).
    DistLauncher,
    /// Check that a built binary names no build-machine path (the workspace,
    /// the user profile, the Cargo home) and no debug-symbol file.
    VerifyDist {
        /// The binary (default: target/release-dist/victory.exe).
        file: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::InstallHooks => {
            let status = Command::new("git")
                .args(["config", "core.hooksPath", ".githooks"])
                .current_dir(&root)
                .status()
                .context("running git")?;
            anyhow::ensure!(
                status.success(),
                "git config failed (is this a git repository?)"
            );
            println!("hooks installed: commits now run `cargo xtask policy --staged`");
            Ok(())
        }
        Cmd::Switches { check } => switches::run(&root, check),
        Cmd::Dist => run_dist(&root),
        Cmd::DistLauncher => run_dist_launcher(&root),
        Cmd::VerifyDist { file } => verify_dist(&root, &file.unwrap_or_else(|| dist_binary(&root))),
    }
}

/// Where `dist` leaves the binary.
fn dist_binary(root: &Path) -> PathBuf {
    let name = if cfg!(windows) { "victory.exe" } else { "victory" };
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| root.join("target"), PathBuf::from);
    target.join("release-dist").join(name)
}

/// The build machine's folders a distribution binary must not name, each
/// with what the compiler writes instead, least specific first.
fn host_prefixes(root: &Path) -> Vec<(PathBuf, &'static str)> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = var("USERPROFILE").or_else(|| var("HOME"));
    let cargo_home = var("CARGO_HOME").or_else(|| home.as_ref().map(|h| h.join(".cargo")));
    let rustup_home = var("RUSTUP_HOME").or_else(|| home.as_ref().map(|h| h.join(".rustup")));
    let mut all = Vec::new();
    all.extend(home.map(|p| (p, "/user")));
    all.extend(rustup_home.map(|p| (p, "/rustup")));
    all.extend(cargo_home.map(|p| (p, "/cargo")));
    all.push((root.to_path_buf(), "/src"));
    all
}

/// The compiler flags of the distribution build. `existing` is the caller's
/// `RUSTFLAGS`, split on whitespace as Cargo does.
fn dist_rustflags(prefixes: &[(PathBuf, &'static str)], existing: &str, windows: bool) -> Vec<String> {
    let mut flags: Vec<String> = existing.split_whitespace().map(str::to_owned).collect();
    // rustc applies the last rule that matches, so the most specific folder
    // (the workspace, which may be inside the user profile) comes last.
    for (from, to) in prefixes {
        flags.push(format!("--remap-path-prefix={}={to}", from.display()));
    }
    // Leaves out what is compiled from the build machine's paths
    // (`victory_core::policy`'s own source tree).
    flags.extend(["--cfg", "victory_dist"].map(String::from));
    if windows {
        // The C runtime is linked in: nothing to install beside the binary.
        flags.extend(["-C", "target-feature=+crt-static"].map(String::from));
        // No debug directory (no symbol-file path), and no timestamp or
        // other per-build value in the headers.
        flags.extend(["-C", "link-arg=/DEBUG:NONE"].map(String::from));
        flags.extend(["-C", "link-arg=/Brepro"].map(String::from));
    }
    flags
}

/// The distribution build's flags on this machine, as Cargo takes them in
/// `CARGO_ENCODED_RUSTFLAGS` (the caller's own flags first).
fn encoded_dist_rustflags(root: &Path) -> String {
    let existing = match std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        Ok(encoded) if !encoded.is_empty() => encoded.replace('\x1f', " "),
        _ => std::env::var("RUSTFLAGS").unwrap_or_default(),
    };
    dist_rustflags(&host_prefixes(root), &existing, cfg!(windows)).join("\x1f")
}

/// Where `dist-launcher` leaves the launcher. Its target folder is named to
/// Cargo outright, so it is here whatever `CARGO_TARGET_DIR` says.
fn launcher_target(root: &Path) -> PathBuf {
    root.join("launcher").join("target")
}

fn launcher_binary(root: &Path) -> PathBuf {
    let name = if cfg!(windows) { "roxbox-launcher.exe" } else { "roxbox-launcher" };
    launcher_target(root).join("release").join(name)
}

/// The launcher is a workspace of its own (`launcher/`): it is built there,
/// with its own lock file and release profile, and the same flags as the
/// engine's distribution build, so that what is shipped beside the engine
/// passes the same check.
fn run_dist_launcher(root: &Path) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--locked", "--release", "--manifest-path"])
        .arg(root.join("launcher").join("Cargo.toml"))
        .arg("--target-dir")
        .arg(launcher_target(root))
        .current_dir(root)
        .env("CARGO_ENCODED_RUSTFLAGS", encoded_dist_rustflags(root))
        .env_remove("RUSTFLAGS")
        .status()
        .context("running cargo build --release for the launcher")?;
    anyhow::ensure!(status.success(), "the launcher's distribution build failed");
    verify_dist(root, &launcher_binary(root))
}

fn run_dist(root: &Path) -> Result<()> {
    let flags = encoded_dist_rustflags(root);
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--locked", "--profile", "release-dist", "-p", "victory"])
        .current_dir(root)
        .env("CARGO_ENCODED_RUSTFLAGS", flags)
        .env_remove("RUSTFLAGS")
        .status()
        .context("running cargo build --profile release-dist")?;
    anyhow::ensure!(status.success(), "hermetic distribution build failed");
    verify_dist(root, &dist_binary(root))
}

/// Fails if the binary at `file` names a build-machine folder or a
/// debug-symbol file.
fn verify_dist(root: &Path, file: &Path) -> Result<()> {
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let prefixes: Vec<String> = host_prefixes(root)
        .iter()
        .map(|(path, _)| path.display().to_string())
        .collect();
    let mut found = host_paths_in(&bytes, &prefixes);
    found.extend(symbol_files_in(&bytes));
    anyhow::ensure!(
        found.is_empty(),
        "{} is not hermetic:\n  {}",
        file.display(),
        found.join("\n  ")
    );
    println!("hermetic: {} ({} bytes) names no build-machine path", file.display(), bytes.len());
    Ok(())
}

/// Lower case, with one kind of path separator: how paths are compared.
fn folded(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .map(|b| if *b == b'/' { b'\\' } else { b.to_ascii_lowercase() })
        .collect()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The `prefixes` (folders) found in `bytes`, as 8-bit or 16-bit text, in
/// either case and with either separator.
fn host_paths_in(bytes: &[u8], prefixes: &[String]) -> Vec<String> {
    let haystack = folded(bytes);
    let mut found = Vec::new();
    for prefix in prefixes {
        let narrow = folded(prefix.trim_end_matches(['\\', '/']).as_bytes());
        // Too short to be a folder of the build machine's own.
        if narrow.len() < 4 {
            continue;
        }
        let wide: Vec<u8> = narrow.iter().flat_map(|&b| [b, 0]).collect();
        for (needle, kind) in [(&narrow, "text"), (&wide, "16-bit text")] {
            if let Some(at) = find(&haystack, needle) {
                found.push(format!("contains the path {prefix} ({kind}, at offset {at:#x})"));
            }
        }
    }
    found
}

/// Paths of debug-symbol files in `bytes`: a CodeView record in the PE debug
/// directory, or any text that is a path ending in `.pdb`.
fn symbol_files_in(bytes: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    if has_codeview_record(bytes) == Some(true) {
        found.push("has a CodeView debug record (the path of a symbol file)".to_owned());
    }
    let haystack = folded(bytes);
    let mut from = 0;
    while let Some(at) = find(&haystack[from..], b".pdb").map(|at| at + from) {
        let start = haystack[..at]
            .iter()
            .rposition(|b| !(b.is_ascii_graphic() || *b == b' '))
            .map_or(0, |p| p + 1);
        let before = &bytes[start..at + 4];
        if before.contains(&b'\\') || before.contains(&b'/') {
            found.push(format!(
                "names a symbol file: {} (at offset {start:#x})",
                String::from_utf8_lossy(before)
            ));
        }
        from = at + 4;
    }
    found
}

/// Whether the PE image in `bytes` has a CodeView entry in its debug
/// directory; `None` if it is no PE32+ image.
fn has_codeview_record(bytes: &[u8]) -> Option<bool> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    if u16_at(0)? != 0x5a4d {
        return None;
    }
    let pe = u32_at(0x3c)? as usize;
    if u32_at(pe)? != 0x4550 || u16_at(pe + 24)? != 0x20b {
        return None;
    }
    let sections = u16_at(pe + 6)? as usize;
    let optional = pe + 24;
    let table = optional + u16_at(pe + 20)? as usize;
    // Data directory 6 is the debug directory.
    let (rva, size) = (u32_at(optional + 112 + 6 * 8)?, u32_at(optional + 112 + 6 * 8 + 4)?);
    if rva == 0 || size == 0 {
        return Some(false);
    }
    let offset = (0..sections).find_map(|i| {
        let at = table + i * 40;
        let (start, raw_size, raw) = (u32_at(at + 12)?, u32_at(at + 16)?, u32_at(at + 20)?);
        (rva >= start && rva < start + raw_size).then(|| (rva - start + raw) as usize)
    })?;
    // Entries of 28 bytes; the type is at 12, CodeView is 2.
    Some((0..size as usize / 28).any(|i| u32_at(offset + i * 28 + 12) == Some(2)))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workspace_remap_comes_last() {
        let prefixes = vec![
            (PathBuf::from("C:/Users/example"), "/user"),
            (PathBuf::from("C:/Users/example/.cargo"), "/cargo"),
            (PathBuf::from("C:/Users/example/work/tree"), "/src"),
        ];
        let flags = dist_rustflags(&prefixes, "  -C  target-cpu=x86-64-v3\t--cfg extra ", true);
        // The caller's flags are split, and come first.
        assert_eq!(&flags[..4], ["-C", "target-cpu=x86-64-v3", "--cfg", "extra"]);
        let remaps: Vec<&String> =
            flags.iter().filter(|f| f.starts_with("--remap-path-prefix=")).collect();
        assert_eq!(remaps.len(), 3);
        assert!(remaps[0].ends_with("=/user") && remaps[2].ends_with("=/src"), "{remaps:?}");
        let has = |a: &str, b: &str| flags.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(has("--cfg", "victory_dist"));
        assert!(has("-C", "target-feature=+crt-static"));
        assert!(has("-C", "link-arg=/Brepro"));
        assert!(has("-C", "link-arg=/DEBUG:NONE"));
        // Elsewhere: no linker arguments of the Windows linker.
        let other = dist_rustflags(&prefixes, "", false);
        assert!(!other.iter().any(|f| f.contains("link-arg") || f.contains("crt-static")));
    }

    #[test]
    fn the_launcher_is_built_in_its_own_target_folder() {
        let root = Path::new("work").join("tree");
        let binary = launcher_binary(&root);
        assert!(binary.starts_with(root.join("launcher").join("target").join("release")), "{binary:?}");
        assert_eq!(binary.file_stem().and_then(|s| s.to_str()), Some("roxbox-launcher"));
        assert_ne!(binary, dist_binary(&root));
    }

    #[test]
    fn host_paths_are_found_in_any_spelling() {
        let prefixes = vec!["C:\\Users\\Example\\".to_owned(), "D:\\".to_owned()];
        assert!(host_paths_in(b"nothing of the kind /src/crates/x.rs", &prefixes).is_empty());
        assert_eq!(host_paths_in(b"at c:/users/example/.cargo/x", &prefixes).len(), 1);
        assert_eq!(host_paths_in(b"at C:\\USERS\\EXAMPLE\\x", &prefixes).len(), 1);
        let wide: Vec<u8> = "C:\\Users\\Example\\x".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let found = host_paths_in(&wide, &prefixes);
        assert!(found.len() == 1 && found[0].contains("16-bit"), "{found:?}");
    }

    #[test]
    fn symbol_file_paths_are_found() {
        assert!(symbol_files_in(b"reads the .pdb path of an image\0x.pdb\0").is_empty());
        let found = symbol_files_in(b"\0C:\\build\\deps\\victory.pdb\0");
        assert!(found.len() == 1 && found[0].contains("victory.pdb"), "{found:?}");
        assert_eq!(symbol_files_in(b"\x01/out/a.PDB\0 and \x02b\\c.pdb").len(), 2);
    }

    /// A PE32+ image of one section holding a debug directory of `kinds`.
    fn image_with_debug_entries(kinds: &[u32]) -> Vec<u8> {
        let mut image = vec![0u8; 0x400 + 0x200];
        let put16 = |image: &mut Vec<u8>, at: usize, v: u16| image[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let put32 = |image: &mut Vec<u8>, at: usize, v: u32| image[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put16(&mut image, 0, 0x5a4d);
        put32(&mut image, 0x3c, 0x80);
        put32(&mut image, 0x80, 0x4550);
        put16(&mut image, 0x80 + 6, 1);
        put16(&mut image, 0x80 + 20, 240);
        put16(&mut image, 0x80 + 24, 0x20b);
        let optional = 0x80 + 24;
        put32(&mut image, optional + 112 + 48, if kinds.is_empty() { 0 } else { 0x1000 });
        put32(&mut image, optional + 112 + 52, kinds.len() as u32 * 28);
        let section = optional + 240;
        put32(&mut image, section + 12, 0x1000);
        put32(&mut image, section + 16, 0x200);
        put32(&mut image, section + 20, 0x400);
        for (i, kind) in kinds.iter().enumerate() {
            put32(&mut image, 0x400 + i * 28 + 12, *kind);
        }
        image
    }

    #[test]
    fn a_codeview_record_is_found_in_the_debug_directory() {
        assert_eq!(has_codeview_record(b"not an image"), None);
        assert_eq!(has_codeview_record(&image_with_debug_entries(&[])), Some(false));
        // 16 is the reproducible-build entry the linker leaves.
        assert_eq!(has_codeview_record(&image_with_debug_entries(&[16])), Some(false));
        assert_eq!(has_codeview_record(&image_with_debug_entries(&[16, 2])), Some(true));
    }
}
