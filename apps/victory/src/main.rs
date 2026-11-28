//! `victory`: the command-line front end.
//!
//! Every command reads the user's own files and writes only to stdout or to
//! the user's cache directory (outside the repository).

mod census;
mod defaults;
mod inspect;
#[cfg(windows)]
mod play;
mod preflight;
#[cfg(all(windows, feature = "diagnostics"))]
mod replay;
mod report;
mod run;
mod selftest;
mod shader;
mod translate;
#[cfg(windows)]
mod warm;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "victory",
    version,
    about = "Run console-platform PE32+ / D3D12.X titles natively on Windows",
    after_help = "First time: copy victory.example.toml to victory.toml, set game_dir in it, \
                  then `victory check`."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
// (Parsed once a process: the size of the largest command does not matter.)
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Summarise one PE image: headers, sections, imports, TLS, build id.
    Inspect {
        file: PathBuf,
        /// List every imported function.
        #[arg(long)]
        imports: bool,
        /// List every exported function.
        #[arg(long)]
        exports: bool,
    },
    /// Aggregate the imports of every PE image under the given paths and
    /// show which ones our runtime covers. Defaults to the configured
    /// game and test-title directories.
    Census {
        paths: Vec<PathBuf>,
        /// Do not count host pass-through as covered.
        #[arg(long)]
        strict: bool,
        /// List every missing `dll!function`, most used first.
        #[arg(long)]
        missing: bool,
    },
    /// List the `dll!export` pairs an import library describes.
    LibExports {
        lib: PathBuf,
        /// List every export, not just per-DLL counts.
        #[arg(long)]
        list: bool,
    },
    /// Find shader containers in files or directories (also inside packed
    /// shader archives and uncompressed archives) and report their layout.
    ShaderInfo {
        paths: Vec<PathBuf>,
        /// Print one line per container found.
        #[arg(long)]
        verbose: bool,
        /// Also list the decoded native instructions (implies --verbose).
        #[arg(long)]
        disasm: bool,
    },
    /// Run every shader found (containers, packed shader archives) through the
    /// translator and report what it cannot handle yet.
    Translate {
        paths: Vec<PathBuf>,
        /// Check emitted DXIL with dxil.dll's validator (`VICTORY_DXIL`,
        /// else the newest Windows SDK's).
        #[arg(long)]
        validate: bool,
        /// Also create a host D3D12 pipeline for each emitted shader.
        #[arg(long)]
        pipelines: bool,
        /// Print one line per program and outcome.
        #[arg(long)]
        list: bool,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
        /// Warm the run's shader cache: a file of shader hashes (the 16
        /// hex digits a run's log names a shader by, anywhere in a line);
        /// the containers with those hashes are translated into the
        /// cache and nothing else is done. Set `VICTORY_HOST_WAVE` and
        /// `VICTORY_VIEW_LIMIT` to what the run's log says of the host.
        #[arg(long)]
        warm: Option<PathBuf>,
    },
    /// Translate every shader found (as `translate` finds them) into the
    /// shader cache, so that no run has to: a few minutes, once per change
    /// of the translator.
    Shaders {
        paths: Vec<PathBuf>,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Make the host pipelines of every pipeline the title ever created
    /// (runs with `VICTORY_PIPELINE_BOOK=1` write them down), so that the
    /// driver's cache on disk has them before a run needs one: once per change
    /// of the translator or the driver. Goes on where it was stopped.
    #[cfg(windows)]
    Warm {
        /// Processes at once (default: two thirds of the logical cores).
        #[arg(long)]
        jobs: Option<usize>,
        /// At most this many pipelines.
        #[arg(long)]
        limit: Option<usize>,
        /// Those made before too.
        #[arg(long)]
        again: bool,
        /// Only the pipelines earlier runs drew with.
        #[arg(long)]
        used: bool,
        /// Then one process makes the pipelines earlier runs drew with and
        /// keeps them, with the device, until the driver's background threads
        /// ("D3D Background Thread") have used under 5 % of a core for 30 s in
        /// a row, saying once a minute how far they are.
        #[arg(long)]
        settle: bool,
        /// The longest `--settle`, in minutes (40).
        #[arg(long, requires = "settle")]
        settle_max: Option<u64>,
        /// (What the processes it starts are given.)
        #[arg(long, hide = true)]
        these: Option<String>,
        /// (What the settling process is given: its minutes.)
        #[arg(long, hide = true)]
        settle_for: Option<u64>,
    },
    /// Prepare, map and bind a title executable; with --execute, run it.
    Run {
        /// The title executable (default: the only .exe in the configured
        /// game folder).
        exe: Option<PathBuf>,
        /// What unimplemented imports do when called.
        #[arg(long, value_enum, default_value_t = Unimplemented::Trap)]
        unimplemented: Unimplemented,
        /// Disable host pass-through (everything must be implemented by us).
        #[arg(long)]
        strict: bool,
        /// Actually start the title. Without it, stop after binding.
        #[arg(long)]
        execute: bool,
        /// The console model the title sees (device type, memory).
        #[arg(long, value_enum, default_value_t = Console::SeriesX)]
        console: Console,
        /// Arguments for the title, after `--` (it reads the whole command
        /// line; a title that relaunches itself passes some).
        #[arg(last = true)]
        title_args: Vec<String>,
    },
    /// A run as a player starts it: the check, the preparation a run needs
    /// (the title's shaders, and with --warm the pipeline warm-up), then the
    /// run; with --log its output also goes to a log file. What the launcher
    /// starts.
    #[cfg(windows)]
    Play(play::Play),
    /// Run a captured frame's GPU work again (diagnostics builds; see
    /// docs/replay.md).
    #[cfg(all(windows, feature = "diagnostics"))]
    Replay(replay::Replay),
    /// Show the resolved configuration and check it.
    Config,
    /// Run the frame path end to end with a stand-in title of our own (no
    /// game files, no config): device, clears and presents, title memory, a
    /// translated shader. Exit code 0 when every stage passes.
    Selftest {
        /// On the software adapter (WARP), to tell a driver problem from a
        /// layer problem.
        #[arg(long)]
        warp: bool,
        /// Frames of the clear-and-present stage.
        #[arg(long, default_value_t = 120)]
        frames: u32,
        /// Also time a few of the layer's own hot paths.
        #[arg(long)]
        bench: bool,
    },
    /// Check everything a run needs (config, game folder, title executable,
    /// processor, Windows, memory, graphics card) and say how to fix what
    /// is missing. Starts nothing.
    Check {
        /// The title executable (default: the only .exe in the configured
        /// game folder).
        exe: Option<PathBuf>,
    },
    /// Show the newest run report (every `run --execute` writes one; it is
    /// safe to attach to a public issue), then its path on stderr.
    Report {
        /// List the reports kept, newest first.
        #[arg(long, conflicts_with = "path")]
        list: bool,
        /// Print only the newest report's path.
        #[arg(long)]
        path: bool,
        /// Compare two reports (older, then newer; each a file or a number
        /// from `--list`, 1 = newest); none: the two newest. Needs no game
        /// folder, and no config file when given two paths.
        #[arg(long, num_args = 0..=2, value_names = ["A", "B"], conflicts_with_all = ["list", "path"])]
        diff: Option<Vec<String>>,
        /// With --diff: the percent a key must change by to be shown.
        #[arg(long, requires = "diff", default_value_t = report::DEFAULT_THRESHOLD)]
        threshold: f64,
        /// With --diff: show every key, changed or not.
        #[arg(long, requires = "diff")]
        all: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Unimplemented {
    Trap,
    Zero,
}

#[derive(Clone, Copy, ValueEnum)]
enum Console {
    SeriesX,
    SeriesS,
}

/// `victory play` checks the config file itself (its first step is the check).
fn is_play(command: &Command) -> bool {
    #[cfg(windows)]
    if matches!(command, Command::Play(_)) {
        return true;
    }
    let _ = command;
    false
}

/// Errors end the process with exit code 1 and what went wrong in plain
/// words on stderr (a wrong command line: clap's message and exit code 2).
fn main() -> std::process::ExitCode {
    match real_main() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if !error.is::<preflight::Reported>() {
                report(&error);
            }
            // No Direct3D 12 device at all: the self-test's code, which the
            // tests skip on (`apps/victory/tests/demo_title.rs`).
            if error.is::<preflight::NoDevice>() {
                return std::process::ExitCode::from(selftest::NO_DEVICE as u8);
            }
            std::process::ExitCode::FAILURE
        }
    }
}

/// An error and what led to it, one cause a line.
fn report(error: &anyhow::Error) {
    eprintln!("error: {error}");
    for cause in error.chain().skip(1) {
        eprintln!("  because: {cause}");
    }
    if let Some(hint) = preflight::memory_hint(error) {
        eprintln!("  hint: {hint}");
    }
}

/// Gives the code that reads the user's optional files (patches, shader
/// profile, format tables) the paths the config file names, for every
/// command: they are read from the environment, which takes precedence.
fn publish_external_paths(cfg: &victory_core::Config) {
    for (key, value) in [
        ("VICTORY_PATCHES_FILE", cfg.patches_file.as_deref()),
        ("VICTORY_SHADER_PROFILE", cfg.shader_profile.as_deref()),
        ("VICTORY_BCPACK_TABLES", cfg.bcpack_tables.as_deref()),
    ] {
        if let Some(path) = value
            && std::env::var_os(key).is_none_or(|v| v.is_empty())
        {
            // SAFETY: called on the main thread during startup before worker or guest threads exist.
            unsafe { std::env::set_var(key, path) };
        }
    }
}

fn real_main() -> Result<()> {
    victory_core::logging::init(selftest::log_level());
    // The audio recorder `run` starts for `VICTORY_AUDIO_CAPTURE`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if victory_hle::modules::audio::capture::child() {
        return Ok(());
    }
    let cli = Cli::parse();
    // `config` and `check` say themselves what is wrong with the config
    // file, and the commands that only read a file named on the command
    // line work without one.
    let reads_no_config = matches!(
        cli.command,
        Command::Config | Command::Check { .. } | Command::Inspect { .. } | Command::LibExports { .. }
    ) || matches!(cli.command, Command::Selftest { .. })
        || is_play(&cli.command)
        || matches!(cli.command, Command::Report { diff: Some(_), .. });
    // The commands that start the graphics layer get the confirmed defaults
    // (defaults.rs) before any switch is read.
    let graphics = matches!(cli.command, Command::Run { .. } | Command::Selftest { .. });
    #[cfg(windows)]
    let graphics = graphics || matches!(cli.command, Command::Warm { .. });
    #[cfg(all(windows, feature = "diagnostics"))]
    let graphics = graphics || matches!(cli.command, Command::Replay(_));
    if graphics {
        let set = defaults::apply();
        eprintln!("Rox-box: {set} switches set from the built-in defaults (VICTORY_DEFAULTS=0 for none, =safe for the base set)");
    }
    match victory_core::Config::load() {
        Ok(cfg) => publish_external_paths(&cfg),
        Err(_) if reads_no_config => {}
        Err(e) => return Err(e),
    }
    match cli.command {
        Command::Inspect {
            file,
            imports,
            exports,
        } => inspect::run(&file, imports, exports),
        Command::Census {
            paths,
            strict,
            missing,
        } => census::run(paths, strict, missing),
        Command::LibExports { lib, list } => inspect::lib_exports(&lib, list),
        Command::ShaderInfo {
            paths,
            verbose,
            disasm,
        } => shader::run(paths, verbose || disasm, disasm),
        Command::Translate {
            paths,
            validate,
            pipelines,
            list,
            jobs,
            warm,
        } => {
            // `VICTORY_VIEW_LIMIT=<null slot>`: translate as on a host whose view heap is capped
            if let Some(n) = std::env::var("VICTORY_VIEW_LIMIT").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_view_heap_limit(n);
            }
            // `VICTORY_HOST_WAVE=32`: translate as for a GPU whose waves are at most 32 lanes
            if let Some(n) = std::env::var("VICTORY_HOST_WAVE").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_host_max_wave(n);
            }
            if let Some(file) = warm {
                return translate::warm(paths, &file, jobs);
            }
            translate::run(paths, validate, pipelines, list, jobs)
        }
        Command::Shaders { paths, jobs } => translate::precompile(paths, jobs),
        #[cfg(windows)]
        Command::Warm { jobs, limit, again, used, settle, settle_max, these, settle_for } => {
            let settle = settle.then(|| settle_max.unwrap_or(40));
            warm::run(jobs, limit, again, used, these, settle, settle_for)
        }
        Command::Run {
            exe,
            unimplemented,
            strict,
            execute,
            console,
            title_args,
        } => {
            victory_hle::runtime::set_title_args(title_args);
            run::run(exe.as_deref(), unimplemented, strict, execute, console)
        }
        #[cfg(all(windows, feature = "diagnostics"))]
        Command::Replay(options) => replay::run(options),
        #[cfg(windows)]
        Command::Play(options) => play::run(options),
        Command::Config => show_config(),
        Command::Check { exe } => preflight::check(exe.as_deref()),
        Command::Report {
            diff: Some(args),
            threshold,
            all,
            ..
        } => report::diff(&args, threshold, all),
        Command::Report { list, path, .. } => report::run(list, path),
        Command::Selftest { warp, frames, bench } => selftest::run(warp, frames, bench),
    }
}

fn show_config() -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let status = |p: &Option<PathBuf>| match p {
        Some(p) if p.exists() => format!("{}", p.display()),
        Some(p) => format!("{}  (does not exist)", p.display()),
        None => "(not set)".to_owned(),
    };
    println!(
        "config file : {}",
        cfg.source
            .as_ref()
            .map_or("(none)".into(), |p| p.display().to_string())
    );
    println!("game_dir    : {}", status(&cfg.game_dir));
    println!("cache_dir   : {}", cfg.cache_dir.display());
    println!("data_dir    : {}", cfg.data_dir.display());
    println!("patches     : {}", status(&cfg.patches_file));
    println!("shaders     : {}", status(&cfg.shader_profile));
    println!("tables      : {}", status(&cfg.bcpack_tables));
    for t in &cfg.test_titles {
        println!("test_title  : {}", status(&Some(t.clone())));
    }
    // The same rule as `victory check`: folders of game files are warned
    // about inside a source tree, the folders written to are refused there.
    for p in cfg.game_dir.iter().chain(&cfg.test_titles) {
        if let Some(inside) = preflight::game_files_in_source_tree(p) {
            println!("WARNING: {inside}; {}", preflight::MOVE_GAME_FILES);
        }
    }
    for p in [&cfg.cache_dir, &cfg.data_dir] {
        if let Err(e) = victory_core::ensure_outside_repo(p) {
            println!("POLICY VIOLATION: {e}");
        }
    }
    Ok(())
}

/// Paths given on the command line, or the configured game + test titles.
pub(crate) fn default_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = victory_core::Config::load()?;
    let all: Vec<PathBuf> = cfg.game_dir.into_iter().chain(cfg.test_titles).collect();
    anyhow::ensure!(
        !all.is_empty(),
        "no paths given and no game folder configured: name a file or folder, or set game_dir \
         in victory.toml (see victory.example.toml)"
    );
    Ok(all)
}
