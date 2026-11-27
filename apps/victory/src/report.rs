//! `victory report`: the newest run report, the list of them, or the newest
//! one's path; and the start of a run's report, for `run --execute`.
//!
//! Every `victory run --execute` writes a run report to `<data_dir>\reports`
//! (`victory_core::report`, docs/run-report.md). It holds numbers and
//! Rox-box's own words only, so a player can attach it to a public issue as
//! it is: the one thing about a run that may be shared.

use anyhow::{Context, Result};
use victory_core::report;

/// `victory report [--list | --path]`.
pub fn run(list: bool, path_only: bool) -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let dir = cfg.data_dir.join(report::FOLDER);
    let reports = report::reports_in(&dir);
    let none = || {
        format!(
            "no run reports in {} yet: every `victory run --execute` writes one",
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
    report::set_platform_dll(platform_dll);
    report::add_sampler(victory_gpu::report_sample);
    let _ = report::begin(dir);
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
}
