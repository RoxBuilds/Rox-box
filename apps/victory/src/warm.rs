//! `victory warm`: host pipelines made ahead of a run (`known_pipelines`), in
//! processes of their own: a compiler that crashes or hangs costs one pipeline.
//!
//! `--settle`: then one process makes the pipelines runs draw with and keeps
//! them until the driver's background threads are idle ([`keep`]). Those threads
//! can use several cores for minutes though only a few pipelines were new: the
//! best explanation is that the driver works through all of the library's
//! pipelines again, which needs many minutes and never ends in a short run, nor
//! in the warm-up's processes, which exit at once.
//! Whether a pass that ends is kept (the next run's background threads then
//! stay quiet) is what this finds out.
//!
//! A warm-up that made pipelines (or settled) names the exe that made them in
//! [`STAMP`], which `scripts\play.ps1` compares with the exe it runs.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use victory_hle::known_pipelines as known;

/// Pipelines a process makes before another takes its place: the compiler's
/// and the translations' memory goes with it.
const AT_ONCE: usize = 40;
/// A pipeline that takes longer is taken for one the compiler hangs on.
const LONGEST: Duration = Duration::from_secs(180);

/// As the runs count one that takes long.
const SLOW_MS: f64 = 1000.0;

fn folder() -> Result<PathBuf> {
    let cfg = victory_core::Config::load()?;
    Ok(cfg.cache_subdir("shaders")?.with_file_name("pipelines"))
}

/// `settle`: `--settle`'s longest wait in minutes; `settle_for`: this is the
/// process that settles ([`keep`]), for so many minutes at most.
pub fn run(
    jobs: Option<usize>,
    limit: Option<usize>,
    again: bool,
    used: bool,
    these: Option<String>,
    settle: Option<u64>,
    settle_for: Option<u64>,
) -> Result<()> {
    let dir = folder()?;
    if let Some(these) = these {
        return make(&dir, &these);
    }
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let jobs = jobs.unwrap_or(cores * 2 / 3).max(1);
    if let Some(minutes) = settle_for {
        return keep(&dir, minutes, jobs);
    }
    let began = Instant::now();
    victory_hle::known_pipelines::use_translations(dir.with_file_name("shaders"));
    let fingerprint = known::fingerprint();
    let done_file = dir.join("warm-done.txt");
    let bad_file = dir.join("warm-bad.txt");
    let keys = known::keys(&dir);
    let listed = std::fs::read_to_string(&done_file).unwrap_or_default();
    let mut done: HashSet<&str> = HashSet::new();
    for line in listed.lines() {
        if let Some((of, key)) = line.split_once(' ')
            && of == fingerprint
        {
            done.insert(key);
        }
    }
    let bad = std::fs::read_to_string(&bad_file).unwrap_or_default();
    let bad: HashSet<&str> = bad.lines().filter_map(|l| l.split(' ').next()).collect();
    let early = std::fs::read_to_string(dir.join("early-pipelines.txt")).unwrap_or_default();
    let early: HashSet<&str> = early.lines().collect();
    // Those earlier runs drew with first, the others in no order (by key).
    let mut all: Vec<usize> = (0..keys.len())
        .filter(|&n| {
            let key = keys[n].as_str();
            (again || !done.contains(key)) && !bad.contains(key) && (!used || early.contains(key))
        })
        .collect();
    all.sort_by_key(|&n| (!early.contains(keys[n].as_str()), &keys[n]));
    all.truncate(limit.unwrap_or(usize::MAX));
    println!(
        "{} pipelines known, {} made before ({fingerprint}), {} bad; {} to make, {jobs} processes",
        keys.len(),
        done.len(),
        bad.len(),
        all.len()
    );
    let chunks: Vec<&[usize]> = all.chunks(AT_ONCE).collect();
    let next = AtomicUsize::new(0);
    let state = Mutex::new((Vec::<f64>::new(), 0usize, 0usize));
    let done_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&done_file)?);
    // (Read back by the runs: these are made beside the others, not before.)
    let slow_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(dir.join("slow-pipelines.txt"))?);
    let bad_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&bad_file)?);
    std::thread::scope(|s| {
        for _ in 0..jobs.min(chunks.len()) {
            s.spawn(|| {
                while let Some(&chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let mut rest = chunk;
                    while !rest.is_empty() {
                        let (made, ended) = child(rest, |n, ms, there| {
                            if ms >= SLOW_MS {
                                let _ = writeln!(lock(&slow_out), "\n{} {ms:.0}", keys[n]);
                            }
                            let mut state = lock(&state);
                            state.0.push(ms);
                            state.1 += usize::from(!there);
                            let count = state.0.len();
                            if count.is_multiple_of(1000) {
                                println!("{count} of {}, {:.0} s", all.len(), began.elapsed().as_secs_f64());
                            }
                            drop(state);
                            let _ = writeln!(lock(&done_out), "{fingerprint} {}", keys[n]);
                        });
                        rest = &rest[made.min(rest.len())..];
                        match (ended, rest.split_first()) {
                            (Some(why), Some((&n, after))) => {
                                // The one it was at.
                                println!("pipeline {n} {}: {why}", keys[n]);
                                let _ = writeln!(lock(&bad_out), "{} {why}", keys[n]);
                                lock(&state).2 += 1;
                                rest = after;
                            }
                            _ => break,
                        }
                    }
                }
            });
        }
    });
    let (mut times, refused, crashed) = state.into_inner().unwrap_or_else(|e| e.into_inner());
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times.get((times.len() as f64 * q) as usize).copied().unwrap_or(0.0);
    println!(
        "{} made in {:.0} s ({refused} have no host pipeline, {crashed} ended their process); ms each: median {:.1}, p90 {:.1}, p99 {:.1}, most {:.0}, sum {:.0} s",
        times.len(),
        began.elapsed().as_secs_f64(),
        at(0.5),
        at(0.9),
        at(0.99),
        times.last().copied().unwrap_or(0.0),
        times.iter().sum::<f64>() / 1e3
    );
    let settled = match settle {
        Some(minutes) => Some(settle_in_a_process(minutes, jobs)?),
        None => None,
    };
    match (times.len(), settled) {
        (_, Some(false)) => println!("stamp: the settling process did not end well: {STAMP} left as it was"),
        (0, None) => println!(
            "stamp: nothing was made with this build: {STAMP} left as it was (--settle, or --again, makes them with it)"
        ),
        _ => match std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| {
            let text = stamp_text(&exe, settled == Some(true), SystemTime::now())?;
            std::fs::write(dir.join(STAMP), text)?;
            Ok(exe)
        }) {
            Ok(exe) => println!("stamp: {} names {} ({BUILD})", dir.join(STAMP).display(), exe.display()),
            Err(e) => println!("stamp: not written: {e:#}"),
        },
    }
    Ok(())
}

/// Beside the pipelines' lists: the exe whose processes made the pipelines
/// last. The driver's cache holds them for one build of ours: a warm-up with a
/// diag exe rebuilt a minute later was made all again by the run (several
/// cores of driver threads, library loads of seconds), where runs of the exe
/// that warmed loaded over a thousand pipelines within seconds.
/// `scripts\play.ps1` compares
/// the exe it runs with this and warns when another build warmed (or none).
pub const STAMP: &str = "warm-stamp.txt";
/// How much of the exe's start [`STAMP`] hashes (SHA-256, which play.ps1 has
/// from .NET): its headers (the link time among them) and the first code.
const STAMP_HEAD: u64 = 1 << 20;
/// The build this is: the folder play.ps1 takes it from.
const BUILD: &str = if cfg!(feature = "diagnostics") { "diag" } else { "release" };

/// [`STAMP`]'s text for the exe at `exe`, one `name=value` a line: its path,
/// [`BUILD`], its size, its last write time (seconds since 1970, UTC), the
/// SHA-256 of its first [`STAMP_HEAD`] bytes; whether `--settle` ran to its end;
/// `now` (seconds).
fn stamp_text(exe: &Path, settled: bool, now: SystemTime) -> Result<String> {
    let meta = std::fs::metadata(exe).with_context(|| exe.display().to_string())?;
    let mut head = Vec::new();
    std::fs::File::open(exe)?.take(STAMP_HEAD).read_to_end(&mut head)?;
    let hash: String = sha256(&head).iter().map(|b| format!("{b:02x}")).collect();
    let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    Ok(format!(
        "# victory warm: the build that made the pipelines (scripts\\play.ps1 compares the exe it runs with it)\n\
         exe={}\nbuild={BUILD}\nsize={}\nmodified={}\nhead_sha256={hash}\nsettled={}\nwarmed={}\n",
        exe.display(),
        meta.len(),
        secs(meta.modified()?),
        if settled { "yes" } else { "no" },
        secs(now)
    ))
}

/// SHA-256 (FIPS 180-4) of `data`.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend((data.len() as u64 * 8).to_be_bytes());
    for block in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes(bytes.try_into().expect("four bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for (k, w) in K.iter().zip(w) {
            let [a, b, c, d, e, f, g, last] = v;
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let t1 = last.wrapping_add(s1).wrapping_add((e & f) ^ (!e & g)).wrapping_add(*k).wrapping_add(w);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let t2 = s0.wrapping_add((a & b) ^ (a & c) ^ (b & c));
            v = [t1.wrapping_add(t2), a, b, c, d.wrapping_add(t1), e, f, g];
        }
        for (x, y) in h.iter_mut().zip(v) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (bytes, x) in out.chunks_exact_mut(4).zip(h) {
        bytes.copy_from_slice(&x.to_be_bytes());
    }
    out
}

/// What `--settle` waits for: the background threads used under this share of
/// a core...
const QUIET_SHARE: f64 = 0.05;
/// ...for this long in a row.
const QUIET_FOR: Duration = Duration::from_secs(30);
/// How often their times are read, new ones are looked for, and a line is said.
const SAMPLE: Duration = Duration::from_secs(1);
const LOOK: Duration = Duration::from_secs(30);
const TELL: Duration = Duration::from_secs(60);

/// `--settle`: [`keep`] in a process of its own, below normal priority (the
/// PC stays usable for its up to `minutes` minutes), whose lines are this
/// one's. In it the driver's background work is left on
/// (`VICTORY_D3D12_BACKGROUND=1`, for a PC that could switch it off) and its
/// threads are not put on the slow cores (`VICTORY_BACKGROUND_SLOW=0`),
/// unless those are set otherwise. Whether it ended well.
fn settle_in_a_process(minutes: u64, jobs: usize) -> Result<bool> {
    use std::os::windows::process::CommandExt;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x4000;
    let mut child = Command::new(std::env::current_exe()?);
    child.args(["warm", "--settle-for", &minutes.to_string(), "--jobs", &jobs.to_string()]).stdin(Stdio::null());
    for (name, value) in [("VICTORY_D3D12_BACKGROUND", "1"), ("VICTORY_BACKGROUND_SLOW", "0")] {
        if std::env::var_os(name).is_none() {
            child.env(name, value);
        }
    }
    let status = child.creation_flags(BELOW_NORMAL_PRIORITY_CLASS).status().context("the settling process")?;
    if !status.success() {
        println!("settle: its process ended: {status}");
    }
    Ok(status.success())
}

/// The settling process: makes the pipelines earlier runs drew with (those of
/// `early-pipelines.txt`, but for those in `warm-bad.txt`) on `jobs` threads,
/// as the warm-up's processes do, keeps them with the device, and watches the
/// driver's background threads ([`watch`]); all in `minutes` minutes at most (a
/// compile that hangs while they are made ends the process then).
fn keep(dir: &Path, minutes: u64, jobs: usize) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(minutes.saturating_mul(60));
    let making = AtomicBool::new(true);
    let cfg = victory_core::Config::load()?;
    known::use_translations(cfg.cache_subdir("shaders")?);
    let keys = known::keys(dir);
    let early = std::fs::read_to_string(dir.join("early-pipelines.txt")).unwrap_or_default();
    let early: HashSet<&str> = early.lines().collect();
    let bad = std::fs::read_to_string(dir.join("warm-bad.txt")).unwrap_or_default();
    let bad: HashSet<&str> = bad.lines().filter_map(|l| l.split(' ').next()).collect();
    let which: Vec<usize> =
        (0..keys.len()).filter(|&n| early.contains(keys[n].as_str()) && !bad.contains(keys[n].as_str())).collect();
    if which.is_empty() {
        println!("settle: no pipeline earlier runs drew with (early-pipelines.txt) is known: nothing to keep");
        return Ok(());
    }
    let began = Instant::now();
    let (done, none) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let kept: Vec<known::Kept> = std::thread::scope(|s| {
        let (done, none, all, making) = (&done, &none, which.len(), &making);
        s.spawn(move || {
            while making.load(Ordering::Relaxed) {
                if Instant::now() >= deadline {
                    let done = done.load(Ordering::Relaxed);
                    println!("settle: {minutes} min passed while the pipelines were being made ({done} of {all})");
                    let _ = std::io::stdout().flush();
                    std::process::exit(1);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let makers: Vec<_> = which
            .chunks(all.div_ceil(jobs))
            .map(|part| {
                s.spawn(move || {
                    known::make_kept(dir, part, |_, came| {
                        if let Some((_, there)) = came {
                            none.fetch_add(usize::from(!there), Ordering::Relaxed);
                            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                            if n % 500 == 0 {
                                println!("settle: {n} of {all} made, {:.0} s", began.elapsed().as_secs_f64());
                            }
                        }
                    })
                })
            })
            .collect();
        let kept = makers.into_iter().map(|m| m.join()).collect::<std::thread::Result<_>>();
        making.store(false, Ordering::Relaxed);
        kept
    })
    .map_err(|_| anyhow::anyhow!("a thread that made pipelines to keep failed"))?;
    let pipelines: usize = kept.iter().map(known::Kept::len).sum();
    println!(
        "settle: {pipelines} of {} pipelines made and kept in {:.0} s ({} have no host pipeline); the driver's \
         background threads are watched now, to {minutes} min from the start at most",
        which.len(),
        began.elapsed().as_secs_f64(),
        none.into_inner()
    );
    watch(deadline, minutes, pipelines);
    // Without what a run's end does, as `make` (and the pipelines are not let go
    // one by one: they stay alive to the end).
    let _ = std::io::stdout().flush();
    std::process::exit(0)
}

/// Reads the processor time of this process's "D3D Background Thread"s once a
/// [`SAMPLE`] (looking for new ones every [`LOOK`]) until [`Quiet`] says they
/// are idle or the `deadline` (`minutes` from the start) passed, with a line
/// every [`TELL`]: how many threads, what they used, the `pipelines` kept.
/// Unless `VICTORY_BACKGROUND_SLOW` asks for the slow cores, each thread found
/// runs at normal priority on every core.
fn watch(deadline: Instant, minutes: u64, pipelines: usize) {
    let let_run = !std::env::var("VICTORY_BACKGROUND_SLOW").is_ok_and(|v| v.trim() != "0");
    let mut threads = known::BackgroundThreads::new();
    threads.look(let_run);
    let total = |t: &[(u32, Duration)]| t.iter().map(|t| t.1).sum::<Duration>();
    let before = total(&threads.used());
    println!(
        "settle: {} background threads, {:.0} s of processor time used by them so far",
        threads.len(),
        before.as_secs_f64()
    );
    let began = Instant::now();
    let mut quiet = Quiet::default();
    let (mut looked, mut told, mut used_then) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    loop {
        let at = began.elapsed();
        let now = threads.used();
        let settled = quiet.sample(at, &now);
        let all = total(&now).as_secs_f64();
        if settled {
            println!(
                "settle: background threads idle after {:.0} s (under 5 % of a core since {:.0} s): {} threads, \
                 {:.0} s of processor time since the pipelines were made, {all:.0} s in all; {pipelines} \
                 pipelines kept",
                at.as_secs_f64(),
                at.saturating_sub(QUIET_FOR).as_secs_f64(),
                threads.len(),
                quiet.used.as_secs_f64()
            );
            return;
        }
        if Instant::now() >= deadline {
            println!(
                "settle: background threads still busy after {minutes} min: {} threads, {:.0} s of processor time \
                 since the pipelines were made, {all:.0} s in all, quiet for the last {:.0} s; {pipelines} \
                 pipelines kept",
                threads.len(),
                quiet.used.as_secs_f64(),
                quiet.quiet.as_secs_f64()
            );
            return;
        }
        if at.saturating_sub(looked) >= LOOK {
            threads.look(let_run);
            looked = at;
        }
        if at.saturating_sub(told) >= TELL {
            let share = (quiet.used - used_then).as_secs_f64() / (at - told).as_secs_f64();
            println!(
                "settle: {:.0} min, {} background threads, {:.0} s of processor time since the pipelines were made, \
                 {:.0} % of a core in the last minute; {pipelines} pipelines kept",
                at.as_secs_f64() / 60.0,
                threads.len(),
                quiet.used.as_secs_f64(),
                share * 100.0
            );
            (told, used_then) = (at, quiet.used);
        }
        std::thread::sleep(SAMPLE);
    }
}

/// The driver's background threads, sample after sample: how long they have
/// used under [`QUIET_SHARE`] of a core in a row, and what they used.
#[derive(Default)]
struct Quiet {
    /// Each thread's processor time at the last sample, by id.
    last: HashMap<u32, Duration>,
    /// When the last sample was taken (since the watch began); none before the
    /// first.
    at: Option<Duration>,
    /// How long they have been quiet in a row, up to the last sample.
    quiet: Duration,
    /// Processor time they used since the first sample.
    used: Duration,
}

impl Quiet {
    /// Takes the threads' processor times so far, `now` (id, time), read at
    /// `at`: whether they have been quiet for [`QUIET_FOR`] now. The first
    /// sample is where the count starts; a thread first seen later counts all
    /// of its time (it may have worked before it was found).
    fn sample(&mut self, at: Duration, now: &[(u32, Duration)]) -> bool {
        let first = self.at.is_none();
        let mut used = Duration::ZERO;
        for &(id, time) in now {
            let before = self.last.insert(id, time).unwrap_or(if first { time } else { Duration::ZERO });
            used += time.saturating_sub(before);
        }
        self.used += used;
        let span = at.saturating_sub(self.at.unwrap_or(at));
        self.at = Some(at);
        if !span.is_zero() {
            let busy = used.as_secs_f64() >= QUIET_SHARE * span.as_secs_f64();
            self.quiet = if busy { Duration::ZERO } else { self.quiet + span };
        }
        self.quiet >= QUIET_FOR
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Has a process make `these`; how many it made, and how it ended if not well.
fn child(these: &[usize], mut each: impl FnMut(usize, f64, bool)) -> (usize, Option<String>) {
    let list: Vec<String> = these.iter().map(usize::to_string).collect();
    let spawned = std::env::current_exe().and_then(|exe| {
        use std::os::windows::process::CommandExt;
        const IDLE_PRIORITY_CLASS: u32 = 0x40;
        Command::new(exe)
            .args(["warm", "--these", &list.join(",")])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(IDLE_PRIORITY_CLASS)
            .spawn()
    });
    let Ok(mut process) = spawned else { return (these.len(), None) };
    let Some(out) = process.stdout.take() else { return (these.len(), None) };
    let process = Mutex::new(process);
    let (heard, over) = (Mutex::new(Instant::now()), AtomicBool::new(false));
    let hung = AtomicBool::new(false);
    let mut made = 0;
    std::thread::scope(|s| {
        s.spawn(|| {
            while !over.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if lock(&heard).elapsed() > LONGEST {
                    hung.store(true, Ordering::Relaxed);
                    let _ = lock(&process).kill();
                    return;
                }
            }
        });
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            *lock(&heard) = Instant::now();
            let mut words = line.split(' ');
            if words.next() == Some("made")
                && let (Some(n), Some(ms), Some(there)) = (words.next(), words.next(), words.next())
                && let (Ok(n), Ok(ms)) = (n.parse(), ms.parse())
            {
                made += 1;
                each(n, ms, there == "1");
            }
        }
        over.store(true, Ordering::Relaxed);
    });
    let status = lock(&process).wait();
    let well = status.as_ref().is_ok_and(|s| s.success());
    if well {
        return (these.len(), None);
    }
    let why = if hung.load(Ordering::Relaxed) {
        format!("hung for {} s", LONGEST.as_secs())
    } else {
        status.map_or_else(|e| e.to_string(), |s| format!("{s}"))
    };
    (made, Some(why))
}

/// What such a process does.
fn make(dir: &Path, these: &str) -> Result<()> {
    let these: Vec<usize> = these.split(',').map(|n| n.parse().context("--these")).collect::<Result<_>>()?;
    let cfg = victory_core::Config::load()?;
    victory_hle::known_pipelines::use_translations(cfg.cache_subdir("shaders")?);
    let mut out = std::io::stdout();
    known::make(dir, &these, |n, came| {
        let told = match came {
            None => writeln!(out, "at {n}"),
            Some((ms, there)) => writeln!(out, "made {n} {ms:.2} {}", u8::from(there)),
        };
        if told.and_then(|()| out.flush()).is_err() {
            // Whoever asked is gone.
            std::process::exit(1);
        }
    });
    // Without what a run's end does (nothing to save, the host's threads).
    let _ = out.flush();
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-4's examples, the empty message, and one of 1 MiB + 1 against
    /// a sum taken elsewhere (Python's hashlib).
    #[test]
    fn sha256_known_answers() {
        let hex = |d: [u8; 32]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(
            hex(sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let long: Vec<u8> = (0..(1u32 << 20) + 1).map(|i| (i % 251) as u8).collect();
        assert_eq!(hex(sha256(&long)), "5769f52bc3eef28afa39c6fc68cadb7d0bd69812ae3a3d71452f519ec3c7aa56");
    }

    /// The stamp names the file, its size and time and the hash of its first
    /// MiB only.
    #[test]
    fn the_stamp_of_a_file() {
        let path = std::env::temp_dir().join(format!("victory-stamp-test-{}.bin", std::process::id()));
        let bytes: Vec<u8> = (0..(1u32 << 20) + 1000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let text = stamp_text(&path, true, UNIX_EPOCH + Duration::from_secs(1_800_000_000)).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::remove_file(&path).unwrap();
        let field = |name: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{name}="))).unwrap().to_string();
        assert_eq!(field("exe"), path.display().to_string());
        assert_eq!(field("build"), BUILD);
        assert_eq!(field("size"), bytes.len().to_string());
        assert_eq!(field("modified"), modified.duration_since(UNIX_EPOCH).unwrap().as_secs().to_string());
        // (scripts\play.ps1 -SelfTest checks .NET's hash of the same bytes.)
        assert_eq!(field("head_sha256"), "631b84027d6b9e52b539c4e8373622d23032dfadc64d60af87339c9037e4f769");
        assert_eq!((field("settled"), field("warmed")), ("yes".to_string(), "1800000000".to_string()));
        assert!(text.starts_with('#'), "a comment line first");
    }

    /// Samples one a second from `from` to `to` (seconds since the watch
    /// began), each of the threads in `times` using `ms` of processor time a
    /// second meanwhile: the second at which [`Quiet`] said they were idle.
    fn feed(quiet: &mut Quiet, times: &mut [(u32, Duration)], seconds: std::ops::Range<u64>, ms: u64) -> Option<u64> {
        for second in seconds {
            if second > 0 {
                for t in times.iter_mut() {
                    t.1 += Duration::from_millis(ms);
                }
            }
            if quiet.sample(Duration::from_secs(second), times) {
                return Some(second);
            }
        }
        None
    }

    /// Four threads busy (0.78 of a core each) for 100 s,
    /// then nearly idle: idle 30 s after they stopped, with all they used.
    #[test]
    fn busy_threads_that_stop_are_idle_thirty_seconds_later() {
        let mut quiet = Quiet::default();
        let mut times: Vec<(u32, Duration)> = (1..=4).map(|id| (id, Duration::from_secs(600))).collect();
        assert_eq!(feed(&mut quiet, &mut times, 0..101, 780), None);
        assert_eq!(quiet.used, Duration::from_millis(4 * 780 * 100), "the time before the first sample not counted");
        assert_eq!(feed(&mut quiet, &mut times, 101..200, 2), Some(130));
        assert_eq!(quiet.quiet, QUIET_FOR);
    }

    /// One busy second in a quiet stretch starts the count again.
    #[test]
    fn a_busy_second_starts_the_quiet_count_again() {
        let mut quiet = Quiet::default();
        let mut times = vec![(7, Duration::ZERO)];
        assert_eq!(feed(&mut quiet, &mut times, 0..21, 1), None);
        assert_eq!(quiet.quiet, Duration::from_secs(20));
        assert_eq!(feed(&mut quiet, &mut times, 21..22, 200), None);
        assert_eq!(quiet.quiet, Duration::ZERO);
        assert_eq!(feed(&mut quiet, &mut times, 22..100, 1), Some(51));
    }

    /// Under 5 % of a core is quiet; 5 % is not.
    #[test]
    fn under_five_percent_of_a_core_is_quiet() {
        let mut quiet = Quiet::default();
        assert_eq!(feed(&mut quiet, &mut [(1, Duration::ZERO)], 0..100, 49), Some(30));
        let mut quiet = Quiet::default();
        assert_eq!(feed(&mut quiet, &mut [(1, Duration::ZERO)], 0..100, 50), None);
        // Two threads of 3 % each are 6 % together.
        let mut quiet = Quiet::default();
        assert_eq!(feed(&mut quiet, &mut [(1, Duration::ZERO), (2, Duration::ZERO)], 0..100, 30), None);
    }

    /// A thread found later counts all its time; one that is gone counts
    /// nothing more; no threads at all are idle after 30 s.
    #[test]
    fn threads_found_late_or_gone() {
        let mut quiet = Quiet::default();
        assert_eq!(feed(&mut quiet, &mut [(1, Duration::ZERO)], 0..11, 0), None);
        // At 11 s a new one with 2 s behind it.
        let mut times = vec![(1, Duration::ZERO), (2, Duration::from_secs(2))];
        assert!(!quiet.sample(Duration::from_secs(11), &times));
        assert_eq!((quiet.quiet, quiet.used), (Duration::ZERO, Duration::from_secs(2)));
        // Thread 2 ends: its time stays (it is still read) and adds nothing.
        assert_eq!(feed(&mut quiet, &mut times[..1], 12..100, 0), Some(41));
        times.clear();
        let mut quiet = Quiet::default();
        assert_eq!(feed(&mut quiet, &mut times, 0..100, 0), Some(30));
    }

    /// Samples that come late, or twice at one time, count for the time between.
    #[test]
    fn samples_at_uneven_times() {
        let mut quiet = Quiet::default();
        let at = |s: f64| Duration::from_secs_f64(s);
        let one = [(1, Duration::ZERO)];
        assert!(!quiet.sample(at(0.0), &one));
        assert!(!quiet.sample(at(2.5), &[(1, Duration::from_millis(100))]));
        assert_eq!(quiet.quiet, at(2.5), "4 % of 2.5 s");
        assert!(!quiet.sample(at(2.5), &[(1, Duration::from_millis(100))]));
        assert!(!quiet.sample(at(20.0), &[(1, Duration::from_millis(100))]));
        assert!(quiet.sample(at(30.0), &[(1, Duration::from_millis(400))]));
    }
}
