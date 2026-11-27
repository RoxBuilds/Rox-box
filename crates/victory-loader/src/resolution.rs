//! Optional, user-supplied byte-checked binary patches applied right after
//! the main executable image is mapped and before title code executes.
//!
//! By default (when `VICTORY_PATCHES_FILE` and `[paths].patches_file` in
//! `victory.toml` are unset), this module is a complete no-op and writes nothing
//! into the guest image. When a user supplies an external TOML patch manifest,
//! every patch verifies `expected` bytes at `rva` (relative to the mapped
//! image base) before modifying memory, and entries sharing a `group` are
//! applied atomically (all verified first, nothing written if any check fails, and what
//! was written is put back if a later write of the group fails). Groups are applied in
//! the order of the file; an entry that overlaps another is left out with its group.

use std::ffi::OsString;
use std::path::Path;

use serde::Deserialize;

const PATCHES_ENV: &str = "VICTORY_PATCHES_FILE";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchManifest {
    #[serde(default)]
    patch: Vec<PatchSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchSpec {
    name: String,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    when_env: Option<String>,
    #[serde(default)]
    when_value: Option<String>,
    rva: String,
    expected: String,
    replace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedPatch {
    name: String,
    group: Option<String>,
    rva: usize,
    expected: Vec<u8>,
    replace: Vec<u8>,
}

/// A manifest read and checked: the entries whose condition holds, in file order.
#[derive(Debug, Default, PartialEq, Eq)]
struct Parsed {
    active: Vec<ParsedPatch>,
    /// Entries whose `when_env` condition does not hold.
    inactive: usize,
    /// Variables some entry's `when_env` names that are set to a value no entry matched.
    unmatched: Vec<String>,
}

/// What became of a manifest's entries.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Summary {
    /// Entries whose condition holds, and those whose condition does not.
    active: usize,
    inactive: usize,
    /// Patches written.
    applied: usize,
    /// Groups written whole, and groups left out whole (a single entry is a group of one).
    groups_applied: usize,
    groups_skipped: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum ApplyOutcome {
    NoFile,
    Applied { count: usize },
}

fn parse_rva(raw: &str) -> Result<usize, String> {
    let s = raw.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        let clean: String = hex.chars().filter(|&c| c != '_').collect();
        usize::from_str_radix(&clean, 16).map_err(|e| format!("invalid hex RVA '{raw}': {e}"))
    } else {
        let clean: String = s.chars().filter(|&c| c != '_').collect();
        clean
            .parse::<usize>()
            .map_err(|e| format!("invalid decimal RVA '{raw}': {e}"))
    }
}

fn parse_hex_bytes(raw: &str) -> Result<Vec<u8>, String> {
    let hex: String = raw
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '_')
        .collect();
    if hex.is_empty() {
        return Err("empty hex byte string".into());
    }
    if !hex.len().is_multiple_of(2) {
        return Err(format!("odd number of hex nibbles in '{raw}'"));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let s = std::str::from_utf8(pair).map_err(|_| "invalid UTF-8 in hex string")?;
        let byte = u8::from_str_radix(s, 16).map_err(|e| format!("bad hex byte '{s}': {e}"))?;
        out.push(byte);
    }
    Ok(out)
}

fn env_matches(
    when_env: Option<&str>,
    when_value: Option<&str>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> bool {
    let Some(key) = when_env.filter(|k| !k.is_empty()) else {
        return true;
    };
    let Some(val) = env(key).filter(|v| !v.is_empty()) else {
        return false;
    };
    let text = val.to_string_lossy();
    match when_value {
        Some(want) => text == want,
        None => text != "0" && !text.eq_ignore_ascii_case("false"),
    }
}

fn parse_manifest(toml_text: &str, env: &dyn Fn(&str) -> Option<OsString>) -> Result<Parsed, String> {
    let manifest: PatchManifest =
        toml::from_str(toml_text).map_err(|e| format!("parsing patches TOML: {e}"))?;
    let mut out = Parsed::default();
    // Every variable a condition names, in file order, and whether an entry matched it.
    let mut named: Vec<(String, bool)> = Vec::new();
    // Every entry is checked, also one whose condition does not hold: a mistake in a
    // manifest must not wait for the run that sets the variable.
    for spec in manifest.patch {
        let when_env = spec.when_env.as_deref().filter(|k| !k.is_empty());
        if spec.when_value.is_some() && when_env.is_none() {
            return Err(format!("patch '{}': when_value without when_env", spec.name));
        }
        let rva = parse_rva(&spec.rva).map_err(|e| format!("patch '{}': {e}", spec.name))?;
        let expected =
            parse_hex_bytes(&spec.expected).map_err(|e| format!("patch '{}': expected: {e}", spec.name))?;
        let replace =
            parse_hex_bytes(&spec.replace).map_err(|e| format!("patch '{}': replace: {e}", spec.name))?;
        if expected.len() != replace.len() {
            return Err(format!(
                "patch '{}': expected {} bytes but replace has {} bytes",
                spec.name,
                expected.len(),
                replace.len()
            ));
        }
        let matches = env_matches(when_env, spec.when_value.as_deref(), env);
        if let Some(key) = when_env {
            match named.iter_mut().find(|(k, _)| k == key) {
                Some(entry) => entry.1 |= matches,
                None => named.push((key.to_owned(), matches)),
            }
        }
        if !matches {
            out.inactive += 1;
            continue;
        }
        out.active.push(ParsedPatch {
            name: spec.name,
            group: spec.group,
            rva,
            expected,
            replace,
        });
    }
    for (key, matched) in named {
        // (a variable set to an off value asks for nothing)
        let asks = env(&key).is_some_and(|v| {
            let text = v.to_string_lossy();
            !text.is_empty() && text != "0" && !text.eq_ignore_ascii_case("false")
        });
        if asks && !matched {
            out.unmatched.push(key);
        }
    }
    Ok(out)
}

/// A group's name (the entry's own for one without a group) and its entries.
type Group<'a> = (&'a str, Vec<&'a ParsedPatch>);

/// The active patches by group: groups in the order of their first entry in the file,
/// entries in file order.
fn grouped(patches: &[ParsedPatch]) -> Vec<Group<'_>> {
    let mut groups: Vec<(Option<&str>, Group<'_>)> = Vec::new();
    for p in patches {
        let key = p.group.as_deref();
        match groups.iter_mut().find(|g| key.is_some() && g.0 == key) {
            Some(group) => group.1.1.push(p),
            None => groups.push((key, (key.unwrap_or(&p.name), vec![p]))),
        }
    }
    groups.into_iter().map(|(_, group)| group).collect()
}

/// The groups of `patches` that can be written whole, in file order: every entry within
/// the image, its `expected` bytes there, and no byte of it in another entry of the group
/// or of a group taken before it. A group that fails any of these is left out whole.
fn verify_and_collect<'a>(base: *const u8, size: usize, patches: &'a [ParsedPatch]) -> Vec<Group<'a>> {
    let mut ready: Vec<Group<'a>> = Vec::new();
    // The byte ranges of the groups taken so far, and whose they are.
    let mut taken: Vec<(usize, usize, &str)> = Vec::new();
    for (group_name, entries) in grouped(patches) {
        let mut ranges: Vec<(usize, usize, &str)> = Vec::with_capacity(entries.len());
        let mut all_ok = true;
        for p in &entries {
            let Some(end) = p.rva.checked_add(p.expected.len()) else {
                tracing::warn!(patch = %p.name, rva = format_args!("{:#x}", p.rva), "patch RVA overflow");
                all_ok = false;
                break;
            };
            if end > size {
                tracing::warn!(patch = %p.name, rva = format_args!("{:#x}", p.rva), size, "patch past image end");
                all_ok = false;
                break;
            }
            // SAFETY: caller guarantees `base..base + size` is a readable mapped image.
            let actual = unsafe { std::slice::from_raw_parts(base.add(p.rva), p.expected.len()) };
            if actual != p.expected.as_slice() {
                tracing::warn!(
                    patch = %p.name,
                    rva = format_args!("{:#x}", p.rva),
                    "external patch bytes did not match this image; skipping"
                );
                all_ok = false;
                break;
            }
            if let Some(other) = taken.iter().chain(&ranges).find(|r| p.rva < r.1 && r.0 < end) {
                tracing::warn!(
                    patch = %p.name,
                    rva = format_args!("{:#x}", p.rva),
                    other = other.2,
                    "external patch overlaps another; skipping"
                );
                all_ok = false;
                break;
            }
            ranges.push((p.rva, end, &p.name));
        }
        if all_ok {
            taken.extend(ranges);
            ready.push((group_name, entries));
        } else if entries.len() > 1 {
            tracing::warn!(group = group_name, entries = entries.len(), "external patch group skipped whole");
        }
    }
    ready
}

/// Writes `replace` over `expected` at `rva`; false, and nothing written, when the bytes there
/// are not `expected` or cannot be written.
#[cfg(windows)]
unsafe fn write_checked(base: usize, size: usize, rva: usize, expected: &[u8], replace: &[u8]) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    if rva.saturating_add(replace.len()) > size {
        return false;
    }
    let at = (base + rva) as *mut u8;
    // SAFETY: `at..at+len` lies within the mapped image.
    let current = unsafe { std::slice::from_raw_parts(at, expected.len()) };
    if current != expected {
        return false;
    }
    let mut old = 0u32;
    // SAFETY: changes protection of mapped image bytes only long enough to copy `replace`.
    unsafe {
        if VirtualProtect(at.cast(), replace.len(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return false;
        }
        std::ptr::copy_nonoverlapping(replace.as_ptr(), at, replace.len());
        let mut _unused = 0u32;
        VirtualProtect(at.cast(), replace.len(), old, &mut _unused);
        FlushInstructionCache(GetCurrentProcess(), at.cast(), replace.len());
    }
    true
}

/// As the Windows one, without the page protection.
#[cfg(not(windows))]
unsafe fn write_checked(base: usize, size: usize, rva: usize, expected: &[u8], replace: &[u8]) -> bool {
    if rva.saturating_add(replace.len()) > size {
        return false;
    }
    let at = (base + rva) as *mut u8;
    // SAFETY: unit tests pass a valid writable buffer of `size` bytes.
    unsafe {
        let current = std::slice::from_raw_parts(at, expected.len());
        if current != expected {
            return false;
        }
        std::ptr::copy_nonoverlapping(replace.as_ptr(), at, replace.len());
    }
    true
}

/// A write of [`write_checked`]'s kind at (`rva`, `expected`, `replace`): what [`apply_groups`]
/// writes through, so that a test can make one fail.
type Write<'a> = &'a mut dyn FnMut(usize, &[u8], &[u8]) -> bool;

/// Writes each group whole or not at all, in order: when a write fails part-way, the bytes
/// the group's earlier entries replaced are put back. The patches written and the groups
/// left out.
fn apply_groups(groups: &[Group<'_>], write: Write<'_>) -> (usize, usize) {
    let (mut applied, mut skipped) = (0usize, 0usize);
    for (group_name, group) in groups {
        let failed = group.iter().position(|p| !write(p.rva, &p.expected, &p.replace));
        let Some(failed) = failed else {
            for p in group {
                tracing::info!(
                    patch = %p.name,
                    rva = format_args!("{:#x}", p.rva),
                    bytes = p.replace.len(),
                    "applied external binary patch"
                );
            }
            applied += group.len();
            continue;
        };
        skipped += 1;
        let mut restored = 0usize;
        for p in group[..failed].iter().rev() {
            if write(p.rva, &p.replace, &p.expected) {
                restored += 1;
            } else {
                tracing::error!(
                    patch = %p.name,
                    rva = format_args!("{:#x}", p.rva),
                    "external patch could not be taken back; the image keeps its bytes"
                );
            }
        }
        tracing::warn!(
            group = *group_name,
            patch = %group[failed].name,
            rva = format_args!("{:#x}", group[failed].rva),
            restored,
            "external patch could not be written; its group is left out"
        );
    }
    (applied, skipped)
}

/// Applies all active patches parsed from `toml_text` into `[base .. base + size)`,
/// writing through `write`.
///
/// # Safety
/// `base..base + size` must point to a mapped image (or writable test buffer) owned by
/// this process with no guest threads concurrently executing it.
unsafe fn apply_from_toml_with(
    base: usize,
    size: usize,
    toml_text: &str,
    env: &dyn Fn(&str) -> Option<OsString>,
    write: Write<'_>,
) -> Result<Summary, String> {
    let parsed = parse_manifest(toml_text, env)?;
    for key in &parsed.unmatched {
        tracing::warn!(env = %key, "external patches: the variable is set, but no entry's when_value is its value");
    }
    let all = grouped(&parsed.active).len();
    let groups = verify_and_collect(base as *const u8, size, &parsed.active);
    let (applied, unwritten) = apply_groups(&groups, write);
    Ok(Summary {
        active: parsed.active.len(),
        inactive: parsed.inactive,
        applied,
        groups_applied: groups.len() - unwritten,
        groups_skipped: all - groups.len() + unwritten,
    })
}

/// Applies all active patches parsed from `toml_text` into `[base .. base + size)`.
///
/// # Safety
/// As [`apply_from_toml_with`].
unsafe fn apply_from_toml(
    base: usize,
    size: usize,
    toml_text: &str,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Summary, String> {
    let mut write = |rva: usize, expected: &[u8], replace: &[u8]| {
        // SAFETY: caller guarantees `base..base+size` validity.
        unsafe { write_checked(base, size, rva, expected, replace) }
    };
    // SAFETY: as above.
    unsafe { apply_from_toml_with(base, size, toml_text, env, &mut write) }
}

fn apply_from_file(path: &Path, base: usize, size: usize) -> ApplyOutcome {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "failed to read external patches file; skipping");
            return ApplyOutcome::NoFile;
        }
    };
    // SAFETY: caller guarantees mapped image validity.
    match unsafe { apply_from_toml(base, size, &text, &|k| std::env::var_os(k)) } {
        Ok(done) => {
            tracing::info!(
                path = %path.display(),
                active = done.active,
                inactive = done.inactive,
                applied = done.applied,
                groups_applied = done.groups_applied,
                groups_skipped = done.groups_skipped,
                "external patches"
            );
            ApplyOutcome::Applied { count: done.applied }
        }
        Err(why) => {
            tracing::warn!(path = %path.display(), error = %why, "invalid external patches file; skipping");
            ApplyOutcome::NoFile
        }
    }
}

/// Applies external user-supplied byte-checked patches from `VICTORY_PATCHES_FILE`
/// if configured; otherwise performs no operation.
///
/// # Safety
/// `base..base + size` must be the mapped executable image and no guest threads
/// may be executing inside it yet.
pub(crate) unsafe fn apply_asked(base: usize, size: usize) {
    let Some(path) = std::env::var_os(PATCHES_ENV).filter(|v| !v.is_empty()) else {
        return;
    };
    let _ = apply_from_file(Path::new(&path), base, size);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_env_is_a_noop() {
        let mut buf = [0x90u8; 32];
        let snapshot = buf;
        let out = apply_from_file(Path::new("Z:\\does\\not\\exist\\patches.toml"), buf.as_mut_ptr() as usize, buf.len());
        assert_eq!(out, ApplyOutcome::NoFile);
        assert_eq!(buf, snapshot);
    }

    #[test]
    fn byte_checked_patch_applies_only_when_expected_matches_and_env_allows() {
        let toml = r#"
[[patch]]
name = "nop_trap"
when_env = "TEST_PATCH_ENABLE"
rva = "0x4"
expected = "cc cc cc"
replace = "90 90 c3"

[[patch]]
name = "gated_off"
when_env = "TEST_PATCH_ENABLE"
when_value = "special"
rva = "0x8"
expected = "00 00"
replace = "11 22"
"#;
        let mut buf = [0x00u8; 16];
        buf[4..7].copy_from_slice(&[0xcc, 0xcc, 0xcc]);

        // Disabled by env -> 0 applied.
        let n = unsafe { apply_from_toml(buf.as_mut_ptr() as usize, buf.len(), toml, &|_| None) }.unwrap().applied;
        assert_eq!(n, 0);
        assert_eq!(&buf[4..7], &[0xcc, 0xcc, 0xcc]);

        // Enabled with "1" -> applies first, skips second.
        let env = |k: &str| (k == "TEST_PATCH_ENABLE").then(|| OsString::from("1"));
        let n = unsafe { apply_from_toml(buf.as_mut_ptr() as usize, buf.len(), toml, &env) }.unwrap().applied;
        assert_eq!(n, 1);
        assert_eq!(&buf[4..7], &[0x90, 0x90, 0xc3]);
        assert_eq!(&buf[8..10], &[0x00, 0x00]);

        // Second run with wrong expected bytes -> skips cleanly.
        let n2 = unsafe { apply_from_toml(buf.as_mut_ptr() as usize, buf.len(), toml, &env) }.unwrap().applied;
        assert_eq!(n2, 0);
    }

    #[test]
    fn grouped_patches_roll_back_if_any_member_mismatches() {
        let toml = r#"
[[patch]]
name = "first"
group = "atomic_pair"
rva = "2"
expected = "aa bb"
replace = "11 22"

[[patch]]
name = "second_mismatch"
group = "atomic_pair"
rva = "6"
expected = "cc dd"
replace = "33 44"
"#;
        let mut buf = [0u8; 16];
        buf[2..4].copy_from_slice(&[0xaa, 0xbb]);
        buf[6..8].copy_from_slice(&[0xff, 0xff]); // mismatch

        let n = unsafe { apply_from_toml(buf.as_mut_ptr() as usize, buf.len(), toml, &|_| None) }.unwrap().applied;
        assert_eq!(n, 0);
        // First patch must NOT have been written.
        assert_eq!(&buf[2..4], &[0xaa, 0xbb]);
    }

    fn run(buf: &mut [u8], toml: &str, env: &dyn Fn(&str) -> Option<OsString>) -> Result<Summary, String> {
        // SAFETY: `buf` is this test's own writable buffer.
        unsafe { apply_from_toml(buf.as_mut_ptr() as usize, buf.len(), toml, env) }
    }

    fn entry(name: &str, more: &str, rva: usize, expected: &str, replace: &str) -> String {
        let bytes = format!("expected = \"{expected}\"\nreplace = \"{replace}\"\n");
        format!("[[patch]]\nname = \"{name}\"\n{more}\nrva = \"{rva}\"\n{bytes}")
    }

    #[test]
    fn a_malformed_manifest_is_refused_and_writes_nothing() {
        let good = entry("good", "", 0, "00", "11");
        let cases = [
            "[[patch]\nname = 1".to_owned(),
            // (an unknown key, a missing one)
            good.clone() + "extra = 1\n",
            "[[patch]]\nname = \"no bytes\"\nrva = \"0\"\n".to_owned(),
            good.clone() + &entry("odd", "", 4, "0", "1"),
            good.clone() + &entry("lengths", "", 4, "00 00", "11"),
            good.clone() + &entry("rva", "", 4, "00", "11").replace("\"4\"", "\"0xzz\""),
            // (checked although its condition does not hold)
            good.clone() + &entry("inactive", "when_env = \"TEST_PATCH_NEVER_SET\"", 4, "zz", "11"),
        ];
        for text in cases {
            let mut buf = [0u8; 8];
            assert!(run(&mut buf, &text, &|_| None).is_err(), "{text}");
            assert_eq!(buf, [0u8; 8], "{text}");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("patches.toml");
        std::fs::write(&path, "[[patch]\n").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(apply_from_file(&path, buf.as_mut_ptr() as usize, buf.len()), ApplyOutcome::NoFile);
        assert_eq!(buf, [0u8; 8]);
    }

    #[test]
    fn a_when_value_without_a_when_env_is_refused() {
        for more in ["when_value = \"a\"", "when_env = \"\"\nwhen_value = \"a\""] {
            let text = entry("first", "", 0, "00", "11") + &entry("orphan", more, 4, "00", "11");
            let mut buf = [0u8; 8];
            let why = run(&mut buf, &text, &|_| None).unwrap_err();
            assert!(why.contains("orphan") && why.contains("when_value without when_env"), "{why}");
            assert_eq!(buf, [0u8; 8]);
        }
    }

    #[test]
    fn a_when_value_picks_its_group_and_an_unmatched_value_is_told() {
        let text = entry("a1", "group = \"a\"\nwhen_env = \"TEST_PATCH_PICK\"\nwhen_value = \"a\"", 0, "00", "a1")
            + &entry("b1", "group = \"b\"\nwhen_env = \"TEST_PATCH_PICK\"\nwhen_value = \"b\"", 0, "00", "b1")
            + &entry("a2", "group = \"a\"\nwhen_env = \"TEST_PATCH_PICK\"\nwhen_value = \"a\"", 2, "00", "a2")
            + &entry("b2", "group = \"b\"\nwhen_env = \"TEST_PATCH_PICK\"\nwhen_value = \"b\"", 2, "00", "b2");
        let set = |value: &'static str| move |k: &str| (k == "TEST_PATCH_PICK").then(|| OsString::from(value));
        for (value, want) in [("a", [0xa1, 0, 0xa2, 0]), ("b", [0xb1, 0, 0xb2, 0])] {
            let mut buf = [0u8; 4];
            let done = run(&mut buf, &text, &set(value)).unwrap();
            assert_eq!(buf, want);
            let want = Summary { active: 2, inactive: 2, applied: 2, groups_applied: 1, groups_skipped: 0 };
            assert_eq!(done, want);
            assert!(parse_manifest(&text, &set(value)).unwrap().unmatched.is_empty());
        }
        // a value no entry names: nothing written, and the variable is named
        let mut buf = [0u8; 4];
        let done = run(&mut buf, &text, &set("c")).unwrap();
        assert_eq!((buf, done.active, done.inactive), ([0u8; 4], 0, 4));
        assert_eq!(parse_manifest(&text, &set("c")).unwrap().unmatched, ["TEST_PATCH_PICK"]);
        // unset, or set to an off value: nothing asked for
        for env in [&set("0") as &dyn Fn(&str) -> Option<OsString>, &set("false"), &|_| None] {
            assert!(parse_manifest(&text, env).unwrap().unmatched.is_empty());
        }
    }

    #[test]
    fn overlapping_patches_are_left_out() {
        // within a group: the group is left out whole
        let text =
            entry("one", "group = \"g\"", 0, "00 00", "11 11") + &entry("two", "group = \"g\"", 1, "00 00", "22 22");
        let mut buf = [0u8; 8];
        let done = run(&mut buf, &text, &|_| None).unwrap();
        assert_eq!(buf, [0u8; 8]);
        assert_eq!((done.applied, done.groups_applied, done.groups_skipped), (0, 0, 1));
        // across groups: the first in the file is written, the one that overlaps it is not,
        // and one that only touches it is
        let text = entry("late", "group = \"z\"", 4, "00 00", "11 11")
            + &entry("early", "group = \"a\"", 5, "00 00", "22 22")
            + &entry("early too", "group = \"a\"", 0, "00", "22")
            + &entry("beside", "", 6, "00", "33");
        let mut buf = [0u8; 8];
        let done = run(&mut buf, &text, &|_| None).unwrap();
        assert_eq!(buf, [0, 0, 0, 0, 0x11, 0x11, 0x33, 0]);
        assert_eq!((done.applied, done.groups_applied, done.groups_skipped), (2, 2, 1));
    }

    #[test]
    fn groups_are_applied_in_file_order() {
        let text = entry("z", "group = \"z\"", 0, "00", "11")
            + &entry("m", "", 1, "00", "22")
            + &entry("a", "group = \"a\"", 2, "00", "33")
            + &entry("z again", "group = \"z\"", 3, "00", "44");
        let parsed = parse_manifest(&text, &|_| None).unwrap();
        let order: Vec<Vec<&str>> =
            grouped(&parsed.active).iter().map(|g| g.1.iter().map(|p| p.name.as_str()).collect()).collect();
        assert_eq!(order, [vec!["z", "z again"], vec!["m"], vec!["a"]]);
        let mut buf = [0u8; 4];
        let mut seen = Vec::new();
        let base = buf.as_mut_ptr() as usize;
        let mut write = |rva: usize, expected: &[u8], replace: &[u8]| {
            seen.push(rva);
            // SAFETY: `buf` is this test's own writable buffer.
            unsafe { write_checked(base, 4, rva, expected, replace) }
        };
        // SAFETY: as above.
        unsafe { apply_from_toml_with(base, 4, &text, &|_| None, &mut write) }.unwrap();
        assert_eq!(seen, [0, 3, 1, 2]);
        assert_eq!(buf, [0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn a_write_that_fails_part_way_puts_the_groups_bytes_back() {
        let text = entry("before", "", 0, "aa", "01")
            + &entry("first", "group = \"g\"", 2, "aa bb", "11 22")
            + &entry("second", "group = \"g\"", 4, "aa", "33")
            + &entry("third", "group = \"g\"", 6, "aa bb", "44 55")
            + &entry("after", "", 8, "aa", "02");
        let start = [0xaa, 0, 0xaa, 0xbb, 0xaa, 0, 0xaa, 0xbb, 0xaa, 0];
        let mut buf = start;
        let base = buf.as_mut_ptr() as usize;
        let mut writes = 0;
        // (the group's third write fails: the fourth of the run)
        let mut write = |rva: usize, expected: &[u8], replace: &[u8]| {
            writes += 1;
            // SAFETY: `buf` is this test's own writable buffer.
            writes != 4 && unsafe { write_checked(base, start.len(), rva, expected, replace) }
        };
        // SAFETY: as above.
        let done = unsafe { apply_from_toml_with(base, start.len(), &text, &|_| None, &mut write) }.unwrap();
        // the entries around the group are written, the group's bytes are as they were
        assert_eq!(buf, [0x01, 0, 0xaa, 0xbb, 0xaa, 0, 0xaa, 0xbb, 0x02, 0]);
        assert_eq!(done, Summary { active: 5, inactive: 0, applied: 2, groups_applied: 2, groups_skipped: 1 });
        // (three of the group, two taken back, the two around it)
        assert_eq!(writes, 7);
        // and without the failure the whole group is written
        let mut buf = start;
        let done = run(&mut buf, &text, &|_| None).unwrap();
        assert_eq!(buf, [0x01, 0, 0x11, 0x22, 0x33, 0, 0x44, 0x55, 0x02, 0]);
        assert_eq!((done.applied, done.groups_applied, done.groups_skipped), (5, 3, 0));
    }
}
