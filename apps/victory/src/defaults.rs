//! The switches a run gets without being asked: one table, applied at start-up.
//!
//! Each `VICTORY_*` switch keeps its own default where its code reads it (off,
//! for most). The settings below are the ones runs of real titles on real
//! cards have confirmed, so every player gets them without a long `-More`
//! text. A variable already set in the environment always wins, so a person
//! (or the launcher, or `play.cmd -More`) can still turn any of them off or
//! change it.
//!
//! `VICTORY_DEFAULTS` chooses the table:
//! - unset or `1`: all of it;
//! - `safe`: only the long-standing base set, without the newer optimisations
//!   (what `play.cmd -Safe` asks for), plus the two settings that undo newer
//!   behaviour;
//! - `0`: none of it (each switch at its own default).

/// One entry: the variable's name after `VICTORY_` (written without the
/// prefix so that `cargo xtask switches` does not take the table for the
/// place a switch is read: it lists these values in its own column), its
/// value, and whether it belongs to the newer set that `safe` leaves out.
struct Default {
    name: &'static str,
    value: &'static str,
    newer: bool,
}

const fn base(name: &'static str, value: &'static str) -> Default {
    Default { name, value, newer: false }
}

const fn newer(name: &'static str, value: &'static str) -> Default {
    Default { name, value, newer: true }
}

/// The table. Keep it sorted by name within each group; a change here is a
/// change of what every player runs, so it needs a run of a title behind it.
const TABLE: &[Default] = &[
    // The long-standing base set (play.cmd has given every run these).
    base("AMBIENT", "1"),
    base("CARD_REHASH_RUNS", "1"),
    base("CARD_STEPS", "2"),
    base("D3D12_DRED", "light"),
    base("INDIRECT_BATCH", "2"),
    base("INDIRECT_WINDOW", "1"),
    base("JOIN_COPIES", "1"),
    base("MAP_THREADS", "1"),
    // What `victory warm` makes ahead of a run is what earlier runs wrote down.
    base("PIPELINE_BOOK", "1"),
    base("RELAY", "2"),
    base("RELAY_COPY", "1"),
    base("SHOW_AFTER", "1"),
    base("TEXTURE_NO_WAIT", "1"),
    base("WIDE_BACK", "1"),
    // Newer: submission threads and texture uploads (play.cmd without -Safe).
    newer("JOIN_PARTS", "8"),
    newer("THIN", "1"),
    newer("UPLOAD_AHEAD", "1"),
    // Newer: the low-memory preset (play.cmd -Fast) for every run.
    newer("CARD_AWAKE", "0"),
    newer("MWAITX_WAIT", "1"),
    // Newer: descriptor-heap and memory sync, confirmed on a title since run 2.
    newer("HEAP_SYNC_AT_SUBMIT", "1"),
    newer("HEAP_SYNC_FIRST", "1"),
    newer("OWNERLESS_VIEWS", "1048576"),
    newer("SPANS_QUICK", "1"),
    newer("SYNC_FINE", "1"),
    newer("SYNC_IN_PLACE", "1"),
    newer("WC_NO_BACK", "1"),
    // Newer: texture order, confirmed on a title since run 4.
    newer("PRELUDE_UPLOADS", "1"),
    newer("SHOW_NEW_LATER", "1"),
    newer("TABLED_REREAD", "1"),
    // Newer: small title buffers on the card, and the ambient stand-in's
    // probes filtered, confirmed on a title since run 9.
    newer("AMBIENT_CLEAN", "1"),
    newer("CARD_BUFFERS_MIN_KB", "64"),
];

/// What `safe` adds: the settings that undo newer behaviour.
const SAFE_EXTRA: &[(&str, &str)] = &[("LAND_ECHO", "0"), ("UNDER_WIDE", "0")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    All,
    Safe,
    None,
}

fn choice(value: Option<&str>) -> Choice {
    match value.map(str::trim) {
        Some("0") => Choice::None,
        Some(v) if v.eq_ignore_ascii_case("safe") => Choice::Safe,
        _ => Choice::All,
    }
}

/// The pairs a choice gives, in table order, with their full names.
fn pairs(choice: Choice) -> Vec<(String, &'static str)> {
    let full = |(name, value): (&str, &'static str)| (format!("VICTORY_{name}"), value);
    match choice {
        Choice::None => Vec::new(),
        Choice::All => TABLE.iter().map(|d| full((d.name, d.value))).collect(),
        Choice::Safe => TABLE
            .iter()
            .filter(|d| !d.newer)
            .map(|d| (d.name, d.value))
            .chain(SAFE_EXTRA.iter().copied())
            .map(full)
            .collect(),
    }
}

/// Sets every default whose variable is not set yet. Returns how many were set.
///
/// Must run on the main thread before any other thread exists and before any
/// switch is read (they are read once and kept).
pub fn apply() -> usize {
    // `VICTORY_DEFAULTS`: which of victory.exe's built-in defaults a run gets: unset or `1` all of them, `safe` only
    // the long-standing base set (what `play.cmd -Safe` asks for), `0` none (each switch at its own default). A
    // variable already set always wins over the table.
    let choice = choice(std::env::var("VICTORY_DEFAULTS").ok().as_deref());
    let mut set = 0;
    for (name, value) in pairs(choice) {
        if std::env::var_os(&name).is_none() {
            // SAFETY: called on the main thread during start-up, before worker or guest threads exist.
            unsafe { std::env::set_var(&name, value) };
            set += 1;
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_read_as_documented() {
        assert_eq!(choice(None), Choice::All);
        assert_eq!(choice(Some("1")), Choice::All);
        assert_eq!(choice(Some(" 0 ")), Choice::None);
        assert_eq!(choice(Some("Safe")), Choice::Safe);
    }

    #[test]
    fn names_are_unique_and_switch_names() {
        let mut names: Vec<&str> = TABLE.iter().map(|d| d.name).chain(SAFE_EXTRA.iter().map(|p| p.0)).collect();
        assert!(names.iter().all(|n| !n.starts_with("VICTORY_") && !n.is_empty() && n.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')));
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "a switch is named twice");
    }

    #[test]
    fn safe_leaves_out_the_newer_set() {
        let safe = pairs(Choice::Safe);
        assert!(safe.contains(&("VICTORY_PIPELINE_BOOK".to_owned(), "1")));
        assert!(safe.contains(&("VICTORY_LAND_ECHO".to_owned(), "0")));
        assert!(!safe.iter().any(|(n, _)| n == "VICTORY_THIN" || n == "VICTORY_SYNC_FINE"));
        assert!(pairs(Choice::None).is_empty());
        assert_eq!(pairs(Choice::All).len(), TABLE.len());
    }
}
