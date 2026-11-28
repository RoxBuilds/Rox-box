//! Copies of what titles saved, kept by the launcher when asked to: after a
//! session ends, the save folder is packed into one timestamped zip in the
//! backup folder, and only the newest few are kept.
//!
//! The launcher knows no title's save folder. It packs the folder the person
//! names, or, when none is named, the engine's own data folder (`data_dir`
//! as `victory config` states it: "what titles save"). The zip is made by
//! the `tar.exe` Windows ships.

use std::path::{Path, PathBuf};

use crate::runner::{Kind, Spec};

/// How a backup's file begins and ends.
const PREFIX: &str = "saves-";
const SUFFIX: &str = ".zip";

/// The most a folder nobody named may hold and still be packed (250 MB).
pub const UNASKED_LIMIT: u64 = 250 * 1024 * 1024;

/// What is packed when no folder is named: a `saves` folder in the engine's
/// data folder if there is one, else the data folder itself (which
/// [`UNASKED_LIMIT`] then guards).
pub fn default_source(data_dir: &Path) -> PathBuf {
    let saves = data_dir.join("saves");
    if saves.is_dir() { saves } else { data_dir.to_path_buf() }
}

/// The bytes of the files under `dir`, counted until they pass `stop_past`
/// (a large folder is not walked to its end to learn that it is large).
pub fn folder_size(dir: &Path, stop_past: u64) -> u64 {
    let mut total = 0;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(next) = todo.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().filter_map(|e| e.ok()) {
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => todo.push(entry.path()),
                Ok(meta) => total += meta.len(),
                Err(_) => {}
            }
            if total > stop_past {
                return total;
            }
        }
    }
    total
}

/// The file a backup made at `stamp` goes to.
pub fn file_for(backup_dir: &Path, stamp: &str) -> PathBuf {
    backup_dir.join(format!("{PREFIX}{stamp}{SUFFIX}"))
}

/// Whether `name` is one of ours (`saves-<yyyyMMdd-HHmmss>.zip`): only those are ever removed.
pub fn is_ours(name: &str) -> bool {
    let Some(stamp) = name.strip_prefix(PREFIX).and_then(|n| n.strip_suffix(SUFFIX)) else { return false };
    stamp.len() == 15 && stamp.chars().enumerate().all(|(i, c)| if i == 8 { c == '-' } else { c.is_ascii_digit() })
}

/// Of the files in a backup folder, the backups to remove so that the
/// newest `keep` stay (the stamp in the name orders them). `keep` of 0 keeps all.
pub fn to_remove(names: &[String], keep: usize) -> Vec<String> {
    let mut ours: Vec<&String> = names.iter().filter(|n| is_ours(n)).collect();
    ours.sort();
    if keep == 0 || ours.len() <= keep {
        return Vec::new();
    }
    ours[..ours.len() - keep].iter().map(|n| n.to_string()).collect()
}

/// Removes the older backups of `backup_dir`; says how many went.
pub fn prune(backup_dir: &Path, keep: usize) -> usize {
    let names: Vec<String> = std::fs::read_dir(backup_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    to_remove(&names, keep).iter().filter(|n| std::fs::remove_file(backup_dir.join(n)).is_ok()).count()
}

/// The command that packs `save_dir` (the folder itself, by its name) into `zip`.
pub fn pack(save_dir: &Path, zip: &Path) -> Option<Spec> {
    let parent = save_dir.parent()?;
    let name = save_dir.file_name()?.to_string_lossy().into_owned();
    Some(Spec {
        kind: Kind::Backup,
        program: crate::update::system_tool("tar.exe"),
        args: vec!["-a".into(), "-c".into(), "-f".into(), zip.to_string_lossy().into_owned(), "-C".into(), parent.to_string_lossy().into_owned(), name],
        envs: Vec::new(),
        cwd: None,
        detached: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_backups_are_counted_and_the_newest_stay() {
        assert!(is_ours("saves-20261128-101500.zip"));
        assert!(!is_ours("saves-2026.zip") && !is_ours("my-saves-20261128-101500.zip") && !is_ours("saves-20261128x101500.zip"));
        let names: Vec<String> = ["saves-20261128-101500.zip", "holiday.zip", "saves-20261127-090000.zip", "saves-20261129-000001.zip", "saves-20250101-000000.zip"]
            .iter()
            .map(|n| n.to_string())
            .collect();
        assert_eq!(to_remove(&names, 2), ["saves-20250101-000000.zip", "saves-20261127-090000.zip"]);
        assert!(to_remove(&names, 4).is_empty());
        assert!(to_remove(&names, 0).is_empty(), "0 keeps every backup");
        assert_eq!(file_for(Path::new("D:\\b"), "20261128-101500"), PathBuf::from("D:\\b\\saves-20261128-101500.zip"));
    }

    #[test]
    fn a_saves_folder_is_preferred_and_sizes_are_counted() {
        let data = std::env::temp_dir().join(format!("roxbox-launcher-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(data.join("reports").join("deep")).unwrap();
        std::fs::write(data.join("reports").join("deep").join("a.txt"), vec![1u8; 3000]).unwrap();
        std::fs::write(data.join("b.bin"), vec![2u8; 2000]).unwrap();
        assert_eq!(default_source(&data), data, "no saves folder: the data folder itself");
        assert_eq!(folder_size(&data, u64::MAX), 5000);
        assert!(folder_size(&data, 1000) > 1000, "counting stops once the limit is passed");
        assert_eq!(folder_size(&data.join("nowhere"), u64::MAX), 0);
        std::fs::create_dir_all(data.join("saves")).unwrap();
        assert_eq!(default_source(&data), data.join("saves"));
        assert_eq!(UNASKED_LIMIT, 262_144_000);
    }

    #[test]
    fn a_folder_is_packed_into_a_zip_and_old_ones_go() {
        let dir = std::env::temp_dir().join(format!("roxbox-launcher-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let saves = dir.join("data");
        let backups = dir.join("backups");
        std::fs::create_dir_all(saves.join("slot")).unwrap();
        std::fs::create_dir_all(&backups).unwrap();
        std::fs::write(saves.join("slot").join("save.bin"), vec![7u8; 4096]).unwrap();
        for stamp in ["20200101-000000", "20200102-000000"] {
            std::fs::write(file_for(&backups, stamp), b"old").unwrap();
        }
        std::fs::write(backups.join("keep-me.txt"), b"not ours").unwrap();
        let zip = file_for(&backups, "20261128-101500");
        let spec = pack(&saves, &zip).unwrap();
        assert_eq!(spec.args[4..], ["-C".to_string(), dir.to_string_lossy().into_owned(), "data".to_string()]);
        let job = crate::runner::start(spec, || {}).unwrap();
        let code = loop {
            match job.events.recv().unwrap() {
                crate::runner::Event::Exit(code) => break code,
                crate::runner::Event::Line(_) => {}
                crate::runner::Event::Failed(e) => panic!("{e}"),
            }
        };
        assert_eq!(code, Some(0));
        let made = std::fs::read(&zip).unwrap();
        assert_eq!(&made[..2], b"PK", "a zip was written");
        assert_eq!(prune(&backups, 2), 1);
        assert!(!file_for(&backups, "20200101-000000").exists() && file_for(&backups, "20200102-000000").exists() && zip.exists());
        assert!(backups.join("keep-me.txt").exists());
    }
}
