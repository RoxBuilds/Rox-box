//! `cargo xtask switches`: a browsable reference of every `VICTORY_*` switch,
//! generated from the source into `docs/switches.md`.
//!
//! There is no hand-written central list on purpose: a switch is read where it
//! is used and described in a comment there, so code and meaning stay
//! together. This command only collects what the source already says, so the
//! source stays the single truth. `--check` (and a unit test, so that
//! `cargo test` and CI catch it) fails when the file on disk is stale.
//!
//! The scan is plain string handling, not a parser. Its heuristics, and what
//! they miss, are listed in the header of the generated file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;

/// The generated file, relative to the repository root.
pub const OUTPUT: &str = "docs/switches.md";
/// The launcher script whose `Get-RunSettings` sets the default switches.
const LAUNCHER: &str = "scripts/play.ps1";
/// Longest description, in characters, ellipsis included.
const MAX_DESCRIPTION: usize = 400;
/// The names read here are paths, not behaviour: they get their own section.
const PATH_SOURCE: &str = "crates/victory-core/src/config.rs";
/// The crate whose sources `victory-shader/build.rs` scans for the shader
/// cache key.
const SHADER_SRC: &str = "crates/victory-shader/src/";

pub fn run(root: &Path, check: bool) -> Result<()> {
    let text = generate_for(root)?;
    let path = root.join(OUTPUT);
    if check {
        let on_disk = std::fs::read_to_string(&path).map(|t| t.replace("\r\n", "\n"));
        match on_disk {
            Ok(t) if t == text => {
                println!("{OUTPUT} is up to date");
                Ok(())
            }
            Ok(_) => bail!(
                "{OUTPUT} is out of date with the source (a switch was added, removed or \
                 re-described); run `cargo xtask switches` and commit the result"
            ),
            Err(_) => bail!("{OUTPUT} is missing; run `cargo xtask switches` and commit the result"),
        }
    } else {
        std::fs::write(&path, text.as_bytes()).with_context(|| format!("writing {OUTPUT}"))?;
        println!("wrote {OUTPUT}");
        Ok(())
    }
}

/// What `docs/switches.md` should say for the repository at `root`.
pub fn generate_for(root: &Path) -> Result<String> {
    let mut sources = Vec::new();
    for dir in ["crates", "apps"] {
        let walk = WalkDir::new(root.join(dir))
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| e.file_name() != "target");
        for entry in walk {
            let entry = entry.with_context(|| format!("walking {dir}"))?;
            if !entry.file_type().is_file() || entry.path().extension().is_none_or(|x| x != "rs") {
                continue;
            }
            let rel = entry.path().strip_prefix(root)?.to_string_lossy().replace('\\', "/");
            let bytes = std::fs::read(entry.path()).with_context(|| format!("reading {rel}"))?;
            // Checkouts may have CRLF endings; the output must not depend on that.
            sources.push((rel, String::from_utf8_lossy(&bytes).replace("\r\n", "\n")));
        }
    }
    let launcher = std::fs::read(root.join(LAUNCHER)).with_context(|| format!("reading {LAUNCHER}"))?;
    let launcher = String::from_utf8_lossy(&launcher).replace("\r\n", "\n");
    Ok(generate(&sources, Some(&launcher)))
}

// ---------------------------------------------------------------- scanning

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `toggled!`: on unless set to `0`.
    OnUnlessZero,
    /// `switch::on`: off unless set to something other than `0`.
    OffUnlessSet,
    /// Anything else: a value parsed where it is read.
    Value,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::OnUnlessZero => "on unless `0`",
            Kind::OffUnlessSet => "off unless set",
            Kind::Value => "value",
        }
    }
}

/// One place a switch is read.
#[derive(Debug)]
struct Read {
    name: String,
    /// 1-based line of the literal. Used to find the comment; never printed.
    line: u32,
    kind: Kind,
    /// Inside an item or module under `#[cfg(feature = "diagnostics")]`.
    diagnostics: bool,
    /// The comment block above, as one paragraph, unescaped and uncut.
    description: Option<String>,
}

/// `mod name;` and the conditions it was declared under.
#[derive(Debug)]
struct ModDecl {
    name: String,
    diagnostics: bool,
    test: bool,
}

#[derive(Debug, Default)]
struct Scan {
    reads: Vec<Read>,
    mods: Vec<ModDecl>,
    /// `#![cfg(...)]` at the top of the file.
    file_diagnostics: bool,
    file_test: bool,
}

/// `(diagnostics only, test only)` for the text inside `#[...]`.
fn classify_attribute(inner: &str) -> (bool, bool) {
    let a: String = inner.chars().filter(|c| !c.is_whitespace()).collect();
    if a == "test" {
        return (false, true);
    }
    let Some(cond) = a.strip_prefix("cfg(").and_then(|s| s.strip_suffix(')')) else {
        return (false, false);
    };
    let all = cond.strip_prefix("all(").and_then(|s| s.strip_suffix(')'));
    let parts: Vec<&str> = all.unwrap_or(cond).split(',').collect();
    // `any(..)` and `not(..)` can be true in a build without the feature.
    if parts.iter().any(|p| p.contains('(')) {
        return (false, false);
    }
    (parts.contains(&"feature=\"diagnostics\""), parts.contains(&"test"))
}

fn is_switch_name(s: &str) -> bool {
    s.strip_prefix("VICTORY_")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
}

/// A tiny lexer: it skips comments, strings, characters and attributes, keeps
/// track of braces and of which `cfg` conditions an item sits under, and
/// reports every string literal that is exactly a switch name and is not an
/// argument of a call that only sets or clears the variable.
fn scan_source(src: &str) -> Scan {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let lines: Vec<&str> = src.lines().collect();
    let mut line_at = Vec::with_capacity(n + 1);
    let mut l = 1u32;
    for &c in &chars {
        line_at.push(l);
        if c == '\n' {
            l += 1;
        }
    }
    line_at.push(l);

    let mut out = Scan::default();
    // Flags (diagnostics, test) of each open brace, with the bracket depth to
    // return to when it closes.
    let mut scopes: Vec<((bool, bool), i32)> = Vec::new();
    // An attribute seen, not yet attached to the brace or `;` of its item.
    let mut pending = (false, false);
    let mut paren = 0i32;
    // The code of the current statement: no comments, strings as `""`.
    let mut stmt = String::new();
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i < n {
        let c = chars[i];
        let top = scopes.last().map_or((false, false), |s| s.0);
        let active = (top.0 || pending.0, top.1 || pending.1);
        match c {
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                let mut depth = 0;
                while i < n {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            '#' if matches!(chars.get(i + 1), Some('[')) || (chars.get(i + 1) == Some(&'!') && chars.get(i + 2) == Some(&'[')) => {
                let inner_attr = chars[i + 1] == '!';
                i += if inner_attr { 3 } else { 2 };
                let start = i;
                let mut depth = 1;
                while i < n && depth > 0 {
                    match chars[i] {
                        '[' => depth += 1,
                        ']' => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                let text: String = chars[start..i.saturating_sub(1).max(start)].iter().collect();
                let (diag, test) = classify_attribute(&text);
                if inner_attr {
                    out.file_diagnostics |= diag;
                    out.file_test |= test;
                } else {
                    pending.0 |= diag;
                    pending.1 |= test;
                }
            }
            '"' => {
                let at = i;
                i += 1;
                let mut content = String::new();
                while i < n && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < n {
                        content.push(chars[i]);
                        i += 1;
                    }
                    content.push(chars[i]);
                    i += 1;
                }
                i += 1;
                literal(&mut out, &lines, &stmt, &content, line_at[at], active);
                stmt.push_str("\"\"");
            }
            'r' | 'b' if (i == 0 || !ident(chars[i - 1])) && raw_string_at(&chars, i).is_some() => {
                let (hashes, body) = raw_string_at(&chars, i).unwrap_or((0, i));
                let mut j = body;
                let close: Vec<char> = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
                while j < n && chars[j..].len() >= close.len() && chars[j..j + close.len()] != close[..] {
                    j += 1;
                }
                let content: String = chars[body..j.min(n)].iter().collect();
                literal(&mut out, &lines, &stmt, &content, line_at[i], active);
                stmt.push_str("\"\"");
                i = (j + close.len()).min(n);
            }
            '\'' => {
                if chars.get(i + 1) == Some(&'\\') {
                    let mut j = i + 3;
                    while j < n && chars[j] != '\'' {
                        j += 1;
                    }
                    i = j + 1;
                } else if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else {
                    i += 1; // a lifetime
                }
            }
            '(' | '[' => {
                paren += 1;
                stmt.push(c);
                i += 1;
            }
            ')' | ']' => {
                paren -= 1;
                stmt.push(c);
                i += 1;
            }
            '{' => {
                scopes.push((active, paren));
                if paren == 0 {
                    pending = (false, false);
                }
                paren = 0;
                stmt.clear();
                i += 1;
            }
            '}' => {
                if let Some((_, was)) = scopes.pop() {
                    paren = was;
                }
                pending = (false, false);
                stmt.clear();
                i += 1;
            }
            ';' if paren <= 0 => {
                if let Some(name) = module_name(&stmt) {
                    out.mods.push(ModDecl { name, diagnostics: active.0, test: active.1 });
                }
                pending = (false, false);
                stmt.clear();
                paren = 0;
                i += 1;
            }
            _ => {
                stmt.push(c);
                i += 1;
            }
        }
    }
    out
}

/// If a raw string literal (`r"`, `r#"`, `br"`, ...) starts at `i`: its number
/// of `#`s and the index of the first character of its body.
fn raw_string_at(chars: &[char], i: usize) -> Option<(usize, usize)> {
    let mut j = i;
    if chars[j] == 'b' {
        j += 1;
    }
    if chars.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let mut hashes = 0;
    while chars.get(j) == Some(&'#') {
        hashes += 1;
        j += 1;
    }
    (chars.get(j) == Some(&'"')).then_some((hashes, j + 1))
}

/// `name` of a statement that is `mod name`, `pub mod name`, `pub(crate) mod name`.
fn module_name(stmt: &str) -> Option<String> {
    let words: Vec<&str> = stmt.split_whitespace().collect();
    let at = words.iter().position(|w| *w == "mod")?;
    let visibility_only = words[..at].iter().all(|w| w.starts_with("pub"));
    (visibility_only && words.len() == at + 2 && words[at + 1].chars().all(|c| c.is_alphanumeric() || c == '_'))
        .then(|| words[at + 1].to_owned())
}

/// Records a string literal if it is a switch name that is read.
fn literal(out: &mut Scan, lines: &[&str], stmt: &str, content: &str, line: u32, active: (bool, bool)) {
    if !is_switch_name(content) {
        return;
    }
    let before = stmt.trim_end();
    // Setting or clearing a variable for a child process or a test is no
    // read, and `env!` is the build's own variable, fixed at compile time.
    if [".env(", ".env_remove(", "set_var(", "remove_var(", "env!("].iter().any(|s| before.ends_with(s)) {
        return;
    }
    if active.1 {
        return;
    }
    let kind = if before.ends_with("toggled!(") {
        Kind::OnUnlessZero
    } else if before.ends_with("switch::on(") {
        Kind::OffUnlessSet
    } else {
        Kind::Value
    };
    out.reads.push(Read {
        name: content.to_owned(),
        line,
        kind,
        diagnostics: active.0,
        description: description_above(lines, line as usize),
    });
}

/// The comment block directly above the statement the read is in, as one
/// paragraph. A read on a continuation line looks a few lines up.
fn description_above(lines: &[&str], line: usize) -> Option<String> {
    let mut at = line;
    for _ in 0..4 {
        if let Some(text) = comment_block_above(lines, at) {
            return Some(text);
        }
        if at < 2 {
            break;
        }
        let prev = lines.get(at - 2)?.trim();
        if prev.is_empty() || prev.starts_with("//") || prev.starts_with("#[") || prev.ends_with([';', '{', '}', ',']) {
            break;
        }
        at -= 1;
    }
    // A small function that exists to read the switch: its doc comment says
    // what the switch means.
    for up in 1..=6usize {
        let Some(prev) = line.checked_sub(up + 1).and_then(|i| lines.get(i)) else { break };
        let t = prev.trim();
        if t.is_empty() || t.ends_with('}') || t.starts_with("//") {
            break;
        }
        let is_fn = ["fn ", "pub fn ", "pub(crate) fn ", "const fn ", "pub const fn ", "unsafe fn "]
            .iter()
            .any(|p| t.starts_with(p));
        if is_fn {
            return comment_block_above(lines, line - up);
        }
    }
    None
}

/// The `//`, `///` or `//!` lines directly above 1-based line `at`, skipping
/// attributes, joined with single spaces.
fn comment_block_above(lines: &[&str], at: usize) -> Option<String> {
    let mut idx = at.checked_sub(1)?; // 0-based index of the line `at`
    while idx > 0 && lines[idx - 1].trim_start().starts_with("#[") {
        idx -= 1;
    }
    let mut block = Vec::new();
    while idx > 0 {
        let l = lines[idx - 1].trim_start();
        let Some(rest) = l.strip_prefix("//") else { break };
        let rest = rest.strip_prefix(['/', '!']).unwrap_or(rest);
        block.push(rest.trim());
        idx -= 1;
    }
    block.reverse();
    let text = block.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

// ------------------------------------------------------------- the launcher

/// What the launcher sets: name to a list of `(conditions, value)`, in the
/// order of `Get-RunSettings`. A value that starts with `$` is a parameter.
type LauncherSettings = BTreeMap<String, Vec<(Vec<String>, String)>>;

/// Reads `Get-RunSettings` line by line: a stack of `if ($Switch) {` blocks
/// gives the launcher option each assignment is under.
fn launcher_settings(ps1: &str) -> LauncherSettings {
    let mut found = LauncherSettings::new();
    let mut in_function = false;
    // One entry per open brace: its condition, if it opened an `if` or `else`.
    let mut stack: Vec<Option<String>> = Vec::new();
    let mut last_closed: Option<String> = None;
    for raw in ps1.lines() {
        if !in_function {
            in_function = raw.starts_with("function Get-RunSettings");
            if !in_function {
                continue;
            }
        } else if raw.starts_with('}') {
            break;
        }
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut segment = String::new();
        let flush = |segment: &mut String, stack: &[Option<String>], found: &mut LauncherSettings| {
            let conds: Vec<String> = stack.iter().flatten().cloned().collect();
            for (name, value) in assignments(segment) {
                found.entry(name).or_default().push((conds.clone(), value));
            }
        };
        for c in line.chars() {
            match c {
                '{' => {
                    let text = segment.trim().to_owned();
                    let cond = if text == "else" {
                        last_closed.take().map(|c| c.replacen("with ", "without ", 1))
                    } else {
                        text.strip_prefix("if ($")
                            .and_then(|r| r.split(')').next())
                            .map(|v| format!("with -{v}"))
                    };
                    flush(&mut segment, &stack, &mut found);
                    stack.push(cond);
                    segment.clear();
                }
                '}' => {
                    flush(&mut segment, &stack, &mut found);
                    last_closed = stack.pop().flatten();
                    segment.clear();
                }
                _ => segment.push(c),
            }
        }
        flush(&mut segment, &stack, &mut found);
    }
    found
}

/// `VICTORY_X = '1'` and `VICTORY_X = $Display` in a piece of PowerShell.
fn assignments(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("VICTORY_") {
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        let name = &tail[..end];
        let after = tail[end..].trim_start();
        if let Some(value) = after.strip_prefix('=') {
            let value = value.trim_start();
            let value = if let Some(q) = value.strip_prefix('\'') {
                q.split('\'').next().map(str::to_owned)
            } else if value.starts_with('$') {
                Some(value.split(|c: char| !(c.is_alphanumeric() || c == '$' || c == '_')).next().unwrap_or("").to_owned())
            } else {
                None
            };
            if let Some(value) = value {
                found.push((name.to_owned(), value));
            }
        }
        rest = &tail[end..];
    }
    found
}

fn launcher_cell(entries: &[(Vec<String>, String)]) -> String {
    let mut cells: Vec<String> = Vec::new();
    for (conds, value) in entries {
        let cell = if let Some(param) = value.strip_prefix('$') {
            format!("the value of `-{param}`")
        } else if conds.is_empty() {
            format!("`{value}`")
        } else {
            format!("`{value}` {}", conds.join(", "))
        };
        if !cells.contains(&cell) {
            cells.push(cell);
        }
    }
    cells.join("<br>")
}

// ------------------------------------------------------------- aggregating

#[derive(Debug)]
struct Switch {
    name: String,
    krate: String,
    kind: Kind,
    shader: bool,
    diagnostics_only: bool,
    description: Option<String>,
    /// File to the line of its first read there.
    files: BTreeMap<String, u32>,
}

/// `crates/foo/src/x.rs` and `apps/foo/src/x.rs` belong to `foo`.
fn crate_of(path: &str) -> &str {
    path.split('/').nth(1).unwrap_or("?")
}

/// The switches the shader translator reads, as `victory-shader/build.rs`
/// collects them: every `"VICTORY_NAME"` in its sources, tests left out.
fn shader_switches(sources: &[(String, String)]) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for (path, text) in sources {
        let Some(rel) = path.strip_prefix(SHADER_SRC) else { continue };
        if rel.ends_with("_tests.rs") || rel.ends_with("/tests.rs") {
            continue;
        }
        let mut rest = &text[..];
        while let Some(at) = rest.find("\"VICTORY_") {
            let tail = &rest[at + 1..];
            let end = tail
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(tail.len());
            if tail[end..].starts_with('"') {
                set.insert(tail[..end].to_owned());
            }
            rest = &tail[end..];
        }
    }
    set
}

/// Whether a file as a whole is under `cfg(test)` / `cfg(feature = "diagnostics")`,
/// through its own `#![cfg]`, its place in a tests folder, or the `mod` line
/// that declares it (in a parent that may be flagged itself).
fn file_flags(path: &str, scans: &BTreeMap<&str, Scan>) -> (bool, bool) {
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut flags = (false, name == "tests.rs" || name.ends_with("_tests.rs") || path.contains("/tests/"));
    if let Some(scan) = scans.get(path) {
        flags.0 |= scan.file_diagnostics;
        flags.1 |= scan.file_test;
    }
    for (parent, scan) in scans {
        if *parent == path {
            continue;
        }
        let (dir, file) = parent.rsplit_once('/').unwrap_or(("", parent));
        let module_dir = if matches!(file, "lib.rs" | "main.rs" | "mod.rs") {
            dir.to_owned()
        } else {
            format!("{dir}/{}", file.trim_end_matches(".rs"))
        };
        for decl in &scan.mods {
            let file_form = format!("{module_dir}/{}.rs", decl.name);
            let dir_form = format!("{module_dir}/{}/", decl.name);
            if path == file_form || path.starts_with(&dir_form) {
                let up = file_flags(parent, scans);
                flags.0 |= decl.diagnostics | up.0;
                flags.1 |= decl.test | up.1;
            }
        }
    }
    flags
}

fn collect(sources: &[(String, String)]) -> Vec<Switch> {
    let scans: BTreeMap<&str, Scan> = sources.iter().map(|(p, t)| (p.as_str(), scan_source(t))).collect();
    let shader = shader_switches(sources);
    // Per name: every read with its file, in file then source order.
    let mut reads: BTreeMap<&str, Vec<(&str, &Read, bool)>> = BTreeMap::new();
    for (path, scan) in &scans {
        let (file_diag, file_test) = file_flags(path, &scans);
        if file_test {
            continue;
        }
        for read in &scan.reads {
            reads.entry(&read.name).or_default().push((path, read, file_diag || read.diagnostics));
        }
    }
    reads
        .into_iter()
        .map(|(name, sites)| {
            let described = sites.iter().find(|(_, r, _)| r.description.is_some());
            let krate = crate_of(described.unwrap_or(&sites[0]).0).to_owned();
            let kind = sites.iter().map(|(_, r, _)| r.kind).find(|k| *k != Kind::Value).unwrap_or(Kind::Value);
            let mut files = BTreeMap::new();
            for (path, read, _) in &sites {
                files.entry((*path).to_owned()).or_insert(read.line);
            }
            Switch {
                name: name.to_owned(),
                krate,
                kind,
                shader: shader.contains(name),
                diagnostics_only: sites.iter().all(|(_, _, d)| *d),
                description: described.and_then(|(_, r, _)| r.description.clone()),
                files,
            }
        })
        .collect()
}

// --------------------------------------------------------------- rendering

/// Keeps a description to `MAX_DESCRIPTION` characters, cut at a word where
/// that costs little, ending with an ellipsis when cut.
fn cap(text: &str) -> String {
    if text.chars().count() <= MAX_DESCRIPTION {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(MAX_DESCRIPTION - 3).collect();
    if let Some(space) = cut.rfind(' ').filter(|s| cut.len() - s < 40) {
        cut.truncate(space);
    }
    let cut = cut.trim_end_matches([' ', ',', ';', ':', '.']);
    format!("{cut}...")
}

/// Escapes text for a table cell. Code spans (balanced backticks) stay code;
/// in them only `|` needs a backslash.
fn escape_markdown(text: &str) -> String {
    let plain = |s: &str, out: &mut String| {
        for c in s.chars() {
            if matches!(c, '\\' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '|' | '~' | '`') {
                out.push('\\');
            }
            out.push(c);
        }
    };
    let mut out = String::new();
    if text.matches('`').count() % 2 == 1 {
        plain(text, &mut out);
        return out;
    }
    for (k, part) in text.split('`').enumerate() {
        if k % 2 == 1 {
            out.push('`');
            out.push_str(&part.replace('|', "\\|"));
            out.push('`');
        } else {
            plain(part, &mut out);
        }
    }
    out
}

pub fn generate(sources: &[(String, String)], launcher: Option<&str>) -> String {
    let mut switches = collect(sources);
    let launcher = launcher.map(launcher_settings).unwrap_or_default();
    let mut left_out = 0usize;
    let mut described: BTreeMap<String, String> = BTreeMap::new();
    for s in &mut switches {
        let Some(text) = s.description.take() else { continue };
        let escaped = escape_markdown(&cap(&text));
        let problems = crate::policy::check_with(OUTPUT, escaped.as_bytes(), &crate::policy::Denylist::default());
        if problems.is_empty() {
            described.insert(s.name.clone(), escaped);
        } else {
            left_out += 1;
        }
    }

    let (paths, behaviour): (Vec<&Switch>, Vec<&Switch>) =
        switches.iter().partition(|s| s.files.contains_key(PATH_SOURCE));
    let mut by_crate: BTreeMap<&str, Vec<&Switch>> = BTreeMap::new();
    for s in &behaviour {
        by_crate.entry(&s.krate).or_default().push(s);
    }

    let mut out = String::new();
    out.push_str("# Switches\n\n");
    out.push_str(
        "Every `VICTORY_*` environment variable the source reads. This file is generated by \
         `cargo xtask switches`: do not edit it by hand. There is no hand-written list on \
         purpose; each switch is read where it is used and described in a comment there, and \
         this reference is collected from those comments.\n\n",
    );
    out.push_str(
        "To add a switch, write it in the code (`switch::on`, `toggled!` or a value read), \
         describe it in a comment directly above the read, and run `cargo xtask switches`. \
         `cargo xtask switches --check` (and `cargo test`) fail when this file is stale. \
         `scripts\\play.ps1` clears every other `VICTORY_*` variable; see \
         [ARCHITECTURE.md](../ARCHITECTURE.md#configuration-and-switches).\n\n",
    );
    out.push_str(&format!(
        "{} switches: {} paths, {}.\n\n",
        switches.len(),
        paths.len(),
        by_crate.iter().map(|(k, v)| format!("{} in `{k}`", v.len())).collect::<Vec<_>>().join(", ")
    ));
    out.push_str("How to read the tables:\n\n");
    out.push_str(
        "- **Kind**: `on unless 0` is `toggled!` (on unless the value is `0`, read once); \
         `off unless set` is `switch::on` (off unless set to something other than `0`); \
         `value` is anything else, parsed where it is read. **shader cache key** marks a switch \
         the shader translator reads: it is collected as `victory-shader/build.rs` does and its \
         value is part of the shader cache key. **diagnostics builds only** marks a switch whose \
         every read sits in an item or module under `#[cfg(feature = \"diagnostics\")]`.\n",
    );
    out.push_str(
        "- **Set by launcher**: what `Get-RunSettings` in `scripts/play.ps1` sets by default, \
         and under which launcher options (`-Safe`, `-Fast`, `-RayTracing`).\n",
    );
    out.push_str(
        "- **Description**: the comment block directly above the first read that has one, \
         cut to 400 characters. Empty when the code has no such comment.\n",
    );
    out.push_str(
        "- **Where**: every file that reads it. Line numbers are left out so that unrelated \
         edits do not make this file stale.\n",
    );
    out.push_str(
        "- The scan is a heuristic, not a parser. It misses names built at run time \
         (`format!`), counts only a string that is exactly the name, ignores reads in tests, and \
         files its section by the crate of the first read that has a description. The \
         diagnostics marker follows `#[cfg]` attributes on items, modules and `mod` lines only.\n\n",
    );

    let table = |out: &mut String, rows: &[&Switch], with_kind: bool| {
        if with_kind {
            out.push_str("| Name | Kind | Set by launcher | Description | Where |\n|---|---|---|---|---|\n");
        } else {
            out.push_str("| Name | Description | Where |\n|---|---|---|\n");
        }
        for s in rows {
            let mut cells = vec![format!("`{}`", s.name)];
            if with_kind {
                let mut kind = s.kind.label().to_owned();
                if s.shader {
                    kind.push_str("<br>shader cache key");
                }
                if s.diagnostics_only {
                    kind.push_str("<br>diagnostics builds only");
                }
                cells.push(kind);
                cells.push(launcher.get(&s.name).map(|e| launcher_cell(e)).unwrap_or_default());
            }
            cells.push(described.get(&s.name).cloned().unwrap_or_default());
            cells.push(s.files.keys().map(|f| format!("`{f}`")).collect::<Vec<_>>().join("<br>"));
            out.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
        out.push('\n');
    };

    if !paths.is_empty() {
        out.push_str(
            "## Paths\n\nFolders and files, also settable under `[paths]` in `victory.toml` \
             (the environment variable wins). See [victory.example.toml](../victory.example.toml).\n\n",
        );
        table(&mut out, &paths, false);
    }
    for (krate, rows) in &by_crate {
        out.push_str(&format!("## {krate}\n\n"));
        table(&mut out, rows, true);
    }
    out.push_str(&format!(
        "{left_out} descriptions left out (a comment that would trip the asset policy, such as a \
         sample path).\n"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(src: &str) -> Scan {
        scan_source(src)
    }

    fn names(scan: &Scan) -> Vec<&str> {
        scan.reads.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn the_repository_reference_is_current() {
        run(&crate::workspace_root(), true).unwrap();
    }

    #[test]
    fn every_read_form_is_found_with_its_kind() {
        let s = one(r##"
fn f() {
    let a = crate::toggled!("VICTORY_A");
    let b = crate::switch::on("VICTORY_B");
    let c = std::env::var("VICTORY_C").ok();
    let d = std::env::var_os(
        "VICTORY_D",
    );
    let e = number("VICTORY_E");
    let g = r"VICTORY_G";
    let h = br#"VICTORY_H"#;
}
"##);
        let found: Vec<(&str, Kind)> = s.reads.iter().map(|r| (r.name.as_str(), r.kind)).collect();
        assert_eq!(
            found,
            [
                ("VICTORY_A", Kind::OnUnlessZero),
                ("VICTORY_B", Kind::OffUnlessSet),
                ("VICTORY_C", Kind::Value),
                ("VICTORY_D", Kind::Value),
                ("VICTORY_E", Kind::Value),
                ("VICTORY_G", Kind::Value),
                ("VICTORY_H", Kind::Value),
            ]
        );
        assert_eq!(s.reads[3].line, 7);
    }

    #[test]
    fn comments_messages_and_setters_are_not_reads() {
        let s = one(r##"
// VICTORY_COMMENT is described here
/// and "VICTORY_DOC" here
/* "VICTORY_BLOCK" /* nested */ "VICTORY_BLOCK2" */
fn f() {
    log("VICTORY_MESSAGE is not set");
    let c = std::process::Command::new("x").env("VICTORY_CHILD", "1").env_remove("VICTORY_GONE");
    unsafe { std::env::set_var("VICTORY_SET", "1"); std::env::remove_var("VICTORY_UNSET"); }
    let built = env!("VICTORY_BUILT");
    let lifetime: &'a str = "VICTORY_REAL"; let ch = '"'; let esc = '\'';
    let x = "a \" VICTORY_IN_STRING \"";
}
"##);
        assert_eq!(names(&s), ["VICTORY_REAL"]);
    }

    #[test]
    fn test_code_is_not_read() {
        let s = one(r##"
fn f() { std::env::var("VICTORY_REAL"); }
#[cfg(test)]
mod tests {
    fn t() { std::env::var("VICTORY_IN_TEST"); }
}
#[test]
fn single() { std::env::var("VICTORY_SINGLE"); }
"##);
        assert_eq!(names(&s), ["VICTORY_REAL"]);
    }

    #[test]
    fn the_description_is_the_comment_above() {
        let s = one(r##"
// Unrelated.

/// Skips the thing,
/// because it is slow.
#[allow(dead_code)]
static ON: bool = switch::on("VICTORY_DOC");

fn g() {
    // Why `a|b` is here: a <b> & c.
    let v = std::env::var_os("VICTORY_LINE");
    // Above a statement that continues.
    let w = foo
        .bar(
            "VICTORY_CONT",
        );
    let none = std::env::var("VICTORY_NONE");
    let (p, q) = (1, std::env::var("VICTORY_AFTER_CODE"));
}

/// Whether the helper is on.
fn helper() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HELPER").is_some())
}
"##);
        let d = |i: usize| s.reads[i].description.as_deref();
        assert_eq!(d(0), Some("Skips the thing, because it is slow."));
        assert_eq!(d(1), Some("Why `a|b` is here: a <b> & c."));
        assert_eq!(d(2), Some("Above a statement that continues."));
        assert_eq!(d(3), None);
        assert_eq!(d(4), None);
        assert_eq!(d(5), Some("Whether the helper is on."));
    }

    #[test]
    fn the_diagnostics_heuristic_follows_cfg_on_items_and_modules() {
        let s = one(r##"
#[cfg(feature = "diagnostics")]
fn only() { std::env::var("VICTORY_DIAG_FN"); }
#[cfg(all(windows, feature = "diagnostics"))]
mod inline { fn x() { std::env::var("VICTORY_DIAG_MOD"); } }
#[cfg(any(test, feature = "diagnostics"))]
fn either() { std::env::var("VICTORY_EITHER"); }
#[cfg(not(feature = "diagnostics"))]
fn without() { std::env::var("VICTORY_WITHOUT"); }
fn both() {
    #[cfg(feature = "diagnostics")]
    let a = std::env::var("VICTORY_DIAG_LET");
    let b = std::env::var("VICTORY_PLAIN");
    #[cfg(feature = "diagnostics")]
    {
        let c = std::env::var("VICTORY_DIAG_BLOCK");
    }
}
#[cfg(feature = "diagnostics")]
mod ab;
pub(crate) mod plain;
"##);
        let flags: Vec<(&str, bool)> = s.reads.iter().map(|r| (r.name.as_str(), r.diagnostics)).collect();
        assert_eq!(
            flags,
            [
                ("VICTORY_DIAG_FN", true),
                ("VICTORY_DIAG_MOD", true),
                ("VICTORY_EITHER", false),
                ("VICTORY_WITHOUT", false),
                ("VICTORY_DIAG_LET", true),
                ("VICTORY_PLAIN", false),
                ("VICTORY_DIAG_BLOCK", true),
            ]
        );
        let mods: Vec<(&str, bool)> = s.mods.iter().map(|m| (m.name.as_str(), m.diagnostics)).collect();
        assert_eq!(mods, [("ab", true), ("plain", false)]);
    }

    fn files(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(p, t)| ((*p).to_owned(), (*t).to_owned())).collect()
    }

    #[test]
    fn a_module_declared_under_cfg_marks_its_file() {
        let sources = files(&[
            ("crates/a/src/lib.rs", "#[cfg(feature = \"diagnostics\")]\nmod ab;\n#[cfg(test)]\nmod tests;\nmod other;\n"),
            ("crates/a/src/ab.rs", "fn f() { std::env::var(\"VICTORY_AB_ONLY\"); mod_in_ab(); }\n"),
            ("crates/a/src/other.rs", "fn f() { std::env::var(\"VICTORY_SHARED\"); }\n"),
            ("crates/a/src/tests.rs", "fn f() { std::env::var(\"VICTORY_TEST_ONLY\"); }\n"),
            ("apps/b/src/main.rs", "#[cfg(feature = \"diagnostics\")]\nfn f() { std::env::var(\"VICTORY_SHARED\"); }\n"),
        ]);
        let all = collect(&sources);
        let by = |n: &str| all.iter().find(|s| s.name == n);
        assert!(by("VICTORY_AB_ONLY").unwrap().diagnostics_only);
        assert!(by("VICTORY_TEST_ONLY").is_none());
        // One plain read among diagnostics reads: not diagnostics-only.
        let shared = by("VICTORY_SHARED").unwrap();
        assert!(!shared.diagnostics_only);
        assert_eq!(shared.files.len(), 2);
    }

    #[test]
    fn shader_reads_follow_the_build_script() {
        let sources = files(&[
            ("crates/victory-shader/src/lib.rs", "fn f() { std::env::var(\"VICTORY_SHADER_X\"); }\n"),
            ("crates/victory-shader/src/lower_tests.rs", "fn f() { var(\"VICTORY_ONLY_IN_TEST\"); }\n"),
            ("crates/victory-gpu/src/lib.rs", "fn f() { std::env::var(\"VICTORY_GPU_X\"); }\n"),
        ]);
        let all = collect(&sources);
        let by = |n: &str| all.iter().find(|s| s.name == n).unwrap();
        assert!(by("VICTORY_SHADER_X").shader);
        assert!(!by("VICTORY_GPU_X").shader);
    }

    #[test]
    fn markdown_is_escaped_outside_code_and_pipes_inside() {
        assert_eq!(escape_markdown("a|b <n> & *x* [y] _z_"), "a\\|b \\<n\\> \\& \\*x\\* \\[y\\] \\_z\\_");
        assert_eq!(escape_markdown("see `a|b <n>` or `x`"), "see `a\\|b <n>` or `x`");
        assert_eq!(escape_markdown("one ` tick"), "one \\` tick");
    }

    #[test]
    fn long_descriptions_are_cut_with_an_ellipsis() {
        let long = "word ".repeat(200);
        let cut = cap(&long);
        assert!(cut.chars().count() <= MAX_DESCRIPTION && cut.ends_with("word..."), "{cut}");
        assert_eq!(cap("short"), "short");
    }

    #[test]
    fn the_launcher_settings_know_their_options() {
        let ps1 = "function Get-RunSettings([switch]$Safe) {\n    $settings = @{\n        VICTORY_A = '1'; VICTORY_B = '2'\n        # VICTORY_C = '9'\n    }\n    if ($Safe) {\n        $settings.VICTORY_D = '0'; $settings.VICTORY_B = '3'\n    } else {\n        $settings.VICTORY_E = '1'\n    }\n    if ($Display) { $settings.VICTORY_F = $Display }\n    if ($Fast) {\n        $preset = [ordered]@{ VICTORY_A = '0' }\n    }\n    $settings\n}\n$x = @{ VICTORY_Z = '1' }\n";
        let got = launcher_settings(ps1);
        let cell = |n: &str| launcher_cell(&got[n]);
        assert_eq!(cell("VICTORY_A"), "`1`<br>`0` with -Fast");
        assert_eq!(cell("VICTORY_B"), "`2`<br>`3` with -Safe");
        assert_eq!(cell("VICTORY_D"), "`0` with -Safe");
        assert_eq!(cell("VICTORY_E"), "`1` without -Safe");
        assert_eq!(cell("VICTORY_F"), "the value of `-Display`");
        assert!(!got.contains_key("VICTORY_C") && !got.contains_key("VICTORY_Z"));
    }

    #[test]
    fn the_output_is_deterministic_and_well_formed() {
        let a = ("crates/b/src/lib.rs".to_owned(), "// Does b.\nfn f() { std::env::var(\"VICTORY_B\"); }\n".to_owned());
        let c = ("apps/c/src/main.rs".to_owned(), "fn f() { crate::toggled!(\"VICTORY_A\"); std::env::var(\"VICTORY_B\"); }\n".to_owned());
        let ps1 = "function Get-RunSettings {\n    $s = @{ VICTORY_A = '1' }\n}\n";
        let one = generate(&[a.clone(), c.clone()], Some(ps1));
        let two = generate(&[c, a], Some(ps1));
        assert_eq!(one, two);
        assert!(one.ends_with('\n') && !one.contains('\r') && !one.ends_with("\n\n"));
        assert!(one.contains("2 switches"), "{one}");
        assert!(one.contains("| `VICTORY_A` | on unless `0` | `1` |  | `apps/c/src/main.rs` |"), "{one}");
        assert!(one.contains("| `VICTORY_B` | value |  | Does b. | `apps/c/src/main.rs`<br>`crates/b/src/lib.rs` |"), "{one}");
        // No line numbers in the table.
        assert!(!one.contains("main.rs:"));
    }

    #[test]
    fn a_description_that_trips_the_policy_is_left_out_and_counted() {
        let path = ["C:", "Users", "someone", "game"].join("\\");
        let src = format!("// Reads a file such as {path} here.\nfn f() {{ std::env::var(\"VICTORY_PROFILE\"); }}\n");
        let out = generate(&[("crates/x/src/lib.rs".to_owned(), src)], None);
        assert!(out.contains("| `VICTORY_PROFILE` | value |  |  | `crates/x/src/lib.rs` |"), "{out}");
        assert!(out.contains("1 descriptions left out"), "{out}");
        assert!(!out.contains("someone"));
    }
}
