//! `victory selftest`: the frame path end to end with a stand-in title of
//! our own (`victory_gpu::selftest`), so that a player can find out
//! whether their graphics card and driver work with the layer before
//! setting anything up, and CI covers the whole path. Needs no config file
//! and no game folder. It writes one file: its report, beside the run
//! reports (`<data_dir>\reports\selftest-<UTC time>.txt`; the default data
//! folder's without a config file; `VICTORY_REPORT=0`: none), which holds
//! the host's facts, each stage's outcome in our fixed words and the
//! benchmark's rates, and may be attached to an issue as it is.
//!
//! The stand-in's imports are bound as a title's are: looked up in the
//! runtime's registry under the DLL and name a title imports them by.
//!
//! Exit codes: 0 every stage passed; 1 a stage failed; 3 there is no
//! Direct3D 12 device at all (the tests skip on that); 4 a step hung; 5
//! the host GPU failed (device removed).

use anyhow::Result;

/// Exit code when the PC has no Direct3D 12 device at all.
pub const NO_DEVICE: i32 = 3;
const HUNG: i32 = 4;
const GPU_FAILED: i32 = 5;

/// The default log level: the layer's info lines would bury the report.
pub fn log_level() -> &'static str {
    if std::env::args().nth(1).as_deref() == Some("selftest") { "warn" } else { "info" }
}

#[cfg(not(windows))]
pub fn run(_warp: bool, _frames: u32, _bench: bool) -> Result<()> {
    anyhow::bail!("the self-test needs Windows and Direct3D 12")
}

#[cfg(windows)]
pub fn run(warp: bool, frames: u32, bench: bool) -> Result<()> {
    use std::io::Write;
    use victory_gpu::selftest::{self, Options, Outcome};

    let warp = warp || std::env::var("VICTORY_D3D12_WARP").is_ok_and(|v| v != "0");
    let imports = imports()?;
    // Before the runtime's device creation installs the handler that ends a
    // title's run: ours says which way the self-test failed.
    victory_gpu::on_failure(gpu_failed);
    WARP.store(warp, std::sync::atomic::Ordering::Relaxed);
    *seen() = Seen { warp, frames, bench, stages: Vec::new() };
    crate::report::selftest_begins();
    println!(
        "victory {} self-test{} (a stand-in title of our own; no game files)\n",
        env!("CARGO_PKG_VERSION"),
        if warp { " on the software adapter (WARP)" } else { "" }
    );
    let options = Options { warp, frames, bench, ..Options::default() };
    let report = selftest::run(&imports, &options, &mut |stage| {
        print_stage(stage.name, &stage.outcome);
        let _ = std::io::stdout().flush();
        seen().stages.push(stage_of(stage));
    }, hung);
    let rates = report
        .rates
        .iter()
        .map(|r| victory_core::report::SelftestRate {
            name: victory_core::report::Ours::new(r.name),
            value: r.value,
            unit: victory_core::report::Ours::new(unit_words(r.unit)),
        })
        .collect();
    if !report.rates.is_empty() {
        println!("\nBenchmark (name, value, unit; docs/selftest.md says how to read them):");
        for r in &report.rates {
            println!("{:<26} {:>12.2} {}", r.name, r.value, r.unit);
        }
    }
    println!();
    let ok = report.stages.iter().filter(|s| matches!(s.outcome, Outcome::Ok(_))).count();
    let skipped = report.stages.iter().filter(|s| matches!(s.outcome, Outcome::Skip(_))).count();
    match report.first_failure() {
        None => {
            println!("Self-test passed: {ok} of {} stages OK, {skipped} skipped.", report.stages.len());
            let _ = std::io::stdout().flush();
            write_report("completed", None, rates);
            Ok(())
        }
        Some(stage) => {
            let Outcome::Fail(reason) = &stage.outcome else { unreachable!() };
            println!("Self-test failed at \"{}\": {reason}", stage.name);
            println!("  {}", advice(warp));
            let _ = std::io::stdout().flush();
            write_report(if report.no_device { "no_device" } else { "completed" }, None, rates);
            if report.no_device {
                std::process::exit(NO_DEVICE);
            }
            Err(crate::preflight::Reported.into())
        }
    }
}

#[cfg(windows)]
static WARP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the self-test was asked and the stages done so far, for its report,
/// which a hang or a GPU failure writes from another thread.
#[cfg(windows)]
struct Seen {
    warp: bool,
    frames: u32,
    bench: bool,
    stages: Vec<victory_core::report::SelftestStage>,
}

#[cfg(windows)]
fn seen() -> std::sync::MutexGuard<'static, Seen> {
    static SEEN: std::sync::Mutex<Seen> =
        std::sync::Mutex::new(Seen { warp: false, frames: 0, bench: false, stages: Vec::new() });
    SEEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// A stage as the report holds it: its name, its outcome, and for a skip
/// our fixed reason (never the stage's own text, built at run time).
#[cfg(windows)]
fn stage_of(stage: &victory_gpu::selftest::Stage) -> victory_core::report::SelftestStage {
    use victory_core::report::{Ours, SelftestStage, StageOutcome};
    use victory_gpu::selftest::Outcome;
    SelftestStage {
        name: Ours::new(stage.name),
        outcome: match stage.outcome {
            Outcome::Ok(_) => StageOutcome::Ok,
            Outcome::Fail(_) => StageOutcome::Fail,
            Outcome::Skip(_) => StageOutcome::Skip,
        },
        reason: stage.skip_reason().map(Ours::new),
    }
}

/// A rate's unit in words (the report writes no slash).
#[cfg(windows)]
fn unit_words(unit: &'static str) -> &'static str {
    match unit {
        "MB/s" => "MB per s",
        "GB/s" => "GB per s",
        "us/GiB" => "us per GiB",
        "calls/s" => "calls per s",
        other => other,
    }
}

/// Writes the self-test's report: ended as `ended`, with the stages seen,
/// one more if `last` (the stage a hang or failure stopped), and `rates`.
#[cfg(windows)]
fn write_report(
    ended: &'static str,
    last: Option<victory_core::report::SelftestStage>,
    rates: Vec<victory_core::report::SelftestRate>,
) {
    let test = {
        let seen = seen();
        victory_core::report::Selftest {
            software_adapter: seen.warp,
            frames: seen.frames,
            bench: seen.bench,
            ended: victory_core::report::Ours::new(ended),
            stages: seen.stages.iter().copied().chain(last).collect(),
            rates,
        }
    };
    crate::report::selftest_ends(&test, ended == "completed" || ended == "no_device");
}

/// The stage a hang or a host GPU failure stopped, failed for `why`.
#[cfg(windows)]
fn stopped(name: &'static str, why: &'static str) -> victory_core::report::SelftestStage {
    use victory_core::report::{Ours, SelftestStage, StageOutcome};
    SelftestStage { name: Ours::new(name), outcome: StageOutcome::Fail, reason: Some(Ours::new(why)) }
}

/// What the player should do after a failure.
#[cfg(windows)]
fn advice(warp: bool) -> &'static str {
    if warp {
        "The software adapter failed too, so the fault is the layer's, not the driver's: please report the FAIL line."
    } else {
        "Update the graphics driver, then run `victory selftest --warp`: if that passes, the driver or the card is \
         at fault; if it fails too, the layer is (please report the FAIL line)."
    }
}

#[cfg(windows)]
fn print_stage(name: &str, outcome: &victory_gpu::selftest::Outcome) {
    use victory_gpu::selftest::Outcome;
    let (tag, text) = match outcome {
        Outcome::Ok(t) => ("[  OK   ]", t),
        Outcome::Fail(t) => ("[ FAIL  ]", t),
        Outcome::Skip(t) => ("[ SKIP  ]", t),
    };
    println!("{tag} {name:<17} {text}");
}

/// The exports the stand-in imports, bound through the runtime's registry
/// as a title's imports are.
#[cfg(windows)]
fn imports() -> Result<victory_gpu::selftest::Imports> {
    use victory_gpu::selftest::{CreateDevice, Imports, SerializeRoot};
    use victory_loader::{ImportResolver, Resolution};
    let resolver = victory_hle::HleResolver::new(victory_hle::HostPolicy::none());
    let find = |name: &str| -> Result<usize> {
        let symbol = victory_pe::ImportSymbol::Name { hint: 0, name: name.into() };
        match resolver.resolve("d3d12_xs.dll", &symbol) {
            Resolution::Found { address, .. } => Ok(address),
            Resolution::Missing => anyhow::bail!("the runtime does not export d3d12_xs.dll!{name}"),
        }
    };
    let (create, serialize) = (find("D3D12XboxCreateDevice")?, find("D3D12SerializeRootSignature")?);
    // SAFETY: the registry lists these exports at these addresses, and they
    // have these signatures (`victory_hle::modules::d3d12x`).
    Ok(unsafe {
        Imports {
            create_device: std::mem::transmute::<usize, CreateDevice>(create),
            serialize_root: std::mem::transmute::<usize, SerializeRoot>(serialize),
        }
    })
}

#[cfg(windows)]
fn hung(stage: &'static str) -> ! {
    use std::io::Write;
    print_stage(stage, &victory_gpu::selftest::Outcome::Fail("no answer from the GPU in 60 s (a hang)".into()));
    println!("\nSelf-test failed at \"{stage}\": it hung.");
    println!("  {}", advice(WARP.load(std::sync::atomic::Ordering::Relaxed)));
    let _ = std::io::stdout().flush();
    write_report("hung", Some(stopped(stage, "no answer from the GPU in 60 s")), Vec::new());
    std::process::exit(HUNG)
}

#[cfg(windows)]
fn gpu_failed(what: &str) -> ! {
    use std::io::Write;
    println!("[ FAIL  ] {:<17} host GPU failure: {what}", "GPU");
    println!("\nSelf-test failed: the host GPU failed.");
    println!("  {}", advice(WARP.load(std::sync::atomic::Ordering::Relaxed)));
    let _ = std::io::stdout().flush();
    write_report("host_gpu_failed", Some(stopped("GPU", "the host GPU failed")), Vec::new());
    std::process::exit(GPU_FAILED)
}
