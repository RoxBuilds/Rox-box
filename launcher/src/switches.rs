//! Saved switches: the `VICTORY_*` settings a person would otherwise paste
//! into `-More` for every run, kept as toggles in profiles.
//!
//! The launcher knows no switch by name. A switch is a name and a value the
//! person gives (or pastes, as the `NAME=value;NAME=value` text they already
//! use), so switches the engine gains or loses need no change here. Profiles
//! live in `local\switches.toml`, beside `local\launcher.toml`: a file of this
//! PC that is never committed, so what belongs to one title (a list of its
//! programs, a choice from its patch file) stays out of the repository.
//!
//! A profile can be tied to a title folder: choosing that title chooses it.

use std::path::Path;

use crate::config::{string_value, toml_path};

/// One setting of a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switch {
    pub name: String,
    pub value: String,
    /// Off: kept in the profile, not given to the run.
    pub on: bool,
    /// The person's own words about it.
    pub note: String,
}

/// A named set of switches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
    pub name: String,
    pub switches: Vec<Switch>,
}

/// Everything in `local\switches.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Book {
    pub profiles: Vec<Profile>,
    /// The profile runs are given (none: no saved switches).
    pub active: Option<String>,
    /// Title folders and the profile each chooses.
    pub by_title: Vec<(String, String)>,
}

/// Whether `name` can be an environment variable's name as the engine reads
/// them: capitals, digits and `_`, not starting with a digit.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Whether `value` can travel in `-More`: one line, and neither the `;` that
/// separates pairs nor a `"` (the whole text is one quoted argument).
pub fn valid_value(value: &str) -> bool {
    !value.contains([';', '"', '\n', '\r'])
}

/// The pairs of a pasted text: `NAME=value;NAME=value`, with or without the
/// `-More` before it and the quotes around it, as `play.ps1` reads it. Each
/// becomes a switch that is on. Says what is wrong with the first pair that
/// is not one.
pub fn import(text: &str) -> Result<Vec<Switch>, String> {
    let mut text = text.trim();
    if let Some(rest) = text.strip_prefix("-More").or_else(|| text.strip_prefix("-more")) {
        text = rest.trim();
    }
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            text = &text[1..text.len() - 1];
        }
    }
    let mut found: Vec<Switch> = Vec::new();
    for pair in text.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((name, value)) = pair.split_once('=') else { return Err(format!("'{pair}' is not NAME=value (pairs are separated by ';')")) };
        let (name, value) = (name.trim(), value.trim());
        if !valid_name(name) {
            return Err(format!("'{name}' is not a switch's name (capitals, digits and _)"));
        }
        if !valid_value(value) {
            return Err(format!("the value of {name} holds a quote"));
        }
        // A name given twice: the later value stands, as in the script's own table.
        found.retain(|s| s.name != name);
        found.push(Switch { name: name.to_string(), value: value.to_string(), on: true, note: String::new() });
    }
    if found.is_empty() { Err("no NAME=value pair in it".to_string()) } else { Ok(found) }
}

impl Profile {
    /// The switches that are on, as `NAME=value;NAME=value`.
    pub fn more(&self) -> String {
        self.switches.iter().filter(|s| s.on && valid_name(&s.name) && valid_value(&s.value)).map(|s| format!("{}={}", s.name, s.value)).collect::<Vec<_>>().join(";")
    }

    pub fn on_count(&self) -> usize {
        self.switches.iter().filter(|s| s.on).count()
    }

    /// Adds `switch`, or gives one of the same name its value and state.
    pub fn put(&mut self, switch: Switch) {
        match self.switches.iter_mut().find(|s| s.name == switch.name) {
            Some(old) => {
                old.value = switch.value;
                old.on = switch.on;
                if !switch.note.is_empty() {
                    old.note = switch.note;
                }
            }
            None => self.switches.push(switch),
        }
    }
}

/// What a run is given as `-More`: the active profile's switches, then the
/// extra text of Settings (whose pairs come later, so they stand where both
/// name the same switch).
pub fn more_for_run(profile: Option<&Profile>, extra: &str) -> String {
    let saved = profile.map(Profile::more).unwrap_or_default();
    let extra = extra.trim().trim_matches(';');
    match (saved.is_empty(), extra.is_empty()) {
        (true, _) => extra.to_string(),
        (false, true) => saved,
        (false, false) => format!("{saved};{extra}"),
    }
}

impl Book {
    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn profile_mut(&mut self, name: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.name == name)
    }

    pub fn active_profile(&self) -> Option<&Profile> {
        self.profile(self.active.as_deref()?)
    }

    /// A name no profile has yet, from `wanted`.
    pub fn free_name(&self, wanted: &str) -> String {
        let wanted = if wanted.trim().is_empty() { "Profile" } else { wanted.trim() };
        if self.profile(wanted).is_none() {
            return wanted.to_string();
        }
        (2..).map(|n| format!("{wanted} {n}")).find(|name| self.profile(name).is_none()).unwrap_or_default()
    }

    /// Adds an empty profile and makes it the active one; gives its name.
    pub fn add(&mut self, wanted: &str) -> String {
        let name = self.free_name(wanted);
        self.profiles.push(Profile { name: name.clone(), switches: Vec::new() });
        self.active = Some(name.clone());
        name
    }

    /// Removes a profile, and what pointed at it.
    pub fn remove(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
        self.by_title.retain(|(_, profile)| profile != name);
        if self.active.as_deref() == Some(name) {
            self.active = None;
        }
    }

    /// Gives a profile another name, everywhere it is named.
    pub fn rename(&mut self, from: &str, to: &str) -> bool {
        let to = to.trim();
        if to.is_empty() || to == from || self.profile(to).is_some() {
            return false;
        }
        let Some(profile) = self.profile_mut(from) else { return false };
        profile.name = to.to_string();
        for (_, name) in &mut self.by_title {
            if name == from {
                *name = to.to_string();
            }
        }
        if self.active.as_deref() == Some(from) {
            self.active = Some(to.to_string());
        }
        true
    }

    /// The profile a title folder chooses, if one is tied to it.
    pub fn for_title(&self, folder: &str) -> Option<&str> {
        self.by_title.iter().find(|(f, _)| f.eq_ignore_ascii_case(folder)).map(|(_, p)| p.as_str())
    }

    /// Ties `folder` to `profile`, or unties it (`None`).
    pub fn tie(&mut self, folder: &str, profile: Option<&str>) {
        self.by_title.retain(|(f, _)| !f.eq_ignore_ascii_case(folder));
        if let Some(profile) = profile {
            self.by_title.push((folder.to_string(), profile.to_string()));
        }
    }

    /// Reads the file's text. Lines it does not know are left out; a profile
    /// without a name, and a switch without a valid one, too.
    pub fn parse(text: &str) -> Book {
        #[derive(PartialEq)]
        enum At {
            Top,
            Profile,
            Switch,
            Title,
        }
        let mut book = Book::default();
        let mut at = At::Top;
        let mut title: (String, String) = Default::default();
        let end_title = |book: &mut Book, title: &mut (String, String)| {
            if !title.0.is_empty() && !title.1.is_empty() {
                book.by_title.push(std::mem::take(title));
            }
            *title = Default::default();
        };
        for line in text.lines() {
            let t = line.trim();
            match t {
                "[[profile]]" => {
                    end_title(&mut book, &mut title);
                    book.profiles.push(Profile::default());
                    at = At::Profile;
                }
                "[[profile.switch]]" if !book.profiles.is_empty() => {
                    if let Some(p) = book.profiles.last_mut() {
                        p.switches.push(Switch { name: String::new(), value: String::new(), on: true, note: String::new() });
                    }
                    at = At::Switch;
                }
                "[[title]]" => {
                    end_title(&mut book, &mut title);
                    at = At::Title;
                }
                _ if t.starts_with('[') => at = At::Top,
                _ => match at {
                    At::Top => {
                        if let Some(v) = string_value(line, "active") {
                            book.active = Some(v);
                        }
                    }
                    At::Profile => {
                        if let (Some(v), Some(p)) = (string_value(line, "name"), book.profiles.last_mut()) {
                            p.name = v;
                        }
                    }
                    At::Switch => {
                        let Some(s) = book.profiles.last_mut().and_then(|p| p.switches.last_mut()) else { continue };
                        if let Some(v) = string_value(line, "name") {
                            s.name = v;
                        } else if let Some(v) = string_value(line, "value") {
                            s.value = v;
                        } else if let Some(v) = string_value(line, "note") {
                            s.note = v;
                        } else if let Some((key, value)) = line.split_once('=')
                            && key.trim() == "on"
                        {
                            s.on = value.trim() == "true";
                        }
                    }
                    At::Title => {
                        if let Some(v) = string_value(line, "folder") {
                            title.0 = v;
                        } else if let Some(v) = string_value(line, "profile") {
                            title.1 = v;
                        }
                    }
                },
            }
        }
        end_title(&mut book, &mut title);
        book.profiles.retain(|p| !p.name.is_empty());
        for profile in &mut book.profiles {
            profile.switches.retain(|s| valid_name(&s.name) && valid_value(&s.value));
        }
        let names: Vec<String> = book.profiles.iter().map(|p| p.name.clone()).collect();
        book.by_title.retain(|(_, p)| names.contains(p));
        if book.active.as_ref().is_some_and(|a| !names.contains(a)) {
            book.active = None;
        }
        book
    }

    pub fn render(&self) -> String {
        let mut out = String::from(
            "# Rox-Box: your saved switches, in profiles. The launcher writes this file.\n# It can hold what belongs to one title or one PC: it is never committed (local\\ is git-ignored).\n\n",
        );
        if let Some(active) = &self.active {
            out += &format!("active = {}\n", toml_path(active));
        }
        for profile in &self.profiles {
            out += &format!("\n[[profile]]\nname = {}\n", toml_path(&profile.name));
            for s in &profile.switches {
                out += &format!("\n[[profile.switch]]\nname = {}\nvalue = {}\non = {}\n", toml_path(&s.name), toml_path(&s.value), s.on);
                if !s.note.is_empty() {
                    out += &format!("note = {}\n", toml_path(&s.note));
                }
            }
        }
        for (folder, profile) in &self.by_title {
            out += &format!("\n[[title]]\nfolder = {}\nprofile = {}\n", toml_path(folder), toml_path(profile));
        }
        out
    }

    pub fn load(file: &Path) -> Book {
        std::fs::read_to_string(file).map(|t| Book::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, self.render())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn switch(name: &str, value: &str, on: bool) -> Switch {
        Switch { name: name.into(), value: value.into(), on, note: String::new() }
    }

    #[test]
    fn a_pasted_more_text_becomes_switches() {
        let got = import(" -More \"VICTORY_AMBIENT=0.8; VICTORY_CRASH_SAVE=10:20,30:40 ;;VICTORY_PIPELINE_BOOK=0\" ").unwrap();
        assert_eq!(got, [switch("VICTORY_AMBIENT", "0.8", true), switch("VICTORY_CRASH_SAVE", "10:20,30:40", true), switch("VICTORY_PIPELINE_BOOK", "0", true)]);
        assert_eq!(import("A_B=x=y").unwrap(), [switch("A_B", "x=y", true)], "a value may hold =");
        assert_eq!(import("'A=1;A=2;B='").unwrap(), [switch("A", "2", true), switch("B", "", true)], "the later value stands");
        assert!(import("VICTORY_AMBIENT").unwrap_err().contains("not NAME=value"));
        assert!(import("victory_x=1").unwrap_err().contains("not a switch's name"));
        assert!(import("A=say \"hi\"").unwrap_err().contains("quote"));
        assert!(import("  ;; ").is_err());
    }

    #[test]
    fn names_and_values_are_checked() {
        assert!(valid_name("VICTORY_UNDER_UNWATCHED_MB") && valid_name("_X1"));
        assert!(!valid_name("") && !valid_name("1X") && !valid_name("Victory") && !valid_name("A B") && !valid_name("A=B"));
        assert!(valid_value("") && valid_value("0.8,1:2 = x"));
        assert!(!valid_value("a;b") && !valid_value("a\"b") && !valid_value("a\nb"));
    }

    #[test]
    fn a_run_is_given_the_switches_that_are_on_then_the_extra_text() {
        let profile = Profile { name: "P".into(), switches: vec![switch("A", "1", true), switch("B", "2", false), switch("C", "x,y", true)] };
        assert_eq!(profile.more(), "A=1;C=x,y");
        assert_eq!(profile.on_count(), 2);
        assert_eq!(more_for_run(Some(&profile), ""), "A=1;C=x,y");
        assert_eq!(more_for_run(Some(&profile), " A=9; "), "A=1;C=x,y;A=9", "the extra text comes last, so it stands");
        assert_eq!(more_for_run(None, " D=4 "), "D=4");
        assert_eq!(more_for_run(None, ""), "");
        assert_eq!(more_for_run(Some(&Profile::default()), "D=4"), "D=4");
    }

    #[test]
    fn the_book_reads_back_what_it_wrote() {
        let mut book = Book::default();
        assert_eq!(book.add("Fast sync"), "Fast sync");
        assert_eq!(book.add("Fast sync"), "Fast sync 2");
        assert_eq!(book.add(" "), "Profile");
        let first = book.profile_mut("Fast sync").unwrap();
        first.put(switch("VICTORY_A", "1", true));
        first.put(Switch { name: "VICTORY_LIST".into(), value: "aa11,bb22".into(), on: false, note: "it's one title's own".into() });
        first.put(switch("VICTORY_A", "2", false));
        assert_eq!(first.switches.len(), 2, "the same name is one switch");
        assert_eq!(first.switches[0], switch("VICTORY_A", "2", false));
        book.tie("D:\\Titles\\One", Some("Fast sync"));
        book.tie("d:\\titles\\one", Some("Fast sync 2"));
        assert_eq!(book.by_title.len(), 1);
        assert_eq!(book.for_title("D:\\TITLES\\ONE"), Some("Fast sync 2"));
        book.active = Some("Fast sync".into());
        assert_eq!(Book::parse(&book.render()), book);

        assert!(book.rename("Fast sync 2", "Slow"));
        assert!(!book.rename("Slow", "Fast sync"), "a name that is taken");
        assert_eq!(book.for_title("D:\\Titles\\One"), Some("Slow"));
        book.remove("Slow");
        assert!(book.by_title.is_empty() && book.profile("Slow").is_none());
        book.remove("Fast sync");
        assert_eq!(book.active, None);
        assert_eq!(Book::parse(&book.render()), book);
    }

    #[test]
    fn what_the_file_should_not_hold_is_left_out() {
        let text = "active = 'Gone'\n[[profile]]\n[[profile.switch]]\nname = 'A'\n[[profile]]\nname = 'Kept'\n[[profile.switch]]\nname = 'bad name'\nvalue = '1'\n[[profile.switch]]\nname = 'GOOD'\nvalue = 'a;b'\n[[profile.switch]]\nname = 'FINE'\nvalue = '3'\non = false\n[other]\nname = 'x'\n[[title]]\nfolder = 'D:\\T'\nprofile = 'Gone'\n[[title]]\nfolder = 'D:\\U'\nprofile = 'Kept'\n";
        let book = Book::parse(text);
        assert_eq!(book.active, None);
        assert_eq!(book.profiles, [Profile { name: "Kept".into(), switches: vec![switch("FINE", "3", false)] }]);
        assert_eq!(book.by_title, [("D:\\U".to_string(), "Kept".to_string())]);
        assert_eq!(Book::parse(""), Book::default());
    }
}
