//! Rule #1, statically: the repository only ever contains small text files
//! that we wrote. See docs/asset-policy.md.
//!
//! This is an allowlist, not a blocklist: a file passes only if its name is
//! on the list, it is not too big, it is text, and it does not smuggle data
//! inside text (long hex/base64 runs). Two hygiene checks ride along: no
//! file may name a user-profile directory of the machine it was written on
//! ([`profile_paths`]), and no file may hold a literal listed in the optional,
//! git-ignored `local/policy-denylist.txt` ([`Denylist`]).

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

const MAX_FILE_SIZE: usize = 640 * 1024;
/// Rust source may be larger: the shader translator's lowering is one file of
/// well over 600 KiB, and it is code we wrote, not data. Everything else (and
/// so every place data could hide as "text") keeps [`MAX_FILE_SIZE`].
const MAX_SOURCE_SIZE: usize = 768 * 1024;
const MAX_ENCODED_RUN: usize = 512;

/// The optional local denylist, relative to the repository root. `local/` is
/// git-ignored, so the list itself is never published.
const DENYLIST_FILE: &str = "local/policy-denylist.txt";

/// Directory names that may follow a user-profile root without naming anyone:
/// placeholders and the system's own shared profiles.
const GENERIC_PROFILES: &[&str] = &[
    "user", "username", "name", "you", "me", "example", "public", "default", "shared",
];

const ALLOWED_EXTENSIONS: &[&str] = &[
    "rs", "toml", "md", "txt", "lock", "yml", "yaml", "json", "hlsl", "hlsli", "ps1", "cmd", "sh",
];
const ALLOWED_NAMES: &[&str] = &[
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".victory-root",
    "pre-commit",
    "LICENSE",
    "LICENSE-MIT",
    "LICENSE-APACHE",
];

/// Paths explicitly exempted, with a reason. Keep this empty if at all possible.
const EXCEPTIONS: &[(&str, &str)] = &[];

/// Signatures of formats that must never be committed.
const FORBIDDEN_MAGIC: &[(&[u8], &str)] = &[
    (b"MZ", "PE executable"),
    (b"DXBC", "shader container"),
    (b"DDS ", "DDS texture"),
    (b"RIFF", "RIFF (audio/video)"),
    (b"BC\xC0\xDE", "LLVM bitcode"),
    (b"Microsoft C/C++ MSF", "PDB"),
    (b"!<arch>", "library archive"),
    (b"PK\x03\x04", "zip"),
    (b"7z\xBC\xAF", "7-zip"),
    (b"MSCF", "cabinet"),
    (b"\xD0\xCF\x11\xE0", "MSI/OLE"),
];

pub fn run(root: &Path, staged: bool) -> Result<()> {
    let files = list_files(root, staged)?;
    let denylist = Denylist::load(root)?;
    let mut violations = Vec::new();
    for path in &files {
        if EXCEPTIONS.iter().any(|(p, _)| p == path) {
            continue;
        }
        let content = if staged {
            let out = Command::new("git")
                .args(["show", &format!(":{path}")])
                .current_dir(root)
                .output()?;
            if !out.status.success() {
                continue; // deleted in the index
            }
            out.stdout
        } else {
            match std::fs::read(root.join(path)) {
                Ok(c) => c,
                Err(_) => continue,
            }
        };
        for problem in check_with(path, &content, &denylist) {
            violations.push(format!("{path}: {problem}"));
        }
    }
    if violations.is_empty() {
        if denylist.is_empty() {
            println!("asset policy: {} files ok", files.len());
        } else {
            println!(
                "asset policy: {} files ok ({} local denylist entries checked)",
                files.len(),
                denylist.len()
            );
        }
        return Ok(());
    }
    for v in &violations {
        eprintln!("POLICY: {v}");
    }
    bail!(
        "{} asset-policy violation(s). Nothing from the game may be committed; \
         see docs/asset-policy.md",
        violations.len()
    )
}

fn list_files(root: &Path, staged: bool) -> Result<Vec<String>> {
    let git = |args: &[&str]| -> Result<Option<Vec<String>>> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .context("running git")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(
            out.stdout
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).replace('\\', "/"))
                .collect(),
        ))
    };
    if staged {
        return git(&[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACMR",
            "-z",
        ])?
        .context("--staged needs a git repository");
    }
    if let Some(mut tracked) = git(&[
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ])? {
        tracked.sort();
        tracked.dedup();
        return Ok(tracked);
    }
    // Not a git repository: walk everything except build output.
    Ok(walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("target" | ".git")))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .collect())
}

/// Literals that must not appear in any publishable file, from
/// [`DENYLIST_FILE`]: one per line, blank lines and lines starting with `#`
/// skipped. A line of eight or more hex digits is matched anywhere, in either
/// case (a hash stays a hash inside `0x...` or a longer string); any other
/// line is matched as a whole word, ignoring ASCII case (so a short name does
/// not match inside a longer word). Violations name the list's line, never the
/// literal, so the log of a failed check does not repeat what the list hides.
#[derive(Default)]
pub struct Denylist {
    /// Lower-case hex literals by length: (length, literal -> list line).
    hex: Vec<(usize, HashMap<Vec<u8>, usize>)>,
    /// Lower-case words with their list line.
    words: Vec<(Vec<u8>, usize)>,
}

impl Denylist {
    /// The list under `root`, empty when the file is not there.
    fn load(root: &Path) -> Result<Self> {
        let path = root.join(DENYLIST_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Self::parse(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Pure, for testing.
    pub fn parse(text: &[u8]) -> Self {
        let mut list = Self::default();
        for (i, line) in text.split(|&b| b == b'\n').enumerate() {
            let line = line.trim_ascii();
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if lower.len() >= 8 && lower.iter().all(u8::is_ascii_hexdigit) {
                let at = match list.hex.iter().position(|(len, _)| *len == lower.len()) {
                    Some(at) => at,
                    None => {
                        list.hex.push((lower.len(), HashMap::new()));
                        list.hex.len() - 1
                    }
                };
                list.hex[at].1.entry(lower).or_insert(i + 1);
            } else {
                list.words.push((lower, i + 1));
            }
        }
        list
    }

    pub fn len(&self) -> usize {
        self.hex.iter().map(|(_, set)| set.len()).sum::<usize>() + self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// (line of `content`, line of the list) for every hit, in file order.
    fn hits(&self, content: &[u8]) -> Vec<(usize, usize)> {
        if self.is_empty() {
            return Vec::new();
        }
        let lower = content.to_ascii_lowercase();
        let mut hits = Vec::new();
        // Hex: every window of a listed length inside each run of hex digits.
        let mut i = 0;
        while i < lower.len() {
            if !lower[i].is_ascii_hexdigit() {
                i += 1;
                continue;
            }
            let end = i + lower[i..].iter().take_while(|b| b.is_ascii_hexdigit()).count();
            for (len, set) in &self.hex {
                for (k, window) in lower[i..end].windows(*len).enumerate() {
                    if let Some(&entry) = set.get(window) {
                        hits.push((i + k, entry));
                    }
                }
            }
            i = end;
        }
        // Words: whole words only.
        let word_byte = |b: Option<&u8>| b.is_some_and(u8::is_ascii_alphanumeric);
        for (word, entry) in &self.words {
            let mut from = 0;
            while let Some(at) = find(&lower[from..], word).map(|at| at + from) {
                let before = at.checked_sub(1).and_then(|b| lower.get(b));
                if !word_byte(before) && !word_byte(lower.get(at + word.len())) {
                    hits.push((at, *entry));
                }
                from = at + 1;
            }
        }
        hits.sort_unstable();
        hits.into_iter().map(|(at, entry)| (line_of(content, at), entry)).collect()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let first = needle[0];
    let last = haystack.len() - needle.len();
    let mut at = 0;
    while at <= last {
        at += haystack[at..=last].iter().position(|&b| b == first)?;
        if haystack[at..].starts_with(needle) {
            return Some(at);
        }
        at += 1;
    }
    None
}

/// The 1-based line of byte `at`.
fn line_of(content: &[u8], at: usize) -> usize {
    content[..at].iter().filter(|&&b| b == b'\n').count() + 1
}

/// Lines that name somebody's profile directory: a drive's `Users` folder
/// (a drive letter and colon, then `Users`, with backslashes or slashes
/// between) or a Unix home (`home` or `Users` under the root, at the start of
/// a path), followed by a directory name that is not a placeholder
/// ([`GENERIC_PROFILES`]; `<name>`, `%USERNAME%` and `$HOME` are no names at
/// all). Such a path is the fingerprint of the machine a log or a note was
/// pasted from.
fn profile_paths(content: &[u8]) -> Vec<usize> {
    let lower = content.to_ascii_lowercase();
    let seps = |s: &[u8]| s.iter().take_while(|b| matches!(b, b'\\' | b'/')).count();
    let named = |rest: &[u8]| {
        let len = rest
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            .count();
        let name = &rest[..len];
        let generic = GENERIC_PROFILES.iter().any(|g| g.as_bytes() == name);
        !generic && name.iter().any(u8::is_ascii_alphabetic)
    };
    let part_of_more = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'\\');
    let mut lines = Vec::new();
    for at in 0..lower.len() {
        let rest = &lower[at..];
        let after = if rest[0].is_ascii_alphabetic() && rest.get(1) == Some(&b':') {
            // A drive letter that is a word of its own, then `Users`.
            let alone = at == 0 || !lower[at - 1].is_ascii_alphanumeric();
            let root = &rest[2 + seps(&rest[2..])..];
            (alone && seps(&rest[2..]) > 0 && root.starts_with(b"users")).then(|| &root[5..])
        } else if rest[0] == b'/' && (at == 0 || !part_of_more(lower[at - 1])) {
            [&b"/home"[..], b"/users"].iter().find(|r| rest.starts_with(r)).map(|r| &rest[r.len()..])
        } else {
            None
        };
        if let Some(after) = after
            && seps(after) > 0
            && named(&after[seps(after)..])
        {
            let line = line_of(content, at);
            if lines.last() != Some(&line) {
                lines.push(line);
            }
        }
    }
    lines
}

/// All problems with one file, without a denylist. Pure, for testing.
#[cfg(test)]
pub fn check(path: &str, content: &[u8]) -> Vec<String> {
    check_with(path, content, &Denylist::default())
}

/// All problems with one file. Pure, for testing.
pub fn check_with(path: &str, content: &[u8], denylist: &Denylist) -> Vec<String> {
    let mut problems = Vec::new();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    let allowed = ALLOWED_NAMES.contains(&name)
        || ext
            .as_deref()
            .is_some_and(|e| ALLOWED_EXTENSIONS.contains(&e));
    if !allowed {
        problems.push("file type not on the allowlist".to_owned());
    }
    let limit = if ext.as_deref() == Some("rs") { MAX_SOURCE_SIZE } else { MAX_FILE_SIZE };
    if content.len() > limit {
        problems.push(format!("{} bytes exceeds the {limit}-byte limit", content.len()));
    }
    for (magic, what) in FORBIDDEN_MAGIC {
        if content.starts_with(magic) {
            problems.push(format!("looks like a {what}"));
        }
    }
    if content.contains(&0) {
        problems.push("binary content (NUL bytes)".to_owned());
    } else if let Some(run) = longest_encoded_run(content)
        && run > MAX_ENCODED_RUN
    {
        problems.push(format!("{run}-character hex/base64 run (embedded data?)"));
    } else {
        let digits = longest_hex_literal_run(content);
        if digits > MAX_ENCODED_RUN {
            problems.push(format!(
                "{digits} hex digits in one run of \\x/0x literals (embedded data?)"
            ));
        }
    }
    for line in profile_paths(content) {
        problems.push(format!(
            "line {line}: a user-profile path (a machine fingerprint; write a placeholder such as <name>)"
        ));
    }
    for (line, entry) in denylist.hits(content) {
        problems.push(format!("line {line}: holds the literal of {DENYLIST_FILE} line {entry}"));
    }
    problems
}

/// Longest run of characters that could be hex or base64 payload.
fn longest_encoded_run(content: &[u8]) -> Option<usize> {
    let is_payload = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
    content.split(|b| !is_payload(b)).map(<[u8]>::len).max()
}

/// Most hex digits in `\xNN` escapes or `0x` literals with only commas,
/// whitespace or backslashes between them: a byte string or `xxd -i` array,
/// which `longest_encoded_run` sees as many 3- or 4-character runs.
/// shortcut: decimal arrays (`[77, 90, ...]`) still pass, add a literal count if one shows up.
fn longest_hex_literal_run(content: &[u8]) -> usize {
    let (mut i, mut run, mut longest) = (0, 0, 0);
    while i < content.len() {
        match &content[i..] {
            [b'\\' | b'0', b'x' | b'X', rest @ ..] => {
                let len = rest
                    .iter()
                    .take_while(|b| b.is_ascii_hexdigit() || **b == b'_')
                    .count();
                run += rest[..len].iter().filter(|b| b.is_ascii_hexdigit()).count();
                longest = longest.max(run);
                // Skip a type suffix such as `u8` or `u32`.
                let suffix = rest[len..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric())
                    .count();
                i += 2 + len + suffix;
            }
            [b, ..] if matches!(b, b',' | b'\\') || b.is_ascii_whitespace() => i += 1,
            _ => {
                run = 0;
                i += 1;
            }
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::{Denylist, check, check_with};

    /// Enforces the policy wherever tests run, even in clones without the hook.
    #[test]
    fn repository_passes_policy() {
        super::run(&crate::workspace_root(), false).unwrap();
    }

    #[test]
    fn accepts_source() {
        assert!(check("crates/x/src/lib.rs", b"fn main() {}\n").is_empty());
        assert!(check(".gitignore", b"/target/\n").is_empty());
        assert!(check(".githooks/pre-commit", b"#!/bin/sh\n").is_empty());
    }

    #[test]
    fn rejects_game_like_files() {
        assert!(!check("assets/tex.dds", b"DDS \x7c\0\0\0").is_empty());
        assert!(
            !check("notes.txt", b"MZ\x90\0").is_empty(),
            "magic wins over extension"
        );
        assert!(
            !check("data.bin", b"hello").is_empty(),
            "extension not allowed"
        );
        assert!(
            !check("a.rs", &vec![b'a'; 600 * 1024]).is_empty(),
            "too big"
        );
    }

    /// Lines of ordinary text (no long payload run), at least `bytes` in all.
    fn prose(bytes: usize) -> Vec<u8> {
        let line = b"// a line of text\n";
        line.repeat(bytes.div_ceil(line.len()))
    }

    #[test]
    fn source_may_be_larger_than_other_text() {
        assert!(check("src/big.rs", &prose(700 * 1024)).is_empty());
        assert!(!check("src/huge.rs", &prose(800 * 1024)).is_empty());
        assert!(!check("notes/big.md", &prose(700 * 1024)).is_empty());
        assert!(check("notes/small.md", &prose(600 * 1024)).is_empty());
    }

    /// A line with a path under `root`, put together here so that this file
    /// itself names nobody's profile.
    fn profile(root: &str, sep: &str, name: &str) -> String {
        format!("see {root}{sep}{name}{sep}work{sep}notes.txt for the rest\n")
    }

    #[test]
    fn rejects_user_profile_paths() {
        let users = |drive: &str, sep: &str| format!("{drive}:{sep}Users");
        for text in [
            profile(&users("C", "\\"), "\\", "alice"),
            profile(&users("d", "\\\\"), "\\\\", "Bob.Smith"),
            profile(&users("C", "/"), "/", "carol_1"),
            profile("/home", "/", "dave"),
            profile("/Users", "/", "erin"),
            format!("error: could not open \"{}\"\n", profile("/home", "/", "frank").trim()),
        ] {
            let problems = check("docs/notes.md", text.as_bytes());
            assert_eq!(problems.len(), 1, "{text:?}: {problems:?}");
            assert!(problems[0].starts_with("line 1: a user-profile path"), "{problems:?}");
        }
        // The line is the path's.
        let text = format!("one\ntwo\n{}", profile("/home", "/", "grace"));
        assert!(check("a.md", text.as_bytes())[0].starts_with("line 3:"));
    }

    #[test]
    fn accepts_placeholder_and_system_paths() {
        let users = format!("C:{}Users", "\\");
        for text in [
            profile(&users, "\\", "<name>"),
            profile(&users, "\\", "%USERNAME%"),
            profile(&users, "\\", "Public"),
            profile(&users, "\\", "user"),
            profile("/home", "/", "$USER"),
            profile("/home", "/", "user"),
            // Not a profile root: part of a longer path or word.
            profile("https://example.org/home", "/", "page"),
            profile("src/users", "/", "table"),
            profile("C:\\Program Files", "\\", "tool"),
            "the users/groups table; a home/away switch; HKLM:\\SOFTWARE\\Users\n".to_owned(),
        ] {
            assert!(check("docs/notes.md", text.as_bytes()).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn the_local_denylist_is_matched_by_word_and_by_hex() {
        // Synthetic entries only: a made-up name, a phrase and hex ids.
        let list = Denylist::parse(
            b"# a comment\n\nzorblat\nMade Up Studio\r\n00aa11bb22cc33dd\nfeedc0de\nzz9\n",
        );
        assert_eq!(list.len(), 5);
        let hit = |text: &str| check_with("src/x.rs", text.as_bytes(), &list);
        // Words: whole words, in any case; not inside a longer word.
        assert_eq!(hit("the Zorblat build\n").len(), 1);
        assert_eq!(
            hit("a\npath\\zorblat\\file\n"),
            ["line 2: holds the literal of local/policy-denylist.txt line 3"]
        );
        assert_eq!(hit("by made up studio.\n").len(), 1);
        assert!(hit("zorblatter and prezorblat; made up studios\n").is_empty());
        assert_eq!(hit("zorblat_dir = 1\n").len(), 1, "an underscore ends a word");
        assert!(hit("zz90 azz9\n").is_empty());
        // Hex: anywhere, in either case, also inside a longer run.
        assert_eq!(hit("hash 00AA11BB22CC33DD\n").len(), 1);
        assert_eq!(hit("0x00aa11bb22cc33dd_u64\n").len(), 1);
        assert_eq!(hit("ff00aa11bb22cc33ddff\n").len(), 1);
        assert_eq!(hit("xfeedc0dex 00aa11bb22cc33dd\n").len(), 2);
        assert!(hit("00aa11bb22cc33de feedc0d\n").is_empty());
        // No list, no hits; the plain check never sees one.
        assert!(check("src/x.rs", b"the zorblat build 00aa11bb22cc33dd\n").is_empty());
        assert!(Denylist::parse(b"\n# nothing\n").is_empty());
    }

    #[test]
    fn rejects_data_smuggled_into_text() {
        let blob = format!("const DATA: &str = \"{}\";", "A1".repeat(400));
        assert!(!check("src/blob.rs", blob.as_bytes()).is_empty());
    }

    /// Synthetic bytes, never game data.
    fn synthetic(n: u32) -> impl Iterator<Item = u32> {
        (0..n).map(|i| (i * 37 + 11) % 256)
    }

    #[test]
    fn rejects_data_smuggled_as_an_escaped_byte_string() {
        // 4096 bytes as one unbroken `\xNN` hex run, the shape a byte-string
        // literal of copied data takes.
        let esc: String = synthetic(4096).map(|b| format!("\\x{b:02x}")).collect();
        let src = format!("pub const DATA: &[u8] = b\"{esc}\";\n");
        assert!(
            !check("src/data.rs", src.as_bytes()).is_empty(),
            "{} bytes of \\x-escaped hex passed",
            src.len()
        );
    }

    #[test]
    fn rejects_data_smuggled_as_a_hex_byte_array() {
        // `xxd -i` style: 4096 bytes as hex literals.
        let lits: Vec<String> = synthetic(4096).map(|b| format!("0x{b:02x}")).collect();
        let src = format!("pub const DATA: [u8; 4096] = [{}];\n", lits.join(", "));
        assert!(
            !check("src/data.rs", src.as_bytes()).is_empty(),
            "{} bytes of hex array passed",
            src.len()
        );
        // A short constant table, like the 16-entry ones in the tree, stays legal.
        let table: Vec<String> = synthetic(16).map(|w| format!("0x{w:08x}_u32")).collect();
        let src = format!(
            "const T: [u32; 16] = [\n    {},\n];\n",
            table.join(",\n    ")
        );
        assert!(check("src/table.rs", src.as_bytes()).is_empty());
    }
}
