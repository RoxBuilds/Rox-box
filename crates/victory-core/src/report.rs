//! The run report: a small text file every `victory run --execute` writes
//! (`<data_dir>\reports\run-<UTC time>.txt`) that tells a developer where a
//! run's time and failures went, and that is safe to attach to a public issue
//! by construction (docs/run-report.md).
//!
//! The project's rules forbid posting logs, screenshots or dumps of a run:
//! they hold the title's text, names, hashes and addresses. This report is the
//! one thing a player can share, so what it may hold is an allowlist, enforced
//! by the types here rather than by care at each call:
//!
//! - numbers;
//! - [`Ours`]: a `&'static str` written in Rox-box's source (an enum's name, a
//!   reason's wording, one of our interface or method names);
//! - [`HostText`]: what Windows or the processor says of the host (adapter
//!   name, processor model), through a constructor that withholds anything
//!   path-like, long, or hex-like;
//! - names of unimplemented imports of platform DLLs and the names (not the
//!   values) of `VICTORY_*` switches, through their own sanitizers.
//!
//! No public function takes a runtime `String`. Rendering is private and
//! passes every string it writes through [`guarded`] once more, which
//! replaces anything with a backslash, a slash, `:\` or a run of eight or
//! more hex digits by `"<withheld>"`.
//!
//! Feeding it is cheap on purpose: counters are static atomics, per-reason
//! counts are fixed arrays, the tables keyed by our words (methods not
//! written yet, translator failures) are fixed open-addressed arrays of
//! atomics. Nothing on a per-draw or per-frame path allocates or locks. The
//! file itself is rendered and written by a low-priority helper thread every
//! [`EVERY`] (to `.part`, then renamed, so a killed process still leaves a
//! recent report), when the run ends, and on a crash, where the crashing
//! thread only stores the end reason and wakes the helper.
//!
//! `VICTORY_REPORT=0` turns it off: no thread, no file, no host queries.
//!
//! `victory selftest` writes its result through the same renderer and guard
//! (`selftest-<UTC time>.txt` beside the run reports, [`write_selftest`]).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed, Ordering::SeqCst};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// The report format's version, the first key of every report. 2 added the
/// frame line's figures (`[frame_time]`), memory pressure, textures picked by
/// index and pipelines the title went without. 3: `kind` (`run`, or
/// `selftest` for the self-test's report); `[frame_time]` holds the frames
/// of the last part of a second too; a short run's `[frame_time.steady]` is
/// by whole seconds, or left without figures when there is none. 4 added
/// `[memory.map]` and `[memory.map.peak]`: the process's memory by our own
/// categories. 5 added `[gpu_time]` and `[gpu_time.steady]`: the card's time
/// by kind of work, its volumes, and the memory the budget governor saw. 6
/// added to `[gpu_time]` the figures of the title's compute pipelines that
/// name none of them, and `asked_to_leave_out` to `[gpu_work.left_out]`.
pub const FORMAT: u32 = 6;
/// Reports kept in the folder: the newest, this one included.
pub const KEEP: usize = 20;
/// How often the helper thread writes the report while the run goes on.
pub const EVERY: Duration = Duration::from_secs(30);
/// The folder under `data_dir` the reports go to.
pub const FOLDER: &str = "reports";

/// Whether reports are written (`VICTORY_REPORT`, on unless `0`, read once).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| switch_on(std::env::var_os("VICTORY_REPORT").as_deref()))
}

fn switch_on(value: Option<&OsStr>) -> bool {
    value.is_none_or(|v| v != "0")
}

// ---------------------------------------------------------------------------
// What may be written.

/// A word of Rox-box's own source code: an enum name, a reason, one of our
/// interface names. Only string literals belong here; [`guarded`] checks the
/// rendered text again in any case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ours(&'static str);

impl Ours {
    pub const fn new(word: &'static str) -> Ours {
        Ours(word)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// A fact about the host as Windows or the processor states it (adapter
/// name, processor model), cleaned. The one way to make one is
/// [`HostText::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostText(String);

impl HostText {
    /// The longest text kept (a processor's brand string is 48 bytes).
    pub const MAX: usize = 48;

    /// `raw`, trimmed, with any character other than letters, digits and
    /// ` .,()+-_#@&/` replaced by `_`; `"<withheld>"` if it is longer than
    /// [`HostText::MAX`], looks like a path, or holds a run of eight or more
    /// hex digits; `"unknown"` if it is empty.
    pub fn new(raw: &str) -> HostText {
        let raw = raw.trim().trim_end_matches('\0').trim();
        if raw.is_empty() {
            return HostText("unknown".to_owned());
        }
        if raw.chars().count() > Self::MAX || host_path_like(raw) || hex_run(raw) >= 8 {
            return HostText(WITHHELD.to_owned());
        }
        HostText(
            raw.chars()
                .map(|c| if host_char(c) { c } else { '_' })
                .collect(),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || " .,()+-_#@&/".contains(c)
}

/// What every rejected string becomes.
const WITHHELD: &str = "<withheld>";

/// The longest run of ASCII hex digits in `s`.
fn hex_run(s: &str) -> usize {
    let (mut best, mut run) = (0, 0);
    for b in s.bytes() {
        run = if b.is_ascii_hexdigit() { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

/// Path-like for the host's own texts: a backslash, a drive or URL colon
/// (`:\`, `:/`), `//`, a leading slash, or a slash that is not one between
/// two letters or digits (`XT/XTX` is an adapter name, `a/b/c` is a path).
fn host_path_like(s: &str) -> bool {
    if s.contains('\\') || s.contains(":/") || s.contains("//") || s.starts_with('/') {
        return true;
    }
    let b = s.as_bytes();
    let slashes: Vec<usize> = (0..b.len()).filter(|&i| b[i] == b'/').collect();
    slashes.len() > 1
        || slashes.iter().any(|&i| {
            i == 0
                || i + 1 >= b.len()
                || !b[i - 1].is_ascii_alphanumeric()
                || !b[i + 1].is_ascii_alphanumeric()
        })
}

/// The render-time guard: whether `s` may be written as it is. Outside the
/// host section: no backslash, no slash, no `:\`, no run of eight or more hex
/// digits, at most [`GUARD_MAX`] characters, printable ASCII without quotes.
/// In it the same, but a slash between two letters or digits is allowed.
fn guarded(s: &str, host: bool) -> bool {
    let printable = s.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"');
    let slash = if host {
        host_path_like(s)
    } else {
        s.contains('/') || s.contains('\\')
    };
    printable && !slash && !s.contains(":\\") && hex_run(s) < 8 && s.len() <= GUARD_MAX
}

/// The longest string written: the translator's longest wording of a
/// construct it cannot handle, with its kind, is under 90.
const GUARD_MAX: usize = 112;

/// The name of an unimplemented import, as `dll!symbol`, if both parts are
/// plain identifiers (letters, digits, `_.@?$#-`, at most 64 characters each,
/// no run of eight hex digits). Only called for DLLs our registry or host
/// policy knows (see [`set_platform_dll`]).
fn import_name(dll: &str, symbol: &str) -> Option<String> {
    let ident = |s: &str| {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.@?$#-".contains(&b))
            && hex_run(s) < 8
    };
    (ident(dll) && ident(symbol)).then(|| format!("{dll}!{symbol}"))
}

/// The name and value written for a `VICTORY_*` variable, or `None` for
/// any other variable (or a name that is not a plain identifier).
fn switch_entry(name: &OsStr, value: &OsStr) -> Option<(String, String)> {
    let name = name.to_str()?;
    let ok = name.len() <= 48
        && name
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("VICTORY_"))
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !ok {
        return None;
    }
    let name = name.to_ascii_uppercase();
    let shown = value
        .to_str()
        .filter(|v| !path_switch(&name) && plain_value(v))
        .map_or_else(|| "<set>".to_owned(), str::to_owned);
    Some((name, shown))
}

/// Switches whose value is, or may be, a path, a list of hashes or of title
/// offsets: never written, whatever it looks like.
fn path_switch(name: &str) -> bool {
    const NAMED: [&str; 18] = [
        "VICTORY_CONFIG",
        "VICTORY_SHADER_PROFILE",
        "VICTORY_BCPACK_TABLES",
        "VICTORY_TEST_TITLES",
        "VICTORY_PROFILE",
        "VICTORY_CAPTURE",
        "VICTORY_RECORD",
        "VICTORY_XMA_DUMP",
        "VICTORY_AUDIO_CAPTURE",
        "VICTORY_SKIP_PIPELINES",
        "VICTORY_WATCH",
        "VICTORY_POKE",
        "VICTORY_CRASH_SAVE",
        "VICTORY_DXIL",
        "VICTORY_LOG",
        "VICTORY_RELAUNCHED_FROM",
        "VICTORY_AB",
        "VICTORY_TRAP_LOG",
    ];
    NAMED.contains(&name)
        || ["_DIR", "_FILE", "_PATH", "_LOG", "_DUMP", "_TABLES"]
            .iter()
            .any(|s| name.ends_with(s))
        || [
            "CAPTURE",
            "RECORD",
            "PROFILE",
            "HASH",
            "RVA",
            "SAVE",
            "FOLDER",
            "_OUT",
            "DXC",
            "REFERENCE",
            "DUMP",
            "SITES",
            "MATCH",
            "TARGETS",
            "TRACE",
            "WATCH",
            "POKE",
        ]
        .iter()
        .any(|s| name.contains(s))
}

/// A switch value that is written as it is: `^[A-Za-z0-9.,x:+-]{0,24}$`, and
/// nothing like a file name (a dot before a letter), a hex number (`0x`, or
/// five or more hex digits with both a letter and a digit among them).
fn plain_value(v: &str) -> bool {
    let b = v.as_bytes();
    let charset = v.len() <= 24
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b".,:+-".contains(c));
    let file = b
        .windows(2)
        .any(|w| w[0] == b'.' && w[1].is_ascii_alphabetic());
    let hexish = v
        .split([',', ':', '+', '-'])
        .any(|part| part.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("0x")))
        || v.split(|c: char| !c.is_ascii_hexdigit()).any(|run| {
            run.len() >= 5
                && run.bytes().any(|c| c.is_ascii_digit())
                && run.bytes().any(|c| c.is_ascii_alphabetic())
        });
    charset && !file && !hexish
}

// ---------------------------------------------------------------------------
// Counters.

/// A number the report keeps, fed where it is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Stat {
    /// `ExecuteCommandLists` calls.
    Submissions,
    /// Pipeline states the title created.
    PipelinesCreated,
    /// Host pipelines compiled during the run, and served by the pipeline
    /// library instead.
    PipelinesMadeNow,
    PipelinesFromLibrary,
    /// Pipelines whose host pipeline could not be made.
    PipelinesFailed,
    /// Shader translations done during the run, served by the shader cache,
    /// and failed.
    ShadersTranslated,
    ShadersFromCache,
    ShadersFailed,
    /// Allocations refused for want of memory that went through when asked
    /// again, and those that did not.
    OomRetriedOk,
    OomRetriedFailed,
    /// Changes of the budget governor's level, and the worst level seen
    /// (1 green, 2 yellow, 3 red; 0 never looked).
    GovernorChanges,
    GovernorWorst,
    /// Peak use of the card's own memory and its budget then, the same for
    /// the PC's memory shared with the card, in bytes.
    LocalPeak,
    LocalBudget,
    SharedPeak,
    SharedBudget,
    /// The process's peak commit and working set, in bytes.
    CommitPeak,
    WorkingSetPeak,
    /// Views a host view heap holds (0: not known).
    ViewHeapViews,
    /// The display mode reported to the title (0: not asked yet).
    DisplayWidth,
    DisplayHeight,
    /// Panics of ours.
    Panics,
    /// Calls of a pipeline creation method that is not implemented (each a
    /// pipeline the title went without).
    PipelinesGoneWithout,
    /// Times the commit guard found commit critical (crossing its line), its
    /// stagings held back, and the allocations refused it saw, so far.
    CommitCritical,
    CommitHeldBack,
    CommitRefusals,
    /// Waits for new memory to be made resident, and for new card memory to
    /// be mapped: times, microseconds in all, the longest (microseconds).
    ResidentWaits,
    ResidentUs,
    ResidentLongestUs,
    MappedWaits,
    MappedUs,
    MappedLongestUs,
    /// Looks of the budget governor that found the card's own memory in use
    /// past its budget.
    CardOverBudgetLooks,
    /// Textures picked by index: uploaded so far and their bytes; slots
    /// waiting for their memory, the most and the last seen; passes that
    /// found the card with no room; uploads again of a texture that changed
    /// with nothing to tell of it; uploads of textures on the card made while
    /// it had no room; host copies of only the smallest levels and their
    /// bytes.
    PickedUploaded,
    PickedBytes,
    PickedWaitingMost,
    PickedWaitingLast,
    PickedNoRoom,
    PickedChangedUnseen,
    PickedReuploadedFull,
    TailCopies,
    TailBytes,
    /// Host copies made with fewer levels for the card and for the PC's
    /// commit; copies made whole in the room the blocks of textures had
    /// free; slots that showed the neutral texture, or fewer levels, instead
    /// of a copy's memory no upload went into.
    DetailCutCard,
    DetailCutPc,
    PoolRoomWhole,
    UnwrittenKept,
    // `[memory.views]`: the runtime's views of title pages and the
    // descriptor table's entries of views without a resource.
    /// Views of the host's mapped at title addresses since the start, the
    /// pages they showed, and views unmapped there.
    TitleViewsMapped,
    TitleViewPages,
    TitleViewsUnmapped,
    /// Views kept in the runtime's books alone (`VICTORY_LAZY_VIEWS`,
    /// `VICTORY_SOFT_VIEWS`): noted, let go before anything touched them,
    /// made views of the host's, and such views now.
    BookViewsNoted,
    BookViewsLetGo,
    BookViewsMadeReal,
    BookViewsNow,
    /// Descriptor table entries of views no resource releases: kept now, and
    /// freed for their cap since the start (`VICTORY_OWNERLESS_VIEWS`).
    OwnerlessViewsKept,
    OwnerlessViewsDropped,
}

const STATS: usize = Stat::OwnerlessViewsDropped as usize + 1;
static STAT: [AtomicU64; STATS] = [const { AtomicU64::new(0) }; STATS];

/// One more of `stat`.
pub fn bump(stat: Stat) {
    STAT[stat as usize].fetch_add(1, Relaxed);
}

/// `stat` is `value` now (a counter kept elsewhere, sampled).
pub fn set(stat: Stat, value: u64) {
    STAT[stat as usize].store(value, Relaxed);
}

/// `stat` is at least `value`.
pub fn max(stat: Stat, value: u64) {
    STAT[stat as usize].fetch_max(value, Relaxed);
}

/// Why a draw, mesh dispatch or dispatch the title recorded was left out
/// (not run on the host), in the words the GPU side already tells them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LeftOut {
    /// Its host pipeline was not made within the frame's wait budget.
    PipelineNotReady,
    /// A shader of its pipeline does not translate.
    ShaderNotTranslated,
    /// The host refused to make its pipeline.
    PipelineRefused,
    /// Memory for it was refused (constants, scratch, vertices).
    OutOfMemory,
    /// Its descriptor tables found no free slots.
    NoDescriptorSlots,
    /// Indirect arguments that could not be bounded.
    IndirectNotBounded,
    /// A stage of the ray-tracing structure builder, which is off.
    BuilderOff,
    /// No pipeline was set.
    NoPipeline,
    /// A shader of its pipeline is named in `VICTORY_LEAVE_OUT`.
    AskedToLeaveOut,
    Other,
}

impl LeftOut {
    pub const ALL: [LeftOut; 10] = [
        LeftOut::PipelineNotReady,
        LeftOut::ShaderNotTranslated,
        LeftOut::PipelineRefused,
        LeftOut::OutOfMemory,
        LeftOut::NoDescriptorSlots,
        LeftOut::IndirectNotBounded,
        LeftOut::BuilderOff,
        LeftOut::NoPipeline,
        LeftOut::AskedToLeaveOut,
        LeftOut::Other,
    ];

    /// The key the report writes it under.
    pub const fn key(self) -> &'static str {
        match self {
            LeftOut::PipelineNotReady => "pipeline_not_ready",
            LeftOut::ShaderNotTranslated => "shader_not_translated",
            LeftOut::PipelineRefused => "pipeline_refused",
            LeftOut::OutOfMemory => "out_of_memory",
            LeftOut::NoDescriptorSlots => "no_descriptor_slots",
            LeftOut::IndirectNotBounded => "indirect_not_bounded",
            LeftOut::BuilderOff => "ray_tracing_builder_off",
            LeftOut::NoPipeline => "no_pipeline_set",
            LeftOut::AskedToLeaveOut => "asked_to_leave_out",
            LeftOut::Other => "other",
        }
    }
}

static LEFT_OUT: [AtomicU64; LeftOut::ALL.len()] =
    [const { AtomicU64::new(0) }; LeftOut::ALL.len()];

/// A draw or dispatch left out for `why`.
pub fn left_out(why: LeftOut) {
    LEFT_OUT[why as usize].fetch_add(1, Relaxed);
}

fn left_out_total() -> u64 {
    LEFT_OUT.iter().map(|n| n.load(Relaxed)).sum()
}

/// Video memory now, as `[budget, used]` bytes of the card's own memory and
/// of the PC's memory shared with it (`QueryVideoMemoryInfo`): keeps the
/// peaks, with the budget at the time of each.
pub fn video_memory(local: [u64; 2], shared: [u64; 2]) {
    let peak = |used: Stat, budget: Stat, [b, u]: [u64; 2]| {
        if STAT[used as usize].fetch_max(u, Relaxed) < u {
            set(budget, b);
        }
    };
    peak(Stat::LocalPeak, Stat::LocalBudget, local);
    peak(Stat::SharedPeak, Stat::SharedBudget, shared);
}

/// The display mode the title is told of.
pub fn display_mode(width: u32, height: u32) {
    set(Stat::DisplayWidth, u64::from(width));
    set(Stat::DisplayHeight, u64::from(height));
}

/// The lowest free physical memory and commit headroom seen, in bytes
/// (`u64::MAX`: never seen).
static LOWEST: [AtomicU64; 2] = [const { AtomicU64::new(u64::MAX) }; 2];

/// Free physical memory and commit headroom now, in bytes, as the budget
/// governor asks the system (at most every 32 ms): keeps the lowest.
/// `u64::MAX` (not known) changes nothing.
pub fn memory_seen(free_ram: u64, commit_headroom: u64) {
    LOWEST[0].fetch_min(free_ram, Relaxed);
    LOWEST[1].fetch_min(commit_headroom, Relaxed);
}

/// Microseconds at each level of the budget governor (green, yellow, red),
/// the time of its last look and the level it found then (1 green, 2
/// yellow, 3 red; 0 none yet).
static GOVERNOR_US: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static LAST_LOOK_US: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_LEVEL: AtomicU64 = AtomicU64::new(0);

/// A look of the budget governor (about once a second) that found `level`
/// (1 green, 2 yellow, 3 red) and the card's own memory at `card` (`[budget,
/// used]` bytes): the time since the last look is the last level's, and a
/// look with the card past its budget is counted.
pub fn memory_look(level: u64, card: [u64; 2]) {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    let now = now_us();
    let last = LAST_LOOK_US.swap(now, Relaxed);
    let was = LAST_LEVEL.swap(level.min(3), Relaxed);
    if last != u64::MAX && (1..=3).contains(&was) {
        GOVERNOR_US[was as usize - 1].fetch_add(now.saturating_sub(last), Relaxed);
    }
    let [budget, used] = card;
    if budget > 0 && used > budget {
        bump(Stat::CardOverBudgetLooks);
    }
}

// ---------------------------------------------------------------------------
// Tables keyed by our words, for any thread, without a lock.

/// A fixed table of counts keyed by two of our words and a number, filled by
/// open addressing with atomics: a slot is claimed once by compare and swap
/// and its words set once, so a count is one probe and one add. A slot is
/// told apart by the words' addresses and lengths (every literal is one
/// string), and rendering merges equal texts.
struct Keyed<const N: usize> {
    slots: [Slot; N],
    /// Counts that found the table full.
    lost: AtomicU64,
}

struct Slot {
    key: AtomicU64,
    words: OnceLock<(&'static str, &'static str, u32)>,
    count: AtomicU64,
}

impl Slot {
    const fn new() -> Slot {
        Slot {
            key: AtomicU64::new(0),
            words: OnceLock::new(),
            count: AtomicU64::new(0),
        }
    }
}

/// One entry of a [`Keyed`] table: two words, a number, a count.
type KeyedEntry = (Ours, Ours, u32, u64);

impl<const N: usize> Keyed<N> {
    const fn new() -> Self {
        Keyed {
            slots: [const { Slot::new() }; N],
            lost: AtomicU64::new(0),
        }
    }

    fn bump(&self, a: &'static str, b: &'static str, n: u32) {
        let same = |x: &str, y: &str| x.as_ptr() == y.as_ptr() && x.len() == y.len();
        let mut key = 0xcbf2_9ce4_8422_2325u64;
        for part in [
            a.as_ptr() as u64,
            a.len() as u64,
            b.as_ptr() as u64,
            b.len() as u64,
            u64::from(n),
        ] {
            key = (key ^ part).wrapping_mul(0x0000_0100_0000_01b3);
        }
        let key = key | 1;
        let start = (key % N as u64) as usize;
        for i in 0..N {
            let slot = &self.slots[(start + i) % N];
            let mut held = slot.key.load(std::sync::atomic::Ordering::Acquire);
            if held == 0 {
                match slot.key.compare_exchange(
                    0,
                    key,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                ) {
                    Ok(_) => {
                        let _ = slot.words.set((a, b, n));
                        slot.count.fetch_add(1, Relaxed);
                        return;
                    }
                    Err(now) => held = now,
                }
            }
            // (Words not set yet are the claimer's, a moment away.)
            if held == key
                && slot
                    .words
                    .get()
                    .is_none_or(|&(x, y, m)| same(x, a) && same(y, b) && m == n)
            {
                slot.count.fetch_add(1, Relaxed);
                return;
            }
        }
        self.lost.fetch_add(1, Relaxed);
    }

    /// Everything counted, equal texts merged, most counted first.
    fn entries(&self) -> Vec<KeyedEntry> {
        let mut all: Vec<KeyedEntry> = Vec::new();
        for slot in &self.slots {
            let (Some(&(a, b, n)), count) = (slot.words.get(), slot.count.load(Relaxed)) else {
                continue;
            };
            match all
                .iter_mut()
                .find(|e| e.0.0 == a && e.1.0 == b && e.2 == n)
            {
                Some(e) => e.3 += count,
                None => all.push((Ours(a), Ours(b), n, count)),
            }
        }
        all.sort_by(|x, y| {
            y.3.cmp(&x.3)
                .then_with(|| (x.0.0, x.1.0, x.2).cmp(&(y.0.0, y.1.0, y.2)))
        });
        all
    }
}

static METHODS: Keyed<256> = Keyed::new();
static SHADER_FAILURES: Keyed<128> = Keyed::new();

/// A method not written yet was called: our name of the interface and the
/// method's offset in its function table.
pub fn method_missing(interface: &'static str, offset: u32) {
    METHODS.bump(interface, "", offset);
}

/// A shader did not translate, for the translator's reason: its kind of
/// failure and its own wording of the construct (both from its source).
pub fn shader_failed(kind: &'static str, what: &'static str) {
    bump(Stat::ShadersFailed);
    SHADER_FAILURES.bump(kind, what, 0);
}

// ---------------------------------------------------------------------------
// Frames.

/// Frame-time buckets for the percentiles: a quarter of a millisecond each
/// to 100 ms, then 10 ms each to 2 s, then one for anything longer.
const FINE: usize = 400 + 190 + 1;
static FINE_FRAMES: [AtomicU32; FINE] = [const { AtomicU32::new(0) }; FINE];

fn fine_bucket(us: u64) -> usize {
    if us < 100_000 {
        (us / 250) as usize
    } else if us < 2_000_000 {
        400 + ((us - 100_000) / 10_000) as usize
    } else {
        FINE - 1
    }
}

/// The upper edge of a fine bucket, in ms (infinite for the last).
fn fine_upper_ms(i: usize) -> f64 {
    if i < 400 {
        (i + 1) as f64 * 0.25
    } else if i < FINE - 1 {
        100.0 + (i - 399) as f64 * 10.0
    } else {
        f64::INFINITY
    }
}

/// The histogram's edges, in microseconds: 120, 90, 60, 45, 30, 20, 10, 4
/// and 1 frames a second.
const EDGES_US: [u64; 9] = [
    8_333, 11_111, 16_667, 22_222, 33_333, 50_000, 100_000, 250_000, 1_000_000,
];
const BUCKET_KEYS: [&str; 10] = [
    "under 8.3",
    "8.3 to 11.1",
    "11.1 to 16.7",
    "16.7 to 22.2",
    "22.2 to 33.3",
    "33.3 to 50",
    "50 to 100",
    "100 to 250",
    "250 to 1000",
    "1000 and over",
];
static BUCKETS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];

/// The timeline: presents and the worst frame (ms) per window of
/// [`WINDOW_S`] since the run began; a longer run adds to the last.
const WINDOW_S: u64 = 10;
const WINDOWS: usize = 2160;
static WINDOW_PRESENTS: [AtomicU32; WINDOWS] = [const { AtomicU32::new(0) }; WINDOWS];
static WINDOW_WORST_MS: [AtomicU32; WINDOWS] = [const { AtomicU32::new(0) }; WINDOWS];
/// The timeline is shown in at most this many windows (merged in twos,
/// threes... for a long run).
const SHOWN_WINDOWS: usize = 180;

static PRESENTS: AtomicU64 = AtomicU64::new(0);
/// Microseconds since the start of the run at the last present, at the
/// first, and at the first that had draws run on the host before it.
static LAST_PRESENT_US: AtomicU64 = AtomicU64::new(u64::MAX);
static FIRST_PRESENT_US: AtomicU64 = AtomicU64::new(u64::MAX);
static FIRST_DRAW_US: AtomicU64 = AtomicU64::new(u64::MAX);

/// The timeline's window of `us` since the start (a longer run adds to the
/// last).
fn window_of(us: u64) -> usize {
    ((us / 1_000_000 / WINDOW_S) as usize).min(WINDOWS - 1)
}
/// Frame intervals counted, their sum and the longest, in microseconds.
static INTERVALS: AtomicU64 = AtomicU64::new(0);
static INTERVAL_SUM_US: AtomicU64 = AtomicU64::new(0);
static INTERVAL_MAX_US: AtomicU64 = AtomicU64::new(0);

/// Per frame (between two presents): submissions, draws run, dispatches
/// run, draws and dispatches left out. The totals at the last present, the
/// sums over the frames counted and the most in one.
const WORK: usize = 4;
static WORK_AT: [AtomicU64; WORK] = [const { AtomicU64::new(0) }; WORK];
static WORK_SUM: [AtomicU64; WORK] = [const { AtomicU64::new(0) }; WORK];
static WORK_MAX: [AtomicU64; WORK] = [const { AtomicU64::new(0) }; WORK];

/// A present. `drawn` and `dispatched` are the GPU side's totals since the
/// start of draws (mesh dispatches included) and dispatches run on the host.
pub fn present(drawn: u64, dispatched: u64) {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    let now = now_us();
    PRESENTS.fetch_add(1, Relaxed);
    let _ = FIRST_PRESENT_US.compare_exchange(u64::MAX, now, Relaxed, Relaxed);
    if drawn > 0 && FIRST_DRAW_US.load(Relaxed) == u64::MAX {
        let _ = FIRST_DRAW_US.compare_exchange(u64::MAX, now, Relaxed, Relaxed);
    }
    let last = LAST_PRESENT_US.swap(now, Relaxed);
    let window = window_of(now);
    let second = second_of(now);
    WINDOW_PRESENTS[window].fetch_add(1, Relaxed);
    let totals = [
        STAT[Stat::Submissions as usize].load(Relaxed),
        drawn,
        dispatched,
        left_out_total(),
    ];
    // The first present only sets the starting totals: what came before it
    // is the loading, not a frame.
    let first = last == u64::MAX;
    for (i, total) in totals.into_iter().enumerate() {
        let delta = total.saturating_sub(WORK_AT[i].swap(total, Relaxed));
        if !first {
            WORK_SUM[i].fetch_add(delta, Relaxed);
            WORK_MAX[i].fetch_max(delta, Relaxed);
            if i == 3 {
                FIGURES[window][Figure::LeftOut as usize].fetch_add(delta, Relaxed);
                if let Some(s) = second {
                    SECONDS[s][Figure::LeftOut as usize].fetch_add(delta, Relaxed);
                }
            }
        }
    }
    if !first {
        FIGURES[window][Figure::Intervals as usize].fetch_add(1, Relaxed);
        if let Some(s) = second {
            SECONDS[s][Figure::Intervals as usize].fetch_add(1, Relaxed);
        }
    }
    if first || now < last {
        return;
    }
    let us = now - last;
    INTERVALS.fetch_add(1, Relaxed);
    INTERVAL_SUM_US.fetch_add(us, Relaxed);
    INTERVAL_MAX_US.fetch_max(us, Relaxed);
    FINE_FRAMES[fine_bucket(us)].fetch_add(1, Relaxed);
    BUCKETS[EDGES_US.iter().take_while(|&&e| us >= e).count()].fetch_add(1, Relaxed);
    WINDOW_WORST_MS[window].fetch_max(u32::try_from(us / 1000).unwrap_or(u32::MAX), Relaxed);
}

// ---------------------------------------------------------------------------
// The frame line's figures.

/// What the GPU side's once-a-second frame line says of the frames since its
/// last one: microseconds, bytes or counts in all over those frames. The line
/// takes its meters with `swap(0)`, so they cannot be read a second time:
/// they are given here where the line takes them, and added up per
/// [`WINDOW_S`] window of the run so that the steady state
/// ([`steady_from_us`]) can be told from the loading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Figure {
    /// Frames the frame meter counted (presents the swap chain took), and
    /// the microseconds its windows lasted.
    Frames,
    MeterUs,
    /// Preparing submissions on the CPU; of it, copying title buffers up
    /// (microseconds, bytes) and refreshing textures (microseconds, bytes).
    PreparingUs,
    CopiesUpUs,
    CopiesUpBytes,
    TextureRefreshUs,
    TextureRefreshBytes,
    /// The waits for the card in submissions (the line's "on the GPU").
    CardWaitUs,
    /// Copying what shaders wrote back into title memory: microseconds,
    /// bytes.
    CopyBackUs,
    CopyBackBytes,
    /// The card's clock: nothing of ours queued; of that, while the
    /// submitting thread was working; the submitting thread stood waiting
    /// for the card.
    CardIdleUs,
    CardIdleBehindUs,
    StoodUs,
    /// Passes over the heap's buffer views for shaders that pick them by
    /// index, and their microseconds.
    HeapBufferPasses,
    HeapBufferUs,
    /// Texture refreshes on all threads, microseconds.
    RefreshAllUs,
    /// The waiting for the card in settles of submissions sent ahead, by
    /// what settled them, microseconds.
    SettledSync,
    SettledCardDone,
    SettledValue,
    SettledStale,
    SettledRead,
    SettledStep,
    SettledMost,
    SettledWrites,
    SettledFence,
    /// Work switches of the unified-memory synchronisation spare: bytes of
    /// shader writes left on the card in copies of write-combined memory
    /// (`VICTORY_WC_NO_BACK`); heap pages whose records were read in place
    /// (`VICTORY_SYNC_IN_PLACE`); writes of view slots that left the placed
    /// views' spans alone (`VICTORY_SPANS_QUICK`); rounds of the heaps'
    /// records begun as a submission took the lock (`VICTORY_HEAP_SYNC_FIRST`);
    /// preparings that found their round asked for at the title's submit
    /// (`VICTORY_HEAP_SYNC_AT_SUBMIT`).
    SparedLeftOnCardBytes,
    SparedPagesInPlace,
    SparedSpans,
    SparedRoundsFirst,
    SparedRoundsAtSubmit,
    /// Work moved off the threads a frame waits for, microseconds: objects
    /// let go on a thread of their own (`VICTORY_BURY_ASIDE`), the log's
    /// table of pipelines put together on one (`VICTORY_PIPE_TABLE_ASIDE`),
    /// snapshots taken beside the submitting thread's work and its wait for
    /// them after (`VICTORY_SNAPSHOTS_BESIDE`).
    MovedBuryUs,
    MovedTableUs,
    MovedSnapshotsUs,
    SnapshotsWaitedUs,
    /// Fed by [`present`]: frames between two presents, and the draws and
    /// dispatches left out in them.
    Intervals,
    LeftOut,
}

const FIGS: usize = Figure::LeftOut as usize + 1;
/// Per window of the timeline, every [`Figure`]. (Zeroed memory that is only
/// touched as the run reaches each window.)
static FIGURES: [[AtomicU64; FIGS]; WINDOWS] = [const { [const { AtomicU64::new(0) }; FIGS] }; WINDOWS];

/// The work spared by switches, under our keys, with the scale of each.
const SPARED: [(Figure, &str, f64); 5] = [
    (Figure::SparedLeftOnCardBytes, "shader_writes_left_on_card_mb", 1e6),
    (Figure::SparedPagesInPlace, "heap_pages_read_in_place", 1.0),
    (Figure::SparedSpans, "slot_writes_sparing_spans", 1.0),
    (Figure::SparedRoundsFirst, "heap_rounds_begun_first", 1.0),
    (Figure::SparedRoundsAtSubmit, "heap_rounds_asked_at_submit", 1.0),
];

/// The work moved off the threads a frame waits for, under our keys (ms per
/// frame).
const MOVED: [(Figure, &str); 4] = [
    (Figure::MovedBuryUs, "objects_let_go_aside_ms"),
    (Figure::MovedTableUs, "pipeline_table_aside_ms"),
    (Figure::MovedSnapshotsUs, "snapshots_beside_ms"),
    (Figure::SnapshotsWaitedUs, "snapshots_waited_ms"),
];

/// The settles' causes as the frame line words them, under our keys.
const SETTLED: [(Figure, &str); 9] = [
    (Figure::SettledSync, "sync_submission_or_present"),
    (Figure::SettledCardDone, "card_had_run_them"),
    (Figure::SettledValue, "wait_for_memory_value"),
    (Figure::SettledStale, "title_memory_changed_under_shader_writes"),
    (Figure::SettledRead, "title_memory_read"),
    (Figure::SettledStep, "step_of_ours"),
    (Figure::SettledMost, "lists_in_the_air"),
    (Figure::SettledWrites, "bottom_of_pipe_writes"),
    (Figure::SettledFence, "blocking_fence_wait"),
];

/// The same figures by whole seconds from the first present, for the first
/// [`SECONDS_KEPT`]: the steady state of a run too short for a whole window
/// after its first third ([`steady_state`]) is made from them. Such a run
/// lasts under 30 s from its first present, so these are enough.
const SECONDS_KEPT: usize = 32;
static SECONDS: [[AtomicU64; FIGS]; SECONDS_KEPT] = [const { [const { AtomicU64::new(0) }; FIGS] }; SECONDS_KEPT];

/// The second from the first present that `us` (since the start) is in, if
/// it is one of those kept; `None` before the first present.
fn second_of(us: u64) -> Option<usize> {
    let first = FIRST_PRESENT_US.load(Relaxed);
    let second = (us.checked_sub(first)? / 1_000_000) as usize;
    (first != u64::MAX && second < SECONDS_KEPT).then_some(second)
}

/// Figures of the frame line, added to the window of now (once a second,
/// and once more when the report that names the run's end is written).
pub fn figures(values: &[(Figure, u64)]) {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    let now = now_us();
    let window = &FIGURES[window_of(now)];
    let second = second_of(now).map(|s| &SECONDS[s]);
    for &(figure, value) in values {
        window[figure as usize].fetch_add(value, Relaxed);
        if let Some(second) = second {
            second[figure as usize].fetch_add(value, Relaxed);
        }
    }
}

/// Whether the report being written is the one that names how the run
/// ended: the GPU side then gives the frame line's meters one last time
/// (what they hold since the line's last second), so that `[frame_time]`
/// covers every frame without the log saying more.
pub fn ending() -> bool {
    END.load(SeqCst) != 0
}

const WINDOW_US: u64 = WINDOW_S * 1_000_000;

/// Where the steady state begins: at the first window's start at or after a
/// third of the time from the first present to `duration_us`. Loading and
/// warm-up (shaders, pipelines and textures made for the first time) come
/// in the first third; what a frame costs once they are done is the rest.
/// shortcut: by time, not by what was drawn, so a run that ends in a long
/// loading screen has some of it in its steady state.
fn steady_from_us(first_present_us: u64, duration_us: u64) -> u64 {
    let cut = first_present_us + duration_us.saturating_sub(first_present_us) / 3;
    cut.div_ceil(WINDOW_US) * WINDOW_US
}

/// The figures summed over all `windows`, and over those from `from_us` on.
fn split_figures(windows: &[[u64; FIGS]], from_us: Option<u64>) -> ([u64; FIGS], [u64; FIGS]) {
    let (mut whole, mut steady) = ([0u64; FIGS], [0u64; FIGS]);
    let first = from_us.map_or(usize::MAX, |us| (us / WINDOW_US) as usize);
    for (i, w) in windows.iter().enumerate() {
        for (f, &value) in w.iter().enumerate() {
            whole[f] = whole[f].saturating_add(value);
            if i >= first {
                steady[f] = steady[f].saturating_add(value);
            }
        }
    }
    (whole, steady)
}

/// The steady state of a run: from where, its figures, and whether it is a
/// short run's, by seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Steady {
    from_us: u64,
    figures: [u64; FIGS],
    by_seconds: bool,
}

/// The steady state of a run whose first present came at `first_us` and
/// that has lasted `duration_us`: the windows from [`steady_from_us`] on,
/// when one of them is complete. A shorter run (under 30 s from its first
/// present) has none, and its figures there were all zeros: its steady
/// state is then the last two thirds of its frames by time at the frame
/// meter's resolution: the readings begun at or after the first whole second
/// (from the first present, `from_us`) at or after a third of the time from
/// the first present. A reading is kept in the second it was taken in and
/// covers about the second before it, so those are the readings taken from
/// the second after that one on. `None` when no reading begun that late was
/// taken: the run was too short (a run of about a second has one reading,
/// of all of it).
fn steady_state(
    first_us: u64,
    duration_us: u64,
    windows: &[[u64; FIGS]],
    seconds: &[[u64; FIGS]],
) -> Option<Steady> {
    let from_us = steady_from_us(first_us, duration_us);
    if from_us + WINDOW_US <= duration_us {
        let (_, figures) = split_figures(windows, Some(from_us));
        return Some(Steady { from_us, figures, by_seconds: false });
    }
    let from_second = (duration_us.saturating_sub(first_us) / 3).div_ceil(1_000_000);
    let skip = from_second as usize + 1;
    let mut figures = [0u64; FIGS];
    for second in seconds.iter().skip(skip) {
        for (f, &value) in second.iter().enumerate() {
            figures[f] = figures[f].saturating_add(value);
        }
    }
    let read = figures[Figure::Frames as usize] > 0 || figures[Figure::Intervals as usize] > 0;
    read.then_some(Steady {
        from_us: first_us + from_second * 1_000_000,
        figures,
        by_seconds: true,
    })
}

// ---------------------------------------------------------------------------
// The card's time by kind of work (`[gpu_time]`, `[gpu_time.steady]`).

/// What the GPU side measures of the card's own time on the host queue, by
/// kind of work (GPU timestamps, `VICTORY_GPU_SPLIT`), what that work moved,
/// and what the budget governor saw of memory meanwhile: totals over the
/// frames of a frame-line second (nanoseconds, bytes, counts), and per look
/// of the governor. Kept per [`WINDOW_S`] window and per second as the
/// frame line's figures are, so its steady state is theirs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum GpuFigure {
    /// Frames of the frame meter, and of them those partly measured.
    Frames,
    PartlyFrames,
    /// Nanoseconds by kind, in the order of [`GPU_KINDS`].
    DrawsNs,
    DispatchesWave64Ns,
    DispatchesOtherNs,
    TitleOtherNs,
    FillsNs,
    HashesNs,
    ComparesNs,
    CopyBacksNs,
    UploadsNs,
    TextureUploadsNs,
    OtherPassesNs,
    UnattributedNs,
    /// The volumes: fills done on the card and their bytes, hash passes and
    /// the bytes they read, compare passes and theirs, bytes fetched back,
    /// ranges sent up and their bytes, bytes of textures uploaded.
    Fills,
    FillBytes,
    HashPasses,
    HashedBytes,
    ComparePasses,
    ComparedBytes,
    CopiedBackBytes,
    UploadRanges,
    UploadedBytes,
    TextureBytes,
    /// The governor's looks that knew the card's memory: their sums of use
    /// and budget of the card's own and of the shared memory, the most used.
    Looks,
    CardUsedSum,
    CardBudgetSum,
    CardUsedPeak,
    SharedUsedSum,
    SharedBudgetSum,
    SharedUsedPeak,
    /// The looks that knew free memory, its sum, and the lowest (kept as
    /// its bitwise complement, so that the most kept is the lowest seen; 0:
    /// none).
    RamLooks,
    FreeRamSum,
    FreeRamLowestNot,
    /// Looks with the card's memory past its budget, and with free memory
    /// under [`LOW_RAM`].
    CardOverLooks,
    RamLowLooks,
    /// The title's compute pipelines (`VICTORY_GPU_SPLIT`, `pipelines`),
    /// named nowhere: the frame-line seconds that had any, the sum over them
    /// of how many ran in each, and the card's time of the costliest 1, 5
    /// and 20 of each second, summed.
    PipelineSeconds,
    ComputePipelinesSum,
    TopPipelineNs,
    Top5PipelinesNs,
    Top20PipelinesNs,
}

const GPU_FIGS: usize = GpuFigure::Top20PipelinesNs as usize + 1;

/// The kinds of the card's time, in order, under their keys.
pub const GPU_KINDS: [GpuFigure; 12] = [
    GpuFigure::DrawsNs,
    GpuFigure::DispatchesWave64Ns,
    GpuFigure::DispatchesOtherNs,
    GpuFigure::TitleOtherNs,
    GpuFigure::FillsNs,
    GpuFigure::HashesNs,
    GpuFigure::ComparesNs,
    GpuFigure::CopyBacksNs,
    GpuFigure::UploadsNs,
    GpuFigure::TextureUploadsNs,
    GpuFigure::OtherPassesNs,
    GpuFigure::UnattributedNs,
];

const GPU_KIND_KEYS: [&str; 12] = [
    "draws_ms",
    "dispatches_wave64_ms",
    "dispatches_other_ms",
    "title_other_ms",
    "fills_ms",
    "hashes_ms",
    "compares_ms",
    "copy_backs_ms",
    "uploads_ms",
    "texture_uploads_ms",
    "other_passes_ms",
    "unattributed_ms",
];

/// How many of [`GPU_KINDS`] are the title's (the rest are ours, then the
/// unattributed).
const GPU_TITLE_KINDS: usize = 4;

/// Free memory under which a look counts as low: 512 MiB.
pub const LOW_RAM: u64 = 512 << 20;

/// Whether a figure is the most of its windows (else their sum).
fn gpu_most(f: usize) -> bool {
    [GpuFigure::CardUsedPeak, GpuFigure::SharedUsedPeak, GpuFigure::FreeRamLowestNot]
        .iter()
        .any(|&m| m as usize == f)
}

static GPU_WINDOWS: [[AtomicU64; GPU_FIGS]; WINDOWS] = [const { [const { AtomicU64::new(0) }; GPU_FIGS] }; WINDOWS];
static GPU_SECONDS: [[AtomicU64; GPU_FIGS]; SECONDS_KEPT] =
    [const { [const { AtomicU64::new(0) }; GPU_FIGS] }; SECONDS_KEPT];

/// Whether the split is measured: 0 no word yet, else [`GpuSplit`] + 1.
static GPU_STATE: AtomicU64 = AtomicU64::new(0);

/// What the GPU side says of its split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum GpuSplit {
    /// Measured.
    On,
    /// `VICTORY_GPU_SPLIT=0`.
    Off,
    /// The driver refused timestamp queries on the host queue.
    Refused,
    /// On, and no list has marked a run yet.
    NotStarted,
}

impl GpuSplit {
    const fn key(self) -> &'static str {
        match self {
            GpuSplit::On => "on",
            GpuSplit::Off => "off",
            GpuSplit::Refused => "refused_by_driver",
            GpuSplit::NotStarted => "not_started",
        }
    }
}

/// Adds `values` to the window and second of now (`*Peak` and
/// `FreeRamLowestNot` keep the most).
fn gpu_add(values: &[(GpuFigure, u64)]) {
    let now = now_us();
    let window = &GPU_WINDOWS[window_of(now)];
    let second = second_of(now).map(|s| &GPU_SECONDS[s]);
    for &(figure, value) in values {
        let f = figure as usize;
        for at in std::iter::once(window).chain(second) {
            if gpu_most(f) {
                at[f].fetch_max(value, Relaxed);
            } else {
                at[f].fetch_add(value, Relaxed);
            }
        }
    }
}

/// The card's time by kind and the volumes of a frame-line second (and of
/// the run's last part of one), as totals over its frames; and the split's
/// state.
pub fn gpu_time(state: GpuSplit, values: &[(GpuFigure, u64)]) {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    GPU_STATE.store(state as u64 + 1, Relaxed);
    gpu_add(values);
}

/// One look of the budget governor (about once a second): the card's own
/// memory and the shared memory as `[budget, used]` bytes (zeros while not
/// known), free physical memory (`u64::MAX` while not known).
pub fn gpu_time_look(card: [u64; 2], shared: [u64; 2], free_ram: u64) {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    if card[0] > 0 {
        gpu_add(&[
            (GpuFigure::Looks, 1),
            (GpuFigure::CardBudgetSum, card[0]),
            (GpuFigure::CardUsedSum, card[1]),
            (GpuFigure::CardUsedPeak, card[1]),
            (GpuFigure::SharedBudgetSum, shared[0]),
            (GpuFigure::SharedUsedSum, shared[1]),
            (GpuFigure::SharedUsedPeak, shared[1]),
            (GpuFigure::CardOverLooks, u64::from(card[1] > card[0])),
        ]);
    }
    if free_ram != u64::MAX {
        gpu_add(&[
            (GpuFigure::RamLooks, 1),
            (GpuFigure::FreeRamSum, free_ram),
            (GpuFigure::FreeRamLowestNot, !free_ram),
            (GpuFigure::RamLowLooks, u64::from(free_ram < LOW_RAM)),
        ]);
    }
}

/// `windows` folded into one: summed, the most where a figure keeps it.
fn gpu_fold<'a>(windows: impl Iterator<Item = &'a [u64; GPU_FIGS]>) -> [u64; GPU_FIGS] {
    let mut out = [0u64; GPU_FIGS];
    for w in windows {
        for (f, &v) in w.iter().enumerate() {
            out[f] = if gpu_most(f) { out[f].max(v) } else { out[f].saturating_add(v) };
        }
    }
    out
}

/// The card's time over the run and at the steady state the frame line's
/// figures have (`steady`, from the first present at `first_us`): the same
/// windows, or for a short run the same seconds.
fn gpu_steady(
    steady: &Steady,
    first_us: u64,
    windows: &[[u64; GPU_FIGS]],
    seconds: &[[u64; GPU_FIGS]],
) -> [u64; GPU_FIGS] {
    if steady.by_seconds {
        let skip = (steady.from_us.saturating_sub(first_us) / 1_000_000) as usize + 1;
        gpu_fold(seconds.iter().skip(skip))
    } else {
        gpu_fold(windows.iter().skip((steady.from_us / WINDOW_US) as usize))
    }
}

/// What the report says of the card's time.
#[derive(Clone, Debug, PartialEq)]
struct GpuTime {
    state: Option<GpuSplit>,
    whole: [u64; GPU_FIGS],
    /// At the steady state (`None` where `[frame_time.steady]` has none).
    steady: Option<[u64; GPU_FIGS]>,
}

impl GpuTime {
    fn take(windows_used: usize, first_us: Option<u64>, steady: Option<&Steady>) -> GpuTime {
        let take = |w: &[AtomicU64; GPU_FIGS]| -> [u64; GPU_FIGS] { std::array::from_fn(|f| w[f].load(Relaxed)) };
        let windows: Vec<[u64; GPU_FIGS]> = GPU_WINDOWS[..windows_used].iter().map(take).collect();
        let seconds: Vec<[u64; GPU_FIGS]> = GPU_SECONDS.iter().map(take).collect();
        let state = match GPU_STATE.load(Relaxed) {
            1 => Some(GpuSplit::On),
            2 => Some(GpuSplit::Off),
            3 => Some(GpuSplit::Refused),
            4 => Some(GpuSplit::NotStarted),
            _ => None,
        };
        GpuTime {
            state,
            whole: gpu_fold(windows.iter()),
            steady: steady.zip(first_us).map(|(s, first)| gpu_steady(s, first, &windows, &seconds)),
        }
    }
}

/// `[gpu_time]` and `[gpu_time.steady]`.
fn gpu_time_sections(d: &mut Doc, g: &GpuTime, steady: Option<&Steady>, presented: bool) {
    d.table("gpu_time");
    d.comment("the card's own time on the host queue per frame by kind of work, ms (GPU timestamps around");
    d.comment("runs of commands of one kind; VICTORY_GPU_SPLIT), what our work moved, MB (10^6 bytes), and");
    d.comment("the memory the budget governor saw meanwhile, MiB; whole run here, [gpu_time.steady] as");
    d.comment("[frame_time.steady]");
    d.word("split", Ours(g.state.map_or("not_known", GpuSplit::key)));
    gpu_time_figures(d, &g.whole);
    d.table("gpu_time.steady");
    match (&g.steady, steady) {
        (Some(f), Some(s)) => {
            d.float("from_s", s.from_us as f64 / 1e6);
            gpu_time_figures(d, f);
        }
        _ if presented => d.comment("none (see [frame_time.steady])"),
        _ => d.comment("no present yet"),
    }
}

/// The card's time of `f` per frame, its volumes, the memory meanwhile.
fn gpu_time_figures(d: &mut Doc, f: &[u64; GPU_FIGS]) {
    let get = |figure: GpuFigure| f[figure as usize];
    let frames = get(GpuFigure::Frames);
    let per = |v: u64, scale: f64| v as f64 / scale / frames.max(1) as f64;
    d.int("frames", frames);
    d.float("partly_measured_pct", 100.0 * get(GpuFigure::PartlyFrames) as f64 / frames.max(1) as f64);
    d.comment("the title's work");
    let ns: Vec<u64> = GPU_KINDS.iter().map(|&k| get(k)).collect();
    for (i, (&key, &n)) in GPU_KIND_KEYS.iter().zip(&ns).enumerate() {
        match i {
            GPU_TITLE_KINDS => d.comment("ours"),
            11 => d.comment("runs no one kind could be given (past the frame's budget of boundaries)"),
            _ => {}
        }
        d.float(key, per(n, 1e6));
    }
    let title: u64 = ns[..GPU_TITLE_KINDS].iter().sum();
    let ours: u64 = ns[GPU_TITLE_KINDS..11].iter().sum();
    d.comment("sums: the title's, ours, and all measured (the unattributed with them)");
    d.float("title_ms", per(title, 1e6));
    d.float("ours_ms", per(ours, 1e6));
    d.float("measured_ms", per(ns.iter().sum(), 1e6));
    d.comment("what our work moved, per frame");
    d.float("fills", per(get(GpuFigure::Fills), 1.0));
    d.float("fills_mb", per(get(GpuFigure::FillBytes), 1e6));
    d.float("hash_passes", per(get(GpuFigure::HashPasses), 1.0));
    d.float("hashed_mb", per(get(GpuFigure::HashedBytes), 1e6));
    d.float("compare_passes", per(get(GpuFigure::ComparePasses), 1.0));
    d.float("compared_mb", per(get(GpuFigure::ComparedBytes), 1e6));
    d.float("copied_back_mb", per(get(GpuFigure::CopiedBackBytes), 1e6));
    d.float("upload_ranges", per(get(GpuFigure::UploadRanges), 1.0));
    d.float("uploaded_mb", per(get(GpuFigure::UploadedBytes), 1e6));
    d.float("texture_uploads_mb", per(get(GpuFigure::TextureBytes), 1e6));
    d.comment("memory while those frames ran: the budget governor's looks (about one a second), MiB");
    let looks = get(GpuFigure::Looks);
    let mib = |v: u64| v as f64 / (1u64 << 20) as f64;
    let avg = |sum: GpuFigure, n: u64| mib(get(sum)) / n.max(1) as f64;
    d.int("governor_looks", looks);
    d.float("card_used_mib_avg", avg(GpuFigure::CardUsedSum, looks));
    d.float("card_used_mib_peak", mib(get(GpuFigure::CardUsedPeak)));
    d.float("card_budget_mib_avg", avg(GpuFigure::CardBudgetSum, looks));
    d.float("shared_used_mib_avg", avg(GpuFigure::SharedUsedSum, looks));
    d.float("shared_used_mib_peak", mib(get(GpuFigure::SharedUsedPeak)));
    d.float("shared_budget_mib_avg", avg(GpuFigure::SharedBudgetSum, looks));
    let ram_looks = get(GpuFigure::RamLooks);
    d.float("free_ram_mib_avg", avg(GpuFigure::FreeRamSum, ram_looks));
    match get(GpuFigure::FreeRamLowestNot) {
        0 => d.comment("free_ram_mib_lowest: not seen"),
        not => d.float("free_ram_mib_lowest", mib(!not)),
    }
    d.int("looks_card_over_budget", get(GpuFigure::CardOverLooks));
    d.int("looks_free_ram_under_512_mib", get(GpuFigure::RamLowLooks));
    d.comment("the title's compute pipelines (VICTORY_GPU_SPLIT pipelines), named nowhere: how many ran in a");
    d.comment("second of the frame line (seconds with any), and per frame the card's time of the costliest");
    d.comment("1, 5 and 20 of each second, ms");
    let seconds = get(GpuFigure::PipelineSeconds);
    d.int("pipeline_seconds", seconds);
    d.float("compute_pipelines_a_second_avg", get(GpuFigure::ComputePipelinesSum) as f64 / seconds.max(1) as f64);
    d.float("costliest_pipeline_ms", per(get(GpuFigure::TopPipelineNs), 1e6));
    d.float("costliest_5_pipelines_ms", per(get(GpuFigure::Top5PipelinesNs), 1e6));
    d.float("costliest_20_pipelines_ms", per(get(GpuFigure::Top20PipelinesNs), 1e6));
}

// ---------------------------------------------------------------------------
// How the run ended.

/// Where a crash was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CrashIn {
    /// In the title's images.
    TitleCode = 1,
    /// In Rox-box's own executable.
    RoxBoxCode,
    /// In a DLL of the host's.
    HostDll,
    /// In memory no module holds (code the title made, our import stubs).
    NoModule,
}

impl CrashIn {
    const ALL: [CrashIn; 4] = [
        CrashIn::TitleCode,
        CrashIn::RoxBoxCode,
        CrashIn::HostDll,
        CrashIn::NoModule,
    ];

    pub const fn key(self) -> &'static str {
        match self {
            CrashIn::TitleCode => "title_code",
            CrashIn::RoxBoxCode => "rox_box_code",
            CrashIn::HostDll => "host_dll",
            CrashIn::NoModule => "outside_any_module",
        }
    }
}

/// The kind of exception a crash was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Exception {
    AccessViolation = 1,
    IllegalInstruction,
    PrivilegedInstruction,
    IntegerDivideByZero,
    StackOverflow,
    Breakpoint,
    CppException,
    Other,
}

impl Exception {
    const ALL: [Exception; 8] = [
        Exception::AccessViolation,
        Exception::IllegalInstruction,
        Exception::PrivilegedInstruction,
        Exception::IntegerDivideByZero,
        Exception::StackOverflow,
        Exception::Breakpoint,
        Exception::CppException,
        Exception::Other,
    ];

    /// The kind of the exception with code `code` (`EXCEPTION_*`, an
    /// `NTSTATUS`).
    pub const fn of_code(code: u32) -> Exception {
        match code {
            0xC000_0005 => Exception::AccessViolation,
            0xC000_001D => Exception::IllegalInstruction,
            0xC000_0096 => Exception::PrivilegedInstruction,
            0xC000_0094 => Exception::IntegerDivideByZero,
            0xC000_00FD => Exception::StackOverflow,
            0x8000_0003 => Exception::Breakpoint,
            0xE06D_7363 => Exception::CppException,
            _ => Exception::Other,
        }
    }

    pub const fn key(self) -> &'static str {
        match self {
            Exception::AccessViolation => "access_violation",
            Exception::IllegalInstruction => "illegal_instruction",
            Exception::PrivilegedInstruction => "privileged_instruction",
            Exception::IntegerDivideByZero => "integer_divide_by_zero",
            Exception::StackOverflow => "stack_overflow",
            Exception::Breakpoint => "breakpoint",
            Exception::CppException => "cpp_exception",
            Exception::Other => "other",
        }
    }
}

/// How a run ended, in our words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The title exited, or ended its process, with this code.
    TitleExited(i32),
    /// The title called `abort` or `terminate`.
    TitleAborted,
    /// The window was closed or the title relaunched itself: it was
    /// suspended and ended as the console ends a title.
    TitleEnded,
    /// The host removed the graphics device, with this reason (`HRESULT`).
    DeviceRemoved(u32),
    /// The host GPU failed for good without removing the device.
    HostGpuFailed,
    /// An unhandled exception.
    Crash(CrashIn, Exception),
    /// The title called an import nobody implements, which traps.
    UnimplementedImport,
    /// The title's main thread stopped on an error of ours.
    RoxBoxError,
}

impl End {
    fn encode(self) -> u64 {
        let (kind, a, b, code): (u64, u8, u8, u32) = match self {
            End::TitleExited(code) => (1, 0, 0, code as u32),
            End::TitleAborted => (2, 0, 0, 0),
            End::TitleEnded => (3, 0, 0, 0),
            End::DeviceRemoved(hresult) => (4, 0, 0, hresult),
            End::HostGpuFailed => (5, 0, 0, 0),
            End::Crash(place, kind) => (6, place as u8, kind as u8, 0),
            End::UnimplementedImport => (7, 0, 0, 0),
            End::RoxBoxError => (8, 0, 0, 0),
        };
        kind | u64::from(a) << 8 | u64::from(b) << 16 | u64::from(code) << 32
    }

    fn decode(word: u64) -> Option<End> {
        let code = (word >> 32) as u32;
        let (a, b) = ((word >> 8) as u8, (word >> 16) as u8);
        Some(match word & 0xff {
            1 => End::TitleExited(code as i32),
            2 => End::TitleAborted,
            3 => End::TitleEnded,
            4 => End::DeviceRemoved(code),
            5 => End::HostGpuFailed,
            6 => End::Crash(
                *CrashIn::ALL.iter().find(|c| **c as u8 == a)?,
                *Exception::ALL.iter().find(|e| **e as u8 == b)?,
            ),
            7 => End::UnimplementedImport,
            8 => End::RoxBoxError,
            _ => return None,
        })
    }

    const fn key(self) -> &'static str {
        match self {
            End::TitleExited(_) => "title_exited",
            End::TitleAborted => "title_aborted",
            End::TitleEnded => "title_ended_by_rox_box",
            End::DeviceRemoved(_) => "host_device_removed",
            End::HostGpuFailed => "host_gpu_failed",
            End::Crash(..) => "crash",
            End::UnimplementedImport => "unimplemented_import_trapped",
            End::RoxBoxError => "rox_box_error",
        }
    }
}

/// The documented name of a device-removal reason, if it is one of them.
fn removal_reason(hresult: u32) -> &'static str {
    match hresult {
        0x887A_0005 => "device_removed",
        0x887A_0006 => "device_hung",
        0x887A_0007 => "device_reset",
        0x887A_0020 => "driver_internal_error",
        0x8007_000E => "out_of_memory",
        0x8007_0057 => "invalid_argument",
        _ => "other",
    }
}

static END: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// The run: facts set once, and the writer.

/// The graphics adapter the run's host device was made on.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuFacts {
    pub adapter: HostText,
    pub vendor_id: u32,
    /// The driver's version as Windows counts it (four 16-bit parts).
    pub driver: Option<u64>,
    pub dedicated_mib: u64,
    pub software: bool,
    /// The highest feature level, as `(12, 2)`.
    pub feature_level: (u8, u8),
    /// The highest shader model, as `(6, 6)`.
    pub shader_model: (u8, u8),
    /// Wave lane counts (0: not known).
    pub wave_min: u32,
    pub wave_max: u32,
    pub mesh_shaders: bool,
}

/// What the report knows of the run beyond its counters.
struct State {
    dir: Option<PathBuf>,
    name: String,
    switches: Vec<(String, String)>,
    console: Option<Ours>,
    windows: Option<(u32, u32, u32)>,
    cpu: Option<(HostText, u32)>,
    /// Installed memory and free commit memory at the start, in MiB.
    memory: Option<(u64, u64)>,
    gpu: Option<GpuFacts>,
    platform_dll: Option<fn(&str) -> bool>,
    /// Every unimplemented import's names and call count.
    imports: Vec<(&'static str, &'static str, &'static AtomicU64)>,
    samplers: Vec<fn()>,
}

static STATE: Mutex<State> = Mutex::new(State {
    dir: None,
    name: String::new(),
    switches: Vec::new(),
    console: None,
    windows: None,
    cpu: None,
    memory: None,
    gpu: None,
    platform_dll: None,
    imports: Vec::new(),
    samplers: Vec::new(),
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a report is being kept (after [`begin`]).
static ACTIVE: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();

fn now_us() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// Whether a report is being kept, or the self-test's will be written
/// ([`keep_host_facts`]): the GPU side asks before it queries the device for
/// the host facts.
pub fn active() -> bool {
    ACTIVE.load(Relaxed) || FACTS.load(Relaxed)
}

/// Set by [`keep_host_facts`].
static FACTS: AtomicBool = AtomicBool::new(false);

/// The self-test's report will be written ([`write_selftest`]): the host's
/// facts are kept as for a run (the GPU side's, when it makes its device),
/// but nothing of a run is counted and no helper thread starts.
pub fn keep_host_facts() {
    if enabled() {
        FACTS.store(true, Relaxed);
    }
}

/// The console model the title is told it runs on.
pub fn console(model: Ours) {
    state().console = Some(model);
}

/// The Windows version, as `RtlGetVersion` gives it.
pub fn host_windows(major: u32, minor: u32, build: u32) {
    state().windows = Some((major, minor, build));
}

/// The processor's brand string and the logical processors there are.
pub fn host_cpu(brand: HostText, logical: u32) {
    state().cpu = Some((brand, logical));
}

/// Installed memory and free commit memory at the start, in MiB.
pub fn host_memory(installed_mib: u64, free_commit_mib: u64) {
    state().memory = Some((installed_mib, free_commit_mib));
}

/// The graphics adapter of the run's host device.
pub fn host_gpu(facts: GpuFacts) {
    state().gpu = Some(facts);
}

/// Tells which DLLs are the platform's (our registry's or the host policy's):
/// only imports of those are named, any other is only counted.
pub fn set_platform_dll(test: fn(&str) -> bool) {
    state().platform_dll = Some(test);
}

/// An import bound to a stub that says it is not implemented, with its call
/// count (kept by the stub). Its names are written only if [`set_platform_dll`]
/// says the DLL is the platform's.
pub fn import_site(dll: &'static str, symbol: &'static str, calls: &'static AtomicU64) {
    state().imports.push((dll, symbol, calls));
}

/// Called before each report is rendered, to copy counters kept elsewhere in
/// (from the helper thread, or the thread that ends the run).
pub fn add_sampler(sample: fn()) {
    let mut state = state();
    if !state.samplers.contains(&sample) {
        state.samplers.push(sample);
    }
}

/// Starts the report of this run in `dir` (`<data_dir>\reports`, created
/// and checked by the caller): names its file, removes the oldest beyond
/// [`KEEP`], notes the switches set and starts the helper thread that writes
/// it every [`EVERY`]. The report's path; `None` if reports are off.
pub fn begin(dir: PathBuf) -> Option<PathBuf> {
    if !enabled() || ACTIVE.load(Relaxed) {
        return None;
    }
    START.get_or_init(Instant::now);
    let name = new_name(&dir, Kind::Run);
    prune(&dir, Kind::Run, KEEP - 1);
    {
        let mut state = state();
        state.switches = switches(std::env::vars_os());
        state.dir = Some(dir.clone());
        state.name = name.clone();
    }
    ACTIVE.store(true, Relaxed);
    let started = std::thread::Builder::new()
        .name("run report".to_owned())
        .spawn(writer);
    if let Ok(handle) = started {
        let _ = WRITER.set(handle.thread().clone());
    }
    Some(dir.join(name))
}

/// The `VICTORY_*` variables set, sorted.
fn switches(
    vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = vars.filter_map(|(n, v)| switch_entry(&n, &v)).collect();
    all.sort();
    all.dedup_by(|a, b| a.0 == b.0);
    all
}

static WRITER: OnceLock<std::thread::Thread> = OnceLock::new();
static WRITE_NOW: AtomicBool = AtomicBool::new(false);
/// Writes begun, and one more than the last of them done (attempted), for
/// those waiting for a write that began after they asked ([`Written`]): one
/// that began before may have taken its numbers before the crash was stored.
static STARTS: AtomicU64 = AtomicU64::new(0);
static DONE: AtomicU64 = AtomicU64::new(0);
/// One write at a time.
static WRITING: Mutex<()> = Mutex::new(());
/// A report that names how the run ended is written: the last one. A write
/// that comes after it (the helper thread's first, when the run ended before
/// that thread got to it, or its next turn) would only be cut off by the
/// process ending, and leave its `.part` file behind.
static LAST_WRITTEN: AtomicBool = AtomicBool::new(false);

/// The helper thread: a report now, then every [`EVERY`] and whenever asked.
fn writer() {
    lower_priority();
    let mut next = Instant::now();
    loop {
        let now = Instant::now();
        if WRITE_NOW.swap(false, Relaxed) || now >= next {
            let _ = write();
            next = Instant::now() + EVERY;
        } else {
            std::thread::park_timeout(next - now);
        }
    }
}

/// The helper thread works at a priority below the title's threads.
fn lower_priority() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn SetThreadPriority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
        }
        const THREAD_PRIORITY_LOWEST: i32 = -2;
        // SAFETY: the pseudo handle of the calling thread, and a valid priority.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST) };
    }
}

/// Renders and writes the report (to `.part`, then renamed over the last).
fn write() -> Option<PathBuf> {
    let _one = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let begun = STARTS.fetch_add(1, SeqCst);
    if LAST_WRITTEN.load(SeqCst) {
        // (Counted as done all the same: whoever waits for a write begun
        // after their ask has the report they wait for.)
        DONE.fetch_max(begun + 1, SeqCst);
        return None;
    }
    // Read before the numbers are taken: an end stored from here on is in a
    // later write, which is then the last.
    let ended = END.load(SeqCst) != 0;
    let samplers = state().samplers.clone();
    for sample in samplers {
        sample();
    }
    let snapshot = Snapshot::take();
    let text = render(&snapshot);
    let (dir, name) = {
        let state = state();
        (state.dir.clone(), state.name.clone())
    };
    let written = dir.and_then(|dir| {
        let (path, part) = (dir.join(&name), dir.join(format!("{name}.part")));
        std::fs::write(&part, text).ok()?;
        std::fs::rename(&part, &path).ok()?;
        Some(path)
    });
    if let Some(path) = &written {
        say(path);
        if ended {
            LAST_WRITTEN.store(true, SeqCst);
        }
    }
    DONE.fetch_max(begun + 1, SeqCst);
    written
}

/// The end of the run, said once per way of ending (to the console the run
/// was started from; the path is the player's own).
fn say(path: &Path) {
    static SAID: AtomicU64 = AtomicU64::new(0);
    let end = END.load(Relaxed);
    if end != 0 && SAID.swap(end, Relaxed) != end {
        println!(
            "run report     {} (safe to share in an issue: numbers and Rox-box's own words only; \
             `victory report` shows it)",
            path.display()
        );
    }
}

/// The run ended `end`: the report is written now, on this thread. Its path,
/// if it was written.
pub fn finish(end: End) -> Option<PathBuf> {
    if !ACTIVE.load(Relaxed) {
        return None;
    }
    END.store(end.encode(), SeqCst);
    write()
}

/// A crash, from the exception filter: stores the end and wakes the helper
/// thread to write the report, allocating nothing here. Wait with
/// [`Written::wait`] before letting the process end.
pub fn crash(place: CrashIn, kind: Exception) -> Written {
    if !ACTIVE.load(Relaxed) {
        return Written(0);
    }
    END.store(End::Crash(place, kind).encode(), SeqCst);
    // (Any write begun from here on sees the end stored above.)
    let ticket = Written(STARTS.load(SeqCst));
    wake();
    ticket
}

/// A panic of ours: counted, and the report written soon (waited for, for a
/// second, unless this is the helper thread).
pub fn panicked() {
    STAT[Stat::Panics as usize].fetch_add(1, SeqCst);
    if !ACTIVE.load(Relaxed) {
        return;
    }
    let ticket = Written(STARTS.load(SeqCst));
    wake();
    if WRITER
        .get()
        .is_some_and(|w| w.id() != std::thread::current().id())
    {
        ticket.wait(Duration::from_secs(1));
    }
}

fn wake() {
    WRITE_NOW.store(true, Relaxed);
    if let Some(writer) = WRITER.get() {
        writer.unpark();
    }
}

/// A write asked for ([`crash`]): the number of writes begun before.
#[must_use]
pub struct Written(u64);

impl Written {
    /// Waits until a write begun after the ask is done, at most `most`.
    pub fn wait(self, most: Duration) {
        if !ACTIVE.load(Relaxed) {
            return;
        }
        let until = Instant::now() + most;
        while DONE.load(SeqCst) <= self.0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

// ---------------------------------------------------------------------------
// The self-test's report.
//
// `victory selftest` runs no title, so its result was only the terminal's
// text, which a player had to copy, without the host's facts a run report
// collects. It writes a report of its own through the same renderer and
// guard: the host, then each stage's outcome in our words, the benchmark's
// rates as numbers. A stage's detail and a failure's text are built at run
// time (numbers, the adapter's name) and are not written: the outcome and
// one of our fixed reasons are.

/// How a self-test stage ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageOutcome {
    Ok,
    Fail,
    Skip,
}

impl StageOutcome {
    const fn key(self) -> &'static str {
        match self {
            StageOutcome::Ok => "ok",
            StageOutcome::Fail => "fail",
            StageOutcome::Skip => "skip",
        }
    }
}

/// One stage: our name of it, its outcome, and our fixed wording of why
/// (a skip's cause, a failure's kind), if there is one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelftestStage {
    pub name: Ours,
    pub outcome: StageOutcome,
    pub reason: Option<Ours>,
}

/// A rate of the benchmark: our name, the value, our word for its unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelftestRate {
    pub name: Ours,
    pub value: f64,
    pub unit: Ours,
}

/// What a self-test did.
#[derive(Clone, Debug, PartialEq)]
pub struct Selftest {
    /// On the software adapter (`--warp`).
    pub software_adapter: bool,
    /// Frames of the clear-and-present stage, and whether rates were taken.
    pub frames: u32,
    pub bench: bool,
    /// How it ended: `completed`, `no_device`, `hung`, `host_gpu_failed`.
    pub ended: Ours,
    pub stages: Vec<SelftestStage>,
    pub rates: Vec<SelftestRate>,
}

/// Writes the self-test's report in `dir` (`<data_dir>\reports`, made and
/// checked by the caller) as `selftest-<UTC time>.txt`, keeping the newest
/// [`KEEP`] of them. Its path; `None` if reports are off or it could not be
/// written.
pub fn write_selftest(dir: &Path, test: &Selftest) -> Option<PathBuf> {
    if !enabled() {
        return None;
    }
    let switches = switches(std::env::vars_os());
    let text = render_selftest(&Snapshot::take(), test, &switches);
    let name = new_name(dir, Kind::Selftest);
    prune(dir, Kind::Selftest, KEEP - 1);
    let (path, part) = (dir.join(&name), dir.join(format!("{name}.part")));
    std::fs::write(&part, text).ok()?;
    std::fs::rename(&part, &path).ok()?;
    Some(path)
}

/// The opening comment of a self-test's report.
const SELFTEST_PREAMBLE: &str = "\
# Rox-box self-test report
#
# Written by `victory selftest` when it ended. The self-test runs a stand-in
# title made of Rox-box's own code: no game files are read. It is meant to be
# attached to a public issue, and holds only:
#   - numbers: the benchmark's rates, counts of stages;
#   - words from Rox-box's own source code: the stages' names, their outcomes
#     and our fixed reasons;
#   - facts about this PC: the Windows build, the processor model, memory
#     sizes, the graphics adapter's name, vendor and driver version;
#   - the names of the VICTORY_* switches set, with a value only where it
#     is a short plain number or word.
# It never holds: file or folder names, paths, your user or computer name,
# or the text the self-test printed. docs/selftest.md explains every field.
";

/// A self-test's report: the host from `s`, then `test`.
fn render_selftest(s: &Snapshot, test: &Selftest, switches: &[(String, String)]) -> String {
    let mut d = Doc {
        out: String::from(SELFTEST_PREAMBLE),
        host: false,
    };
    d.line(format_args!(""));
    d.int("format", FORMAT);
    d.word("kind", Ours("selftest"));
    host_sections(&mut d, s);

    d.table("selftest");
    d.word("ended", test.ended);
    d.boolean("software_adapter", test.software_adapter);
    d.int("frames", test.frames);
    d.boolean("bench", test.bench);
    let count = |o: StageOutcome| test.stages.iter().filter(|t| t.outcome == o).count();
    d.boolean("passed", count(StageOutcome::Fail) == 0 && test.ended == Ours("completed"));
    d.int("stages_ok", count(StageOutcome::Ok));
    d.int("stages_failed", count(StageOutcome::Fail));
    d.int("stages_skipped", count(StageOutcome::Skip));
    d.table("selftest.switches");
    d.comment("VICTORY_* variables set; a value only where it is a plain number or word");
    let mut named: Vec<String> = Vec::new();
    for (name, value) in switches {
        let (name, value) = (d.quoted(name), d.quoted(value));
        if !named.contains(&name) {
            d.line(format_args!("{name} = {value}"));
            named.push(name);
        }
    }
    if !test.rates.is_empty() {
        d.table("selftest.rates");
        d.comment("the benchmark (docs/selftest.md); each rate's unit is in [selftest.rate_units]");
        d.keyed_floats(test.rates.iter().map(|r| (r.name.as_str(), r.value)));
        d.table("selftest.rate_units");
        for r in &test.rates {
            let (name, unit) = (d.quoted(r.name.as_str()), d.quoted(r.unit.as_str()));
            d.line(format_args!("{name} = {unit}"));
        }
    }
    for stage in &test.stages {
        d.array_table("selftest.stage");
        d.word("name", stage.name);
        d.word("outcome", Ours(stage.outcome.key()));
        if let Some(reason) = stage.reason {
            d.word("reason", reason);
        }
    }
    d.out
}

// ---------------------------------------------------------------------------
// Files.

/// `yyyymmdd-hhmmss` (UTC) of `secs` since the Unix epoch.
fn utc_stamp(secs: u64) -> String {
    let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil from days (the proleptic Gregorian calendar, after H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// What a report file is of: a run (`run-<stamp>.txt`) or a self-test
/// (`selftest-<stamp>.txt`). Each kind keeps its newest [`KEEP`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Run,
    Selftest,
}

impl Kind {
    const fn prefix(self) -> &'static str {
        match self {
            Kind::Run => "run-",
            Kind::Selftest => "selftest-",
        }
    }

    /// The kind of the report file called `name`, if it is one.
    pub fn of_file(name: &str) -> Option<Kind> {
        [Kind::Run, Kind::Selftest]
            .into_iter()
            .find(|k| report_order(name, *k).is_some())
    }
}

/// A free name for a new report of `kind` in `dir` (a second report in the
/// same second gets `-2`, `-3`...).
fn new_name(dir: &Path, kind: Kind) -> String {
    let stamp = utc_stamp(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    );
    let prefix = kind.prefix();
    let mut name = format!("{prefix}{stamp}.txt");
    for n in 2.. {
        if !dir.join(&name).exists() {
            break;
        }
        name = format!("{prefix}{stamp}-{n}.txt");
    }
    name
}

/// The order of a report file's name of `kind` (time, then the number added
/// to a second report in the same second); `None` for any other file.
fn report_order(name: &str, kind: Kind) -> Option<(String, u32)> {
    let stem = name.strip_prefix(kind.prefix())?.strip_suffix(".txt")?;
    let (stamp, n) = match stem.get(15..) {
        Some("") => (stem, 1),
        Some(rest) => (&stem[..15], rest.strip_prefix('-')?.parse().ok()?),
        None => return None,
    };
    let b = stamp.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    (b.len() == 15 && b[8] == b'-' && digits(0..8) && digits(9..15)).then(|| (stamp.to_owned(), n))
}

/// The reports in `dir`, runs' and self-tests', newest first.
pub fn reports_in(dir: &Path) -> Vec<PathBuf> {
    reports_of(dir, &[Kind::Run, Kind::Selftest])
}

/// The reports of `kinds` in `dir`, newest first.
pub fn reports_of(dir: &Path, kinds: &[Kind]) -> Vec<PathBuf> {
    let mut found: Vec<((String, u32), PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            let order = kinds.iter().find_map(|&k| report_order(name, k))?;
            Some((order, e.path()))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, path)| path).collect()
}

/// One line about a report, for `victory report --list`: how the run ended,
/// how long it ran, its average frame rate; for a self-test, how it ended
/// and its stages; `None` if `text` is no report.
pub fn summary(text: &str) -> Option<String> {
    let parsed: toml::Table = toml::from_str(text).ok()?;
    if parsed.get("kind").and_then(|k| k.as_str()) == Some("selftest") {
        let test = parsed.get("selftest")?.as_table()?;
        let n = |key: &str| test.get(key).and_then(|v| v.as_integer()).unwrap_or(0);
        return Some(format!(
            "self-test{}, {}: {} ok, {} failed, {} skipped",
            if test.get("software_adapter").and_then(|v| v.as_bool()) == Some(true) {
                " on the software adapter"
            } else {
                ""
            },
            test.get("ended")?.as_str()?,
            n("stages_ok"),
            n("stages_failed"),
            n("stages_skipped"),
        ));
    }
    let run = parsed.get("run")?.as_table()?;
    let ended = run.get("ended")?.as_str()?;
    let duration = run.get("duration_s")?.as_float()?;
    let fps = parsed
        .get("frames")
        .and_then(|f| f.get("average_fps"))
        .and_then(|v| v.as_float())
        .unwrap_or(0.0);
    Some(format!("{ended}, {duration:.0} s, {fps:.1} fps on average"))
}

/// Removes all but the newest `keep` reports of `kind` in `dir`, and any
/// `.part` left by a process that ended while writing one.
fn prune(dir: &Path, kind: Kind, keep: usize) {
    for old in reports_of(dir, &[kind]).into_iter().skip(keep) {
        let _ = std::fs::remove_file(old);
    }
    for entry in std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_suffix(".part")
            .is_some_and(|n| report_order(n, kind).is_some())
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// The memory map: the process's memory by our own categories (`[memory.map]`).
//
// The GPU side and the HLE side give each category's bytes as they sample
// their counters (`map_set`, before each report is written); nothing is fed
// from a frame's path for it. A category nobody has given is not known, and
// is left out of the section (a category known to be empty is 0). The ones
// of kind `Private` are what the process holds in its own private memory:
// their sum is `accounted_total`, and the process's private commit less that
// sum is `unaccounted` (the driver, the title's own heaps, what we do not
// count). The others are memory the card or a mapped section holds, and
// counts: listed beside, never summed.

/// What a [`Part`] is, for the sum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartKind {
    /// Bytes held in the process's private memory: added to the total.
    Private,
    /// Bytes held beside it (the card's memory, mapped sections, the
    /// driver's heaps): listed, not added.
    Beside,
    /// A count, not bytes.
    Count,
}

/// A category of the memory map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Part {
    /// Memory the title committed (the host pages of its `VirtualAlloc`s,
    /// each counted once, less what it freed).
    TitleCommitted,
    /// Blocks the title took from the default heap.
    TitleDefaultBlocks,
    /// Host copies of the title's buffers (less the windows below): an
    /// estimate, the bytes alive less the windows' lengths.
    TitleBufferCopies,
    /// Host copies of windows of title memory outside buffers (their
    /// lengths; sparse windows hold tiles on the card, counted there).
    Windows,
    /// Snapshot buffers kept for reuse (their capacity).
    SnapshotBuffers,
    /// Scratch chunks of host lists (alive: held, spare and reserve).
    ListScratch,
    /// The other upload buffers of host lists.
    ListUploads,
    /// Staging buffers of textures.
    TextureStaging,
    /// Memory kept spare for sends.
    SendSpare,
    /// The tables of distinct views: an estimate (entries, index nodes).
    ViewTable,
    /// Shader containers the pipeline table keeps.
    ShaderContainers,
    /// Translated shaders' host code the pipeline table keeps.
    ShaderTranslations,
    /// The pipeline library's file, kept whole in memory.
    PipelineLibrary,
    /// The title's page pool (pages it was given): sections, not private
    /// commit.
    TitlePagePool,
    /// Host copies of textures the title fills, on the card.
    CardTextures,
    /// Host textures the host renders to or writes (the rest of the card's
    /// textures), on the card.
    CardTargets,
    /// Tiles given to textures, on the card.
    CardTextureTiles,
    /// Card sides of host copies of title buffers.
    CardBufferCopies,
    /// Tiles given to sparse windows, on the card.
    CardSparseTiles,
    /// Shader-visible descriptor heaps the host made.
    DescriptorHeaps,
    /// Host lists alive (a count; their allocators are not measured).
    HostListsAlive,
}

impl Part {
    pub const ALL: [Part; 21] = [
        Part::TitleCommitted,
        Part::TitleDefaultBlocks,
        Part::TitleBufferCopies,
        Part::Windows,
        Part::SnapshotBuffers,
        Part::ListScratch,
        Part::ListUploads,
        Part::TextureStaging,
        Part::SendSpare,
        Part::ViewTable,
        Part::ShaderContainers,
        Part::ShaderTranslations,
        Part::PipelineLibrary,
        Part::TitlePagePool,
        Part::CardTextures,
        Part::CardTargets,
        Part::CardTextureTiles,
        Part::CardBufferCopies,
        Part::CardSparseTiles,
        Part::DescriptorHeaps,
        Part::HostListsAlive,
    ];

    /// The key the report writes it under (MiB for bytes, else a count).
    pub const fn key(self) -> &'static str {
        match self {
            Part::TitleCommitted => "title_committed_mib",
            Part::TitleDefaultBlocks => "title_default_blocks_mib",
            Part::TitleBufferCopies => "title_buffer_copies_mib",
            Part::Windows => "windows_of_title_memory_mib",
            Part::SnapshotBuffers => "snapshot_buffers_mib",
            Part::ListScratch => "list_scratch_mib",
            Part::ListUploads => "list_upload_buffers_mib",
            Part::TextureStaging => "texture_staging_mib",
            Part::SendSpare => "send_spare_mib",
            Part::ViewTable => "view_table_mib",
            Part::ShaderContainers => "shader_containers_mib",
            Part::ShaderTranslations => "shader_translations_mib",
            Part::PipelineLibrary => "pipeline_library_mib",
            Part::TitlePagePool => "title_page_pool_mib",
            Part::CardTextures => "card_textures_filled_mib",
            Part::CardTargets => "card_textures_rendered_mib",
            Part::CardTextureTiles => "card_texture_tiles_mib",
            Part::CardBufferCopies => "card_buffer_copies_mib",
            Part::CardSparseTiles => "card_sparse_tiles_mib",
            Part::DescriptorHeaps => "descriptor_heaps_mib",
            Part::HostListsAlive => "host_lists_alive",
        }
    }

    pub const fn kind(self) -> PartKind {
        match self {
            Part::TitleCommitted
            | Part::TitleDefaultBlocks
            | Part::TitleBufferCopies
            | Part::Windows
            | Part::SnapshotBuffers
            | Part::ListScratch
            | Part::ListUploads
            | Part::TextureStaging
            | Part::SendSpare
            | Part::ViewTable
            | Part::ShaderContainers
            | Part::ShaderTranslations
            | Part::PipelineLibrary => PartKind::Private,
            Part::HostListsAlive => PartKind::Count,
            _ => PartKind::Beside,
        }
    }
}

/// A part nobody has given, or the process commit not yet asked.
const NOT_KNOWN: u64 = u64::MAX;

static MAP_NOW: [AtomicU64; Part::ALL.len()] = [const { AtomicU64::new(NOT_KNOWN) }; Part::ALL.len()];
static MAP_PEAK: [AtomicU64; Part::ALL.len()] = [const { AtomicU64::new(0) }; Part::ALL.len()];
/// The process's private commit now, and the largest `accounted_total` and
/// `unaccounted` any report found.
static MAP_COMMIT: AtomicU64 = AtomicU64::new(NOT_KNOWN);
static MAP_PEAK_TOTAL: AtomicU64 = AtomicU64::new(0);
static MAP_PEAK_UNACCOUNTED: AtomicU64 = AtomicU64::new(0);

/// `part` holds `value` now (bytes; a count for [`Part::HostListsAlive`]): a
/// counter kept elsewhere, sampled. Keeps the largest value given.
pub fn map_set(part: Part, value: u64) {
    let value = value.min(NOT_KNOWN - 1);
    MAP_NOW[part as usize].store(value, Relaxed);
    MAP_PEAK[part as usize].fetch_max(value, Relaxed);
}

/// What `part` held when it was last given (`None`: never given).
pub fn map_get(part: Part) -> Option<u64> {
    Some(MAP_NOW[part as usize].load(Relaxed)).filter(|&n| n != NOT_KNOWN)
}

/// The process's private commit now, in bytes.
pub fn map_commit(bytes: u64) {
    MAP_COMMIT.store(bytes.min(NOT_KNOWN - 1), Relaxed);
}

/// The memory map as one report holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MapSnapshot {
    now: [u64; Part::ALL.len()],
    peak: [u64; Part::ALL.len()],
    commit: u64,
    peak_total: u64,
    peak_unaccounted: u64,
}

impl Default for MapSnapshot {
    fn default() -> Self {
        MapSnapshot {
            now: [NOT_KNOWN; Part::ALL.len()],
            peak: [0; Part::ALL.len()],
            commit: NOT_KNOWN,
            peak_total: 0,
            peak_unaccounted: 0,
        }
    }
}

impl MapSnapshot {
    /// The bytes of the parts that are private and known, added.
    fn accounted(&self) -> u64 {
        Part::ALL
            .iter()
            .filter(|p| p.kind() == PartKind::Private && self.now[**p as usize] != NOT_KNOWN)
            .map(|p| self.now[*p as usize])
            .fold(0, u64::saturating_add)
    }

    /// The private commit less [`MapSnapshot::accounted`] (0 if the parts
    /// add up to more); `None` if the commit is not known.
    fn unaccounted(&self) -> Option<u64> {
        (self.commit != NOT_KNOWN).then(|| self.commit.saturating_sub(self.accounted()))
    }
}

/// The map as it stands, with the largest totals updated by this look.
fn map_take() -> MapSnapshot {
    let mut m = MapSnapshot {
        now: std::array::from_fn(|i| MAP_NOW[i].load(Relaxed)),
        peak: std::array::from_fn(|i| MAP_PEAK[i].load(Relaxed)),
        commit: MAP_COMMIT.load(Relaxed),
        ..MapSnapshot::default()
    };
    m.peak_total = MAP_PEAK_TOTAL.fetch_max(m.accounted(), Relaxed).max(m.accounted());
    if let Some(u) = m.unaccounted() {
        m.peak_unaccounted = MAP_PEAK_UNACCOUNTED.fetch_max(u, Relaxed).max(u);
    } else {
        m.peak_unaccounted = MAP_PEAK_UNACCOUNTED.load(Relaxed);
    }
    m
}

/// `[memory.map]` and `[memory.map.peak]`: MiB, every part known, and the
/// totals.
fn memory_map(d: &mut Doc, m: &MapSnapshot) {
    let mib = |b: u64| (b + (1 << 19)) >> 20;
    let value = |part: Part, n: u64| match part.kind() {
        PartKind::Count => n,
        _ => mib(n),
    };
    d.table("memory.map");
    d.comment("MiB, sampled when the report is written; a category left out is one nothing counted (not known)");
    d.comment("private: what the process holds in its own private memory; accounted_total is their sum,");
    d.comment("unaccounted the process's private commit less it (the driver, the title's own heaps, what is not counted)");
    if m.commit != NOT_KNOWN {
        d.int("process_private_commit_mib", mib(m.commit));
    }
    d.int("accounted_total_mib", mib(m.accounted()));
    if let Some(u) = m.unaccounted() {
        d.int("unaccounted_mib", mib(u));
    }
    for kind in [PartKind::Private, PartKind::Beside, PartKind::Count] {
        d.comment(match kind {
            PartKind::Private => "private memory, in the total",
            PartKind::Beside => "beside it (card memory, mapped sections, descriptor heaps): not in the total",
            PartKind::Count => "counts",
        });
        for part in Part::ALL.into_iter().filter(|p| p.kind() == kind) {
            let n = m.now[part as usize];
            if n != NOT_KNOWN {
                d.int(part.key(), value(part, n));
            }
        }
    }
    d.table("memory.map.peak");
    d.comment("the most each category held when a report was written (a report is written every 30 s and at the end)");
    d.int("accounted_total_mib", mib(m.peak_total));
    if m.commit != NOT_KNOWN {
        d.int("unaccounted_mib", mib(m.peak_unaccounted));
    }
    for part in Part::ALL {
        if m.now[part as usize] != NOT_KNOWN {
            d.int(part.key(), value(part, m.peak[part as usize]));
        }
    }
}

// ---------------------------------------------------------------------------
// Copies of buffer views' own bytes (`[memory.view_copies]`): how many were
// cut from blocks and how many were buffers of their own, and the blocks.
// Given by the GPU side as it samples its counters (`view_copies_set`); left
// out of the report until given.

/// Copies cut from blocks and buffers of their own made, since the start;
/// blocks now and their bytes ([`NOT_KNOWN`]: not given).
static VIEW_COPIES: [AtomicU64; 4] = [const { AtomicU64::new(NOT_KNOWN) }; 4];

/// The copies of buffer views' own bytes as counted now: `cut` from blocks
/// and `own` buffers of their own made since the start, `blocks` there are
/// now and their `bytes`.
pub fn view_copies_set(cut: u64, own: u64, blocks: u64, bytes: u64) {
    for (at, n) in VIEW_COPIES.iter().zip([cut, own, blocks, bytes]) {
        at.store(n.min(NOT_KNOWN - 1), Relaxed);
    }
}

fn view_copies_take() -> Option<[u64; 4]> {
    let v: [u64; 4] = std::array::from_fn(|i| VIEW_COPIES[i].load(Relaxed));
    (!v.contains(&NOT_KNOWN)).then_some(v)
}

/// `[memory.view_copies]`, if given.
fn view_copies(d: &mut Doc, v: Option<[u64; 4]>) {
    let Some([cut, own, blocks, bytes]) = v else { return };
    d.table("memory.view_copies");
    d.comment("copies of buffer views' own bytes since the start: cut from blocks (VICTORY_VIEW_BYTES_POOL), buffers of their own made");
    d.int("cut_from_blocks", cut);
    d.int("own_buffers_made", own);
    d.comment("the blocks they are cut from now, and their MiB");
    d.int("blocks", blocks);
    d.int("blocks_mib", (bytes + (1 << 19)) >> 20);
}

// ---------------------------------------------------------------------------
// Rendering.

/// Everything a report says, taken at once.
struct Snapshot {
    duration_us: u64,
    end: Option<End>,
    console: Option<Ours>,
    switches: Vec<(String, String)>,
    windows: Option<(u32, u32, u32)>,
    cpu: Option<(HostText, u32)>,
    memory: Option<(u64, u64)>,
    gpu: Option<GpuFacts>,
    stats: [u64; STATS],
    left_out: [u64; LeftOut::ALL.len()],
    presents: u64,
    first_present_us: Option<u64>,
    intervals: u64,
    interval_sum_us: u64,
    interval_max_us: u64,
    fine: Vec<u32>,
    buckets: [u64; 10],
    /// Per window since the start: presents, worst frame in ms.
    timeline: Vec<(u32, u32)>,
    work_sum: [u64; WORK],
    work_max: [u64; WORK],
    first_draw_us: Option<u64>,
    /// The frame line's figures over the whole run, and the steady state
    /// (`None`: no present yet, or a run too short for one).
    figures: [u64; FIGS],
    steady: Option<Steady>,
    /// The lowest free memory and commit headroom seen (bytes; `u64::MAX`
    /// not seen), and the microseconds at each governor level.
    lowest: [u64; 2],
    governor_us: [u64; 3],
    methods: Vec<KeyedEntry>,
    methods_lost: u64,
    shader_failures: Vec<KeyedEntry>,
    shader_failures_lost: u64,
    /// Named imports of platform DLLs with their calls; and of all the
    /// others, how many were called and how often.
    imports: Vec<(String, u64)>,
    other_imports: (u64, u64),
    /// The memory map ([`Part`]).
    map: MapSnapshot,
    /// The card's time by kind (`[gpu_time]`).
    gpu_time: GpuTime,
}

impl Snapshot {
    fn take() -> Snapshot {
        let load = |a: &AtomicU64| a.load(Relaxed);
        let duration_us = now_us();
        let windows_used = ((duration_us / 1_000_000 / WINDOW_S) as usize + 1).min(WINDOWS);
        let first_present_us = Some(load(&FIRST_PRESENT_US)).filter(|&t| t != u64::MAX);
        let take = |w: &[AtomicU64; FIGS]| -> [u64; FIGS] { std::array::from_fn(|f| load(&w[f])) };
        let windows: Vec<[u64; FIGS]> = FIGURES[..windows_used].iter().map(take).collect();
        let seconds: Vec<[u64; FIGS]> = SECONDS.iter().map(take).collect();
        let (figures, _) = split_figures(&windows, None);
        let steady = first_present_us
            .and_then(|first| steady_state(first, duration_us, &windows, &seconds));
        // The time since the governor's last look is the level it found.
        let mut governor_us: [u64; 3] = std::array::from_fn(|i| load(&GOVERNOR_US[i]));
        let (last_look, last_level) = (load(&LAST_LOOK_US), load(&LAST_LEVEL));
        if last_look != u64::MAX && (1..=3).contains(&last_level) {
            let i = last_level as usize - 1;
            governor_us[i] = governor_us[i].saturating_add(duration_us.saturating_sub(last_look));
        }
        let state = state();
        let (imports, other_imports) = called_imports(&state.imports, state.platform_dll);
        Snapshot {
            duration_us,
            end: End::decode(END.load(SeqCst)),
            console: state.console,
            switches: state.switches.clone(),
            windows: state.windows,
            cpu: state.cpu.clone(),
            memory: state.memory,
            gpu: state.gpu.clone(),
            stats: std::array::from_fn(|i| load(&STAT[i])),
            left_out: std::array::from_fn(|i| load(&LEFT_OUT[i])),
            presents: load(&PRESENTS),
            first_present_us,
            intervals: load(&INTERVALS),
            interval_sum_us: load(&INTERVAL_SUM_US),
            interval_max_us: load(&INTERVAL_MAX_US),
            fine: FINE_FRAMES.iter().map(|n| n.load(Relaxed)).collect(),
            buckets: std::array::from_fn(|i| load(&BUCKETS[i])),
            timeline: (0..windows_used)
                .map(|i| {
                    (
                        WINDOW_PRESENTS[i].load(Relaxed),
                        WINDOW_WORST_MS[i].load(Relaxed),
                    )
                })
                .collect(),
            work_sum: std::array::from_fn(|i| load(&WORK_SUM[i])),
            work_max: std::array::from_fn(|i| load(&WORK_MAX[i])),
            first_draw_us: Some(load(&FIRST_DRAW_US)).filter(|&t| t != u64::MAX),
            figures,
            steady,
            lowest: std::array::from_fn(|i| load(&LOWEST[i])),
            governor_us,
            methods: METHODS.entries(),
            methods_lost: load(&METHODS.lost),
            shader_failures: SHADER_FAILURES.entries(),
            shader_failures_lost: load(&SHADER_FAILURES.lost),
            imports,
            other_imports,
            map: map_take(),
            gpu_time: GpuTime::take(windows_used, first_present_us, steady.as_ref()),
        }
    }

    fn stat(&self, stat: Stat) -> u64 {
        self.stats[stat as usize]
    }
}

/// The imports called: named where the DLL is the platform's (`platform`)
/// and the names are plain identifiers, most called first; and of the
/// others, how many were called and how often.
fn called_imports(
    sites: &[(&'static str, &'static str, &'static AtomicU64)],
    platform: Option<fn(&str) -> bool>,
) -> (Vec<(String, u64)>, (u64, u64)) {
    let mut named: Vec<(String, u64)> = Vec::new();
    let mut other = (0, 0);
    for &(dll, symbol, calls) in sites {
        let calls = calls.load(Relaxed);
        if calls == 0 {
            continue;
        }
        let ours = platform.is_some_and(|test| test(dll));
        match import_name(dll, symbol).filter(|_| ours) {
            Some(name) => match named.iter_mut().find(|(n, _)| *n == name) {
                Some(seen) => seen.1 += calls,
                None => named.push((name, calls)),
            },
            None => other = (other.0 + 1, other.1 + calls),
        }
    }
    named.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    (named, other)
}

impl Snapshot {
    /// The frame time (ms) under which a share `p` of the frames fell: the
    /// upper edge of its bucket, at most the longest frame.
    fn percentile_ms(&self, p: f64) -> f64 {
        let total: u64 = self.fine.iter().map(|&n| u64::from(n)).sum();
        if total == 0 {
            return 0.0;
        }
        let rank = ((p * total as f64).ceil() as u64).max(1);
        let mut seen = 0;
        for (i, &n) in self.fine.iter().enumerate() {
            seen += u64::from(n);
            if seen >= rank {
                return fine_upper_ms(i).min(self.interval_max_us as f64 / 1000.0);
            }
        }
        self.interval_max_us as f64 / 1000.0
    }
}

/// The opening comment of every report: what it holds and what it never does.
const PREAMBLE: &str = "\
# Rox-box run report
#
# Written by Rox-box at the end of a run (and every 30 seconds while it ran).
# It is meant to be attached to a public issue, and holds only:
#   - numbers: counts, times, sizes, frame times;
#   - words from Rox-box's own source code: reasons, our enum names, our names
#     of interfaces and methods, names of unimplemented imports of the
#     platform's own DLLs;
#   - facts about this PC: the Windows build, the processor model, memory
#     sizes, the graphics adapter's name, vendor and driver version;
#   - the names of the VICTORY_* switches set, with a value only where it
#     is a short plain number or word.
# It never holds: the title's name or id, file or folder names, paths, your
# user or computer name, hashes of executables or shaders, shader or
# pipeline names, memory addresses or offsets into the title, text from the
# title, or command-line arguments. docs/run-report.md explains every field.
";

/// The TOML being written. Every string goes through [`guarded`]; keys are
/// ours, or a table's words (also guarded).
struct Doc {
    out: String,
    /// In the host section, where an adapter name may hold a slash.
    host: bool,
}

impl Doc {
    fn line(&mut self, text: std::fmt::Arguments) {
        use std::fmt::Write;
        let _ = self.out.write_fmt(text);
        self.out.push('\n');
    }

    fn table(&mut self, name: &'static str) {
        self.line(format_args!("\n[{name}]"));
    }

    fn array_table(&mut self, name: &'static str) {
        self.line(format_args!("\n[[{name}]]"));
    }

    fn comment(&mut self, text: &'static str) {
        self.line(format_args!("# {text}"));
    }

    fn int(&mut self, key: &'static str, value: impl std::fmt::Display) {
        self.line(format_args!("{key} = {value}"));
    }

    fn hex(&mut self, key: &'static str, value: u64) {
        self.line(format_args!("{key} = {value:#x}"));
    }

    fn float(&mut self, key: &'static str, value: f64) {
        let value = if value.is_finite() { value } else { 0.0 };
        self.line(format_args!("{key} = {value:.2}"));
    }

    fn boolean(&mut self, key: &'static str, value: bool) {
        self.line(format_args!("{key} = {value}"));
    }

    fn quoted(&self, s: &str) -> String {
        format!("\"{}\"", if guarded(s, self.host) { s } else { WITHHELD })
    }

    fn word(&mut self, key: &'static str, value: Ours) {
        let value = self.quoted(value.as_str());
        self.line(format_args!("{key} = {value}"));
    }

    fn host_text(&mut self, key: &'static str, value: &HostText) {
        let value = self.quoted(value.as_str());
        self.line(format_args!("{key} = {value}"));
    }

    /// A string we formatted from numbers (`10.0`, `12_2`).
    fn formatted(&mut self, key: &'static str, value: std::fmt::Arguments) {
        let value = self.quoted(&value.to_string());
        self.line(format_args!("{key} = {value}"));
    }

    /// A count under a key that is a table's words, quoted.
    fn keyed(&mut self, key: &str, value: u64) {
        let key = self.quoted(key);
        self.line(format_args!("{key} = {value}"));
    }

    /// Counts under keys that are a table's words, those the guard withholds
    /// (or that are equal once guarded) added up: a TOML table holds a key
    /// once.
    fn keyed_all(&mut self, entries: impl Iterator<Item = (String, u64)>) {
        let mut merged: Vec<(String, u64)> = Vec::new();
        for (key, n) in entries {
            let key = self.quoted(&key);
            match merged.iter_mut().find(|(k, _)| *k == key) {
                Some(seen) => seen.1 += n,
                None => merged.push((key, n)),
            }
        }
        for (key, n) in merged {
            self.line(format_args!("{key} = {n}"));
        }
    }

    /// Numbers under keys that are our words, quoted.
    fn keyed_floats<'a>(&mut self, entries: impl Iterator<Item = (&'a str, f64)>) {
        for (key, value) in entries {
            let key = self.quoted(key);
            let value = if value.is_finite() { value } else { 0.0 };
            self.line(format_args!("{key} = {value:.2}"));
        }
    }

    fn ints(&mut self, key: &'static str, values: &[u64]) {
        self.list(key, values.iter().map(u64::to_string));
    }

    fn floats(&mut self, key: &'static str, values: &[f64]) {
        self.list(key, values.iter().map(|v| format!("{v:.1}")));
    }

    fn list(&mut self, key: &'static str, values: impl Iterator<Item = String>) {
        let values: Vec<String> = values.collect();
        if values.len() <= 16 {
            self.line(format_args!("{key} = [{}]", values.join(", ")));
            return;
        }
        self.line(format_args!("{key} = ["));
        for row in values.chunks(16) {
            self.line(format_args!("  {},", row.join(", ")));
        }
        self.line(format_args!("]"));
    }
}

/// The report's text.
fn render(s: &Snapshot) -> String {
    let mut d = Doc {
        out: String::from(PREAMBLE),
        host: false,
    };
    d.line(format_args!(""));
    d.int("format", FORMAT);
    d.word("kind", Ours("run"));
    host_sections(&mut d, s);
    run_sections(&mut d, s);
    d.out
}

/// `[rox_box]`, `[host]` and `[host.gpu]`: the build and the PC, which a run
/// report and the self-test's share.
fn host_sections(d: &mut Doc, s: &Snapshot) {
    d.table("rox_box");
    d.formatted("version", format_args!("{}", env!("CARGO_PKG_VERSION")));
    d.boolean("diagnostics", cfg!(feature = "diagnostics"));

    d.host = true;
    d.table("host");
    match s.windows {
        Some((major, minor, build)) => {
            d.formatted("windows", format_args!("{major}.{minor}"));
            d.int("windows_build", build);
        }
        None => d.comment("windows: not known"),
    }
    match &s.cpu {
        Some((brand, logical)) => {
            d.host_text("cpu", brand);
            d.int("logical_processors", logical);
        }
        None => d.comment("cpu: not known"),
    }
    match s.memory {
        Some((installed, free)) => {
            d.int("ram_gib", (installed + 512) / 1024);
            d.float("free_commit_gib_at_start", free as f64 / 1024.0);
        }
        None => d.comment("memory: not known"),
    }
    d.table("host.gpu");
    match &s.gpu {
        Some(g) => {
            d.host_text("adapter", &g.adapter);
            d.hex("vendor_id", u64::from(g.vendor_id));
            match g.driver {
                Some(v) => d.formatted(
                    "driver",
                    format_args!(
                        "{}.{}.{}.{}",
                        v >> 48,
                        (v >> 32) & 0xffff,
                        (v >> 16) & 0xffff,
                        v & 0xffff
                    ),
                ),
                None => d.comment("driver: not known"),
            }
            d.int("dedicated_vram_gib", (g.dedicated_mib + 512) / 1024);
            d.boolean("software_adapter", g.software);
            d.formatted(
                "feature_level",
                format_args!("{}_{}", g.feature_level.0, g.feature_level.1),
            );
            d.formatted(
                "shader_model",
                format_args!("{}.{}", g.shader_model.0, g.shader_model.1),
            );
            d.int("wave_lanes_min", g.wave_min);
            d.int("wave_lanes_max", g.wave_max);
            d.boolean("mesh_shaders", g.mesh_shaders);
        }
        None => d.comment("no host device was made (the title had not asked for one yet)"),
    }
    match s.stat(Stat::ViewHeapViews) {
        0 => d.comment("view heap limit: not known yet"),
        views => d.int("view_heap_views", views),
    }
    d.host = false;
}

/// Everything of a run report after the host.
fn run_sections(d: &mut Doc, s: &Snapshot) {
    d.table("run");
    match s.console {
        Some(model) => d.word("console", model),
        None => d.comment("console: not set"),
    }
    match (s.stat(Stat::DisplayWidth), s.stat(Stat::DisplayHeight)) {
        (0, _) | (_, 0) => d.comment("display mode: not asked for yet"),
        (w, h) => d.formatted("display_mode", format_args!("{w}x{h}")),
    }
    d.float("duration_s", s.duration_us as f64 / 1e6);
    match s.first_draw_us {
        Some(us) => d.float("first_draw_s", us as f64 / 1e6),
        None => d.comment("first draw: none run on the host yet"),
    }
    match s.end {
        None => {
            d.word("ended", Ours("killed_or_unknown"));
            d.comment(
                "the run had not ended when this was written: it was killed, or ended in a way",
            );
            d.comment("Rox-box does not see, at most 30 seconds later");
        }
        Some(end) => {
            d.word("ended", Ours(end.key()));
            match end {
                End::TitleExited(code) => d.int("exit_code", code),
                End::DeviceRemoved(hresult) => {
                    d.hex("removed_reason_hresult", u64::from(hresult));
                    d.word("removed_reason", Ours(removal_reason(hresult)));
                }
                End::Crash(place, kind) => {
                    d.word("crash_in", Ours(place.key()));
                    d.word("exception", Ours(kind.key()));
                }
                _ => {}
            }
        }
    }
    d.int("rox_box_panics", s.stat(Stat::Panics));
    d.table("run.switches");
    d.comment("VICTORY_* variables set; a value only where it is a plain number or word");
    let mut named: Vec<String> = Vec::new();
    for (name, value) in &s.switches {
        let (name, value) = (d.quoted(name), d.quoted(value));
        if !named.contains(&name) {
            d.line(format_args!("{name} = {value}"));
            named.push(name);
        }
    }

    d.table("frames");
    d.int("presents", s.presents);
    if let Some(first) = s.first_present_us {
        d.float("first_present_s", first as f64 / 1e6);
    }
    let mean_ms = if s.intervals > 0 {
        s.interval_sum_us as f64 / s.intervals as f64 / 1000.0
    } else {
        0.0
    };
    d.float(
        "average_fps",
        if mean_ms > 0.0 { 1000.0 / mean_ms } else { 0.0 },
    );
    d.comment("present-to-present times, in ms");
    d.float("frame_ms_mean", mean_ms);
    d.float("frame_ms_p50", s.percentile_ms(0.50));
    d.float("frame_ms_p90", s.percentile_ms(0.90));
    d.float("frame_ms_p99", s.percentile_ms(0.99));
    d.float("frame_ms_max", s.interval_max_us as f64 / 1000.0);
    d.table("frames.histogram");
    d.comment("frames by present-to-present time, in ms");
    for (key, n) in BUCKET_KEYS.iter().zip(s.buckets) {
        d.keyed(key, n);
    }

    let merge = s.timeline.len().div_ceil(SHOWN_WINDOWS).max(1);
    let shown: Vec<(u64, u64)> = s
        .timeline
        .chunks(merge)
        .map(|c| {
            (
                c.iter().map(|w| u64::from(w.0)).sum(),
                c.iter().map(|w| u64::from(w.1)).max().unwrap_or(0),
            )
        })
        .collect();
    let window_s = WINDOW_S * merge as u64;
    d.table("timeline");
    d.comment("per window since the start of the run (warm-up first, then steady state)");
    d.int("window_s", window_s);
    d.ints("presents", &shown.iter().map(|w| w.0).collect::<Vec<_>>());
    // The last window ends with the run, so its rate is over the time it
    // covered (at least a second, so that a present just after a window
    // began is no burst): over the whole window it said 3 fps for 30
    // presents in the 0.7 s of a short run, and every run seemed to slow
    // down in its last seconds.
    let last_s = (s.duration_us as f64 / 1e6 - (window_s * (shown.len().max(1) as u64 - 1)) as f64).max(1.0);
    d.floats(
        "fps",
        &shown
            .iter()
            .enumerate()
            .map(|(i, w)| w.0 as f64 / if i + 1 == shown.len() { last_s } else { window_s as f64 })
            .collect::<Vec<_>>(),
    );
    d.ints(
        "worst_frame_ms",
        &shown.iter().map(|w| w.1).collect::<Vec<_>>(),
    );

    d.table("gpu_work");
    d.comment("per frame (between two presents), the first present's loading left out");
    let frames = s.intervals.max(1) as f64;
    for (i, name) in ["submissions", "draws", "dispatches", "left_out"]
        .iter()
        .enumerate()
    {
        let (avg, most): (&'static str, &'static str) = match *name {
            "submissions" => ("submissions_avg", "submissions_max"),
            "draws" => ("draws_avg", "draws_max"),
            "dispatches" => ("dispatches_avg", "dispatches_max"),
            _ => ("left_out_avg", "left_out_max"),
        };
        d.float(avg, s.work_sum[i] as f64 / frames);
        d.int(most, s.work_max[i]);
    }
    if let Some(steady) = &s.steady {
        d.comment("the same at steady state (see [frame_time])");
        let steady = &steady.figures;
        d.float(
            "left_out_avg_steady",
            steady[Figure::LeftOut as usize] as f64 / steady[Figure::Intervals as usize].max(1) as f64,
        );
    }
    d.int("submissions_total", s.stat(Stat::Submissions));
    d.table("gpu_work.left_out");
    d.comment("draws and dispatches not run on the host, whole run, by reason");
    for why in LeftOut::ALL {
        d.int(why.key(), s.left_out[why as usize]);
    }

    d.table("frame_time");
    d.comment("per frame, from the meters of the log's once-a-second frame line; ms, MB (10^6 bytes)");
    d.comment("whole run here; [frame_time.steady]: the steady state, the frames from the first");
    d.comment("10 s window at or after a third of the time from the first present to this report");
    d.comment("(a run with no whole window after that: the readings of the frame meter after it)");
    frame_time(d, &s.figures);
    d.table("frame_time.steady");
    match (&s.steady, s.first_present_us) {
        (Some(steady), _) => {
            if steady.by_seconds {
                d.comment("a short run: the frame meter's readings begun after its first third");
            }
            d.float("from_s", steady.from_us as f64 / 1e6);
            frame_time(d, &steady.figures);
        }
        (None, Some(_)) => {
            d.comment("none: the run was too short, no frame meter reading began after the first");
            d.comment("third of its time from the first present (the whole run is in [frame_time])");
        }
        (None, None) => d.comment("no present yet"),
    }
    d.table("frame_time.waits");
    d.comment("ms per frame the waiting for the card in settles took, by what settled, whole run");
    settle_waits(d, &s.figures);
    d.table("frame_time.waits.steady");
    match &s.steady {
        Some(steady) => settle_waits(d, &steady.figures),
        None => d.comment("none (see [frame_time.steady])"),
    }
    d.table("frame_time.spared");
    d.comment("per frame, work the unified-memory switches spared (0 with them off), whole run");
    spared(d, &s.figures);
    d.table("frame_time.spared.steady");
    match &s.steady {
        Some(steady) => spared(d, &steady.figures),
        None => d.comment("none (see [frame_time.steady])"),
    }
    d.table("frame_time.moved");
    d.comment("ms per frame of work moved off the threads a frame waits for (0 with the switches off), whole run");
    moved(d, &s.figures);
    d.table("frame_time.moved.steady");
    match &s.steady {
        Some(steady) => moved(d, &steady.figures),
        None => d.comment("none (see [frame_time.steady])"),
    }
    gpu_time_sections(d, &s.gpu_time, s.steady.as_ref(), s.first_present_us.is_some());

    d.table("pipelines");
    d.int("created_by_title", s.stat(Stat::PipelinesCreated));
    d.int("host_made_now", s.stat(Stat::PipelinesMadeNow));
    d.int("host_from_library", s.stat(Stat::PipelinesFromLibrary));
    d.int("host_failed", s.stat(Stat::PipelinesFailed));

    d.table("shaders");
    d.int("translated_now", s.stat(Stat::ShadersTranslated));
    d.int("from_cache", s.stat(Stat::ShadersFromCache));
    d.int("failed", s.stat(Stat::ShadersFailed));
    d.table("shaders.failures");
    d.comment("failed translations by the translator's own reason");
    d.keyed_all(s.shader_failures.iter().map(|(kind, what, _, n)| {
        let key = match what.as_str() {
            "" => kind.as_str().to_owned(),
            what => format!("{}: {what}", kind.as_str()),
        };
        (key, *n)
    }));
    if s.shader_failures_lost > 0 {
        d.keyed(
            "(more reasons than the table holds)",
            s.shader_failures_lost,
        );
    }

    let mib = |b: u64| (b + (1 << 19)) >> 20;
    d.table("memory");
    d.comment("MiB");
    d.int("process_commit_peak", mib(s.stat(Stat::CommitPeak)));
    d.int("working_set_peak", mib(s.stat(Stat::WorkingSetPeak)));
    d.int("vram_peak", mib(s.stat(Stat::LocalPeak)));
    d.int("vram_budget_at_peak", mib(s.stat(Stat::LocalBudget)));
    d.int("shared_peak", mib(s.stat(Stat::SharedPeak)));
    d.int("shared_budget_at_peak", mib(s.stat(Stat::SharedBudget)));
    d.int("out_of_memory_retried_ok", s.stat(Stat::OomRetriedOk));
    d.int(
        "out_of_memory_retried_failed",
        s.stat(Stat::OomRetriedFailed),
    );
    d.int("governor_level_changes", s.stat(Stat::GovernorChanges));
    d.word(
        "governor_worst_level",
        Ours(match s.stat(Stat::GovernorWorst) {
            0 => "not_looked",
            1 => "green",
            2 => "yellow",
            _ => "red",
        }),
    );

    d.table("memory.pressure");
    d.comment("MiB; the lowest the budget governor saw (it asks at most every 32 ms)");
    match s.lowest {
        [u64::MAX, u64::MAX] => d.comment("free memory and commit headroom: not seen (the governor is off)"),
        [ram, commit] => {
            d.int("lowest_free_ram_mib", if ram == u64::MAX { 0 } else { ram >> 20 });
            d.int("lowest_commit_headroom_mib", if commit == u64::MAX { 0 } else { commit >> 20 });
        }
    }
    d.comment("seconds at each level of the budget governor (it looks about once a second)");
    for (key, us) in ["governor_green_s", "governor_yellow_s", "governor_red_s"]
        .into_iter()
        .zip(s.governor_us)
    {
        d.float(key, us as f64 / 1e6);
    }
    let over = s.stat(Stat::CardOverBudgetLooks);
    d.boolean("vram_over_budget", over > 0);
    d.int("vram_over_budget_looks", over);
    d.comment("the commit guard: times commit went critical, stagings it held back, refusals it saw");
    d.int("commit_critical_times", s.stat(Stat::CommitCritical));
    d.int("commit_uploads_held_back", s.stat(Stat::CommitHeldBack));
    d.int("commit_allocations_refused", s.stat(Stat::CommitRefusals));
    d.comment("waits for new memory to be made resident, and for new card memory to be mapped");
    let ms = |stat: Stat| s.stat(stat) as f64 / 1e3;
    d.int("resident_waits", s.stat(Stat::ResidentWaits));
    d.float("resident_wait_ms", ms(Stat::ResidentUs));
    d.float("resident_wait_longest_ms", ms(Stat::ResidentLongestUs));
    d.int("mapped_waits", s.stat(Stat::MappedWaits));
    d.float("mapped_wait_ms", ms(Stat::MappedUs));
    d.float("mapped_wait_longest_ms", ms(Stat::MappedLongestUs));

    memory_views(d, s);
    memory_map(d, &s.map);
    view_copies(d, view_copies_take());

    d.table("textures");
    d.comment("textures picked by index (from the view heap by shaders), since the start");
    d.int("picked_uploaded", s.stat(Stat::PickedUploaded));
    d.int("picked_uploaded_mib", s.stat(Stat::PickedBytes) >> 20);
    d.int("waiting_for_memory_most", s.stat(Stat::PickedWaitingMost));
    d.int("waiting_for_memory_last", s.stat(Stat::PickedWaitingLast));
    d.int("no_room_passes", s.stat(Stat::PickedNoRoom));
    d.int("uploaded_again_changed_unseen", s.stat(Stat::PickedChangedUnseen));
    d.int("uploaded_again_while_full", s.stat(Stat::PickedReuploadedFull));
    d.int("smallest_levels_only", s.stat(Stat::TailCopies));
    d.int("smallest_levels_only_mib", s.stat(Stat::TailBytes) >> 20);
    d.int("detail_cut_for_card", s.stat(Stat::DetailCutCard));
    d.int("detail_cut_for_pc", s.stat(Stat::DetailCutPc));
    d.int("made_whole_in_pool_room", s.stat(Stat::PoolRoomWhole));
    d.int("not_shown_unwritten", s.stat(Stat::UnwrittenKept));
    d.comment("texture refreshes on all threads, ms per frame (whole run, steady state)");
    let refresh = |f: &[u64; FIGS]| {
        f[Figure::RefreshAllUs as usize] as f64 / 1e3 / f[Figure::Frames as usize].max(1) as f64
    };
    d.float("refresh_ms", refresh(&s.figures));
    if let Some(steady) = &s.steady {
        d.float("refresh_ms_steady", refresh(&steady.figures));
    }

    d.table("unimplemented");
    d.int("methods_called", s.methods.len() as u64);
    d.int("methods_lost", s.methods_lost);
    d.comment("calls of a pipeline creation method not implemented: pipelines the title went without");
    d.int("pipelines_gone_without", s.stat(Stat::PipelinesGoneWithout));
    d.int("imports_named", s.imports.len() as u64);
    d.comment("imports of DLLs that are not the platform's: only counted");
    d.int("imports_other", s.other_imports.0);
    d.int("imports_other_calls", s.other_imports.1);
    for (interface, _, offset, calls) in &s.methods {
        d.array_table("unimplemented.method");
        d.word("interface", *interface);
        d.hex("offset", u64::from(*offset));
        d.int("calls", calls);
    }
    for (name, calls) in &s.imports {
        d.array_table("unimplemented.import");
        let name = d.quoted(name);
        d.line(format_args!("name = {name}"));
        d.int("calls", calls);
    }
}

/// `[memory.views]`: the runtime's views of title pages and the descriptor
/// table's entries of views without a resource, since the start (kept in a
/// function of its own, apart from the sections beside it).
fn memory_views(d: &mut Doc, s: &Snapshot) {
    d.table("memory.views");
    d.comment("views of the host's at title addresses since the start, and the pages one showed on average");
    let mapped = s.stat(Stat::TitleViewsMapped);
    d.int("title_views_mapped", mapped);
    d.int("title_views_unmapped", s.stat(Stat::TitleViewsUnmapped));
    d.float("title_view_pages_avg", s.stat(Stat::TitleViewPages) as f64 / mapped.max(1) as f64);
    d.comment("views kept in the books alone until touched (VICTORY_LAZY_VIEWS, VICTORY_SOFT_VIEWS)");
    d.int("book_views_noted", s.stat(Stat::BookViewsNoted));
    d.int("book_views_let_go_untouched", s.stat(Stat::BookViewsLetGo));
    d.int("book_views_made_real", s.stat(Stat::BookViewsMadeReal));
    d.int("book_views_now", s.stat(Stat::BookViewsNow));
    d.comment("descriptor table entries of views no resource releases: kept now, freed for the cap (VICTORY_OWNERLESS_VIEWS)");
    d.int("ownerless_entries_kept", s.stat(Stat::OwnerlessViewsKept));
    d.int("ownerless_entries_dropped", s.stat(Stat::OwnerlessViewsDropped));
}

/// The frame line's figures `f` per frame of the frame meter.
fn frame_time(d: &mut Doc, f: &[u64; FIGS]) {
    let get = |figure: Figure| f[figure as usize] as f64;
    let frames = get(Figure::Frames);
    let per = |figure: Figure, scale: f64| get(figure) / scale / frames.max(1.0);
    d.int("frames", f[Figure::Frames as usize]);
    d.float("frame_ms", per(Figure::MeterUs, 1e3));
    for (key, figure, scale) in [
        ("preparing_ms", Figure::PreparingUs, 1e3),
        ("copies_up_ms", Figure::CopiesUpUs, 1e3),
        ("copies_up_mb", Figure::CopiesUpBytes, 1e6),
        ("texture_refresh_ms", Figure::TextureRefreshUs, 1e3),
        ("texture_refresh_mb", Figure::TextureRefreshBytes, 1e6),
        ("waiting_for_card_ms", Figure::CardWaitUs, 1e3),
        ("copying_back_ms", Figure::CopyBackUs, 1e3),
        ("copying_back_mb", Figure::CopyBackBytes, 1e6),
    ] {
        d.float(key, per(figure, scale));
    }
    let meter = get(Figure::MeterUs);
    let busy = if meter > 0.0 {
        (100.0 * (1.0 - get(Figure::CardIdleUs) / meter)).clamp(0.0, 100.0)
    } else {
        0.0
    };
    d.float("card_busy_pct", busy);
    d.float("card_idle_ms", per(Figure::CardIdleUs, 1e3));
    d.float("card_idle_behind_us_ms", per(Figure::CardIdleBehindUs, 1e3));
    d.float("submitting_thread_stood_ms", per(Figure::StoodUs, 1e3));
    d.float("heap_buffer_passes", per(Figure::HeapBufferPasses, 1.0));
    d.float("heap_buffer_ms", per(Figure::HeapBufferUs, 1e3));
}

/// The work spared of `f`, per frame.
fn spared(d: &mut Doc, f: &[u64; FIGS]) {
    let frames = f[Figure::Frames as usize].max(1) as f64;
    for (figure, key, scale) in SPARED {
        d.float(key, f[figure as usize] as f64 / scale / frames);
    }
}

/// The work moved of `f`, ms per frame.
fn moved(d: &mut Doc, f: &[u64; FIGS]) {
    let frames = f[Figure::Frames as usize].max(1) as f64;
    for (figure, key) in MOVED {
        d.float(key, f[figure as usize] as f64 / 1e3 / frames);
    }
}

/// The waiting in settles of `f`, ms per frame, by cause.
fn settle_waits(d: &mut Doc, f: &[u64; FIGS]) {
    let frames = f[Figure::Frames as usize].max(1) as f64;
    for (figure, key) in SETTLED {
        d.float(key, f[figure as usize] as f64 / 1e3 / frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    /// What tests try to smuggle in: a path (with the placeholder user name),
    /// a 16-hex-digit hash, an address and a long free-form string. Built at
    /// run time: the asset policy rightly rejects them as literals.
    fn smuggled() -> Vec<String> {
        let users = format!("C:{}Users{}example", '\\', '\\');
        vec![
            format!("{users}{}Games{}Title.exe", '\\', '\\'),
            "0123456789abcdef".chars().rev().collect(),
            format!("0x7ff6{}", "a1b2c3d4"),
            "the title said this long and free-form sentence, which goes on and on about its own \
             story, its places and its people, and must never reach the report"
                .to_owned(),
        ]
    }

    /// Parts no rendered report may hold (a withheld value names none).
    fn must_not_appear(text: &str) {
        let s = smuggled();
        for needle in [
            &s[0][..],
            "example",
            "Users",
            &s[1],
            &s[2],
            "a1b2c3d4",
            "free-form",
            "Title.exe",
        ] {
            assert!(
                !text.contains(needle),
                "{needle:?} reached the report:\n{text}"
            );
        }
    }

    // The memory map.

    const MIB: u64 = 1 << 20;

    /// A map with some private parts, one known to be empty, one beside, a
    /// count, and the rest not known.
    fn synthetic_map() -> MapSnapshot {
        let mut m = MapSnapshot::default();
        for (part, mib) in [
            (Part::TitleCommitted, 4_000),
            (Part::Windows, 1_500),
            (Part::ListScratch, 700),
            (Part::SendSpare, 0),
            (Part::CardTextures, 3_000),
        ] {
            m.now[part as usize] = mib * MIB;
            m.peak[part as usize] = (mib + 100) * MIB;
        }
        m.now[Part::HostListsAlive as usize] = 640;
        m.peak[Part::HostListsAlive as usize] = 700;
        m.commit = 9_000 * MIB;
        m.peak_total = 6_400 * MIB;
        m.peak_unaccounted = 3_000 * MIB;
        m
    }

    fn rendered_map(m: &MapSnapshot) -> toml::Table {
        let mut d = Doc { out: String::new(), host: false };
        memory_map(&mut d, m);
        let table: toml::Table = toml::from_str(&d.out).unwrap_or_else(|e| panic!("{e}
{}", d.out));
        table["memory"].as_table().expect("a table").clone()
    }

    /// `[memory.view_copies]` says its numbers once they are given, and is
    /// left out before.
    #[test]
    fn the_view_copies_say_their_numbers_once_given() {
        let mut d = Doc { out: String::new(), host: false };
        view_copies(&mut d, None);
        assert!(d.out.is_empty());
        view_copies(&mut d, Some([48_000, 30, 14, 14 * MIB]));
        let table: toml::Table = toml::from_str(&d.out).unwrap_or_else(|e| panic!("{e}\n{}", d.out));
        let v = table["memory"]["view_copies"].as_table().expect("a table");
        assert_eq!(v["cut_from_blocks"].as_integer(), Some(48_000));
        assert_eq!(v["own_buffers_made"].as_integer(), Some(30));
        assert_eq!(v["blocks"].as_integer(), Some(14));
        assert_eq!(v["blocks_mib"].as_integer(), Some(14));
        assert_eq!(v.len(), 4);
    }

    #[test]
    fn the_memory_map_renders_what_is_known_and_leaves_out_the_rest() {
        let map = rendered_map(&synthetic_map());
        let now = map["map"].as_table().expect("a table");
        assert_eq!(now["process_private_commit_mib"].as_integer(), Some(9_000));
        assert_eq!(now["title_committed_mib"].as_integer(), Some(4_000));
        assert_eq!(now["card_textures_filled_mib"].as_integer(), Some(3_000));
        assert_eq!(now["host_lists_alive"].as_integer(), Some(640));
        // Known and empty is 0; never given is not there.
        assert_eq!(now["send_spare_mib"].as_integer(), Some(0));
        assert!(!now.contains_key("shader_containers_mib"));
        assert!(!now.contains_key("card_sparse_tiles_mib"));
        let peak = now["peak"].as_table().expect("a table");
        assert_eq!(peak["title_committed_mib"].as_integer(), Some(4_100));
        assert_eq!(peak["host_lists_alive"].as_integer(), Some(700));
        assert_eq!(peak["accounted_total_mib"].as_integer(), Some(6_400));
        assert_eq!(peak["unaccounted_mib"].as_integer(), Some(3_000));
        assert!(!peak.contains_key("shader_containers_mib"));
    }

    /// Only the private parts that are known are added; the commit less that
    /// sum is what the map does not explain, and never below 0.
    #[test]
    fn the_memory_maps_totals_add_up() {
        let m = synthetic_map();
        assert_eq!(m.accounted(), 6_200 * MIB);
        assert_eq!(m.unaccounted(), Some(2_800 * MIB));
        let table = rendered_map(&m);
        let now = table["map"].as_table().expect("a table");
        let parts: i64 = Part::ALL
            .iter()
            .filter(|p| p.kind() == PartKind::Private)
            .filter_map(|p| now.get(p.key()).and_then(toml::Value::as_integer))
            .sum();
        assert_eq!(now["accounted_total_mib"].as_integer(), Some(parts));
        assert_eq!(
            now["accounted_total_mib"].as_integer().unwrap() + now["unaccounted_mib"].as_integer().unwrap(),
            now["process_private_commit_mib"].as_integer().unwrap()
        );
        // Parts that add up to more than the commit leave nothing over.
        let mut over = m.clone();
        over.commit = 1_000 * MIB;
        assert_eq!(over.unaccounted(), Some(0));
        // No commit asked: no `unaccounted` to say.
        let mut none = m;
        none.commit = NOT_KNOWN;
        assert_eq!(none.unaccounted(), None);
        let table = rendered_map(&none);
        assert!(!table["map"].as_table().expect("a table").contains_key("unaccounted_mib"));
        assert!(!table["map"].as_table().expect("a table").contains_key("process_private_commit_mib"));
    }

    #[test]
    fn the_memory_map_keeps_its_peaks_and_its_keys_are_ours() {
        // A part nobody gave is not known; one given keeps its largest.
        let before = MAP_PEAK[Part::TitlePagePool as usize].load(Relaxed);
        map_set(Part::TitlePagePool, before + 5 * MIB);
        map_set(Part::TitlePagePool, 1);
        assert_eq!(MAP_NOW[Part::TitlePagePool as usize].load(Relaxed), 1);
        assert!(MAP_PEAK[Part::TitlePagePool as usize].load(Relaxed) >= before + 5 * MIB);
        // The sampled values are numbers whatever they hold, in decimal.
        map_set(Part::Windows, 0x7ff6_a1b2_c3d4);
        let text = render(&Snapshot::take());
        assert!(!text.contains("7ff6"), "{text}");
        let table: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}
{text}"));
        assert!(table["memory"]["map"].is_table() && table["memory"]["map"]["peak"].is_table());
        let mut keys = std::collections::HashSet::new();
        for part in Part::ALL {
            assert!(keys.insert(part.key()), "{} twice", part.key());
            assert_eq!(part.key().ends_with("_mib"), part.kind() != PartKind::Count);
        }
    }

    fn synthetic() -> Snapshot {
        let mut fine = vec![0u32; FINE];
        fine[fine_bucket(16_600)] = 900;
        fine[fine_bucket(33_200)] = 90;
        fine[fine_bucket(120_000)] = 10;
        let mut stats = [0; STATS];
        for (stat, v) in [
            (Stat::Submissions, 9_000),
            (Stat::PipelinesCreated, 4_212),
            (Stat::PipelinesMadeNow, 310),
            (Stat::PipelinesFromLibrary, 1_402),
            (Stat::PipelinesFailed, 2),
            (Stat::ShadersTranslated, 120),
            (Stat::ShadersFromCache, 2_960),
            (Stat::ShadersFailed, 3),
            (Stat::OomRetriedOk, 4),
            (Stat::GovernorChanges, 2),
            (Stat::GovernorWorst, 2),
            (Stat::LocalPeak, 7_300 << 20),
            (Stat::LocalBudget, 7_600 << 20),
            (Stat::SharedPeak, 5_100 << 20),
            (Stat::SharedBudget, 7_900 << 20),
            (Stat::CommitPeak, 21_000 << 20),
            (Stat::WorkingSetPeak, 9_800 << 20),
            (Stat::ViewHeapViews, 1_000_000),
            (Stat::DisplayWidth, 3840),
            (Stat::DisplayHeight, 2160),
            (Stat::PipelinesGoneWithout, 36),
            (Stat::CommitCritical, 1),
            (Stat::CommitHeldBack, 14),
            (Stat::ResidentWaits, 52_000),
            (Stat::ResidentUs, 3_400_000),
            (Stat::ResidentLongestUs, 41_000),
            (Stat::MappedWaits, 2_400),
            (Stat::MappedUs, 900_000),
            (Stat::MappedLongestUs, 12_500),
            (Stat::CardOverBudgetLooks, 3),
            (Stat::PickedUploaded, 18_400),
            (Stat::PickedBytes, 5_200 << 20),
            (Stat::PickedWaitingMost, 950),
            (Stat::PickedWaitingLast, 12),
            (Stat::PickedNoRoom, 210),
            (Stat::PickedChangedUnseen, 7),
            (Stat::TailCopies, 330),
            (Stat::TailBytes, 21 << 20),
            (Stat::TitleViewsMapped, 40_000),
            (Stat::TitleViewPages, 100_000),
            (Stat::TitleViewsUnmapped, 39_000),
            (Stat::BookViewsNoted, 0),
            (Stat::BookViewsLetGo, 0),
            (Stat::BookViewsMadeReal, 0),
            (Stat::BookViewsNow, 0),
            (Stat::OwnerlessViewsKept, 1_048_576),
            (Stat::OwnerlessViewsDropped, 52_000),
        ] {
            stats[stat as usize] = v;
        }
        let figures = |values: &[(Figure, u64)]| {
            let mut f = [0; FIGS];
            for &(figure, v) in values {
                f[figure as usize] = v;
            }
            f
        };
        use Figure as F;
        let whole = figures(&[
            (F::Frames, 1_000),
            (F::MeterUs, 20_400_000),
            (F::PreparingUs, 6_200_000),
            (F::CopiesUpUs, 2_100_000),
            (F::CopiesUpBytes, 4_100_000_000),
            (F::TextureRefreshUs, 3_000_000),
            (F::TextureRefreshBytes, 12_000_000_000),
            (F::CardWaitUs, 7_500_000),
            (F::CopyBackUs, 1_200_000),
            (F::CopyBackBytes, 900_000_000),
            (F::CardIdleUs, 6_100_000),
            (F::CardIdleBehindUs, 4_000_000),
            (F::StoodUs, 7_000_000),
            (F::HeapBufferPasses, 2_000),
            (F::HeapBufferUs, 400_000),
            (F::RefreshAllUs, 4_500_000),
            (F::SettledSync, 2_000_000),
            (F::SettledCardDone, 500_000),
            (F::SettledStale, 1_500_000),
            (F::SettledRead, 1_200_000),
            (F::SettledStep, 300_000),
            (F::SettledWrites, 800_000),
            (F::SettledFence, 200_000),
            (F::SparedLeftOnCardBytes, 50_000_000_000),
            (F::SparedPagesInPlace, 9_000),
            (F::MovedBuryUs, 4_000_000),
            (F::MovedSnapshotsUs, 3_000_000),
            (F::SnapshotsWaitedUs, 500_000),
            (F::Intervals, 1_000),
            (F::LeftOut, 1_880),
        ]);
        let steady = figures(&[
            (F::Frames, 370),
            (F::MeterUs, 6_200_000),
            (F::PreparingUs, 1_700_000),
            (F::CopiesUpUs, 520_000),
            (F::CopiesUpBytes, 900_000_000),
            (F::TextureRefreshUs, 400_000),
            (F::TextureRefreshBytes, 1_500_000_000),
            (F::CardWaitUs, 2_900_000),
            (F::CopyBackUs, 300_000),
            (F::CopyBackBytes, 210_000_000),
            (F::CardIdleUs, 900_000),
            (F::CardIdleBehindUs, 600_000),
            (F::StoodUs, 2_700_000),
            (F::HeapBufferPasses, 740),
            (F::HeapBufferUs, 110_000),
            (F::RefreshAllUs, 600_000),
            (F::SettledSync, 700_000),
            (F::SettledCardDone, 200_000),
            (F::SettledStale, 900_000),
            (F::SettledRead, 500_000),
            (F::SettledWrites, 300_000),
            (F::SettledFence, 100_000),
            (F::SparedLeftOnCardBytes, 18_500_000_000),
            (F::SparedPagesInPlace, 3_330),
            (F::MovedBuryUs, 1_110_000),
            (F::Intervals, 370),
            (F::LeftOut, 30),
        ]);
        let mut left_out = [0; LeftOut::ALL.len()];
        left_out[LeftOut::PipelineNotReady as usize] = 1_830;
        left_out[LeftOut::ShaderNotTranslated as usize] = 44;
        left_out[LeftOut::IndirectNotBounded as usize] = 6;
        Snapshot {
            duration_us: 62_400_000,
            end: Some(End::TitleExited(0)),
            console: Some(Ours("series-x")),
            switches: vec![
                ("VICTORY_PIPELINE_WAIT_MS".into(), "500".into()),
                ("VICTORY_LOG".into(), "<set>".into()),
            ],
            windows: Some((10, 0, 26100)),
            cpu: Some((HostText::new("Some Vendor(R) Processor 12-Core"), 24)),
            memory: Some((32_617, 41_200)),
            gpu: Some(GpuFacts {
                adapter: HostText::new("Some Adapter XT/XTX"),
                vendor_id: 0x1234,
                driver: Some(32 << 48 | 15 << 16 | 6094),
                dedicated_mib: 8_188,
                software: false,
                feature_level: (12, 2),
                shader_model: (6, 8),
                wave_min: 32,
                wave_max: 32,
                mesh_shaders: true,
            }),
            stats,
            left_out,
            presents: 1_001,
            first_present_us: Some(21_300_000),
            intervals: 1_000,
            interval_sum_us: 20_400_000,
            interval_max_us: 120_400,
            fine,
            // (As the fine buckets above fall into the histogram's.)
            buckets: [0, 0, 900, 0, 90, 0, 0, 10, 0, 0],
            timeline: vec![
                (0, 0),
                (0, 0),
                (180, 120),
                (600, 34),
                (221, 17),
                (0, 0),
                (0, 0),
            ],
            work_sum: [9_000, 2_100_000, 640_000, 1_880],
            work_max: [14, 3_950, 1_210, 260],
            first_draw_us: Some(20_900_000),
            figures: whole,
            steady: Some(Steady {
                from_us: steady_from_us(21_300_000, 62_400_000),
                figures: steady,
                by_seconds: false,
            }),
            lowest: [612 << 20, 2_100 << 20],
            governor_us: [41_000_000, 18_500_000, 2_900_000],
            methods: vec![
                (Ours("device"), Ours(""), 0x218, 36),
                (Ours("command list"), Ours(""), 0x1a8, 3),
            ],
            methods_lost: 0,
            shader_failures: vec![(Ours("unsupported"), Ours("an instruction"), 0, 3)],
            shader_failures_lost: 0,
            imports: vec![("kernel32.dll!SomeFunction".into(), 1)],
            other_imports: (2, 5),
            map: synthetic_map(),
            gpu_time: synthetic_gpu_time(),
        }
    }

    /// `[gpu_time]` of 1,000 frames, its steady state of 370, from per-frame
    /// figures (ms by kind, then the volumes) and the governor's looks.
    fn synthetic_gpu_time() -> GpuTime {
        use GpuFigure as G;
        let of = |frames: u64, partly: u64, ms: [f64; 12], volumes: [f64; 10], looks: u64| {
            let mut f = [0u64; GPU_FIGS];
            f[G::Frames as usize] = frames;
            f[G::PartlyFrames as usize] = partly;
            for (k, ms) in GPU_KINDS.iter().zip(ms) {
                f[*k as usize] = (ms * 1e6 * frames as f64).round() as u64;
            }
            let volume = [
                G::Fills,
                G::FillBytes,
                G::HashPasses,
                G::HashedBytes,
                G::ComparePasses,
                G::ComparedBytes,
                G::CopiedBackBytes,
                G::UploadRanges,
                G::UploadedBytes,
                G::TextureBytes,
            ];
            for (v, per) in volume.iter().zip(volumes) {
                f[*v as usize] = (per * frames as f64).round() as u64;
            }
            let mib = |n: u64| n << 20;
            f[G::Looks as usize] = looks;
            f[G::CardUsedSum as usize] = looks * mib(7_100);
            f[G::CardBudgetSum as usize] = looks * mib(7_600);
            f[G::CardUsedPeak as usize] = mib(7_300);
            f[G::SharedUsedSum as usize] = looks * mib(4_800);
            f[G::SharedBudgetSum as usize] = looks * mib(7_900);
            f[G::SharedUsedPeak as usize] = mib(5_100);
            f[G::RamLooks as usize] = looks;
            f[G::FreeRamSum as usize] = looks * mib(900);
            f[G::FreeRamLowestNot as usize] = !mib(612);
            f[G::CardOverLooks as usize] = 3;
            // 2 ms, 5 ms and 8 ms a frame of the costliest 1, 5 and 20, in
            // seconds of 120 pipelines each.
            f[G::PipelineSeconds as usize] = looks;
            f[G::ComputePipelinesSum as usize] = looks * 120;
            for (g, ms) in [(G::TopPipelineNs, 2.0), (G::Top5PipelinesNs, 5.0), (G::Top20PipelinesNs, 8.0)] {
                f[g as usize] = (ms * 1e6 * frames as f64).round() as u64;
            }
            f
        };
        let volumes = [2_800.0, 0.9e6, 80.0, 10.5e6, 7.0, 488e6, 3.2e6, 580.0, 52e6, 1.5e6];
        GpuTime {
            state: Some(GpuSplit::On),
            whole: of(1_000, 20, [6.0, 3.5, 1.2, 0.4, 0.8, 2.1, 1.3, 0.3, 0.6, 0.2, 0.5, 0.1], volumes, 60),
            steady: Some(of(370, 0, [5.5, 3.6, 1.1, 0.3, 0.8, 2.0, 1.3, 0.3, 0.5, 0.1, 0.4, 0.0], volumes, 20)),
        }
    }

    /// `[gpu_time]` and its steady state say every kind, the sums, the
    /// volumes and the memory; the title's and ours add up to what was
    /// measured, less what could not be given a kind.
    #[test]
    fn the_gpu_time_says_its_kinds_their_sums_and_the_memory() {
        let text = render(&synthetic());
        let parsed: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let gpu = &parsed["gpu_time"];
        assert_eq!(gpu["split"].as_str(), Some("on"));
        for section in [gpu, &gpu["steady"]] {
            for key in GPU_KIND_KEYS.iter().chain(&[
                "frames",
                "partly_measured_pct",
                "title_ms",
                "ours_ms",
                "measured_ms",
                "fills",
                "fills_mb",
                "hashed_mb",
                "compared_mb",
                "copied_back_mb",
                "uploaded_mb",
                "texture_uploads_mb",
                "governor_looks",
                "card_used_mib_avg",
                "card_used_mib_peak",
                "card_budget_mib_avg",
                "shared_used_mib_avg",
                "shared_used_mib_peak",
                "shared_budget_mib_avg",
                "free_ram_mib_avg",
                "free_ram_mib_lowest",
                "looks_card_over_budget",
                "looks_free_ram_under_512_mib",
                "pipeline_seconds",
                "compute_pipelines_a_second_avg",
                "costliest_pipeline_ms",
                "costliest_5_pipelines_ms",
                "costliest_20_pipelines_ms",
            ]) {
                assert!(section.get(*key).is_some(), "no {key} in\n{text}");
            }
            let f = |k: &str| section[k].as_float().unwrap();
            let kinds: f64 = GPU_KIND_KEYS.iter().map(|k| f(k)).sum();
            assert!((f("measured_ms") - kinds).abs() < 0.05, "{text}");
            assert!((f("title_ms") + f("ours_ms") + f("unattributed_ms") - f("measured_ms")).abs() < 0.05);
        }
        assert_eq!(gpu["frames"].as_integer(), Some(1_000));
        assert_eq!(gpu["draws_ms"].as_float(), Some(6.0));
        assert_eq!(gpu["measured_ms"].as_float(), Some(17.0));
        assert_eq!(gpu["partly_measured_pct"].as_float(), Some(2.0));
        assert_eq!(gpu["hashed_mb"].as_float(), Some(10.5));
        assert_eq!(gpu["compared_mb"].as_float(), Some(488.0));
        assert_eq!(gpu["free_ram_mib_lowest"].as_float(), Some(612.0));
        assert_eq!(gpu["card_used_mib_avg"].as_float(), Some(7_100.0));
        assert_eq!(gpu["looks_card_over_budget"].as_integer(), Some(3));
        assert_eq!(gpu["compute_pipelines_a_second_avg"].as_float(), Some(120.0));
        assert_eq!(gpu["costliest_pipeline_ms"].as_float(), Some(2.0));
        assert_eq!(gpu["costliest_20_pipelines_ms"].as_float(), Some(8.0));
        assert_eq!(gpu["steady"]["from_s"].as_float(), Some(40.0));
        assert_eq!(gpu["steady"]["frames"].as_integer(), Some(370));
    }

    /// With the split off nothing is measured and the report says so; a run
    /// with no steady state has none here either; no look of the governor
    /// leaves the lowest free memory unsaid.
    #[test]
    fn the_gpu_time_says_when_it_is_off_or_has_no_steady_state() {
        let off = GpuTime { state: Some(GpuSplit::Off), whole: [0; GPU_FIGS], steady: None };
        let text = render(&Snapshot { steady: None, gpu_time: off, ..synthetic() });
        let parsed: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(parsed["gpu_time"]["split"].as_str(), Some("off"));
        assert_eq!(parsed["gpu_time"]["measured_ms"].as_float(), Some(0.0));
        assert!(parsed["gpu_time"].get("free_ram_mib_lowest").is_none(), "{text}");
        assert!(parsed["gpu_time"]["steady"].as_table().unwrap().is_empty(), "{text}");
        assert!(text.contains("[gpu_time.steady]\n# none (see [frame_time.steady])"), "{text}");
    }

    /// The most-kept figures fold as the most, the others as sums; the steady
    /// state takes the windows (or a short run's seconds) the frame line's
    /// figures take.
    #[test]
    fn the_gpu_figures_fold_by_their_rule_over_the_steady_windows() {
        let mut a = [0u64; GPU_FIGS];
        let mut b = [0u64; GPU_FIGS];
        a[GpuFigure::DrawsNs as usize] = 5;
        b[GpuFigure::DrawsNs as usize] = 7;
        a[GpuFigure::CardUsedPeak as usize] = 9;
        b[GpuFigure::CardUsedPeak as usize] = 4;
        a[GpuFigure::FreeRamLowestNot as usize] = !100;
        b[GpuFigure::FreeRamLowestNot as usize] = !80;
        let folded = gpu_fold([a, b].iter());
        assert_eq!(folded[GpuFigure::DrawsNs as usize], 12);
        assert_eq!(folded[GpuFigure::CardUsedPeak as usize], 9);
        assert_eq!(!folded[GpuFigure::FreeRamLowestNot as usize], 80);
        // Windows from 40 s on: the fifth and sixth.
        let windows = [a, a, a, a, b, b];
        let steady = Steady { from_us: 40_000_000, figures: [0; FIGS], by_seconds: false };
        assert_eq!(gpu_steady(&steady, 21_300_000, &windows, &[])[GpuFigure::DrawsNs as usize], 14);
        // A short run: the seconds after the one `from_s` begins.
        let seconds = [a, a, a, b];
        let short = Steady { from_us: 4_000_000, figures: [0; FIGS], by_seconds: true };
        assert_eq!(gpu_steady(&short, 2_000_000, &[], &seconds)[GpuFigure::DrawsNs as usize], 7);
    }

    #[test]
    fn a_report_is_valid_toml_with_every_section() {
        let text = render(&synthetic());
        let parsed: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        for section in [
            "rox_box",
            "host",
            "run",
            "frames",
            "timeline",
            "gpu_work",
            "frame_time",
            "pipelines",
            "shaders",
            "memory",
            "textures",
            "unimplemented",
        ] {
            assert!(parsed.contains_key(section), "no [{section}]");
        }
        let frame_time = &parsed["frame_time"];
        for key in [
            "frames",
            "frame_ms",
            "preparing_ms",
            "copies_up_ms",
            "copies_up_mb",
            "texture_refresh_ms",
            "texture_refresh_mb",
            "waiting_for_card_ms",
            "copying_back_ms",
            "copying_back_mb",
            "card_busy_pct",
            "card_idle_ms",
            "card_idle_behind_us_ms",
            "submitting_thread_stood_ms",
            "heap_buffer_passes",
            "heap_buffer_ms",
        ] {
            assert!(frame_time.get(key).is_some(), "no frame_time.{key}");
            assert!(frame_time["steady"].get(key).is_some(), "no frame_time.steady.{key}");
        }
        for (_, key) in SETTLED {
            assert!(frame_time["waits"].get(key).is_some(), "no frame_time.waits.{key}");
            assert!(frame_time["waits"]["steady"].get(key).is_some(), "no steady {key}");
        }
        for (_, key, _) in SPARED {
            assert!(frame_time["spared"].get(key).is_some(), "no frame_time.spared.{key}");
            assert!(frame_time["spared"]["steady"].get(key).is_some(), "no steady {key}");
        }
        assert_eq!(frame_time["spared"]["shader_writes_left_on_card_mb"].as_float(), Some(50.0));
        for (_, key) in MOVED {
            assert!(frame_time["moved"].get(key).is_some(), "no frame_time.moved.{key}");
            assert!(frame_time["moved"]["steady"].get(key).is_some(), "no steady moved {key}");
        }
        assert_eq!(frame_time["moved"]["objects_let_go_aside_ms"].as_float(), Some(4.0));
        assert_eq!(frame_time["moved"]["snapshots_beside_ms"].as_float(), Some(3.0));
        assert_eq!(frame_time["moved"]["snapshots_waited_ms"].as_float(), Some(0.5));
        assert_eq!(frame_time["moved"]["steady"]["objects_let_go_aside_ms"].as_float(), Some(3.0));
        assert_eq!(frame_time["moved"]["pipeline_table_aside_ms"].as_float(), Some(0.0));
        assert_eq!(frame_time["preparing_ms"].as_float(), Some(6.2));
        assert_eq!(frame_time["card_busy_pct"].as_float(), Some(70.1));
        assert_eq!(frame_time["steady"]["from_s"].as_float(), Some(40.0));
        assert_eq!(frame_time["waits"]["sync_submission_or_present"].as_float(), Some(2.0));
        assert_eq!(parsed["gpu_work"]["left_out_avg_steady"].as_float(), Some(0.08));
        assert_eq!(parsed["run"]["first_draw_s"].as_float(), Some(20.9));
        let pressure = &parsed["memory"]["pressure"];
        assert_eq!(pressure["lowest_free_ram_mib"].as_integer(), Some(612));
        assert_eq!(pressure["lowest_commit_headroom_mib"].as_integer(), Some(2_100));
        assert_eq!(pressure["governor_red_s"].as_float(), Some(2.9));
        assert_eq!(pressure["vram_over_budget"].as_bool(), Some(true));
        assert_eq!(pressure["resident_wait_ms"].as_float(), Some(3_400.0));
        let views = &parsed["memory"]["views"];
        assert_eq!(views["title_views_mapped"].as_integer(), Some(40_000));
        assert_eq!(views["title_view_pages_avg"].as_float(), Some(2.5));
        assert_eq!(views["book_views_made_real"].as_integer(), Some(0));
        assert_eq!(views["ownerless_entries_dropped"].as_integer(), Some(52_000));
        assert_eq!(parsed["textures"]["waiting_for_memory_most"].as_integer(), Some(950));
        assert_eq!(parsed["textures"]["picked_uploaded_mib"].as_integer(), Some(5_200));
        assert_eq!(parsed["textures"]["refresh_ms"].as_float(), Some(4.5));
        assert_eq!(parsed["unimplemented"]["pipelines_gone_without"].as_integer(), Some(36));
        assert_eq!(parsed["format"].as_integer(), Some(i64::from(FORMAT)));
        assert_eq!(parsed["run"]["ended"].as_str(), Some("title_exited"));
        assert_eq!(
            parsed["host"]["gpu"]["adapter"].as_str(),
            Some("Some Adapter XT/XTX")
        );
        assert_eq!(
            parsed["host"]["gpu"]["driver"].as_str(),
            Some("32.0.15.6094")
        );
        assert_eq!(
            parsed["gpu_work"]["left_out"]["pipeline_not_ready"].as_integer(),
            Some(1_830)
        );
        assert_eq!(
            parsed["unimplemented"]["method"][0]["offset"].as_integer(),
            Some(0x218)
        );
        assert_eq!(
            parsed["unimplemented"]["method"][1]["offset"].as_integer(),
            Some(0x1a8)
        );
        assert!(text.starts_with("# Rox-box run report\n#\n"), "{text}");
        assert!(!text.contains(WITHHELD), "{text}");
        assert_eq!(
            summary(&text).as_deref(),
            Some("title_exited, 62 s, 49.0 fps on average")
        );
        assert_eq!(summary("not = 'a report'"), None);
        // The sample in docs/run-report.md is this test's output.
        println!("{text}");
    }

    /// The last window's rate is over the time the run spent in it: the
    /// demo title's short runs (30 presents in 0.7 s) said 3 fps.
    #[test]
    fn the_last_window_rate_is_over_the_time_it_covered() {
        let fps = |duration_us: u64, timeline: Vec<(u32, u32)>| {
            let text = render(&Snapshot { duration_us, timeline, ..synthetic() });
            let parsed: toml::Table = toml::from_str(&text).unwrap();
            parsed["timeline"]["fps"].as_array().unwrap().iter().map(|v| v.as_float().unwrap()).collect::<Vec<_>>()
        };
        assert_eq!(fps(2_000_000, vec![(60, 17)]), [30.0]);
        // Under a second in it counts as a second.
        assert_eq!(fps(500_000, vec![(30, 17)]), [30.0]);
        // Full windows as before; the last one 5 s long.
        assert_eq!(fps(25_000_000, vec![(300, 17), (600, 17), (150, 17)]), [30.0, 60.0, 30.0]);
    }

    #[test]
    fn percentiles_come_from_the_fine_buckets() {
        let s = synthetic();
        assert_eq!(s.percentile_ms(0.5), 16.75);
        assert_eq!(s.percentile_ms(0.9), 16.75);
        assert_eq!(s.percentile_ms(0.95), 33.25);
        assert_eq!(s.percentile_ms(0.999), 120.4);
        let text = render(&s);
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(parsed["frames"]["frame_ms_p99"].as_float(), Some(33.25));
        assert_eq!(fine_bucket(0), 0);
        assert_eq!(fine_bucket(99_999), 399);
        assert_eq!(fine_bucket(100_000), 400);
        assert_eq!(fine_bucket(1_999_999), FINE - 2);
        assert_eq!(fine_bucket(u64::MAX), FINE - 1);
    }

    #[test]
    fn every_end_renders_and_round_trips() {
        let ends = [
            End::TitleExited(-1),
            End::TitleAborted,
            End::TitleEnded,
            End::DeviceRemoved(0x887A_0006),
            End::HostGpuFailed,
            End::Crash(CrashIn::NoModule, Exception::CppException),
            End::UnimplementedImport,
            End::RoxBoxError,
        ];
        for end in ends.into_iter().chain(
            CrashIn::ALL
                .iter()
                .flat_map(|&c| Exception::ALL.map(|e| End::Crash(c, e))),
        ) {
            assert_eq!(End::decode(end.encode()), Some(end));
            let text = render(&Snapshot {
                end: Some(end),
                ..synthetic()
            });
            let parsed: toml::Table = toml::from_str(&text).unwrap();
            assert_eq!(parsed["run"]["ended"].as_str(), Some(end.key()));
            assert!(!text.contains(WITHHELD));
        }
        assert_eq!(End::decode(0), None);
        let killed = render(&Snapshot {
            end: None,
            ..synthetic()
        });
        assert!(killed.contains("ended = \"killed_or_unknown\""));
        assert_eq!(Exception::of_code(0xC000_0005), Exception::AccessViolation);
        assert_eq!(Exception::of_code(1), Exception::Other);
    }

    /// All our words pass the guard (none is withheld by accident, say for
    /// eight letters that happen to be hex digits).
    #[test]
    fn our_own_words_pass_the_guard() {
        let words = LeftOut::ALL
            .iter()
            .map(|w| w.key())
            .chain(CrashIn::ALL.iter().map(|c| c.key()))
            .chain(Exception::ALL.iter().map(|e| e.key()))
            .chain(BUCKET_KEYS)
            .chain([
                "killed_or_unknown",
                "not_looked",
                "green",
                "yellow",
                "red",
                "<set>",
                "run",
                "selftest",
            ])
            .chain([StageOutcome::Ok, StageOutcome::Fail, StageOutcome::Skip].map(StageOutcome::key));
        for word in words {
            assert!(guarded(word, false), "{word}");
        }
    }

    #[test]
    fn host_texts_are_cleaned() {
        assert_eq!(
            HostText::new("  Some Adapter (TM) 7800  ").as_str(),
            "Some Adapter (TM) 7800"
        );
        assert_eq!(HostText::new("Brand\u{ae} CPU\0").as_str(), "Brand_ CPU");
        assert_eq!(HostText::new("").as_str(), "unknown");
        assert_eq!(HostText::new("Radeon XT/XTX").as_str(), "Radeon XT/XTX");
        for bad in smuggled().iter().map(String::as_str).chain([
            "/home/example",
            "a/b/c",
            "x:/y",
            "net//share",
            "ab /cd",
        ]) {
            assert_eq!(HostText::new(bad).as_str(), WITHHELD, "{bad}");
        }
    }

    #[test]
    fn switches_show_names_and_only_plain_values() {
        let os = |s: &str| std::ffi::OsString::from(s);
        let s = smuggled();
        let vars = vec![
            (os("VICTORY_PIPELINE_WAIT_MS"), os("500")),
            (os("VICTORY_DISPLAY_MODE"), os("1920x1080")),
            (os("victory_mode_120"), os("1")),
            (os("VICTORY_GAME_DIR"), os("plain")),
            (os("VICTORY_SHADER_PROFILE"), os("p")),
            (os("VICTORY_WATCH"), os("10")),
            (os("VICTORY_SOME_NEW"), os(&s[0])),
            (os("VICTORY_HASHES_LIKE"), os(&s[1])),
            (os("VICTORY_ADDRESS"), os(&s[2])),
            (os("VICTORY_SENTENCE"), os(&s[3])),
            (os("VICTORY_FILE_NAME_LIKE"), os("title.exe")),
            (os("VICTORY_SHORT_HEX"), os("1a2b3c")),
            (os("PATH"), os(&s[0])),
            (os(&format!("VICTORY_{}", s[0])), os("1")),
        ];
        let shown = switches(vars.into_iter());
        let value = |name: &str| {
            shown
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(value("VICTORY_PIPELINE_WAIT_MS"), Some("500"));
        assert_eq!(value("VICTORY_DISPLAY_MODE"), Some("1920x1080"));
        assert_eq!(value("VICTORY_MODE_120"), Some("1"));
        for set in [
            "VICTORY_GAME_DIR",
            "VICTORY_SHADER_PROFILE",
            "VICTORY_WATCH",
            "VICTORY_SOME_NEW",
            "VICTORY_HASHES_LIKE",
            "VICTORY_ADDRESS",
            "VICTORY_SENTENCE",
            "VICTORY_FILE_NAME_LIKE",
            "VICTORY_SHORT_HEX",
        ] {
            assert_eq!(value(set), Some("<set>"), "{set}");
        }
        assert_eq!(shown.len(), 12, "{shown:?}");
        let text = render(&Snapshot {
            switches: shown,
            ..synthetic()
        });
        must_not_appear(&text);
        assert!(toml::from_str::<toml::Table>(&text).is_ok());
    }

    #[test]
    fn import_names_are_identifiers_only() {
        assert_eq!(
            import_name("kernel32.dll", "Sleep").as_deref(),
            Some("kernel32.dll!Sleep")
        );
        assert_eq!(
            import_name("ext-ms-win-a-l1-1-0.dll", "#12").as_deref(),
            Some("ext-ms-win-a-l1-1-0.dll!#12")
        );
        for bad in smuggled() {
            assert_eq!(import_name(&bad, "Sleep"), None, "{bad}");
            assert_eq!(import_name("kernel32.dll", &bad), None, "{bad}");
        }
    }

    /// Every public entry point that takes text, fed what must never be
    /// written; the render guard is checked on its own too.
    #[test]
    fn nothing_smuggled_through_any_entry_point_is_written() {
        for bad in smuggled() {
            let word = leak(bad.clone());
            // Through the keyed tables (as if a caller leaked a runtime string).
            method_missing(word, 0x10);
            shader_failed(word, word);
            shader_failed("unsupported", word);
            console(Ours::new(word));
            host_cpu(HostText::new(&bad), 8);
            host_gpu(GpuFacts {
                adapter: HostText::new(&bad),
                vendor_id: 0,
                driver: None,
                dedicated_mib: 0,
                software: false,
                feature_level: (12, 0),
                shader_model: (6, 6),
                wave_min: 0,
                wave_max: 0,
                mesh_shaders: false,
            });
            let calls: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(1)));
            import_site(word, "Sleep", calls);
            import_site("kernel32.dll", word, calls);
            import_site(word, word, calls);
        }
        // The entry points of numbers alone, fed what an address looks like:
        // what comes out of them is numbers, in decimal.
        ACTIVE.store(true, Relaxed);
        let address = 0x7ff6_a1b2_c3d4u64;
        figures(&[
            (Figure::Frames, 1),
            (Figure::MeterUs, address),
            (Figure::CopiesUpBytes, address),
            (Figure::SettledFence, address),
            (Figure::RefreshAllUs, address),
        ]);
        memory_seen(address, address);
        memory_look(address, [address, address + 1]);
        for stat in [Stat::PickedBytes, Stat::TailBytes, Stat::ResidentUs, Stat::PipelinesGoneWithout] {
            max(stat, address);
        }
        // A DLL test that would let anything through: the names' own rule
        // still holds.
        set_platform_dll(|_| true);
        let text = render(&Snapshot::take());
        must_not_appear(&text);
        assert!(!text.contains("7ff6"), "{text}");
        let parsed = toml::from_str::<toml::Table>(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        fn numbers_only(v: &toml::Value) -> bool {
            match v {
                toml::Value::Table(t) => t.values().all(numbers_only),
                toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Boolean(_) => true,
                _ => false,
            }
        }
        for section in [
            &parsed["frame_time"],
            &parsed["memory"]["pressure"],
            &parsed["memory"]["views"],
            &parsed["memory"]["map"],
            &parsed["textures"],
        ] {
            assert!(numbers_only(section), "{section}");
        }
        assert!(parsed["unimplemented"]["pipelines_gone_without"].is_integer());
        for bad in smuggled() {
            assert!(!guarded(&bad, false) && !guarded(&bad, true), "{bad}");
        }
        assert!(guarded("unsupported: an instruction", false));
        assert!(!guarded("a\"quote", false));
    }

    #[test]
    fn imports_of_other_dlls_are_only_counted() {
        let two: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(2)));
        let none: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
        let sites = [
            ("platform.dll", "Named", two),
            ("platform.dll", "Named", two),
            ("platform.dll", "NeverCalled", none),
            ("own.dll", "Hidden", two),
        ];
        let platform: fn(&str) -> bool = |dll| dll == "platform.dll";
        let (named, other) = called_imports(&sites, Some(platform));
        assert_eq!(named, [("platform.dll!Named".to_owned(), 4)]);
        assert_eq!(other, (1, 2));
        let (named, other) = called_imports(&sites, None);
        assert!(named.is_empty());
        assert_eq!(other, (3, 6));
    }

    #[test]
    fn keyed_tables_count_and_merge() {
        let table: Keyed<4> = Keyed::new();
        for _ in 0..3 {
            table.bump("device", "", 0x40);
        }
        table.bump("device", "", 0x48);
        table.bump("queue", "", 0x40);
        table.bump("list", "", 0x40);
        table.bump("fence", "", 0x40);
        let entries = table.entries();
        assert_eq!(entries[0], (Ours("device"), Ours(""), 0x40, 3));
        assert_eq!(entries.len(), 4);
        assert_eq!(table.lost.load(Relaxed), 1);
    }

    /// The steady state begins at the first window at or after a third of
    /// the time from the first present; the whole run is every window.
    #[test]
    fn the_steady_state_is_the_last_two_thirds_after_the_first_present() {
        assert_eq!(steady_from_us(21_300_000, 62_400_000), 40_000_000);
        assert_eq!(steady_from_us(0, 30_000_000), 10_000_000);
        assert_eq!(steady_from_us(0, 0), 0);
        assert_eq!(steady_from_us(5_000_000, 5_000_000), 10_000_000);
        assert_eq!(steady_from_us(5_000_000, 1_000_000), 10_000_000);
        let mut windows = vec![[0u64; FIGS]; 6];
        for (i, w) in windows.iter_mut().enumerate() {
            w[Figure::Frames as usize] = 10 * (i as u64 + 1);
            w[Figure::LeftOut as usize] = u64::MAX / 4;
        }
        let (whole, steady) = split_figures(&windows, Some(40_000_000));
        assert_eq!(whole[Figure::Frames as usize], 210);
        assert_eq!(steady[Figure::Frames as usize], 50 + 60);
        assert_eq!(whole[Figure::LeftOut as usize], u64::MAX);
        let (_, none) = split_figures(&windows, None);
        assert_eq!(none, [0; FIGS]);
    }

    /// A run with a whole window after its first third keeps the windows'
    /// rule; a shorter one takes the whole seconds after its first third;
    /// one too short for any reading there has no steady state, and says so.
    #[test]
    fn a_short_run_has_a_steady_state_by_seconds_or_none() {
        let frames = |n: u64| {
            let mut f = [0u64; FIGS];
            f[Figure::Frames as usize] = n;
            f
        };
        // 30 s from a first present at 0: [10, 20) is a whole window.
        let windows = vec![frames(10), frames(20), frames(30), frames(0)];
        let seconds: Vec<[u64; FIGS]> = (1..=32).map(frames).collect();
        let long = steady_state(0, 30_000_000, &windows, &seconds).unwrap();
        assert_eq!((long.from_us, long.by_seconds), (10_000_000, false));
        assert_eq!(long.figures[Figure::Frames as usize], 50);
        // 6 s from a first present at 2 s: a third is 2 s; the readings taken
        // in second 3 on (begun at 2 s and later) are kept.
        let short = steady_state(2_000_000, 8_000_000, &windows, &seconds).unwrap();
        assert_eq!((short.from_us, short.by_seconds), (4_000_000, true));
        assert_eq!(short.figures[Figure::Frames as usize], (4..=32).sum::<u64>());
        // 2.5 s: a third is under a second; the reading taken in second 2,
        // which began at 1 s, is the first kept.
        let mut few = vec![[0u64; FIGS]; SECONDS_KEPT];
        few[0] = frames(30);
        few[2] = frames(29);
        let short = steady_state(0, 2_500_000, &windows, &few).unwrap();
        assert_eq!((short.from_us, short.figures[Figure::Frames as usize]), (1_000_000, 29));
        // Under a second, read once at its end: nothing after its first third.
        few[2] = [0; FIGS];
        assert_eq!(steady_state(0, 700_000, &windows, &few), None);
        // About a second, read once in its second 1 (the reading began with
        // the run): too short too, not a steady state as long as the run.
        few[1] = frames(60);
        assert_eq!(steady_state(0, 1_100_000, &windows, &few), None);
        let text = render(&Snapshot { steady: None, ..synthetic() });
        let parsed: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let steady = parsed["frame_time"]["steady"].as_table().unwrap();
        assert!(steady.is_empty(), "{text}");
        assert!(text.contains("# none: the run was too short"), "{text}");
        assert!(parsed["gpu_work"].get("left_out_avg_steady").is_none(), "{text}");
        assert!(parsed["textures"].get("refresh_ms_steady").is_none(), "{text}");
        let by_seconds = render(&Snapshot {
            steady: Some(Steady { from_us: 1_000_000, figures: frames(29), by_seconds: true }),
            ..synthetic()
        });
        let parsed: toml::Table = toml::from_str(&by_seconds).unwrap();
        assert_eq!(parsed["frame_time"]["steady"]["frames"].as_integer(), Some(29));
        assert!(by_seconds.contains("# a short run"), "{by_seconds}");
    }

    fn a_selftest() -> Selftest {
        let stage = |name, outcome, reason| SelftestStage { name: Ours(name), outcome, reason };
        Selftest {
            software_adapter: true,
            frames: 8,
            bench: true,
            ended: Ours("completed"),
            stages: vec![
                stage("Device", StageOutcome::Ok, None),
                stage("Clear + present", StageOutcome::Ok, None),
                stage("Translated shader", StageOutcome::Skip, Some(Ours("the adapter has no shader model 6.6"))),
            ],
            rates: vec![
                SelftestRate { name: Ours("present_interval"), value: 1.25, unit: Ours("ms") },
                SelftestRate { name: Ours("buffer_copy"), value: 2048.5, unit: Ours("MB per s") },
            ],
        }
    }

    /// The self-test's report: valid TOML, the host, the stages and the
    /// rates as numbers; what is smuggled through our words is withheld.
    #[test]
    fn a_selftest_report_holds_its_stages_and_rates() {
        let switches = vec![("VICTORY_D3D12_WARP".to_owned(), "1".to_owned())];
        let text = render_selftest(&synthetic(), &a_selftest(), &switches);
        let parsed: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(parsed["kind"].as_str(), Some("selftest"));
        assert_eq!(parsed["format"].as_integer(), Some(i64::from(FORMAT)));
        assert_eq!(parsed["host"]["gpu"]["adapter"].as_str(), Some("Some Adapter XT/XTX"));
        assert!(parsed.get("run").is_none() && parsed.get("frames").is_none(), "{text}");
        let test = &parsed["selftest"];
        assert_eq!(test["ended"].as_str(), Some("completed"));
        assert_eq!(test["passed"].as_bool(), Some(true));
        assert_eq!((test["stages_ok"].as_integer(), test["stages_skipped"].as_integer()), (Some(2), Some(1)));
        assert_eq!(test["rates"]["buffer_copy"].as_float(), Some(2048.5));
        assert_eq!(test["rate_units"]["buffer_copy"].as_str(), Some("MB per s"));
        assert_eq!(test["switches"]["VICTORY_D3D12_WARP"].as_str(), Some("1"));
        let stages = test["stage"].as_array().unwrap();
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[2]["outcome"].as_str(), Some("skip"));
        assert_eq!(stages[2]["reason"].as_str(), Some("the adapter has no shader model 6.6"));
        assert!(stages[0].get("reason").is_none());
        assert!(text.starts_with("# Rox-box self-test report\n#\n"), "{text}");
        assert!(!text.contains(WITHHELD), "{text}");
        assert_eq!(
            summary(&text).as_deref(),
            Some("self-test on the software adapter, completed: 2 ok, 0 failed, 1 skipped")
        );
        // A caller that leaked a runtime string into our words.
        for bad in smuggled() {
            let word = Ours(leak(bad));
            let mut test = a_selftest();
            test.stages[0].reason = Some(word);
            test.stages[1].name = word;
            test.rates[0].name = word;
            test.rates[1].unit = word;
            let text = render_selftest(&synthetic(), &test, &switches);
            must_not_appear(&text);
            toml::from_str::<toml::Table>(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        }
    }

    /// The frame line's figures go to the window of now, and the governor's
    /// looks give their time to the level found before.
    #[test]
    fn the_frame_line_and_the_governor_feed_the_report() {
        ACTIVE.store(true, Relaxed);
        let before = Snapshot::take();
        figures(&[(Figure::SettledMost, 1_234), (Figure::HeapBufferPasses, 5)]);
        memory_look(2, [100, 50]);
        std::thread::sleep(Duration::from_millis(5));
        memory_look(1, [100, 150]);
        memory_seen(1 << 20, 3 << 20);
        memory_seen(u64::MAX, u64::MAX);
        let after = Snapshot::take();
        let grew = |f: Figure| after.figures[f as usize] - before.figures[f as usize];
        assert!(grew(Figure::SettledMost) >= 1_234);
        assert!(grew(Figure::HeapBufferPasses) >= 5);
        let total = |s: &Snapshot| s.governor_us.iter().sum::<u64>();
        assert!(total(&after) - total(&before) >= 5_000);
        assert!(after.stat(Stat::CardOverBudgetLooks) > before.stat(Stat::CardOverBudgetLooks));
        assert!(after.lowest[0] <= 1 << 20 && after.lowest[1] <= 3 << 20);
    }

    /// The GPU side's split and the governor's looks reach `[gpu_time]`: the
    /// sums grow, the peak and the lowest free memory are the most and the
    /// least seen.
    #[test]
    fn the_gpu_split_and_the_governor_feed_the_gpu_time() {
        ACTIVE.store(true, Relaxed);
        let before = Snapshot::take().gpu_time.whole;
        gpu_time(GpuSplit::On, &[(GpuFigure::Frames, 30), (GpuFigure::HashesNs, 4_000_000), (GpuFigure::HashedBytes, 9)]);
        gpu_time_look([8 << 30, 9 << 30], [8 << 30, 1 << 30], 300 << 20);
        gpu_time_look([0, 0], [0, 0], u64::MAX);
        let after = Snapshot::take().gpu_time;
        let grew = |f: GpuFigure| after.whole[f as usize] - before[f as usize];
        assert!(grew(GpuFigure::Frames) >= 30);
        assert!(grew(GpuFigure::HashesNs) >= 4_000_000);
        assert!(grew(GpuFigure::Looks) >= 1);
        assert!(grew(GpuFigure::CardOverLooks) >= 1);
        assert!(grew(GpuFigure::RamLowLooks) >= 1);
        assert!(after.whole[GpuFigure::CardUsedPeak as usize] >= 9 << 30);
        assert!(!after.whole[GpuFigure::FreeRamLowestNot as usize] <= 300 << 20);
        assert_eq!(after.state, Some(GpuSplit::On));
    }

    #[test]
    fn report_files_are_named_ordered_and_pruned() {
        assert_eq!(utc_stamp(0), "19700101-000000");
        assert_eq!(utc_stamp(951_782_400), "20000229-000000");
        assert_eq!(utc_stamp(1_798_761_599), "20261231-235959");
        assert_eq!(
            report_order("run-20261127-101010.txt", Kind::Run),
            Some(("20261127-101010".into(), 1))
        );
        assert_eq!(
            report_order("run-20261127-101010-3.txt", Kind::Run),
            Some(("20261127-101010".into(), 3))
        );
        assert_eq!(
            report_order("selftest-20261127-101010.txt", Kind::Selftest),
            Some(("20261127-101010".into(), 1))
        );
        assert_eq!(report_order("selftest-20261127-101010.txt", Kind::Run), None);
        assert_eq!(Kind::of_file("selftest-20261127-101010-2.txt"), Some(Kind::Selftest));
        assert_eq!(Kind::of_file("run-20261127-101010.txt"), Some(Kind::Run));
        assert_eq!(report_order("run-2026.txt", Kind::Run), None);
        assert_eq!(Kind::of_file("notes.txt"), None);
        let dir = std::env::temp_dir().join(format!("victory-report-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "run-20260101-000000.txt",
            "run-20260102-000000.txt",
            "run-20260102-000000-2.txt",
            "run-20260103-000000.txt",
            "run-20260103-000000.txt.part",
            "selftest-20260102-120000.txt",
            "selftest-20260101-120000.txt",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        let names = |dir: &Path| -> Vec<String> {
            reports_in(dir)
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            names(&dir),
            [
                "run-20260103-000000.txt",
                "selftest-20260102-120000.txt",
                "run-20260102-000000-2.txt",
                "run-20260102-000000.txt",
                "selftest-20260101-120000.txt",
                "run-20260101-000000.txt"
            ]
        );
        // Each kind keeps its own newest.
        prune(&dir, Kind::Run, 2);
        prune(&dir, Kind::Selftest, 1);
        assert_eq!(
            names(&dir),
            ["run-20260103-000000.txt", "selftest-20260102-120000.txt", "run-20260102-000000-2.txt"]
        );
        assert!(
            dir.join("notes.txt").exists() && !dir.join("run-20260103-000000.txt.part").exists()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Presents feed the counts, the histograms and the per-frame work;
    /// the first present only sets the starting totals. (The only test that
    /// presents, so the deltas are its own.)
    #[test]
    fn presents_feed_the_frame_counters() {
        ACTIVE.store(true, Relaxed);
        let before = Snapshot::take();
        let submissions = before.stat(Stat::Submissions);
        let drawn = WORK_AT[1].load(Relaxed);
        for frame in 1..=4u64 {
            bump(Stat::Submissions);
            std::thread::sleep(Duration::from_millis(2));
            present(drawn + 100 * frame, 0);
        }
        let after = Snapshot::take();
        assert_eq!(after.presents - before.presents, 4);
        let intervals = after.intervals - before.intervals;
        assert!((3..=4).contains(&intervals), "{intervals}");
        assert_eq!(
            after.buckets.iter().sum::<u64>() - before.buckets.iter().sum::<u64>(),
            intervals
        );
        let fine = |s: &Snapshot| s.fine.iter().map(|&n| u64::from(n)).sum::<u64>();
        assert_eq!(fine(&after) - fine(&before), intervals);
        assert!(after.interval_max_us >= 2_000);
        assert_eq!(after.stat(Stat::Submissions) - submissions, 4);
        assert!(after.work_max[1] >= 100);
        assert!(after.timeline.iter().map(|w| u64::from(w.0)).sum::<u64>() >= 4);
    }

    /// In a child process of the next test: a run's report begun in the
    /// folder `VICTORY_REPORT_TEST_DIR` names, ended as `VICTORY_REPORT_TEST_END`
    /// says (on this thread, or by the helper thread after a crash).
    #[test]
    fn report_child() {
        let Some(dir) = std::env::var_os("VICTORY_REPORT_TEST_DIR") else {
            return;
        };
        let path = begin(PathBuf::from(dir)).expect("a report");
        console(Ours::new("series-s"));
        host_windows(10, 0, 26100);
        match std::env::var("VICTORY_REPORT_TEST_END").as_deref() {
            Ok("crash") => {
                crash(CrashIn::TitleCode, Exception::AccessViolation).wait(Duration::from_secs(5))
            }
            _ => assert_eq!(finish(End::TitleExited(3)), Some(path)),
        }
    }

    #[test]
    fn a_run_leaves_a_report_when_it_ends_and_when_it_crashes() {
        for (end, says) in [("exit", "title_exited"), ("crash", "crash")] {
            let dir = std::env::temp_dir()
                .join(format!("victory-report-run-{}-{end}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "report::tests::report_child", "--nocapture"])
                .env("VICTORY_REPORT_TEST_DIR", &dir)
                .env("VICTORY_REPORT_TEST_END", end)
                .env_remove("VICTORY_REPORT")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let reports = reports_in(&dir);
            assert_eq!(reports.len(), 1, "{reports:?}");
            let text = std::fs::read_to_string(&reports[0]).unwrap();
            let parsed: toml::Table =
                toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
            assert_eq!(parsed["run"]["ended"].as_str(), Some(says), "{text}");
            assert_eq!(parsed["run"]["console"].as_str(), Some("series-s"));
            if end == "exit" {
                assert_eq!(parsed["run"]["exit_code"].as_integer(), Some(3));
            } else {
                assert_eq!(parsed["run"]["crash_in"].as_str(), Some("title_code"));
            }
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(
                said.contains("run report") && said.contains("safe to share"),
                "{said}"
            );
            assert!(
                std::fs::read_dir(&dir).unwrap().all(|e| !e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".part"))
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn the_switch_turns_reports_off_only_at_zero() {
        assert!(switch_on(None));
        assert!(switch_on(Some(OsStr::new("1"))));
        assert!(switch_on(Some(OsStr::new(""))));
        assert!(!switch_on(Some(OsStr::new("0"))));
    }
}
