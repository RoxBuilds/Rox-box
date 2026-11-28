//! The title's dispatches by pipeline (`VICTORY_GPU_SPLIT`, `pipelines`, on by
//! default): the card's time and the dispatches of each compute pipeline, the
//! log's table of the costliest once a second and over the steady state at the
//! end, and for the run report only figures that name nothing (how many
//! pipelines ran, the time of the costliest 1, 5 and 20).
//!
//! How. With pipelines on, a run of the title's dispatches begins where the
//! pipeline changes as well as where the kind does ([`super::Marks::mark_dispatch`]),
//! so each run is of one pipeline, and it counts what its dispatches asked
//! for (direct ones and their groups, indirect ones). When a submission lands
//! ([`fold`]) each run's time goes to its pipeline as well as to its kind.
//! Late in a frame's budget a change of pipeline alone is not cut: such a run
//! is of pipelines not told apart ([`MIXED`]); the time by kind stays exact.
//!
//! Which programs have loops ([`Loops`]): from the console program's code,
//! looked at once per pipeline by the thread that puts the table together
//! (one of its own, `VICTORY_PIPE_TABLE_ASIDE`; else the line's, at most 20 ms
//! of it a second), the costliest first, never while recording:
//! a backward branch is a loop (the translator caps every compute loop at its
//! loop limit, 4,096 iterations by default); a loop whose body reads memory
//! (loads or atomics) and stores none is shaped as a poll of memory. A guess
//! by shape, not a proof that it waits for anything.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::Mark;
use crate::pipeline::Shader;
use crate::pipeline::host::HostPipeline;

/// A pipeline of the title's dispatches: its place in the table.
pub(crate) type Pipe = u32;
/// Not of one of the title's dispatches.
pub(crate) const NO_PIPE: Pipe = u32::MAX;
/// Of the title's dispatches of pipelines not told apart (a change of
/// pipeline alone late in the frame's budget, or a table that is full).
pub(crate) const MIXED: Pipe = u32::MAX - 1;
/// The most pipelines the table keeps apart.
const MOST: usize = 1 << 16;
/// Pipelines a second kept for the steady state's table (the rest summed).
const KEPT_A_SECOND: usize = 64;
/// Seconds kept for it.
const SECONDS_KEPT: usize = 7200;
/// Pipelines in a line.
const TOP: usize = 20;

/// What the dispatches of a run asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Asked {
    pub(crate) direct: u32,
    pub(crate) indirect: u32,
    /// The groups of the direct ones.
    pub(crate) groups: u64,
}

impl Asked {
    /// One more dispatch, of `groups` groups (`None`: indirect).
    pub(crate) fn add(&mut self, groups: Option<u64>) {
        match groups {
            Some(g) => {
                self.direct = self.direct.saturating_add(1);
                self.groups = self.groups.saturating_add(g);
            }
            None => self.indirect = self.indirect.saturating_add(1),
        }
    }
}

/// What a compute program's loops are, by its console code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Loops {
    /// The code could not be looked at.
    Unknown,
    None,
    /// A loop (capped by the translator's loop limit).
    Loop,
    /// A loop whose body reads memory and stores none: shaped as a poll.
    Poll,
}

impl Loops {
    fn word(self) -> &'static str {
        match self {
            Loops::Unknown => "loops not known (yet)",
            Loops::None => "no loop",
            Loops::Loop => "a loop",
            Loops::Poll => "a loop shaped as a poll of memory",
        }
    }
}

/// A pipeline of the table.
struct Info {
    /// Its shaders as the runtime logs them (`[compute <hash>]`).
    name: String,
    wave64: bool,
    /// Threads a group.
    threads: u64,
    shader: Option<Arc<Shader>>,
    loops: OnceLock<Loops>,
}

impl Info {
    /// Its loops, looked at now if `until` has not passed (the line's time
    /// for it: the line runs on the thread that counts frames); else not
    /// known yet.
    fn loops(&self, until: std::time::Instant) -> Loops {
        if let Some(&known) = self.loops.get() {
            return known;
        }
        if std::time::Instant::now() >= until {
            return Loops::Unknown;
        }
        *self.loops.get_or_init(|| self.shader.as_deref().map_or(Loops::Unknown, loops_of))
    }
}

/// The time a line may spend looking at programs' loops it has not seen.
const LOOK_BUDGET: std::time::Duration = std::time::Duration::from_millis(20);

#[derive(Default)]
struct Table {
    /// By the host pipeline object's address.
    by_key: HashMap<usize, Pipe>,
    infos: Vec<Arc<Info>>,
}

static TABLE: Mutex<Option<Table>> = Mutex::new(None);

/// The pipeline of host pipeline `p` (at `key`, its object's address), put
/// in the table at its first dispatch. (An address used again by another
/// pipeline gets a place of its own: the name differs.)
pub(super) fn pipe(key: usize, p: &HostPipeline) -> Pipe {
    let mut table = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let table = table.get_or_insert_with(Table::default);
    if let Some(&at) = table.by_key.get(&key)
        && table.infos.get(at as usize).is_some_and(|i| i.name == p.name)
    {
        return at;
    }
    if table.infos.len() >= MOST {
        return MIXED;
    }
    let stage = p.stages.first();
    let translated = stage.map(|s| s.translated());
    let info = Info {
        name: p.name.clone(),
        wave64: translated.is_some_and(|t| t.wave64),
        threads: translated.and_then(|t| t.threads).map_or(0, |t| t.iter().map(|&n| u64::from(n)).product()),
        shader: stage.map(|s| s.shader.clone()),
        loops: OnceLock::new(),
    };
    let at = table.infos.len() as Pipe;
    table.infos.push(Arc::new(info));
    table.by_key.insert(key, at);
    at
}

fn info(pipe: Pipe) -> Option<Arc<Info>> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.infos.get(pipe as usize).cloned()
}

/// A pipeline's card time (ns) and dispatches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sum {
    ns: u64,
    direct: u64,
    indirect: u64,
    groups: u64,
}

impl Sum {
    fn add(&mut self, other: &Sum) {
        self.ns += other.ns;
        self.direct += other.direct;
        self.indirect += other.indirect;
        self.groups += other.groups;
    }
}

/// Since the last line.
static SECOND: Mutex<Option<HashMap<Pipe, Sum>>> = Mutex::new(None);

/// A landed submission's runs ([`super::Pending::fold`]): the time between
/// each timestamp and the next goes to the pipeline of the run it begins (a
/// timestamp not written or out of order gives that run no time, as for the
/// kinds), with what its dispatches asked for.
pub(super) fn fold(ticks: &[u64], marks: &[Mark], frequency: u64) {
    let longest = frequency.saturating_mul(10);
    let mut second = SECOND.lock().unwrap_or_else(|e| e.into_inner());
    let second = second.get_or_insert_with(HashMap::new);
    for (pair, mark) in ticks.windows(2).zip(marks) {
        if mark.pipe == NO_PIPE {
            continue;
        }
        let (a, b) = (pair[0], pair[1]);
        let ticks = if a == 0 || b == 0 || b < a || b - a > longest { 0 } else { b - a };
        let sum = second.entry(mark.pipe).or_default();
        sum.ns += (u128::from(ticks) * 1_000_000_000 / u128::from(frequency.max(1))) as u64;
        sum.direct += u64::from(mark.dispatches.direct);
        sum.indirect += u64::from(mark.dispatches.indirect);
        sum.groups += mark.dispatches.groups;
    }
}

/// A second of the line: its frames and its pipelines, the costliest first
/// (at most [`KEPT_A_SECOND`]), and the rest summed.
struct Kept {
    frames: f64,
    measured_ns: u64,
    top: Vec<(Pipe, Sum)>,
}

static SECONDS: Mutex<Vec<Kept>> = Mutex::new(Vec::new());

/// The pipelines of a second, the costliest first; [`MIXED`] apart.
fn ranked(sums: HashMap<Pipe, Sum>) -> (Vec<(Pipe, Sum)>, Sum) {
    let mut mixed = Sum::default();
    let mut all: Vec<(Pipe, Sum)> = sums
        .into_iter()
        .filter(|(pipe, sum)| {
            if *pipe == MIXED {
                mixed.add(sum);
            }
            *pipe != MIXED
        })
        .collect();
    all.sort_by(|a, b| b.1.ns.cmp(&a.1.ns).then(a.0.cmp(&b.0)));
    (all, mixed)
}

/// The report's figures of a second's pipelines: how many ran, and the card
/// time (ns) of the costliest 1, 5 and 20 of them.
fn figures(ranked: &[(Pipe, Sum)]) -> [u64; 4] {
    let top = |n: usize| ranked.iter().take(n).map(|(_, s)| s.ns).sum::<u64>();
    [ranked.len() as u64, top(1), top(5), top(TOP)]
}

/// The line's second of `frames` frames, whose card time measured was
/// `measured_ns`: what the report takes ([`figures`]), kept for the steady
/// state, and with `log` the table in the log.
pub(super) fn second(frames: f64, measured_ns: u64, log: bool) -> [u64; 4] {
    let sums = SECOND.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    if sums.is_empty() {
        return [0; 4];
    }
    let (ranked, mixed) = ranked(sums);
    let out = figures(&ranked);
    if log && frames > 0.0 && !(table_aside() && aside::give(frames, measured_ns, ranked.clone(), mixed)) {
        let until = std::time::Instant::now() + LOOK_BUDGET;
        tracing::info!("{}", table_line("a second", frames, measured_ns, &ranked, &mixed, until));
    }
    let mut top = ranked;
    let mut rest = mixed;
    for (_, s) in top.iter().skip(KEPT_A_SECOND) {
        rest.add(s);
    }
    top.truncate(KEPT_A_SECOND);
    top.push((MIXED, rest));
    let mut seconds = SECONDS.lock().unwrap_or_else(|e| e.into_inner());
    if seconds.len() < SECONDS_KEPT {
        seconds.push(Kept { frames, measured_ns, top });
    }
    out
}

/// Whether the log's table of each second is put together on a thread of its
/// own (`VICTORY_PIPE_TABLE_ASIDE`, on unless `0`; `0`: by the frame line, as
/// before).
///
/// The table looks at the console code of the pipelines whose loops are not
/// known yet (once each: [`Info::loops`]). By the frame line that was up to
/// [`LOOK_BUDGET`] of the submitting thread once a second, right after a
/// present, for as long as new pipelines came: a profile had it at 3 ms a
/// frame of that thread, minutes into a run. Aside the thread has no budget
/// but [`ASIDE_BUDGET`] a table, and the frame line only hands it the second's
/// figures. What changes: the table is said a little later, by another
/// thread, and names fewer programs "not known (yet)". The report's figures
/// ([`figures`]) are the frame line's, as before.
fn table_aside() -> bool {
    crate::toggled!("VICTORY_PIPE_TABLE_ASIDE")
}

/// The time the thread aside may spend looking at loops for one table (it is
/// behind nothing, but tables queue behind it).
const ASIDE_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// The tables' thread ([`table_aside`]).
mod aside {
    use super::{ASIDE_BUDGET, Pipe, Sum, table_line};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock, mpsc};

    type Table = (f64, u64, Vec<(Pipe, Sum)>, Sum);

    /// Microseconds the thread spent on tables since [`taken_us`] was last
    /// asked.
    static SPENT_US: AtomicU64 = AtomicU64::new(0);

    /// Hands a second's table to the thread; `false` if there is none (it
    /// could not be made), and the caller says it.
    pub(super) fn give(frames: f64, measured_ns: u64, ranked: Vec<(Pipe, Sum)>, mixed: Sum) -> bool {
        static THREAD: OnceLock<Option<Mutex<mpsc::Sender<Table>>>> = OnceLock::new();
        let thread = THREAD.get_or_init(|| {
            let (send, tables) = mpsc::channel::<Table>();
            std::thread::Builder::new()
                .name("victory-pipe-table".into())
                .spawn(move || {
                    for table in tables {
                        let began = std::time::Instant::now();
                        let line = line_of(&table);
                        tracing::info!("{line}");
                        SPENT_US.fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                })
                .ok()
                .map(|_| Mutex::new(send))
        });
        thread
            .as_ref()
            .is_some_and(|send| send.lock().unwrap_or_else(|e| e.into_inner()).send((frames, measured_ns, ranked, mixed)).is_ok())
    }

    /// What the thread says of a table.
    pub(super) fn line_of((frames, measured_ns, ranked, mixed): &Table) -> String {
        table_line("a second", *frames, *measured_ns, ranked, mixed, std::time::Instant::now() + ASIDE_BUDGET)
    }

    /// [`SPENT_US`], taken.
    pub(super) fn taken_us() -> u64 {
        SPENT_US.swap(0, Ordering::Relaxed)
    }
}

/// Microseconds the tables' thread spent since the last call: work the
/// submitting thread did before (`VICTORY_PIPE_TABLE_ASIDE`).
pub(super) fn aside_us() -> u64 {
    aside::taken_us()
}

/// The log's table: the costliest [`TOP`] of `ranked` per frame, and the
/// dispatches of programs with loops; loops not known yet are looked at
/// until `until`.
fn table_line(
    over: &str,
    frames: f64,
    measured_ns: u64,
    ranked: &[(Pipe, Sum)],
    mixed: &Sum,
    until: std::time::Instant,
) -> String {
    let per = |v: u64| v as f64 / frames.max(1e-9);
    let ms = |ns: u64| per(ns) / 1e6;
    let share = |ns: u64| 100.0 * ns as f64 / measured_ns.max(1) as f64;
    let mut text = format!(
        "the card's time per frame by compute pipeline (VICTORY_GPU_SPLIT pipelines; {over}, {frames:.0} frames, \
         {:.2} ms measured): the costliest {} of {} that ran:",
        ms(measured_ns),
        ranked.len().min(TOP),
        ranked.len()
    );
    let (mut looped, mut polled, mut unknown) = (Sum::default(), Sum::default(), Sum::default());
    for (i, (pipe, sum)) in ranked.iter().enumerate() {
        let info = info(*pipe);
        let loops = info.as_ref().map_or(Loops::Unknown, |i| i.loops(until));
        match loops {
            Loops::Loop => looped.add(sum),
            Loops::Poll => {
                looped.add(sum);
                polled.add(sum);
            }
            Loops::Unknown => unknown.add(sum),
            Loops::None => {}
        }
        if i >= TOP {
            continue;
        }
        let (name, wave64, threads) =
            info.as_ref().map_or(("?", false, 0), |i| (i.name.as_str(), i.wave64, i.threads));
        text.push_str(&format!(
            " {}. {name} {:.2} ms {:.1}%, {:.1} dispatches ({:.0} groups, {:.0} threads; {:.1} indirect), {}, {};",
            i + 1,
            ms(sum.ns),
            share(sum.ns),
            per(sum.direct + sum.indirect),
            per(sum.groups),
            per(sum.groups) * threads as f64,
            per(sum.indirect),
            if wave64 { "64-lane" } else { "not 64-lane" },
            loops.word(),
        ));
    }
    text.push_str(&format!(
        " pipelines not told apart (past the budget's cut for them) {:.2} ms {:.1} dispatches; programs with a loop: \
         {:.1} dispatches {:.2} ms, of them with a loop shaped as a poll of memory {:.1} dispatches {:.2} ms;          programs whose loops are not known (yet) {:.1} dispatches {:.2} ms",
        ms(mixed.ns),
        per(mixed.direct + mixed.indirect),
        per(looped.direct + looped.indirect),
        ms(looped.ns),
        per(polled.direct + polled.indirect),
        ms(polled.ns),
        per(unknown.direct + unknown.indirect),
        ms(unknown.ns),
    ));
    text
}

/// At the run's end (once): the table over the steady state, the last two
/// thirds of the seconds that had pipelines (by seconds, as a short run's
/// `[frame_time.steady]`).
pub(super) fn steady_line() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let seconds = SECONDS.lock().unwrap_or_else(|e| e.into_inner());
    let Some((frames, measured, sums)) = steady(&seconds) else {
        return;
    };
    let (ranked, mixed) = ranked(sums);
    let until = std::time::Instant::now() + LOOK_BUDGET;
    tracing::info!("{}", table_line("the steady state, the run's last two thirds of seconds", frames, measured, &ranked, &mixed, until));
}

/// The steady state of `seconds`: its frames, card time measured, and the
/// pipelines' sums. `None` without a second of frames after the first third.
fn steady(seconds: &[Kept]) -> Option<(f64, u64, HashMap<Pipe, Sum>)> {
    let from = seconds.len() / 3;
    let (mut frames, mut measured, mut sums) = (0.0, 0u64, HashMap::<Pipe, Sum>::new());
    for kept in &seconds[from..] {
        frames += kept.frames;
        measured += kept.measured_ns;
        for (pipe, sum) in &kept.top {
            sums.entry(*pipe).or_default().add(sum);
        }
    }
    (frames > 0.0).then_some((frames, measured, sums))
}

// ---------------------------------------------------------------------------
// Loops, from the console program's code.

/// What `shader`'s compute program's loops are.
fn loops_of(shader: &Shader) -> Loops {
    let Some(programs) = shader.programs() else {
        return Loops::Unknown;
    };
    let Some(cs) = programs.iter().find(|p| p.stage == victory_shader::HwStage::Cs) else {
        return Loops::Unknown;
    };
    let Ok(program) = victory_shader::ir::build(&cs.code, cs.wave64, cs.vgprs) else {
        return Loops::Unknown;
    };
    let shapes: Vec<Shape> = program
        .blocks
        .iter()
        .map(|b| {
            let names = || b.insts.iter().map(|i| i.name());
            Shape {
                offset: b.offset,
                next: b.exit.successors(),
                reads: names().any(reads_memory),
                stores: names().any(stores_memory),
            }
        })
        .collect();
    loops_in(&shapes)
}

/// A block as [`loops_in`] looks at it.
#[derive(Clone, Debug)]
struct Shape {
    /// Where its code begins.
    offset: usize,
    /// The blocks it goes on to.
    next: Vec<usize>,
    reads: bool,
    stores: bool,
}

/// A vector memory read: a load of a buffer, of memory or of an image, or an
/// atomic (it reads what it changes).
fn reads_memory(name: &str) -> bool {
    ["buffer_load", "global_load", "flat_load", "image_load"].iter().any(|p| name.starts_with(p)) || name.contains("_atomic")
}

fn stores_memory(name: &str) -> bool {
    ["buffer_store", "global_store", "flat_store", "image_store"].iter().any(|p| name.starts_with(p))
}

/// The loops of `blocks`: a branch to a block whose code is not after the
/// branching block's is a loop, its body the blocks whose code lies from the
/// target's to the branching block's.
fn loops_in(blocks: &[Shape]) -> Loops {
    let mut out = Loops::None;
    for b in blocks {
        for &n in &b.next {
            let Some(head) = blocks.get(n) else { continue };
            if head.offset > b.offset {
                continue;
            }
            let body = || blocks.iter().filter(|x| (head.offset..=b.offset).contains(&x.offset));
            if body().any(|x| x.reads) && !body().any(|x| x.stores) {
                return Loops::Poll;
            }
            out = Loops::Loop;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(offset: usize, next: &[usize], reads: bool, stores: bool) -> Shape {
        Shape { offset, next: next.to_vec(), reads, stores }
    }

    /// A straight program has no loop; a backward branch is one; a loop that
    /// reads and stores nothing is shaped as a poll, one that stores is not.
    #[test]
    fn loops_are_told_by_their_branches() {
        let straight = [shape(0, &[1], true, false), shape(8, &[2], false, true), shape(16, &[], false, false)];
        assert_eq!(loops_in(&straight), Loops::None);
        // 0 -> 1 -> (1 or 2): block 1 loops on itself and reads.
        let poll = [shape(0, &[1], false, false), shape(8, &[1, 2], true, false), shape(16, &[], false, true)];
        assert_eq!(loops_in(&poll), Loops::Poll);
        let work = [shape(0, &[1], false, false), shape(8, &[2], true, false), shape(16, &[1, 3], false, true), shape(24, &[], false, false)];
        assert_eq!(loops_in(&work), Loops::Loop);
        // A loop with no memory read at all is a loop, not a poll.
        let count = [shape(0, &[0, 1], false, false), shape(4, &[], true, false)];
        assert_eq!(loops_in(&count), Loops::Loop);
        assert!(reads_memory("buffer_load_dword") && reads_memory("global_atomic_add") && !reads_memory("s_load_dword"));
        assert!(stores_memory("global_store_dword") && !stores_memory("ds_write_b32"));
    }

    fn sum(ns: u64) -> Sum {
        Sum { ns, direct: 1, indirect: 0, groups: 4 }
    }

    /// The costliest first, the pipelines not told apart kept aside, and
    /// the report's figures of the costliest 1, 5 and 20.
    #[test]
    fn pipelines_are_ranked_and_summed() {
        let mut sums: HashMap<Pipe, Sum> = (0..30).map(|p| (p, sum(u64::from(p) * 10))).collect();
        sums.insert(MIXED, sum(1000));
        let (ranked, mixed) = ranked(sums);
        assert_eq!(mixed.ns, 1000);
        assert_eq!(ranked.len(), 30);
        assert_eq!(ranked[0], (29, sum(290)));
        assert_eq!(figures(&ranked), [30, 290, 290 + 280 + 270 + 260 + 250, (10..30).map(|p| p * 10).sum()]);
        assert_eq!(figures(&[]), [0; 4]);
    }

    /// A run's time goes to its pipeline with what it asked for; runs of no
    /// pipeline and timestamps out of order give none.
    #[test]
    fn runs_fold_into_their_pipelines() {
        let mut asked = Asked::default();
        asked.add(Some(8));
        asked.add(None);
        let m = |pipe: Pipe, dispatches: Asked| Mark { slot: 0, kind: super::super::Kind::Dispatches, pipe, dispatches };
        let marks = [m(7_000, asked), m(NO_PIPE, Asked::default()), m(7_000, asked), m(7_001, asked)];
        let ticks = [100, 150, 160, 400, 390];
        SECOND.lock().unwrap().take();
        fold(&ticks, &marks, 1_000_000_000);
        let got = SECOND.lock().unwrap().take().unwrap();
        // 50 + 240 ns for 7,000 (two runs); 7,001's timestamp is out of order.
        assert_eq!(got[&7_000], Sum { ns: 290, direct: 2, indirect: 2, groups: 16 });
        assert_eq!(got[&7_001].ns, 0);
        assert!(!got.contains_key(&NO_PIPE));
    }

    /// The table the thread aside says is the one the frame line said: the
    /// same words of the same second, whichever thread puts it together.
    #[test]
    fn the_table_aside_is_the_table_said_inline() {
        // Pipelines no dispatch put in the table: their loops are not known
        // on either thread, whatever the budget.
        let ranked: Vec<(Pipe, Sum)> = (60_000..60_030).rev().map(|p| (p, sum(u64::from(p - 59_990) * 1000))).collect();
        let mixed = sum(5000);
        let inline = table_line("a second", 12.0, 2_000_000, &ranked, &mixed, std::time::Instant::now() + LOOK_BUDGET);
        let table = (12.0, 2_000_000, ranked.clone(), mixed);
        let aside = std::thread::spawn(move || aside::line_of(&table)).join().unwrap();
        assert_eq!(aside, inline);
        assert!(inline.contains("the costliest 20 of 30"));
        // A budget run out leaves loops not known, as before.
        let late = table_line("a second", 12.0, 2_000_000, &ranked, &mixed, std::time::Instant::now());
        assert_eq!(late, inline);
    }

    /// The steady state is the last two thirds of the seconds kept.
    #[test]
    fn the_steady_state_is_the_last_two_thirds() {
        let kept = |frames: f64, ns: u64| Kept { frames, measured_ns: ns, top: vec![(1, sum(ns))] };
        let seconds = [kept(1.0, 900), kept(2.0, 10), kept(3.0, 20)];
        let (frames, measured, sums) = steady(&seconds).unwrap();
        assert_eq!((frames, measured), (5.0, 30));
        assert_eq!(sums[&1].ns, 30);
        assert!(steady(&[]).is_none());
    }
}
