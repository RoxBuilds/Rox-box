//! Updates from the project's own GitHub releases, and nothing else.
//!
//! The newest release is asked for (its name, its notes, its files). A newer
//! one is downloaded into a staging folder under `local\`, checked against the
//! checksum file the release publishes, unpacked, and swapped in: every file
//! of the package replaces the one in the Rox-box folder, whose old copy is
//! first renamed aside (Windows lets a running program be renamed, not
//! overwritten), so a failure in the middle puts everything back. What is
//! yours is never part of it: `victory.toml`, `local\`, and anything the
//! package does not hold (caches and saves live elsewhere anyway).
//!
//! Only in a release folder. In a source tree, or when anything fails, the
//! launcher opens the releases page instead.

use std::path::{Path, PathBuf};

/// Whether the Rox-box folder `home` was installed by the setup program.
pub fn installed(home: &Path) -> bool {
    home.join("unins000.exe").is_file()
}

/// The setup program's options for a quiet update over `home`: progress shown,
/// no questions, this user's install, the same folder, the launcher started
/// again afterwards (`/RELAUNCH=1`, read by the setup script).
pub fn setup_arguments(home: &Path) -> Vec<String> {
    ["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CURRENTUSER", "/RELAUNCH=1"]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(format!("/DIR={}", home.display())))
        .collect()
}

/// Where a release's files must come from to be taken at all.
pub const DOWNLOAD_PREFIX: &str = "https://github.com/RoxBuilds/Rox-box/releases/download/";

/// The end of a replaced file's name while its new copy is tried.
const ASIDE: &str = ".old-update";

/// A release as the API describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    /// The release notes, as written (Markdown).
    pub notes: String,
    /// The package: its address and its size in bytes.
    pub package: Option<(String, u64)>,
    /// The package's checksum file, if the release has one.
    pub checksum: Option<String>,
    /// The setup program (an installed copy updates by running it) and its
    /// checksum file, if the release has them.
    pub setup: Option<(String, u64)>,
    pub setup_checksum: Option<String>,
}

/// The JSON string that starts at `text` (after its opening quote), unescaped.
fn json_string(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => {}
                't' => out.push(' '),
                'u' => {
                    let code: String = chars.by_ref().take(4).collect();
                    out.push(u32::from_str_radix(&code, 16).ok().and_then(char::from_u32).unwrap_or(' '));
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// The string value of the first `"key": "..."` in `json`.
fn string_of(json: &str, key: &str) -> Option<String> {
    let rest = &json[json.find(&format!("\"{key}\""))? + key.len() + 2..];
    json_string(rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?)
}

/// The API's answer for a release, read without trusting it: a tag of plain
/// characters, and files only from this project's own release downloads.
pub fn parse_release(json: &str) -> Option<Release> {
    let tag = string_of(json, "tag_name")?;
    if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_alphanumeric() || ".-_+".contains(c)) {
        return None;
    }
    let notes = string_of(json, "body").unwrap_or_default();
    let (mut package, mut checksum, mut setup, mut setup_checksum) = (None, None, None, None);
    let mut chunks = json.split("\"browser_download_url\"");
    let mut before = chunks.next().unwrap_or_default();
    for chunk in chunks {
        let url = chunk.trim_start().strip_prefix(':').and_then(|r| json_string(r.trim_start().strip_prefix('"')?));
        if let Some(url) = url.filter(|u| u.starts_with(DOWNLOAD_PREFIX) && !u.contains(['?', '#', ' ', '"', '\''])) {
            // The file's size stands before its address, in the same object.
            let size = before.rfind("\"size\"").and_then(|at| {
                let digits: String = before[at + 6..].chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
                digits.parse::<u64>().ok()
            });
            if url.ends_with("-windows-x64.zip") {
                package = Some((url, size.unwrap_or(0)));
            } else if url.ends_with("-windows-x64.zip.sha256") {
                checksum = Some(url);
            } else if url.ends_with("-windows-x64-setup.exe") {
                setup = Some((url, size.unwrap_or(0)));
            } else if url.ends_with("-windows-x64-setup.exe.sha256") {
                setup_checksum = Some(url);
            }
        }
        before = chunk;
    }
    Some(Release { tag, notes, package, checksum, setup, setup_checksum })
}

/// The first lines of release notes, for the banner: plain text, short.
pub fn notes_preview(notes: &str, most: usize) -> String {
    let mut out = String::new();
    for line in notes.lines().map(|l| l.trim().trim_start_matches(['#', '*', '-', ' ']).trim()).filter(|l| !l.is_empty()) {
        if !out.is_empty() {
            out.push_str("  ·  ");
        }
        out.push_str(line);
        if out.chars().count() >= most {
            break;
        }
    }
    if out.chars().count() > most {
        out = out.chars().take(most.saturating_sub(1)).collect::<String>() + "\u{2026}";
    }
    out
}

/// SHA-256 of `data`, as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be,
        0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa,
        0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85,
        0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
        0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f,
        0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// The checksum a `.sha256` file states: its first word of 64 hex digits.
pub fn stated_checksum(text: &str) -> Option<String> {
    text.split_whitespace().find(|w| w.len() == 64 && w.chars().all(|c| c.is_ascii_hexdigit())).map(str::to_ascii_lowercase)
}

/// Whether the downloaded package is the one the release's checksum file
/// describes. `None`: the release published no checksum, nothing to compare.
pub fn verify(package: &Path, checksum_file: Option<&Path>) -> Result<Option<String>, String> {
    let Some(file) = checksum_file else { return Ok(None) };
    let text = std::fs::read_to_string(file).map_err(|e| format!("the checksum file could not be read: {e}"))?;
    let stated = stated_checksum(&text).ok_or("the checksum file holds no checksum")?;
    let data = std::fs::read(package).map_err(|e| format!("the download could not be read: {e}"))?;
    let actual = sha256_hex(&data);
    if actual == stated { Ok(Some(actual)) } else { Err("the download does not match the release's checksum".to_string()) }
}

/// Files of a package that are never put over yours.
pub fn protected(relative: &Path) -> bool {
    let first = relative.components().next().map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase());
    let name = relative.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase());
    first.as_deref() == Some("local") || name.as_deref() == Some("victory.toml")
}

fn files_under(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files_under(root, &path, out)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_path_buf());
        }
    }
    Ok(())
}

/// The folder of an unpacked package that holds its files: the package's
/// single top folder, or the staging folder itself. It must hold victory.exe.
pub fn package_root(unpacked: &Path) -> Option<PathBuf> {
    if unpacked.join("victory.exe").is_file() {
        return Some(unpacked.to_path_buf());
    }
    let mut dirs = std::fs::read_dir(unpacked).ok()?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir());
    let only = dirs.next()?;
    (dirs.next().is_none() && only.join("victory.exe").is_file()).then_some(only)
}

fn aside(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{ASIDE}", path.display()))
}

/// Puts the unpacked package's files into the Rox-box folder. Each file that
/// is there is renamed aside first; if any step fails, the new copies made
/// so far are removed and every renamed file is put back, so the folder is as
/// it was. Returns the files installed.
pub fn install(package: &Path, home: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    files_under(package, package, &mut files).map_err(|e| format!("the package could not be read: {e}"))?;
    files.retain(|f| !protected(f));
    files.sort();
    if files.is_empty() {
        return Err("the package is empty".to_string());
    }
    let mut renamed: Vec<PathBuf> = Vec::new();
    let mut copied: Vec<PathBuf> = Vec::new();
    let mut step = || -> std::io::Result<()> {
        for relative in &files {
            let target = home.join(relative);
            if let Some(dir) = target.parent() {
                std::fs::create_dir_all(dir)?;
            }
            if target.exists() {
                let old = aside(&target);
                let _ = std::fs::remove_file(&old);
                std::fs::rename(&target, &old)?;
                renamed.push(target.clone());
            }
            std::fs::copy(package.join(relative), &target)?;
            copied.push(target);
        }
        Ok(())
    };
    match step() {
        Ok(()) => Ok(files),
        Err(e) => {
            for target in &copied {
                let _ = std::fs::remove_file(target);
            }
            for target in renamed.iter().rev() {
                let _ = std::fs::rename(aside(target), target);
            }
            Err(format!("the files could not be replaced ({e}); everything was put back"))
        }
    }
}

/// Removes what an earlier update renamed aside (the program that was
/// running then has ended). Looks in the folder and one level below.
pub fn clean_up(home: &Path) -> usize {
    let mut removed = 0;
    let look = |dir: &Path, deeper: bool, removed: &mut usize| {
        let mut below = Vec::new();
        for path in std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok().map(|e| e.path())) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if path.is_file() && name.ends_with(ASIDE) {
                *removed += usize::from(std::fs::remove_file(&path).is_ok());
            } else if deeper && path.is_dir() && !["local", "target", ".git"].contains(&name.as_str()) {
                below.push(path);
            }
        }
        below
    };
    for dir in look(home, true, &mut removed) {
        look(&dir, false, &mut removed);
    }
    removed
}

/// Where an update is downloaded and unpacked: under `local\`, which no update touches.
pub fn staging(home: &Path, tag: &str) -> PathBuf {
    home.join("local").join("update").join(tag)
}

/// A program Windows ships, by its full path (so that no other of the same name is run).
pub fn system_tool(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from("C:\\Windows"), PathBuf::from);
    root.join("System32").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("roxbox-launcher-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(sha256_hex(b""), concat!("e3b0c442", "98fc1c14", "9afbf4c8", "996fb924", "27ae41e4", "649b934c", "a495991b", "7852b855"));
        assert_eq!(sha256_hex(b"abc"), concat!("ba7816bf", "8f01cfea", "414140de", "5dae2223", "b00361a3", "96177a9c", "b410ff61", "f20015ad"));
        let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(sha256_hex(long), concat!("248d6a61", "d20638b8", "e5c02693", "0c3e6039", "a33ce459", "64ff2167", "f6ecedd4", "19db06c1"));
        assert_eq!(sha256_hex(&[b'a'; 1000]).len(), 64);
    }

    #[test]
    fn a_release_is_read_and_only_its_own_files_are_taken() {
        let json = r###"{"url":"x","tag_name":"v0.3.0","name":"Rox-box v0.3.0","assets":[
          {"name":"Rox-box-0.3.0-windows-x64.zip","size":4515465,"browser_download_url":"https://github.com/RoxBuilds/Rox-box/releases/download/v0.3.0/Rox-box-0.3.0-windows-x64.zip"},
          {"name":"Rox-box-0.3.0-windows-x64.zip.sha256","size":98,"browser_download_url":"https://github.com/RoxBuilds/Rox-box/releases/download/v0.3.0/Rox-box-0.3.0-windows-x64.zip.sha256"},
          {"name":"Rox-box-0.3.0-windows-x64-setup.exe","size":5000000,"browser_download_url":"https://github.com/RoxBuilds/Rox-box/releases/download/v0.3.0/Rox-box-0.3.0-windows-x64-setup.exe"},
          {"name":"Rox-box-0.3.0-windows-x64-setup.exe.sha256","size":104,"browser_download_url":"https://github.com/RoxBuilds/Rox-box/releases/download/v0.3.0/Rox-box-0.3.0-windows-x64-setup.exe.sha256"},
          {"name":"evil.zip","size":1,"browser_download_url":"https://example.invalid/x-windows-x64.zip"}],
          "body":"## What's new\r\n- Faster \"frames\"\n- A launcher\n"}"###;
        let release = parse_release(json).unwrap();
        assert_eq!(release.tag, "v0.3.0");
        assert_eq!(release.package.as_ref().unwrap().1, 4_515_465);
        assert!(release.package.as_ref().unwrap().0.ends_with("/v0.3.0/Rox-box-0.3.0-windows-x64.zip"));
        assert!(release.checksum.as_ref().unwrap().ends_with(".zip.sha256"));
        assert_eq!(release.setup.as_ref().unwrap().1, 5_000_000);
        assert!(release.setup.as_ref().unwrap().0.ends_with("-setup.exe"));
        assert!(release.setup_checksum.as_ref().unwrap().ends_with("-setup.exe.sha256"));
        assert_eq!(release.notes, "## What's new\n- Faster \"frames\"\n- A launcher\n");
        assert_eq!(notes_preview(&release.notes, 200), "What's new  ·  Faster \"frames\"  ·  A launcher");
        assert_eq!(notes_preview(&release.notes, 12).chars().count(), 12);

        let foreign = r#"{"tag_name":"v9.9.9","assets":[{"size":1,"browser_download_url":"https://example.invalid/Rox-box-9-windows-x64.zip"}]}"#;
        assert_eq!(parse_release(foreign).unwrap().package, None, "a file from elsewhere is not taken");
        assert_eq!(parse_release(r#"{"message":"Not Found"}"#), None);
        assert_eq!(parse_release(r#"{"tag_name":"<b>"}"#), None);
    }

    #[test]
    fn a_download_is_checked_against_the_stated_checksum() {
        let dir = scratch("verify");
        let package = dir.join("p.zip");
        std::fs::write(&package, b"abc").unwrap();
        let good = dir.join("good.sha256");
        std::fs::write(&good, format!("{}  p.zip\n", sha256_hex(b"abc").to_uppercase())).unwrap();
        let bad = dir.join("bad.sha256");
        std::fs::write(&bad, format!("{}  p.zip\n", sha256_hex(b"abd"))).unwrap();
        assert_eq!(verify(&package, Some(&good)).unwrap(), Some(sha256_hex(b"abc")));
        assert!(verify(&package, Some(&bad)).unwrap_err().contains("does not match"));
        assert_eq!(verify(&package, None).unwrap(), None);
        std::fs::write(&bad, "no checksum here").unwrap();
        assert!(verify(&package, Some(&bad)).is_err());
    }

    #[test]
    fn an_install_replaces_the_package_files_and_leaves_yours() {
        let home = scratch("home");
        let staged = scratch("staged");
        let package = staged.join("Rox-box-9-windows-x64");
        std::fs::create_dir_all(package.join("local")).unwrap();
        std::fs::create_dir_all(home.join("local")).unwrap();
        for (name, text) in [("victory.exe", "new engine"), ("play.ps1", "new script"), ("victory.toml", "theirs"), ("local\\launcher.toml", "theirs"), ("NEW.txt", "new file")] {
            std::fs::write(package.join(name), text).unwrap();
        }
        for (name, text) in [("victory.exe", "old engine"), ("play.ps1", "old script"), ("victory.toml", "mine"), ("local\\launcher.toml", "mine"), ("notes.txt", "mine")] {
            std::fs::write(home.join(name), text).unwrap();
        }
        assert_eq!(package_root(&staged).unwrap(), package);
        let installed = install(&package, &home).unwrap();
        assert_eq!(installed.len(), 3);
        let read = |name: &str| std::fs::read_to_string(home.join(name)).unwrap();
        assert_eq!(read("victory.exe"), "new engine");
        assert_eq!(read("play.ps1"), "new script");
        assert_eq!(read("NEW.txt"), "new file");
        assert_eq!(read("victory.toml"), "mine");
        assert_eq!(read("local\\launcher.toml"), "mine");
        assert_eq!(read("notes.txt"), "mine");
        assert_eq!(read("victory.exe.old-update"), "old engine");
        assert_eq!(clean_up(&home), 2);
        assert!(!home.join("victory.exe.old-update").exists());
        assert!(protected(Path::new("LOCAL\\x")) && protected(Path::new("sub\\Victory.toml")) && !protected(Path::new("victory.exe")));
    }

    #[test]
    fn a_failed_install_puts_everything_back() {
        let home = scratch("home-fail");
        let package = scratch("package-fail");
        std::fs::write(package.join("a.txt"), "new a").unwrap();
        std::fs::write(package.join("victory.exe"), "new engine").unwrap();
        // Something in the way that cannot be replaced: a folder where a file must go.
        std::fs::write(package.join("z-blocked"), "new").unwrap();
        std::fs::write(home.join("a.txt"), "old a").unwrap();
        std::fs::write(home.join("victory.exe"), "old engine").unwrap();
        std::fs::create_dir_all(home.join("z-blocked.old-update").join("held")).unwrap();
        std::fs::create_dir_all(home.join("z-blocked")).unwrap();
        std::fs::write(home.join("z-blocked").join("held"), "x").unwrap();
        let err = install(&package, &home).unwrap_err();
        assert!(err.contains("put back"), "{err}");
        assert_eq!(std::fs::read_to_string(home.join("a.txt")).unwrap(), "old a");
        assert_eq!(std::fs::read_to_string(home.join("victory.exe")).unwrap(), "old engine");
        assert!(!home.join("a.txt.old-update").exists());
        assert!(package_root(&scratch("empty")).is_none());
        assert!(staging(&home, "v1").ends_with("local\\update\\v1"));
        assert!(system_tool("tar.exe").ends_with("System32\\tar.exe"));
    }
}
