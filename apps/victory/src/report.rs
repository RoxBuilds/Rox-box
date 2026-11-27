//! `victory report`: the newest run report, the list of them, or the newest
//! one's path; and the start of a run's report, for `run --execute`.
//!
//! Every `victory run --execute` writes a run report to `<data_dir>\reports`
//! (`victory_core::report`, docs/run-report.md). It holds numbers and
//! Rox-box's own words only, so a player can attach it to a public issue as
//! it is: the one thing about a run that may be shared.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use toml::{Table, Value};
use victory_core::report;

/// `victory report [--list | --path]`.
pub fn run(list: bool, path_only: bool) -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let dir = cfg.data_dir.join(report::FOLDER);
    let reports = report::reports_in(&dir);
    let none = || {
        format!(
            "no run reports in {} yet: every `victory run --execute` writes one, and \
             `victory selftest` one of its own",
            dir.display()
        )
    };
    if list {
        if reports.is_empty() {
            println!("{}", none());
        }
        for path in &reports {
            let said = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| report::summary(&text));
            println!(
                "{}  {}",
                path.display(),
                said.unwrap_or_else(|| "(not readable)".to_owned())
            );
        }
        return Ok(());
    }
    let newest = reports.first().with_context(none)?;
    if path_only {
        println!("{}", newest.display());
        return Ok(());
    }
    let text = std::fs::read_to_string(newest)
        .with_context(|| format!("the run report {} cannot be read", newest.display()))?;
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
    // The path names the player's own folders: on stderr, so that what is
    // piped or copied from stdout is the report alone.
    eprintln!(
        "(the report above is {}; this line is not part of it)",
        newest.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `victory report --diff`: two reports side by side.
//
// Everything below the entry point is pure (two parsed tables in, lines out)
// so it is tested with small synthetic reports. Only keys and values that are
// in the two reports are printed, never their paths: those go to stderr.

/// The smallest absolute change worth a row. The report writes two decimals,
/// so anything under half a hundredth is rounding, not a difference.
const EPSILON: f64 = 0.005;

/// The default relative change, in percent, under which a key is left out.
pub const DEFAULT_THRESHOLD: f64 = 5.0;

/// Sections in the order the report writes them. The TOML table does not
/// promise the file's order, so the comparison states it; a section this list
/// does not know (a later format) follows, in name order.
const SECTIONS: &[&str] = &[
    "",
    "rox_box",
    "host",
    "host.gpu",
    "run",
    "run.switches",
    "frames",
    "frames.histogram",
    "timeline",
    "gpu_work",
    "gpu_work.left_out",
    "frame_time",
    "frame_time.steady",
    "frame_time.waits",
    "frame_time.waits.steady",
    "pipelines",
    "shaders",
    "shaders.failures",
    "memory",
    "memory.pressure",
    "textures",
    "unimplemented",
    "selftest",
    "selftest.switches",
    "selftest.rates",
    "selftest.rate_units",
];

/// What `--diff` was asked to print.
#[derive(Clone, Copy, Debug)]
pub struct DiffOptions {
    /// Relative change in percent a key must exceed to be printed.
    pub threshold_pct: f64,
    /// Print every key, changed or not.
    pub all: bool,
}

/// `victory report --diff [<a> <b>]`.
pub fn diff(args: &[String], threshold_pct: f64, all: bool) -> Result<()> {
    if !(threshold_pct.is_finite() && threshold_pct >= 0.0) {
        bail!("--threshold must be a percentage of 0 or more");
    }
    // The reports folder is looked up only when it is needed: two paths need
    // no config file at all.
    let mut kept: Option<Vec<PathBuf>> = None;
    let mut newest_first = || -> Result<Vec<PathBuf>> {
        if kept.is_none() {
            let cfg = victory_core::Config::load()?;
            kept = Some(report::reports_in(&cfg.data_dir.join(report::FOLDER)));
        }
        Ok(kept.clone().unwrap_or_default())
    };
    let ((a_path, a_what), (b_path, b_what)) = match args {
        [] => {
            // The two newest of the newest one's kind: a self-test and a run
            // do not compare.
            let all = newest_first()?;
            let kind = |p: &PathBuf| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(report::Kind::of_file)
            };
            let newest = all.first().and_then(kind);
            let list: Vec<PathBuf> = all.iter().filter(|p| kind(p) == newest).cloned().collect();
            if list.len() < 2 {
                bail!(
                    "`victory report --diff` needs two {} reports and {} found: every `victory run \
                     --execute` writes a run report, and `victory selftest` a self-test report",
                    if newest == Some(report::Kind::Selftest) { "self-test" } else { "run" },
                    if list.is_empty() { "none was" } else { "only one was" }
                );
            }
            (
                (list[1].clone(), "A (the second newest)".to_owned()),
                (list[0].clone(), "B (the newest)".to_owned()),
            )
        }
        [a, b] => {
            let mut one = |n: usize, arg: &str| -> Result<(PathBuf, String)> {
                let what = format!("argument {n}");
                if !arg.is_empty() && arg.bytes().all(|c| c.is_ascii_digit()) {
                    let list = newest_first()?;
                    let index: usize = arg.parse().unwrap_or(0);
                    let path = index
                        .checked_sub(1)
                        .and_then(|i| list.get(i))
                        .with_context(|| {
                            format!(
                                "{what} is {arg}, but `victory report --list` has {} report(s) \
                                 (1 is the newest)",
                                list.len()
                            )
                        })?;
                    Ok((path.clone(), what))
                } else {
                    Ok((PathBuf::from(arg), what))
                }
            };
            (one(1, a)?, one(2, b)?)
        }
        _ => bail!("`--diff` takes two reports (older, then newer), or none for the two newest"),
    };
    let a = read_report(&a_what, &a_path)?;
    let b = read_report(&b_what, &b_path)?;
    // The paths name the player's own folders: stderr only.
    eprintln!(
        "(A is {}, B is {}; this line is not part of the comparison)",
        a_path.display(),
        b_path.display()
    );
    let options = DiffOptions { threshold_pct, all };
    for line in diff_lines(&a, &b, &options) {
        println!("{line}");
    }
    Ok(())
}

fn read_report(what: &str, path: &std::path::Path) -> Result<Table> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("report {what} ({}) cannot be read", path.display()))?;
    parse_report(&text).map_err(|e| anyhow::anyhow!("report {what} ({}) {e}", path.display()))
}

/// A report's text as a table; the error says what is wrong with it.
fn parse_report(text: &str) -> std::result::Result<Table, String> {
    let table: Table = toml::from_str(text).map_err(|e| format!("is not valid TOML: {e}"))?;
    if !table.contains_key("format") && !table.contains_key("run") {
        return Err("is TOML, but not a run report (no `format` and no `[run]`)".to_owned());
    }
    Ok(table)
}

/// What a report is of: `selftest` for the self-test's, `run` for a run's
/// (a report before format 3 has no `kind`, and is a run's).
fn kind(t: &Table) -> &str {
    t.get("kind").and_then(Value::as_str).unwrap_or("run")
}

fn is_selftest(t: &Table) -> bool {
    kind(t) == "selftest"
}

/// A number from a report, remembering whether it was written as an integer
/// (counts print without decimals).
#[derive(Clone, Copy, Debug)]
struct Num {
    v: f64,
    int: bool,
}

impl Num {
    fn of(value: &Value) -> Option<Num> {
        match value {
            Value::Integer(i) => Some(Num {
                v: *i as f64,
                int: true,
            }),
            Value::Float(f) => Some(Num { v: *f, int: false }),
            _ => None,
        }
    }

    fn show(self) -> String {
        if self.int {
            format!("{}", self.v as i64)
        } else {
            format!("{:.2}", self.v)
        }
    }
}

/// One numeric key of one report: its section ("frame_time.steady"), its key.
type Leaf = (String, String, Num);

/// Every numeric key of `table`, in the table's order. Arrays of numbers
/// become four keys (`[len]`, `[mean]`, `[min]`, `[max]`); arrays of tables
/// (the unimplemented methods and imports) become one key per entry, named
/// by the entry's own words and valued by its `calls`.
fn leaves(table: &Table, path: &mut Vec<String>, out: &mut Vec<Leaf>) {
    let section = path.join(".");
    for (key, value) in table {
        match value {
            Value::Table(inner) => {
                path.push(key.clone());
                leaves(inner, path, out);
                path.pop();
            }
            Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_table) => {
                for (i, item) in items.iter().filter_map(Value::as_table).enumerate() {
                    let Some(calls) = item.get("calls").and_then(Num::of) else {
                        continue;
                    };
                    let word = |k: &str| item.get(k).and_then(Value::as_str).map(str::to_owned);
                    let label = match (word("name"), word("interface"), item.get("offset")) {
                        (Some(name), ..) => name,
                        (None, Some(interface), Some(Value::Integer(off))) => {
                            format!("{interface} {off:#x}")
                        }
                        _ => format!("#{}", i + 1),
                    };
                    out.push((section.clone(), format!("{key} {label}"), calls));
                }
            }
            Value::Array(items) => {
                let nums: Vec<f64> = items.iter().filter_map(Num::of).map(|n| n.v).collect();
                if nums.len() != items.len() {
                    continue;
                }
                let int = |v: f64| Num { v, int: true };
                let fl = |v: f64| Num { v, int: false };
                out.push((section.clone(), format!("{key}[len]"), int(nums.len() as f64)));
                if !nums.is_empty() {
                    let all_int = items.iter().all(Value::is_integer);
                    let mean = nums.iter().sum::<f64>() / nums.len() as f64;
                    let min = nums.iter().copied().fold(f64::INFINITY, f64::min);
                    let max = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                    let ends = |v| if all_int { int(v) } else { fl(v) };
                    out.push((section.clone(), format!("{key}[mean]"), fl(mean)));
                    out.push((section.clone(), format!("{key}[min]"), ends(min)));
                    out.push((section.clone(), format!("{key}[max]"), ends(max)));
                }
            }
            other => {
                if let Some(n) = Num::of(other) {
                    out.push((section.clone(), key.clone(), n));
                }
            }
        }
    }
}

/// Sections about the PC or the build, which the header reports; they are
/// not figures of the run.
fn is_fact_section(section: &str) -> bool {
    matches!(section, "" | "rox_box" | "host" | "host.gpu")
}

fn is_steady(section: &str, key: &str) -> bool {
    section.ends_with(".steady") || key.ends_with("_steady")
}

fn section_rank(section: &str) -> (usize, String) {
    let rank = SECTIONS
        .iter()
        .position(|s| *s == section)
        .unwrap_or(SECTIONS.len());
    (rank, section.to_owned())
}

type Row = (String, String, Option<Num>, Option<Num>);

/// The numeric keys of both reports, joined: A's keys in A's order, then the
/// keys only B has; stably sorted by section.
fn joined(a: &Table, b: &Table) -> Vec<Row> {
    let (mut la, mut lb) = (Vec::new(), Vec::new());
    leaves(a, &mut Vec::new(), &mut la);
    leaves(b, &mut Vec::new(), &mut lb);
    let mut rows: Vec<Row> = Vec::new();
    let mut at: HashMap<(String, String), usize> = HashMap::new();
    for (side, list) in [(0, la), (1, lb)] {
        for (section, key, n) in list {
            if is_fact_section(&section) {
                continue;
            }
            let i = *at.entry((section.clone(), key.clone())).or_insert_with(|| {
                rows.push((section, key, None, None));
                rows.len() - 1
            });
            // Entries that share a name (two imports of one name) add up.
            let slot = if side == 0 { &mut rows[i].2 } else { &mut rows[i].3 };
            *slot = Some(match *slot {
                Some(old) => Num {
                    v: old.v + n.v,
                    int: old.int && n.int,
                },
                None => n,
            });
        }
    }
    rows.sort_by_cached_key(|(section, ..)| section_rank(section));
    rows
}

fn changed(a: Num, b: Num, threshold_pct: f64) -> bool {
    let d = (b.v - a.v).abs();
    d > EPSILON && (a.v == 0.0 || d / a.v.abs() * 100.0 > threshold_pct)
}

fn delta_text(a: Num, b: Num) -> String {
    let d = b.v - a.v;
    if a.int && b.int {
        if d == 0.0 {
            "0".to_owned()
        } else {
            format!("{:+}", d as i64)
        }
    } else {
        format!("{:+.2}", if d.abs() < EPSILON { 0.0 } else { d })
    }
}

fn percent_text(a: Num, b: Num) -> String {
    if a.v == 0.0 {
        return if b.v == 0.0 { "0.0 %".to_owned() } else { "-".to_owned() };
    }
    let p = (b.v - a.v) / a.v.abs() * 100.0;
    format!("{:+.1} %", if p.abs() < 0.05 { 0.0 } else { p })
}

/// One block of the comparison (the steady state, or the whole run): a line
/// per section, a row per key, columns aligned.
fn table_lines(heading: &str, rows: &[&Row], options: &DiffOptions) -> Vec<String> {
    let mut shown: Vec<(&str, [String; 5])> = Vec::new();
    for (section, key, a, b) in rows {
        let cells = match (a, b) {
            (Some(a), Some(b)) => {
                if !options.all && !changed(*a, *b, options.threshold_pct) {
                    continue;
                }
                [
                    key.clone(),
                    a.show(),
                    b.show(),
                    delta_text(*a, *b),
                    percent_text(*a, *b),
                ]
            }
            // Present on one side only: always shown, so that a missing key
            // (another format, a section that did not exist) is visible.
            (a, b) => [
                key.clone(),
                a.map_or("-".to_owned(), Num::show),
                b.map_or("-".to_owned(), Num::show),
                "-".to_owned(),
                "-".to_owned(),
            ],
        };
        shown.push((section.as_str(), cells));
    }
    let mut lines = vec![format!(
        "{heading}: {} of {} keys shown",
        shown.len(),
        rows.len()
    )];
    if shown.is_empty() {
        return lines;
    }
    let head = ["key", "A", "B", "delta", "percent"].map(str::to_owned);
    let mut width = [0usize; 5];
    for cells in shown.iter().map(|(_, c)| c).chain([&head]) {
        for (w, c) in width.iter_mut().zip(cells) {
            *w = (*w).max(c.chars().count());
        }
    }
    let pad = |cells: &[String; 5]| {
        let mut line = format!("  {:<w$}", cells[0], w = width[0]);
        for i in 1..5 {
            line.push_str(&format!("  {:>w$}", cells[i], w = width[i]));
        }
        line.trim_end().to_owned()
    };
    lines.push(pad(&head));
    let mut last = None;
    for (section, cells) in &shown {
        if last != Some(*section) {
            lines.push(if section.is_empty() {
                "[top level]".to_owned()
            } else {
                format!("[{section}]")
            });
            last = Some(*section);
        }
        lines.push(pad(cells));
    }
    lines
}

fn get<'a>(t: &'a Table, path: &[&str]) -> Option<&'a Value> {
    let (last, parents) = path.split_last()?;
    let mut table = t;
    for p in parents {
        table = table.get(*p)?.as_table()?;
    }
    table.get(*last)
}

fn number(t: &Table, path: &[&str]) -> Option<f64> {
    get(t, path).and_then(Num::of).map(|n| n.v)
}

/// A scalar the way the report writes it, words without their quotes.
fn plain(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Float(f)) => format!("{f:.1}"),
        Some(other) => other.to_string(),
        None => "-".to_owned(),
    }
}

fn run_line(label: &str, t: &Table) -> String {
    if is_selftest(t) {
        let n = |key: &str| plain(get(t, &["selftest", key]));
        let warp = get(t, &["selftest", "software_adapter"]).and_then(Value::as_bool) == Some(true);
        return format!(
            "{label}: Rox-box {}, self-test{}, ended {}, {} ok, {} failed, {} skipped",
            plain(get(t, &["rox_box", "version"])),
            if warp { " on the software adapter" } else { "" },
            n("ended"),
            n("stages_ok"),
            n("stages_failed"),
            n("stages_skipped"),
        );
    }
    let mut line = format!(
        "{label}: Rox-box {}, console {}, display {}, {} s, ended {}",
        plain(get(t, &["rox_box", "version"])),
        plain(get(t, &["run", "console"])),
        plain(get(t, &["run", "display_mode"])),
        plain(get(t, &["run", "duration_s"])),
        plain(get(t, &["run", "ended"])),
    );
    if let Some(code) = get(t, &["run", "exit_code"]) {
        line.push_str(&format!(" (exit code {})", plain(Some(code))));
    }
    line
}

/// The scalar keys of `[host]` and `[host.gpu]` (the latter as `gpu.<key>`)
/// that say which PC it is. `free_commit_gib_at_start` is left out: it is a
/// reading of the moment, other in every run of one PC, and two runs of the
/// same PC would be called different by it.
fn host_facts(t: &Table) -> Vec<(String, Value)> {
    let mut facts = Vec::new();
    if let Some(host) = get(t, &["host"]).and_then(Value::as_table) {
        for (k, v) in host {
            match v {
                _ if k == "free_commit_gib_at_start" => {}
                Value::Table(gpu) if k == "gpu" => {
                    facts.extend(gpu.iter().map(|(k, v)| (format!("gpu.{k}"), v.clone())));
                }
                Value::Table(_) => {}
                v => facts.push((k.clone(), v.clone())),
            }
        }
    }
    facts
}

fn host_lines(a: &Table, b: &Table) -> Vec<String> {
    let (fa, fb) = (host_facts(a), host_facts(b));
    if fa.is_empty() && fb.is_empty() {
        return vec!["host: in neither report".to_owned()];
    }
    let find = |list: &[(String, Value)], k: &str| {
        list.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    };
    let mut keys: Vec<&str> = fa.iter().map(|(k, _)| k.as_str()).collect();
    keys.extend(
        fb.iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| !fa.iter().any(|(n, _)| n == k)),
    );
    let differing: Vec<String> = keys
        .into_iter()
        .filter_map(|k| {
            let (x, y) = (find(&fa, k), find(&fb, k));
            (x != y).then(|| format!("  {k}  {} -> {}", plain(x.as_ref()), plain(y.as_ref())))
        })
        .collect();
    if differing.is_empty() {
        return vec!["host: same ([host] and [host.gpu])".to_owned()];
    }
    let mut lines = vec![format!(
        "host: DIFFERENT in {} key(s); the two runs were not on the same PC or setup",
        differing.len()
    )];
    lines.extend(differing);
    lines
}

fn switch_lines(a: &Table, b: &Table) -> Vec<String> {
    let switches = |t: &Table| {
        get(t, &["run", "switches"])
            .or_else(|| get(t, &["selftest", "switches"]))
            .and_then(Value::as_table)
            .cloned()
            .unwrap_or_default()
    };
    let (sa, sb) = (switches(a), switches(b));
    let show = |v: &Value| plain(Some(v));
    let (mut only_a, mut only_b, mut differ) = (Vec::new(), Vec::new(), Vec::new());
    for (k, v) in &sa {
        match sb.get(k) {
            None => only_a.push(format!("  only in A: {k} = {}", show(v))),
            Some(w) if w != v => differ.push(format!("  differs: {k}  {} -> {}", show(v), show(w))),
            _ => {}
        }
    }
    for (k, v) in &sb {
        if !sa.contains_key(k) {
            only_b.push(format!("  only in B: {k} = {}", show(v)));
        }
    }
    let n = only_a.len() + only_b.len() + differ.len();
    if n == 0 {
        return vec![format!("switches: same ({} set in each)", sa.len())];
    }
    let mut lines = vec![format!("switches: {n} difference(s)")];
    lines.extend(only_a);
    lines.extend(only_b);
    lines.extend(differ);
    lines
}

/// Two self-tests' stages: those whose outcome differs, by name.
fn stage_lines(a: &Table, b: &Table) -> Vec<String> {
    let stages = |t: &Table| -> Vec<(String, String)> {
        get(t, &["selftest", "stage"])
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_table)
                    .map(|s| (plain(s.get("name")), plain(s.get("outcome"))))
                    .collect()
            })
            .unwrap_or_default()
    };
    let (sa, sb) = (stages(a), stages(b));
    let mut names: Vec<&String> = sa.iter().map(|(n, _)| n).collect();
    names.extend(sb.iter().map(|(n, _)| n).filter(|n| !sa.iter().any(|(m, _)| m == *n)));
    let find = |list: &[(String, String)], name: &str| {
        list.iter().find(|(n, _)| n == name).map_or("-".to_owned(), |(_, o)| o.clone())
    };
    let differing: Vec<String> = names
        .into_iter()
        .filter_map(|n| {
            let (x, y) = (find(&sa, n), find(&sb, n));
            (x != y).then(|| format!("  {n}  {x} -> {y}"))
        })
        .collect();
    if differing.is_empty() {
        return vec![format!("stages: same outcomes ({} in each)", sa.len())];
    }
    let mut lines = vec![format!("stages: {} with another outcome", differing.len())];
    lines.extend(differing);
    lines
}

/// The last line of two self-tests: the rates both have, A to B.
fn selftest_verdict(a: &Table, b: &Table) -> String {
    let rates = |t: &Table| {
        get(t, &["selftest", "rates"])
            .and_then(Value::as_table)
            .cloned()
            .unwrap_or_default()
    };
    let (ra, rb) = (rates(a), rates(b));
    let units = get(b, &["selftest", "rate_units"]).and_then(Value::as_table);
    let parts: Vec<String> = ra
        .iter()
        .filter_map(|(name, x)| {
            let (x, y) = (Num::of(x)?.v, Num::of(rb.get(name)?)?.v);
            let unit = units
                .and_then(|u| u.get(name))
                .map(|u| format!(" {}", plain(Some(u))))
                .unwrap_or_default();
            let pct = if x == 0.0 {
                String::new()
            } else {
                format!(" ({:+.1} %)", (y - x) / x * 100.0)
            };
            Some(format!("{name} {x:.2} -> {y:.2}{unit}{pct}"))
        })
        .collect();
    if parts.is_empty() {
        "verdict: the two self-tests share no rate (`--bench` takes them)".to_owned()
    } else {
        format!("verdict: {}", parts.join(", "))
    }
}

/// The last line: facts only, from whichever of the keys both reports have.
fn verdict(a: &Table, b: &Table) -> String {
    let both = |path: &[&str]| number(a, path).zip(number(b, path));
    let mut parts = Vec::new();
    let frame = both(&["frame_time", "steady", "frame_ms"])
        .map(|p| ("steady-state frame time", p))
        .or_else(|| both(&["frame_time", "frame_ms"]).map(|p| ("frame time", p)))
        .or_else(|| both(&["frames", "frame_ms_mean"]).map(|p| ("mean frame time", p)));
    if let Some((what, (x, y))) = frame {
        let pct = if x == 0.0 {
            String::new()
        } else {
            format!(" ({:+.1} %)", (y - x) / x * 100.0)
        };
        parts.push(format!("{what} {x:.1} -> {y:.1} ms{pct}"));
    }
    if let Some((x, y)) = both(&["frames", "average_fps"]) {
        parts.push(format!("average {x:.1} -> {y:.1} frames per second"));
    }
    let short = |v: f64| {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    };
    let left = both(&["gpu_work", "left_out_avg_steady"])
        .map(|p| ("left out per frame (steady state)", p))
        .or_else(|| both(&["gpu_work", "left_out_avg"]).map(|p| ("left out per frame", p)));
    if let Some((what, (x, y))) = left {
        parts.push(format!("{what} {} -> {}", short(x), short(y)));
    }
    if parts.is_empty() {
        "verdict: the two reports share no frame time, frame rate or left-out figure".to_owned()
    } else {
        format!("verdict: {}", parts.join(", "))
    }
}

/// The whole comparison of report `a` (older) with report `b` (newer).
pub fn diff_lines(a: &Table, b: &Table, options: &DiffOptions) -> Vec<String> {
    let format = |t: &Table| number(t, &["format"]).map(|f| f as i64);
    let mut lines = vec![match (format(a), format(b)) {
        (Some(x), Some(y)) if x == y => format!("format {x} in both reports"),
        (x, y) => {
            let text = |f: Option<i64>| f.map_or("none".to_owned(), |f| f.to_string());
            format!(
                "formats differ: A is format {}, B is format {} (a key one lacks shows as -)",
                text(x),
                text(y)
            )
        }
    }];
    if kind(a) != kind(b) {
        // Their figures are of different things: only the PC compares.
        lines.push(format!(
            "kinds differ: A is a {} report, B a {} report; only the build and the PC compare",
            if is_selftest(a) { "self-test" } else { "run" },
            if is_selftest(b) { "self-test" } else { "run" },
        ));
        lines.push(run_line("A", a));
        lines.push(run_line("B", b));
        lines.extend(host_lines(a, b));
        lines.push("verdict: a self-test and a run do not compare".to_owned());
        return lines;
    }
    lines.push(run_line("A", a));
    lines.push(run_line("B", b));
    lines.extend(host_lines(a, b));
    lines.extend(switch_lines(a, b));
    if is_selftest(a) {
        lines.extend(stage_lines(a, b));
    }
    let rows = joined(a, b);
    // A steady-state key is compared under its own heading only when both
    // reports have it; otherwise it is one more whole-run row, with its `-`.
    let in_steady = |r: &&Row| is_steady(&r.0, &r.1) && r.2.is_some() && r.3.is_some();
    let steady: Vec<&Row> = rows.iter().filter(in_steady).collect();
    let whole: Vec<&Row> = rows.iter().filter(|r| !in_steady(r)).collect();
    lines.push(format!(
        "numbers: shown when the change is over {} % and over {EPSILON}{}",
        options.threshold_pct,
        if options.all { " (--all: every key)" } else { "" }
    ));
    if !steady.is_empty() {
        lines.extend(table_lines("steady state", &steady, options));
    }
    lines.extend(table_lines(if is_selftest(a) { "self-test" } else { "whole run" }, &whole, options));
    lines.push(if is_selftest(a) { selftest_verdict(a, b) } else { verdict(a, b) });
    lines
}

/// Starts this run's report: the host's facts that `victory check` asks for
/// too, the console model, how DLLs are told to be the platform's, the GPU
/// side's counters; then `report::begin`. Nothing if `VICTORY_REPORT=0`.
#[cfg(windows)]
pub fn start(cfg: &victory_core::Config) {
    if !report::enabled() {
        return;
    }
    let dir = match cfg.data_subdir(report::FOLDER) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("warning: no run report for this run: {e:#}");
            return;
        }
    };
    report::console(report::Ours::new(
        victory_hle::console::current().cli_name(),
    ));
    gather_host_facts();
    report::set_platform_dll(platform_dll);
    report::add_sampler(victory_gpu::report_sample);
    let _ = report::begin(dir);
}

/// The host's facts that `victory check` asks for too: Windows, the
/// processor, memory. (The graphics adapter's come from the GPU side when
/// it makes its device.)
#[cfg(windows)]
fn gather_host_facts() {
    if let Some((major, minor, build)) = victory_gpu::preflight::windows_version() {
        report::host_windows(major, minor, build);
    }
    let logical = std::thread::available_parallelism().map_or(0, |n| n.get() as u32);
    report::host_cpu(report::HostText::new(&cpu_brand()), logical);
    if let (Some(installed), Some((free, _))) = (
        victory_gpu::preflight::physical_memory_mb(),
        victory_gpu::preflight::commit_memory_mb(),
    ) {
        report::host_memory(installed, free);
    }
}

/// Before the self-test: its report will want the host's facts, the
/// adapter's included (`VICTORY_REPORT=0`: none).
#[cfg(windows)]
pub fn selftest_begins() {
    if report::enabled() {
        report::keep_host_facts();
        gather_host_facts();
    }
}

/// The self-test's report, written when it ends: in `<data_dir>\reports`
/// (with no config file the default data folder's, as a run's), its path
/// said on stderr; a warning there if it cannot be written. Nothing if
/// `VICTORY_REPORT=0`. `sample`: the GPU side's counters are read first,
/// which a hang or a failure of the GPU skips (its threads may hold what
/// they need).
#[cfg(windows)]
pub fn selftest_ends(test: &report::Selftest, sample: bool) {
    if !report::enabled() {
        return;
    }
    let dir = victory_core::Config::load().and_then(|cfg| cfg.data_subdir(report::FOLDER));
    let dir = match dir {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("warning: no self-test report: {e:#}");
            return;
        }
    };
    // The view heap's size and the like, as before a run's report.
    if sample {
        victory_gpu::report_sample();
    }
    match report::write_selftest(&dir, test) {
        // The path names the player's folders: stderr, so that the
        // self-test's output on stdout can be pasted as it is.
        Some(path) => eprintln!(
            "self-test report {} (safe to share in an issue: numbers and Rox-box's own words only; \
             `victory report` shows it)",
            path.display()
        ),
        None => eprintln!("warning: the self-test report could not be written in {}", dir.display()),
    }
}

/// Whether `dll` is the platform's: one our runtime implements or the
/// default host policy passes through. Only imports of those are named in a
/// report; a DLL the title ships is only counted.
#[cfg(windows)]
fn platform_dll(dll: &str) -> bool {
    victory_hle::HostPolicy::default().allows(dll)
        || victory_hle::registry().iter().any(|m| m.serves(dll))
}

/// The processor's brand string (CPUID leaves 0x80000002 to 0x80000004),
/// spaces collapsed; empty if it has none.
#[allow(unused_unsafe)]
fn cpu_brand() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        // SAFETY: `cpuid` is there on every x86-64 processor; the brand
        // leaves are asked only where the highest extended leaf reaches them.
        let bytes: Vec<u8> = unsafe {
            if __cpuid(0x8000_0000).eax < 0x8000_0004 {
                return String::new();
            }
            (0x8000_0002u32..=0x8000_0004)
                .flat_map(|leaf| {
                    let r = __cpuid(leaf);
                    [r.eax, r.ebx, r.ecx, r.edx]
                })
                .flat_map(u32::to_le_bytes)
                .collect()
        };
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
    #[cfg(not(target_arch = "x86_64"))]
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brand_string_is_short_plain_text() {
        let brand = cpu_brand();
        assert!(brand.len() <= 48 && !brand.contains("  "), "{brand:?}");
        assert!(!brand.contains('\0'));
    }

    #[cfg(windows)]
    #[test]
    fn platform_dlls_are_ours_or_the_hosts() {
        assert!(platform_dll("kernel32.dll"));
        assert!(platform_dll("d3d12_xs.dll"));
        assert!(!platform_dll("TitleOwn.dll"));
    }

    // --- `--diff`: synthetic reports, synthetic numbers ---

    const OPTS: DiffOptions = DiffOptions {
        threshold_pct: DEFAULT_THRESHOLD,
        all: false,
    };

    fn table(text: &str) -> Table {
        parse_report(text).unwrap()
    }

    fn diff_text(a: &str, b: &str, options: &DiffOptions) -> String {
        diff_lines(&table(a), &table(b), options).join("\n")
    }

    const BASE: &str = r#"
format = 2
[rox_box]
version = "0.2.0"
[host]
windows_build = 26100
[host.gpu]
adapter = "Some Adapter"
driver = "1.0"
[run]
console = "series-x"
display_mode = "1920x1080"
duration_s = 60.0
ended = "title_exited"
exit_code = 0
[run.switches]
"VICTORY_A" = "1"
"VICTORY_SAME" = "<set>"
[frames]
presents = 1000
average_fps = 40.0
[frame_time]
frame_ms = 30.0
preparing_ms = 5.0
[frame_time.steady]
frame_ms = 25.0
preparing_ms = 4.0
[gpu_work]
left_out_avg = 10.0
left_out_avg_steady = 8.0
[gpu_work.left_out]
pipeline_not_ready = 100
other = 0
"#;

    #[test]
    fn a_changed_key_prints_both_values_the_delta_and_the_percent() {
        let b = BASE.replace("preparing_ms = 5.0", "preparing_ms = 2.5");
        let out = diff_text(BASE, &b, &OPTS);
        let row = out.lines().find(|l| l.trim_start().starts_with("preparing_ms")).unwrap();
        let cols: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(cols, ["preparing_ms", "5.00", "2.50", "-2.50", "-50.0", "%"]);
    }

    #[test]
    fn a_change_under_the_threshold_or_epsilon_is_left_out_unless_all() {
        // 3 % on frame_ms; 0.004 on a tiny figure that doubles.
        let a = BASE.replace("[frames]", "[frames]\ntiny = 0.004");
        let b = a
            .replace("tiny = 0.004", "tiny = 0.008")
            .replace("frame_ms = 30.0", "frame_ms = 30.9");
        let out = diff_text(&a, &b, &OPTS);
        assert!(!out.contains("tiny") && !out.contains("30.90"), "{out}");
        let all = diff_text(&a, &b, &DiffOptions { all: true, ..OPTS });
        assert!(all.contains("tiny") && all.contains("30.90"), "{all}");
        let loose = diff_text(&a, &b, &DiffOptions { threshold_pct: 1.0, ..OPTS });
        assert!(loose.contains("30.90"), "{loose}");
    }

    #[test]
    fn a_key_one_report_lacks_prints_a_dash() {
        let b = BASE.replace("other = 0", "other = 0\nno_pipeline_set = 4");
        let out = diff_text(BASE, &b, &OPTS);
        let row = out.lines().find(|l| l.contains("no_pipeline_set")).unwrap();
        assert_eq!(row.split_whitespace().collect::<Vec<_>>(), ["no_pipeline_set", "-", "4", "-", "-"]);
    }

    #[test]
    fn steady_state_comes_first_and_the_whole_run_after() {
        let b = BASE
            .replace("frame_ms = 25.0", "frame_ms = 12.5")
            .replace("frame_ms = 30.0", "frame_ms = 15.0");
        let out = diff_text(BASE, &b, &OPTS);
        let steady = out.find("steady state:").unwrap();
        let whole = out.find("whole run:").unwrap();
        let steady_row = out.find("[frame_time.steady]").unwrap();
        let whole_row = out.find("[frame_time]").unwrap();
        assert!(steady < steady_row && steady_row < whole && whole < whole_row, "{out}");
        assert!(out.contains("steady-state frame time 25.0 -> 12.5 ms (-50.0 %)"), "{out}");
    }

    #[test]
    fn steady_keys_one_report_lacks_fall_back_to_the_whole_run() {
        let a = BASE.replace("[frame_time.steady]\nframe_ms = 25.0\npreparing_ms = 4.0\n", "");
        let out = diff_text(&a, BASE, &OPTS);
        assert!(out.contains("steady state: 0 of 1 keys"), "{out}");
        assert!(out.contains("[frame_time.steady]"), "{out}");
        assert!(out.contains("verdict: frame time 30.0 -> 30.0 ms"), "{out}");
    }

    #[test]
    fn arrays_are_summarised_not_listed() {
        let a = format!("{BASE}\n[timeline]\npresents = [0, 100, 300]\nfps = [0.0, 10.0, 30.0]\n");
        let b = format!("{BASE}\n[timeline]\npresents = [0, 200, 400, 600]\nfps = [0.0, 10.0, 30.0]\n");
        let out = diff_text(&a, &b, &OPTS);
        let key = |k: &str| out.lines().any(|l| l.split_whitespace().next() == Some(k));
        assert!(key("presents[len]") && key("presents[mean]") && key("presents[max]"), "{out}");
        assert!(!key("fps[len]"), "an unchanged array is not printed: {out}");
        let all = diff_text(&a, &b, &DiffOptions { all: true, ..OPTS });
        let row = all.lines().find(|l| l.contains("presents[mean]")).unwrap();
        assert_eq!(row.split_whitespace().nth(1), Some("133.33"), "{row}");
    }

    #[test]
    fn count_tables_compare_key_by_key_with_keys_on_one_side() {
        let a = format!(
            "{BASE}\n[shaders.failures]\n\"unsupported: x\" = 3\n\"malformed\" = 1\n\
             [[unimplemented.method]]\ninterface = \"device\"\noffset = 0x218\ncalls = 36\n\
             [[unimplemented.import]]\nname = \"some.dll!Func\"\ncalls = 1\n"
        );
        let b = format!(
            "{BASE}\n[shaders.failures]\n\"unsupported: x\" = 9\n\"native_code\" = 2\n\
             [[unimplemented.method]]\ninterface = \"device\"\noffset = 0x218\ncalls = 40\n"
        );
        let out = diff_text(&a, &b, &OPTS);
        let cols = |k: &str| -> Vec<String> {
            let row = out.lines().find(|l| l.trim_start().starts_with(k)).unwrap();
            row.trim_start()[k.len()..].split_whitespace().map(str::to_owned).collect()
        };
        assert_eq!(cols("unsupported: x"), ["3", "9", "+6", "+200.0", "%"]);
        assert_eq!(cols("malformed"), ["1", "-", "-", "-"]);
        assert_eq!(cols("native_code"), ["-", "2", "-", "-"]);
        assert_eq!(cols("method device 0x218"), ["36", "40", "+4", "+11.1", "%"]);
        assert_eq!(cols("import some.dll!Func"), ["1", "-", "-", "-"]);
    }

    #[test]
    fn hosts_that_differ_are_listed_by_key_and_equal_ones_say_so() {
        let same = diff_text(BASE, BASE, &OPTS);
        assert!(same.contains("host: same"), "{same}");
        let b = BASE.replace("driver = \"1.0\"", "driver = \"2.0\"");
        let out = diff_text(BASE, &b, &OPTS);
        assert!(out.contains("host: DIFFERENT in 1 key(s)"), "{out}");
        assert!(out.contains("  gpu.driver  1.0 -> 2.0"), "{out}");
        assert!(!out.contains("gpu.adapter"), "{out}");
    }

    #[test]
    fn switches_only_in_one_or_with_other_values_are_listed() {
        let b = BASE
            .replace("\"VICTORY_A\" = \"1\"", "\"VICTORY_A\" = \"2\"\n\"VICTORY_B\" = \"7\"");
        let out = diff_text(BASE, &b, &OPTS);
        assert!(out.contains("switches: 2 difference(s)"), "{out}");
        assert!(out.contains("only in B: VICTORY_B = 7"), "{out}");
        assert!(out.contains("differs: VICTORY_A  1 -> 2"), "{out}");
        assert!(!out.contains("VICTORY_SAME"), "{out}");
    }

    #[test]
    fn different_formats_still_compare_and_the_first_line_says_so() {
        let old = BASE
            .replace("format = 2", "format = 1")
            .replace("[frame_time.steady]\nframe_ms = 25.0\npreparing_ms = 4.0\n", "");
        let out = diff_text(&old, BASE, &OPTS);
        assert!(out.lines().next().unwrap().starts_with("formats differ: A is format 1, B is format 2"), "{out}");
        let same = diff_text(BASE, BASE, &OPTS);
        assert_eq!(same.lines().next(), Some("format 2 in both reports"));
    }

    #[test]
    fn the_verdict_states_figures_only() {
        let b = BASE
            .replace("frame_ms = 25.0", "frame_ms = 12.5")
            .replace("left_out_avg_steady = 8.0", "left_out_avg_steady = 2.0");
        let out = diff_text(BASE, &b, &OPTS);
        let last = out.lines().last().unwrap();
        assert_eq!(
            last,
            "verdict: steady-state frame time 25.0 -> 12.5 ms (-50.0 %), \
             average 40.0 -> 40.0 frames per second, left out per frame (steady state) 8 -> 2"
        );
    }

    #[test]
    fn nothing_printed_is_a_path_or_a_file_name() {
        let out = diff_text(BASE, BASE, &DiffOptions { all: true, ..OPTS });
        assert!(!out.contains('\\') && !out.contains(".txt"), "{out}");
    }

    const SELFTEST: &str = r#"
format = 3
kind = "selftest"
[rox_box]
version = "0.2.0"
[host]
windows_build = 26100
[host.gpu]
adapter = "Some Adapter"
[selftest]
ended = "completed"
software_adapter = true
frames = 8
stages_ok = 5
stages_failed = 0
stages_skipped = 0
[selftest.switches]
"VICTORY_D3D12_WARP" = "1"
[selftest.rates]
"present_interval" = 2.00
"buffer_copy" = 1000.00
[selftest.rate_units]
"present_interval" = "ms"
"buffer_copy" = "MB per s"
[[selftest.stage]]
name = "Device"
outcome = "ok"
[[selftest.stage]]
name = "Translated draw"
outcome = "ok"
"#;

    #[test]
    fn two_self_tests_compare_their_stages_and_rates() {
        let b = SELFTEST
            .replace("\"present_interval\" = 2.00", "\"present_interval\" = 3.00")
            .replace("name = \"Translated draw\"\noutcome = \"ok\"", "name = \"Translated draw\"\noutcome = \"skip\"");
        let out = diff_text(SELFTEST, &b, &OPTS);
        assert!(out.contains("A: Rox-box 0.2.0, self-test on the software adapter, ended completed, 5 ok"), "{out}");
        assert!(out.contains("stages: 1 with another outcome\n  Translated draw  ok -> skip"), "{out}");
        assert!(out.contains("switches: same (1 set in each)"), "{out}");
        let row = out.lines().find(|l| l.trim_start().starts_with("present_interval")).unwrap();
        assert_eq!(row.split_whitespace().collect::<Vec<_>>(), ["present_interval", "2.00", "3.00", "+1.00", "+50.0", "%"]);
        let last = out.lines().last().unwrap();
        assert!(last.starts_with("verdict: "), "{out}");
        assert!(last.contains("present_interval 2.00 -> 3.00 ms (+50.0 %)"), "{last}");
        assert!(last.contains("buffer_copy 1000.00 -> 1000.00 MB per s (+0.0 %)"), "{last}");
        let same = diff_text(SELFTEST, SELFTEST, &OPTS);
        assert!(same.contains("stages: same outcomes (2 in each)"), "{same}");
    }

    #[test]
    fn a_self_test_and_a_run_compare_only_their_pc() {
        let out = diff_text(BASE, SELFTEST, &OPTS);
        assert!(out.contains("kinds differ: A is a run report, B a self-test report"), "{out}");
        assert!(out.contains("host: DIFFERENT") || out.contains("host: same"), "{out}");
        assert_eq!(out.lines().last(), Some("verdict: a self-test and a run do not compare"));
        assert!(!out.contains("whole run:"), "{out}");
    }

    #[test]
    fn malformed_input_is_an_error_that_says_what_is_wrong() {
        let e = parse_report("format = [").unwrap_err();
        assert!(e.starts_with("is not valid TOML"), "{e}");
        let e = parse_report("a = 1\n").unwrap_err();
        assert!(e.contains("not a run report"), "{e}");
    }
}
