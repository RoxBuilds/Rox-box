//! The card's own time by kind of work, in every build (`VICTORY_GPU_SPLIT`,
//! on by default): the run report's `[gpu_time]` and a line of the log a
//! second.
//!
//! How it is measured. A list marks where a run of commands of one kind
//! begins ([`Marks::mark`]): one timestamp query (`EndQuery` on a TIMESTAMP
//! heap) at the first command of the run, none for the commands after it
//! that are of the same kind (the thousands of draws a frame cost one mark
//! each time the kind changes, not one each). A submission's marks are put
//! in the order its lists run on the queue ([`Sequence`]); a list of ours at
//! the end of its last call to the queue writes one more timestamp and
//! resolves them all (`ResolveQueryData`) into a read-back ring, and they are
//! read when that submission has landed ([`landed`], told where the host
//! already knows it), never by waiting for the card.
//!
//! What a run's time is. Timestamps on one queue say when the card reached
//! that point of it, so the difference between two that follow each other is
//! the time the card spent on the commands between them: their work, the
//! waits of their barriers, and any waiting inside the card for memory to be
//! made resident or paged in from the PC (the card's memory paged out when
//! the PC is short of it) while it ran them. Time between submissions (the
//! card idle) is in no run. Work of another submission that another thread
//! put on the queue between two calls of one submission (a fetch of the
//! relay's on the drawing queue) is inside the run that spans the calls:
//! counted twice (a rare overlap).
//!
//! Where a run cannot be given one kind it is `unattributed`: past the
//! frame's budget of marks, a list with no slot left stops marking, and the
//! run from its last mark to the next list's first holds work of other kinds
//! (the frame is then counted as partly measured). A list with no mark at all
//! (barriers alone, our state changes) is inside the run before it.
//!
//! The title's dispatches are cut by pipeline too (`pipelines`, the default;
//! [`pipes`]): their time and counts by compute pipeline, for the log's table
//! of the costliest and for figures in the report that name none.
//!
//! What it costs: a mark is a relaxed load of the list's current kind, and at
//! a change of kind a lock, an `EndQuery` and a push; a dispatch of the
//! title's, with pipelines on, a compare of its pipeline and a lock to count
//! it, and at a change of pipeline a look-up in the table of pipelines and an
//! `EndQuery` (on the card, a timestamp between two dispatches, which may keep
//! them from overlapping); a submission one list of ours more (one `EndQuery`
//! and a `ResolveQueryData` per run of consecutive slots); a second, the fold
//! of what landed. No command of the title's or ours moves: the queries are
//! added between them. With `VICTORY_GPU_SPLIT=0` nothing is recorded and no
//! query heap is made.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize};

use super::*;

mod pipes;

/// What a run of commands on the host queue is, in the order the report
/// gives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    /// The title's draws, mesh dispatches and indirect draws.
    Draws,
    /// The title's dispatches (direct or indirect) of compute programs
    /// written for 64-lane waves: on a card of narrower waves, the programs
    /// the translator gives one of its forms for that (exact: meetings at
    /// group barriers, halves meeting by themselves, lane-serial; or the
    /// approximate one its guard falls back to). Which form is not kept by
    /// the host pipeline.
    Wave64,
    /// The title's other dispatches.
    Dispatches,
    /// The title's copies, clears and resolves.
    TitleOther,
    /// Ours: the steps done on the card instead of waiting for flights
    /// (`card_steps`: fills of constant buffers, buffer copies).
    Fills,
    /// Ours: the hash passes over copies in the card's memory after ranges
    /// were sent up.
    Hashes,
    /// Ours: the compare passes after a submission's lists.
    Compares,
    /// Ours: what shaders wrote brought down (the fetches of pieces of
    /// copies in the card's memory, textures copied back).
    CopyBacks,
    /// Ours: the ranges of copies in the card's memory sent up.
    Uploads,
    /// Ours: staging buffers copied into textures.
    Textures,
    /// Ours: every other pass (twin copies and depth planes, the state
    /// preludes, the planes of a present put together, the ambient
    /// stand-in, the present's copy, the clamps of indirect arguments).
    Passes,
    /// A run that holds work of more than one kind (see the module notes).
    Unattributed,
    /// Not counted: what follows the end of a list of ours that goes to the
    /// queue in a call of its own, up to the next mark.
    Gap,
}

/// The kinds a time is kept for (all but [`Kind::Gap`]).
pub(crate) const KINDS: usize = Kind::Unattributed as usize + 1;

/// No kind yet (a list just reset).
const NONE: u8 = u8::MAX;

/// Timestamps in the query heap, taken by lists in chunks of [`CHUNK`]:
/// 16 frames of the default budget.
const SLOTS: u32 = 1 << 17;
const CHUNK: u32 = 32;
/// Resolved timestamps the read-back ring holds for submissions not landed
/// yet.
const RING: u32 = 1 << 16;
/// The most boundaries a frame the switch may ask for.
const MOST_BUDGET: u32 = 16_384;

/// Boundaries a frame by default when runs are cut by kind alone: a few
/// thousand, as many as the runs of one kind a heavy frame has.
pub(crate) const BUDGET: u32 = 4096;
/// The same when the title's dispatches are cut by pipeline too: one more
/// boundary for nearly every dispatch.
pub(crate) const PIPELINES_BUDGET: u32 = 8192;

/// What the switch asks for: the frame's budget of boundaries (0: off), and
/// whether the title's dispatches are cut by pipeline ([`pipes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Settings {
    budget: u32,
    pipelines: bool,
}

/// The settings a value of the switch asks for ([`budget`]): words and a
/// number, comma-separated, in any order: `0` off; `kinds` runs by kind alone
/// (as before); `pipelines` (the default) the title's dispatches by pipeline
/// too; a number of 16 or more the budget (at most 16,384); `1` or nothing the
/// default budget of the mode. A word not known is left out.
fn settings_of(value: Option<&str>) -> Settings {
    let (mut budget, mut pipelines, mut off) = (None, true, false);
    for word in value.unwrap_or_default().split(',').map(str::trim) {
        match word {
            "" | "1" => {}
            "0" => off = true,
            "kinds" => pipelines = false,
            "pipelines" => pipelines = true,
            n => budget = n.parse::<u32>().ok().map(|n| n.clamp(16, MOST_BUDGET)).or(budget),
        }
    }
    let default = if pipelines { PIPELINES_BUDGET } else { BUDGET };
    Settings { budget: if off { 0 } else { budget.unwrap_or(default) }, pipelines: pipelines && !off }
}

/// `VICTORY_GPU_SPLIT`, unset or `1`: the card's time by kind, and of the
/// title's dispatches by pipeline (`pipelines`, the log's table of the
/// costliest a second), with the default budget of boundaries a frame
/// ([`PIPELINES_BUDGET`]); `kinds`: by kind alone, as before ([`BUDGET`]); a
/// number of 16 or more: that budget (at most 16,384), with either word
/// (`kinds,4096`); `0`: nothing recorded and no query heap made. Past the
/// budget a frame's lists stop marking and the frame counts as partly
/// measured; the last eighth of it is kept for changes of kind, so that past
/// that a change of pipeline alone is not cut (its run counts as of
/// pipelines not told apart). On by default: what it adds is a timestamp at
/// each change of kind of work in a list and at each change of a dispatch's
/// pipeline, and one small list a submission, and it moves no command.
fn settings() -> Settings {
    static NOW: OnceLock<Settings> = OnceLock::new();
    *NOW.get_or_init(|| settings_of(std::env::var("VICTORY_GPU_SPLIT").ok().as_deref()))
}

/// The frame's budget of boundaries ([`settings`]; 0: off).
pub(crate) fn budget() -> u32 {
    settings().budget
}

/// Whether the title's dispatches are cut by pipeline ([`budget`]).
pub(crate) fn pipelines() -> bool {
    settings().pipelines && on()
}

/// The driver refused the queries (logged once): off from then on.
static REFUSED: AtomicBool = AtomicBool::new(false);

/// Whether marks are recorded.
pub(crate) fn on() -> bool {
    budget() > 0 && !REFUSED.load(Ordering::Relaxed)
}

/// The query heap, the read-back ring and its mapping, the card's ticks a
/// second.
struct Objects {
    heap: ID3D12QueryHeap,
    back: ID3D12Resource,
    mapped: usize,
    frequency: u64,
}

// SAFETY: free-threaded D3D12 objects, and a mapping only read.
unsafe impl Send for Objects {}
// SAFETY: as above.
unsafe impl Sync for Objects {}

static OBJECTS: OnceLock<Option<Objects>> = OnceLock::new();

fn objects() -> Option<&'static Objects> {
    if !on() {
        return None;
    }
    OBJECTS.get_or_init(make).as_ref()
}

fn make() -> Option<Objects> {
    let host = made_host()?;
    // SAFETY: plain queries and object creation; the mapping is of a buffer
    // of that size, kept with it.
    let made = unsafe {
        (|| -> windows::core::Result<Objects> {
            let frequency = host.queue.GetTimestampFrequency()?;
            if frequency == 0 {
                return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
            }
            let mut heap: Option<ID3D12QueryHeap> = None;
            host.device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: SLOTS, NodeMask: 0 },
                &mut heap,
            )?;
            let heap = heap.ok_or_else(windows::core::Error::empty)?;
            let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: u64::from(RING) * 8,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut back: Option<ID3D12Resource> = None;
            host.device.CreateCommittedResource(
                &readback,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut back,
            )?;
            let back = back.ok_or_else(windows::core::Error::empty)?;
            name(&back, || "timestamps of the card's time by kind".into());
            let mut data = std::ptr::null_mut();
            back.Map(0, Some(&D3D12_RANGE { Begin: 0, End: RING as usize * 8 }), Some(&mut data))?;
            Ok(Objects { heap, back, mapped: data as usize, frequency })
        })()
    };
    match made {
        Ok(objects) => {
            tracing::info!(
                budget = budget(),
                ticks_per_s = objects.frequency,
                "the card's time by kind of work is measured (VICTORY_GPU_SPLIT=0: not)"
            );
            Some(objects)
        }
        Err(e) => {
            REFUSED.store(true, Ordering::Relaxed);
            tracing::warn!(error = %e, "no timestamp queries on the host queue: the card's time by kind is not measured");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The heap's slots, by chunks.

/// The free chunks (their first slots); `None` until first asked.
static FREE: Mutex<Option<Vec<u32>>> = Mutex::new(None);

fn take_slots() -> Option<u32> {
    let mut free = FREE.lock().unwrap_or_else(|e| e.into_inner());
    free.get_or_insert_with(|| (0..SLOTS / CHUNK).rev().map(|c| c * CHUNK).collect()).pop()
}

fn give_slots(chunks: Vec<u32>) {
    if chunks.is_empty() {
        return;
    }
    let mut free = FREE.lock().unwrap_or_else(|e| e.into_inner());
    free.get_or_insert_with(Vec::new).extend(chunks);
}

// ---------------------------------------------------------------------------
// The frame's budget.

/// Boundaries recorded since the last present.
static USED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Something of this frame was not given its kind.
static SHORT: AtomicBool = AtomicBool::new(false);
/// Frames partly measured since the last line.
static PARTLY: AtomicU64 = AtomicU64::new(0);

/// A present: the next frame's budget begins.
pub(crate) fn presented() {
    if budget() == 0 {
        return;
    }
    USED.store(0, Ordering::Relaxed);
    if SHORT.swap(false, Ordering::Relaxed) {
        PARTLY.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether one more boundary fits this frame's budget (and counts it).
fn granted(used: u32, budget: u32) -> bool {
    used < budget
}

/// Whether a boundary that changes only a dispatch's pipeline fits: not in
/// the last eighth of the budget, kept for changes of kind.
fn pipeline_cut_granted(used: u32, budget: u32) -> bool {
    used < budget - budget / 8
}

// ---------------------------------------------------------------------------
// A list's marks.

/// The beginning of a run: the slot its timestamp went to, its kind, and for
/// the title's dispatches with pipelines on, the pipeline ([`pipes::Pipe`])
/// and what the run's dispatches asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mark {
    slot: u32,
    kind: Kind,
    pipe: pipes::Pipe,
    dispatches: pipes::Asked,
}

impl Mark {
    fn of(slot: u32, kind: Kind) -> Mark {
        Mark { slot, kind, pipe: pipes::NO_PIPE, dispatches: pipes::Asked::default() }
    }
}

/// Where a list's runs begin: the slots it wrote a timestamp to, in recorded
/// order, and the kind of the run each begins.
pub(crate) struct Marks {
    /// The kind of the run being recorded ([`NONE`]: none yet).
    now: AtomicU8,
    /// Of a run of the title's dispatches, its pipeline (the host pipeline
    /// object's address; 0: none).
    now_pipe: AtomicUsize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    chunks: Vec<u32>,
    /// Slots taken of the last chunk.
    used: u32,
    slots: Vec<Mark>,
    /// A boundary was refused (the budget, no slot left): what follows the
    /// last one is of no one kind.
    short: bool,
}

impl Inner {
    fn slot(&mut self) -> Option<u32> {
        if self.chunks.is_empty() || self.used == CHUNK {
            self.chunks.push(take_slots()?);
            self.used = 0;
        }
        let slot = *self.chunks.last()? + self.used;
        self.used += 1;
        Some(slot)
    }
}

impl Default for Marks {
    fn default() -> Self {
        Marks::new()
    }
}

impl Drop for Marks {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        give_slots(std::mem::take(&mut inner.chunks));
    }
}

impl Marks {
    pub(crate) fn new() -> Marks {
        Marks { now: AtomicU8::new(NONE), now_pipe: AtomicUsize::new(0), inner: Mutex::new(Inner::default()) }
    }

    /// A run of `kind` begins at this point of `list` (an open list these
    /// marks are of): a timestamp if the kind changes, within the frame's
    /// budget.
    pub(crate) fn mark(&self, list: &ID3D12GraphicsCommandList, kind: Kind) {
        if self.now.load(Ordering::Relaxed) == kind as u8 {
            return;
        }
        self.record(list, kind, true);
    }

    /// A dispatch of the title's (`kind` [`Kind::Wave64`] or
    /// [`Kind::Dispatches`]) of host pipeline `p` comes at this point of
    /// `list`, of `groups` groups (`None`: indirect). With pipelines on
    /// ([`pipelines`]) a run begins where the kind or the pipeline changes,
    /// and the run counts what its dispatches asked for; else as
    /// [`Marks::mark`].
    pub(crate) fn mark_dispatch(
        &self,
        list: &ID3D12GraphicsCommandList,
        kind: Kind,
        p: &crate::pipeline::host::HostPipeline,
        groups: Option<u64>,
    ) {
        if !pipelines() {
            self.mark(list, kind);
            return;
        }
        let key = p.pso.as_raw() as usize;
        let same_kind = self.now.load(Ordering::Relaxed) == kind as u8;
        if same_kind && self.now_pipe.load(Ordering::Relaxed) == key {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !inner.short
                && let Some(last) = inner.slots.last_mut()
            {
                last.dispatches.add(groups);
            }
            return;
        }
        let Some(objects) = objects() else {
            return;
        };
        let pipe = pipes::pipe(key, p);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.short {
            return;
        }
        self.now.store(kind as u8, Ordering::Relaxed);
        self.now_pipe.store(key, Ordering::Relaxed);
        // Late in the frame's budget a change of pipeline alone is not cut:
        // the run goes on, as of pipelines not told apart.
        if same_kind
            && !pipeline_cut_granted(USED.load(Ordering::Relaxed), budget())
            && let Some(last) = inner.slots.last_mut()
            && last.kind == kind
        {
            last.pipe = pipes::MIXED;
            last.dispatches.add(groups);
            return;
        }
        if !granted(USED.fetch_add(1, Ordering::Relaxed), budget()) {
            inner.short = true;
            SHORT.store(true, Ordering::Relaxed);
            return;
        }
        let Some(slot) = inner.slot() else {
            inner.short = true;
            SHORT.store(true, Ordering::Relaxed);
            return;
        };
        // SAFETY: as in `record`.
        unsafe { list.EndQuery(&objects.heap, D3D12_QUERY_TYPE_TIMESTAMP, slot) };
        let mut mark = Mark { pipe, ..Mark::of(slot, kind) };
        mark.dispatches.add(groups);
        inner.slots.push(mark);
    }

    /// [`Marks::mark`] outside the budget: the end of a submission, the end
    /// of a list of ours that runs in a call of its own.
    fn force(&self, list: &ID3D12GraphicsCommandList, kind: Kind) {
        self.record(list, kind, false);
    }

    /// The end of a list of ours that goes to the queue in a call of its own,
    /// ahead of a submission's lists: what follows it there up to their first
    /// mark (other work, the card idle) is in no run.
    pub(crate) fn end(&self, list: &ID3D12GraphicsCommandList) {
        self.force(list, Kind::Gap);
    }

    fn record(&self, list: &ID3D12GraphicsCommandList, kind: Kind, budgeted: bool) {
        let Some(objects) = objects() else {
            return;
        };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.short {
            return;
        }
        self.now.store(kind as u8, Ordering::Relaxed);
        self.now_pipe.store(0, Ordering::Relaxed);
        if budgeted && !granted(USED.fetch_add(1, Ordering::Relaxed), budget()) {
            inner.short = true;
            SHORT.store(true, Ordering::Relaxed);
            return;
        }
        let Some(slot) = inner.slot() else {
            inner.short = true;
            SHORT.store(true, Ordering::Relaxed);
            return;
        };
        // SAFETY: an open list (the caller's), our heap, a slot of it that
        // these marks alone hold.
        unsafe { list.EndQuery(&objects.heap, D3D12_QUERY_TYPE_TIMESTAMP, slot) };
        inner.slots.push(Mark::of(slot, kind));
    }

    /// The list is reset: its slots go back (every submission of it has
    /// landed, and its timestamps were resolved by then).
    pub(crate) fn clear(&self) {
        // (No kind since the last reset: nothing was taken.)
        self.now_pipe.store(0, Ordering::Relaxed);
        if self.now.swap(NONE, Ordering::Relaxed) == NONE {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        give_slots(std::mem::take(&mut inner.chunks));
        inner.used = 0;
        inner.slots.clear();
        inner.short = false;
    }
}

// ---------------------------------------------------------------------------
// A submission's marks, in the order they run.

/// The marks of a submission's lists in queue order, and the transient
/// marks of lists of ours that it holds until it has landed.
#[derive(Default)]
pub(crate) struct Sequence {
    slots: Vec<Mark>,
    held: Vec<Marks>,
    /// Something of it was not given its kind.
    partly: bool,
}

impl Sequence {
    /// An empty sequence, if the split is on.
    pub(crate) fn new() -> Option<Sequence> {
        objects()?;
        Some(Sequence::default())
    }

    /// The marks of the list that runs next on the queue.
    pub(crate) fn list(&mut self, marks: &Marks) {
        let inner = marks.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.push(&inner.slots, inner.short);
    }

    fn push(&mut self, slots: &[Mark], short: bool) {
        self.slots.extend_from_slice(slots);
        if short {
            self.partly = true;
            // The run from its last mark (or the one before the list) holds
            // what the list did not mark.
            if let Some(last) = self.slots.last_mut() {
                last.kind = Kind::Unattributed;
                last.pipe = pipes::NO_PIPE;
            }
        }
    }

    /// [`Sequence::list`] of marks of a list of ours that the submission
    /// holds until it has landed.
    pub(crate) fn own(&mut self, marks: Marks) {
        self.list(&marks);
        self.held.push(marks);
    }

    /// `other` runs after what is here.
    pub(crate) fn append(&mut self, other: Sequence) {
        self.slots.extend(other.slots);
        self.held.extend(other.held);
        self.partly |= other.partly;
    }

    /// The submission's last list: one of ours that marks its end and
    /// resolves every timestamp, to go last in its last call to the queue;
    /// with what is to be read once submission `number` has landed.
    pub(crate) fn finish(mut self, number: u64) -> Option<(ID3D12CommandList, Pending)> {
        if self.slots.is_empty() {
            if self.partly {
                SHORT.store(true, Ordering::Relaxed);
            }
            return None;
        }
        let objects = objects()?;
        let resolver = take_resolver()?;
        let at = ring_take(self.slots.len() as u32 + 1);
        let Some(at) = at else {
            SHORT.store(true, Ordering::Relaxed);
            give_resolver(resolver);
            return None;
        };
        let (allocator, list) = &resolver;
        let end = Marks::new();
        // SAFETY: the resolver's last run has landed (it came back from the
        // pool only then); our heap and ring.
        let recorded = unsafe {
            (|| -> windows::core::Result<ID3D12CommandList> {
                allocator.Reset()?;
                list.Reset(allocator, None)?;
                end.force(list, Kind::Gap);
                self.own(end);
                resolve(list, objects, &self.slots, at);
                list.Close()?;
                list.cast()
            })()
        };
        let mut pending = Pending {
            number,
            at,
            marks: self.slots.clone(),
            _held: std::mem::take(&mut self.held),
            resolver: Some(resolver),
            partly: self.partly,
        };
        match recorded {
            Ok(list) => Some((list, pending)),
            Err(_) => {
                // (A list that did not record is not used again.)
                pending.resolver = None;
                SHORT.store(true, Ordering::Relaxed);
                None
            }
        }
    }

    /// For a list of ours that goes to the queue in a call of its own and is
    /// waited for there: its end marked and its timestamps resolved, in the
    /// list itself (open), to be read ([`Pending::fold`]) after the wait.
    pub(crate) fn finish_in(mut self, list: &ID3D12GraphicsCommandList) -> Option<Pending> {
        let objects = objects()?;
        let end = Marks::new();
        end.force(list, Kind::Gap);
        self.own(end);
        let n = self.slots.len() as u32;
        if n < 2 {
            return None;
        }
        let Some(at) = ring_take(n) else {
            SHORT.store(true, Ordering::Relaxed);
            return None;
        };
        // SAFETY: an open list (the caller's); our heap and ring.
        unsafe { resolve(list, objects, &self.slots, at) };
        Some(Pending {
            number: 0,
            at,
            marks: self.slots.clone(),
            _held: std::mem::take(&mut self.held),
            resolver: None,
            partly: self.partly,
        })
    }
}

/// A list of ours that goes to the queue in a call of its own and is waited
/// for there (a fetch of pieces): a run of `kind` begins at its start (`list`
/// open, just reset). `None` with the split off.
pub(crate) fn alone_begin(list: &ID3D12GraphicsCommandList, kind: Kind) -> Option<Marks> {
    objects()?;
    let marks = Marks::new();
    marks.mark(list, kind);
    Some(marks)
}

/// The end of that list: its timestamps resolved in it ([`Sequence::finish_in`]),
/// to be read ([`Pending::fold`]) once the wait for it is over.
pub(crate) fn alone_end(list: &ID3D12GraphicsCommandList, marks: Option<Marks>) -> Option<Pending> {
    let mut sequence = Sequence::new()?;
    sequence.own(marks?);
    sequence.finish_in(list)
}

/// The runs of consecutive slots of `slots`, as (first slot, count), in
/// order: one resolve each, into consecutive places of the ring.
fn slot_runs(slots: &[Mark]) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &Mark { slot, .. } in slots {
        match runs.last_mut() {
            Some((first, n)) if *first + *n == slot => *n += 1,
            _ => runs.push((slot, 1)),
        }
    }
    runs
}

/// Records the resolves of `slots` into the ring from `at` on.
///
/// # Safety
/// An open list; `at` and the slots' count lie in the ring.
unsafe fn resolve(list: &ID3D12GraphicsCommandList, objects: &Objects, slots: &[Mark], at: u32) {
    let mut to = at;
    for (first, n) in slot_runs(slots) {
        // SAFETY: the caller's; the runs are of slots these marks wrote.
        unsafe {
            list.ResolveQueryData(
                &objects.heap,
                D3D12_QUERY_TYPE_TIMESTAMP,
                first,
                n,
                &objects.back,
                u64::from(to) * 8,
            )
        };
        to += n;
    }
}

// ---------------------------------------------------------------------------
// The lists that resolve, and the ring they resolve into.

static RESOLVERS: Mutex<Vec<Prelude>> = Mutex::new(Vec::new());

fn take_resolver() -> Option<Prelude> {
    if let Some(resolver) = RESOLVERS.lock().unwrap_or_else(|e| e.into_inner()).pop() {
        return Some(resolver);
    }
    made_host()?.try_prelude().ok()
}

fn give_resolver(resolver: Prelude) {
    RESOLVERS.lock().unwrap_or_else(|e| e.into_inner()).push(resolver);
}

/// Places of the ring taken, oldest first: (first, count, done with).
struct Ring {
    head: u32,
    live: VecDeque<(u32, u32, bool)>,
}

static RINGS: Mutex<Ring> = Mutex::new(Ring { head: 0, live: VecDeque::new() });

impl Ring {
    /// `n` consecutive places, if they are free.
    fn take(&mut self, n: u32) -> Option<u32> {
        if n == 0 || n >= RING {
            return None;
        }
        let start = match self.live.front().map(|l| l.0) {
            None => 0,
            // Free: from the head to the end, and from 0 to the oldest
            // (never up to it: a full ring would read as an empty one).
            Some(oldest) if self.head > oldest => {
                if self.head + n <= RING {
                    self.head
                } else if n < oldest {
                    0
                } else {
                    return None;
                }
            }
            Some(oldest) if self.head + n < oldest => self.head,
            Some(_) => return None,
        };
        self.live.push_back((start, n, false));
        self.head = start + n;
        Some(start)
    }

    fn give(&mut self, at: u32) {
        if let Some(l) = self.live.iter_mut().find(|l| l.0 == at && !l.2) {
            l.2 = true;
        }
        while self.live.front().is_some_and(|l| l.2) {
            self.live.pop_front();
        }
        if self.live.is_empty() {
            self.head = 0;
        }
    }
}

fn ring_take(n: u32) -> Option<u32> {
    RINGS.lock().unwrap_or_else(|e| e.into_inner()).take(n)
}

// ---------------------------------------------------------------------------
// What landed, folded into the times by kind.

/// A submission's timestamps on their way: its number, where in the ring
/// they come, the kind of the run each begins.
pub(crate) struct Pending {
    number: u64,
    at: u32,
    marks: Vec<Mark>,
    _held: Vec<Marks>,
    resolver: Option<Prelude>,
    partly: bool,
}

// SAFETY: D3D12 objects are free-threaded; the resolver is used again only
// once the submission has landed.
unsafe impl Send for Pending {}

impl Drop for Pending {
    fn drop(&mut self) {
        RINGS.lock().unwrap_or_else(|e| e.into_inner()).give(self.at);
        if let Some(resolver) = self.resolver.take() {
            give_resolver(resolver);
        }
    }
}

static PENDING: Mutex<VecDeque<Pending>> = Mutex::new(VecDeque::new());

/// Submission `pending.number` went to the queue.
pub(crate) fn sent(pending: Pending) {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).push_back(pending);
}

/// Every submission up to `finished` has landed: their timestamps are read.
pub(crate) fn landed(finished: u64) {
    loop {
        let next = {
            let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            match pending.front() {
                Some(p) if p.number <= finished => pending.pop_front(),
                _ => None,
            }
        };
        match next {
            Some(p) => p.fold(),
            None => return,
        }
    }
}

/// Since the last line, the card's ticks by kind; since the start, the same
/// and the runs counted by kind (the self-test's and the tests' look); the
/// runs left out for a timestamp out of order.
static TICKS: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
static TICKS_ALL: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
static RUNS_ALL: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
static IGNORED: AtomicU64 = AtomicU64::new(0);

impl Pending {
    /// The submission has landed: its runs' times go to their kinds.
    pub(crate) fn fold(self) {
        let Some(objects) = OBJECTS.get().and_then(Option::as_ref) else {
            return;
        };
        let n = self.marks.len();
        // SAFETY: the ring's mapping; the card wrote these places when the
        // submission ran, and nothing writes them again before they are
        // given back (when this is dropped).
        let ticks = unsafe { std::slice::from_raw_parts((objects.mapped as *const u64).add(self.at as usize), n) };
        let mut by_kind = [0u64; KINDS];
        let mut runs = [0u64; KINDS];
        let kinds: Vec<Kind> = self.marks.iter().map(|m| m.kind).collect();
        let ignored = fold_runs(ticks, &kinds, objects.frequency, &mut by_kind, &mut runs);
        pipes::fold(ticks, &self.marks, objects.frequency);
        for k in 0..KINDS {
            if by_kind[k] > 0 {
                TICKS[k].fetch_add(by_kind[k], Ordering::Relaxed);
                TICKS_ALL[k].fetch_add(by_kind[k], Ordering::Relaxed);
            }
            if runs[k] > 0 {
                RUNS_ALL[k].fetch_add(runs[k], Ordering::Relaxed);
            }
        }
        if ignored > 0 {
            IGNORED.fetch_add(ignored, Ordering::Relaxed);
        }
        if self.partly {
            SHORT.store(true, Ordering::Relaxed);
        }
    }
}

/// Adds the time between each timestamp of `ticks` and the next to the kind
/// of the run it begins (`kinds`, one each; a [`Kind::Gap`] is not counted),
/// into `by_kind` (ticks) and `runs` (one each). A timestamp of 0 (not
/// written), one earlier than the one before (out of order: its slot written
/// again, or the counter wrapped) or a run longer than ten seconds of ticks is
/// left out: the count of those is returned.
fn fold_runs(ticks: &[u64], kinds: &[Kind], frequency: u64, by_kind: &mut [u64; KINDS], runs: &mut [u64; KINDS]) -> u64 {
    let longest = frequency.saturating_mul(10);
    let mut ignored = 0;
    for (i, pair) in ticks.windows(2).enumerate() {
        let Some(&kind) = kinds.get(i) else { break };
        if kind == Kind::Gap {
            continue;
        }
        let (a, b) = (pair[0], pair[1]);
        if a == 0 || b == 0 || b < a || b - a > longest {
            ignored += 1;
            continue;
        }
        by_kind[kind as usize] += b - a;
        runs[kind as usize] += 1;
    }
    ignored
}

/// Since the start: the card's time of `kind` in nanoseconds, and the runs
/// of it counted; `None` while nothing is measured (off, refused, or no
/// query heap made yet).
pub(crate) fn measured(kind: Kind) -> Option<(u64, u64)> {
    let objects = OBJECTS.get()?.as_ref()?;
    let ticks = TICKS_ALL[kind as usize].load(Ordering::Relaxed);
    let ns = (u128::from(ticks) * 1_000_000_000 / u128::from(objects.frequency.max(1))) as u64;
    Some((ns, RUNS_ALL[kind as usize].load(Ordering::Relaxed)))
}

/// What the report says of the split: on, off by the switch, refused by the
/// driver, or no timestamp taken yet.
pub(crate) fn state() -> victory_pipeline::report::GpuSplit {
    use victory_pipeline::report::GpuSplit;
    if budget() == 0 {
        GpuSplit::Off
    } else if REFUSED.load(Ordering::Relaxed) {
        GpuSplit::Refused
    } else if OBJECTS.get().is_some_and(Option::is_some) {
        GpuSplit::On
    } else {
        GpuSplit::NotStarted
    }
}

// ---------------------------------------------------------------------------
// The volumes beside the times.

/// What our work on the card moved, counted where it is recorded (also with
/// the split off: these are plain counters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Volume {
    /// Fills done on the card (`card_steps`), and their bytes.
    Fills,
    FillBytes,
    /// Hash passes after ranges were sent up, and the bytes they read.
    HashPasses,
    HashedBytes,
    /// Compare passes, and the bytes they read.
    ComparePasses,
    ComparedBytes,
    /// Bytes of pieces fetched from copies in the card's memory.
    CopiedBackBytes,
    /// Ranges sent up to copies in the card's memory, and their bytes.
    UploadRanges,
    UploadedBytes,
    /// Bytes of staging buffers copied into textures.
    TextureBytes,
}

pub(crate) const VOLUMES_N: usize = Volume::TextureBytes as usize + 1;
static VOLUMES: [AtomicU64; VOLUMES_N] = [const { AtomicU64::new(0) }; VOLUMES_N];

/// `n` more of `v`.
pub(crate) fn count(v: Volume, n: u64) {
    if n > 0 {
        VOLUMES[v as usize].fetch_add(n, Ordering::Relaxed);
    }
}

/// The bytes a pass of the compare shader (`CARD_COMPARE`) reads over the
/// pieces `first..first + pieces` of a copy that starts `skew` bytes into its
/// piece 0 and whose buffer holds `size` bytes.
pub(crate) fn pass_bytes(first: u32, pieces: u32, skew: u32, size: u64) -> u64 {
    const PIECE: u64 = 65536;
    let lo = (u64::from(first) * PIECE).saturating_sub(u64::from(skew));
    let hi = ((u64::from(first) + u64::from(pieces)) * PIECE).saturating_sub(u64::from(skew)).min(size);
    hi.saturating_sub(lo)
}

// ---------------------------------------------------------------------------
// Once a second, and at the end.

/// What the line and the report take: nanoseconds by kind, the volumes, the
/// frames partly measured, the runs left out.
fn take() -> ([u64; KINDS], [u64; VOLUMES_N], u64, u64) {
    landed(FINISHED.load(Ordering::Acquire));
    let frequency = OBJECTS.get().and_then(Option::as_ref).map_or(1, |o| o.frequency.max(1));
    let ns: [u64; KINDS] = std::array::from_fn(|k| {
        (u128::from(TICKS[k].swap(0, Ordering::Relaxed)) * 1_000_000_000 / u128::from(frequency)) as u64
    });
    let volumes: [u64; VOLUMES_N] = std::array::from_fn(|v| VOLUMES[v].swap(0, Ordering::Relaxed));
    (ns, volumes, PARTLY.swap(0, Ordering::Relaxed), IGNORED.swap(0, Ordering::Relaxed))
}

/// The frame line's second of `frames` frames: the report is given what
/// landed, and the log says it.
pub(crate) fn line(frames: f64) {
    let (ns, volumes, partly, ignored) = take();
    let pipelines = pipes::second(frames, ns.iter().sum(), on());
    crate::runreport::gpu_split(frames.round() as u64, ns, volumes, partly, pipelines);
    if !on() || ns.iter().all(|&n| n == 0) {
        return;
    }
    let ms = |k: Kind| ns[k as usize] as f64 / 1e6 / frames;
    let per = |v: Volume| volumes[v as usize] as f64 / frames;
    let measured: f64 = ns.iter().sum::<u64>() as f64 / 1e6 / frames;
    tracing::info!(
        "the card's time per frame by kind (VICTORY_GPU_SPLIT): {measured:.1} ms measured; the title's draws {:.1} ms, \
         dispatches of 64-lane programs {:.1}, other dispatches {:.1}, copies and clears {:.1}; ours: fills {:.1}, hash \
         passes {:.1}, compares {:.1}, copy-backs {:.1}, uploads {:.1}, texture uploads {:.1}, other passes {:.1}; \
         unattributed {:.1}; {partly} of {frames:.0} frames partly measured, {ignored} runs out of order left out; per \
         frame {:.0} fills {:.2} MB, {:.0} hash passes {:.1} MB, {:.0} compares {:.1} MB, {:.1} MB copied back, {:.0} \
         ranges {:.1} MB sent up, {:.1} MB of textures",
        ms(Kind::Draws),
        ms(Kind::Wave64),
        ms(Kind::Dispatches),
        ms(Kind::TitleOther),
        ms(Kind::Fills),
        ms(Kind::Hashes),
        ms(Kind::Compares),
        ms(Kind::CopyBacks),
        ms(Kind::Uploads),
        ms(Kind::Textures),
        ms(Kind::Passes),
        ms(Kind::Unattributed),
        per(Volume::Fills),
        per(Volume::FillBytes) / 1e6,
        per(Volume::HashPasses),
        per(Volume::HashedBytes) / 1e6,
        per(Volume::ComparePasses),
        per(Volume::ComparedBytes) / 1e6,
        per(Volume::CopiedBackBytes) / 1e6,
        per(Volume::UploadRanges),
        per(Volume::UploadedBytes) / 1e6,
        per(Volume::TextureBytes) / 1e6,
    );
}

/// Microseconds the thread that puts the log's table of pipelines together
/// spent since the last call (`VICTORY_PIPE_TABLE_ASIDE`): the frame line's
/// work before.
pub(crate) fn table_aside_us() -> u64 {
    pipes::aside_us()
}

/// When the report that names the run's end is written: the frames since
/// the line's last second, as the line would give them, without a line.
/// And the log's table of pipelines over the steady state, once.
pub(crate) fn last(frames: u64) {
    if frames > 0 {
        let (ns, volumes, partly, _) = take();
        let pipelines = pipes::second(frames as f64, ns.iter().sum(), false);
        crate::runreport::gpu_split(frames, ns, volumes, partly, pipelines);
    }
    pipes::steady_line();
}

impl HostList {
    /// A run of `kind` begins at this point of the list ([`Marks::mark`]).
    pub(crate) fn gpu_kind(&self, kind: Kind) {
        self.gpu_marks.mark(&self.list, kind);
    }

    /// A dispatch of the title's of `p` comes ([`Marks::mark_dispatch`]).
    pub(crate) fn gpu_dispatch(&self, kind: Kind, p: &crate::pipeline::host::HostPipeline, groups: Option<u64>) {
        self.gpu_marks.mark_dispatch(&self.list, kind, p, groups);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_on_by_default_and_takes_a_budget() {
        let budget_of = |v| settings_of(v).budget;
        assert_eq!(budget_of(None), PIPELINES_BUDGET);
        assert_eq!(budget_of(Some("1")), PIPELINES_BUDGET);
        assert_eq!(budget_of(Some("0")), 0);
        assert_eq!(budget_of(Some(" 8192 ")), 8192);
        assert_eq!(budget_of(Some("3")), 16);
        assert_eq!(budget_of(Some("1000000")), MOST_BUDGET);
        assert_eq!(budget_of(Some("lots")), PIPELINES_BUDGET);
    }

    /// Pipelines are on by default; `kinds` cuts by kind alone with its own
    /// default budget, as before; words and a number go together.
    #[test]
    fn the_switch_says_whether_pipelines_are_cut() {
        assert_eq!(settings_of(None), Settings { budget: PIPELINES_BUDGET, pipelines: true });
        assert_eq!(settings_of(Some("pipelines")), Settings { budget: PIPELINES_BUDGET, pipelines: true });
        assert_eq!(settings_of(Some("kinds")), Settings { budget: BUDGET, pipelines: false });
        assert_eq!(settings_of(Some("kinds, 2048")), Settings { budget: 2048, pipelines: false });
        assert_eq!(settings_of(Some("12000,pipelines")), Settings { budget: 12000, pipelines: true });
        assert_eq!(settings_of(Some("0,pipelines")), Settings { budget: 0, pipelines: false });
    }

    /// A change of pipeline alone is cut up to the last eighth of the budget.
    #[test]
    fn the_last_eighth_of_the_budget_is_kept_for_kinds() {
        assert!(pipeline_cut_granted(0, 8192));
        assert!(pipeline_cut_granted(7167, 8192));
        assert!(!pipeline_cut_granted(7168, 8192));
        assert!(granted(7168, 8192));
    }

    #[test]
    fn the_budget_grants_up_to_its_count() {
        assert!(granted(0, 4));
        assert!(granted(3, 4));
        assert!(!granted(4, 4));
        assert!(!granted(0, 0));
    }

    /// Each run's time goes to the kind of the mark it begins at; a gap is
    /// not counted; the last timestamp ends the last run.
    #[test]
    fn runs_fold_into_their_kinds() {
        let ticks = [100, 150, 160, 400, 410, 500];
        let kinds = [Kind::Draws, Kind::Fills, Kind::Draws, Kind::Gap, Kind::Hashes, Kind::Gap];
        let (mut by, mut runs) = ([0; KINDS], [0; KINDS]);
        assert_eq!(fold_runs(&ticks, &kinds, 1000, &mut by, &mut runs), 0);
        assert_eq!(by[Kind::Draws as usize], 50 + 240);
        assert_eq!(by[Kind::Fills as usize], 10);
        assert_eq!(by[Kind::Hashes as usize], 90);
        assert_eq!(runs[Kind::Draws as usize], 2);
        assert_eq!(by.iter().sum::<u64>(), 390);
    }

    /// A timestamp out of order (its slot written again, the counter
    /// wrapped), one not written, and a run of more than ten seconds are left
    /// out; the runs around them still count.
    #[test]
    fn a_wrapped_or_out_of_order_timestamp_is_left_out() {
        let ticks = [100, 90, 200, 0, 300, 300 + 20_000, 300 + 20_010];
        let kinds = [Kind::Draws, Kind::Dispatches, Kind::Uploads, Kind::Compares, Kind::Passes, Kind::Textures, Kind::Gap];
        let (mut by, mut runs) = ([0; KINDS], [0; KINDS]);
        let ignored = fold_runs(&ticks, &kinds, 1000, &mut by, &mut runs);
        // 100 -> 90 out of order; 200 -> 0 and 0 -> 300 not written; 20,000
        // ticks of 1,000 a second is past ten seconds.
        assert_eq!(ignored, 4);
        assert_eq!(by[Kind::Dispatches as usize], 110);
        assert_eq!(by[Kind::Textures as usize], 10);
        assert_eq!(by[Kind::Draws as usize], 0);
        assert_eq!(runs.iter().sum::<u64>(), 2);
    }

    /// Consecutive slots resolve in one call, in the order they run.
    #[test]
    fn consecutive_slots_resolve_together() {
        let k = Kind::Draws;
        let slots = [64, 65, 66, 3, 4, 100, 67].map(|s| Mark::of(s, k));
        assert_eq!(slot_runs(&slots), vec![(64, 3), (3, 2), (100, 1), (67, 1)]);
        assert_eq!(slot_runs(&[]), vec![]);
    }

    /// A list that ran out of budget makes the run its marks end in
    /// unattributed, and the sequence partly measured; one with no mark is
    /// inside the run before it.
    #[test]
    fn a_short_list_leaves_its_run_unattributed() {
        let mut seq = Sequence::default();
        seq.push(&[Mark::of(0, Kind::Draws), Mark::of(1, Kind::Dispatches)], false);
        seq.push(&[], false);
        assert!(!seq.partly);
        let of_pipe = Mark { pipe: 3, ..Mark::of(32, Kind::Wave64) };
        seq.push(&[of_pipe], true);
        assert!(seq.partly);
        // (Of no pipeline either: what it holds is not of one.)
        assert_eq!(seq.slots.last().map(|s| (s.kind, s.pipe)), Some((Kind::Unattributed, pipes::NO_PIPE)));
        // A short list with no mark: the run before it is unattributed.
        let mut seq = Sequence::default();
        seq.push(&[Mark::of(0, Kind::Fills)], false);
        seq.push(&[], true);
        assert_eq!(seq.slots, vec![Mark::of(0, Kind::Unattributed)]);
        let mut first = Sequence::default();
        first.push(&[Mark::of(5, Kind::Uploads), Mark::of(6, Kind::Hashes), Mark::of(7, Kind::Gap)], false);
        first.append(seq);
        assert_eq!(first.slots.len(), 4);
        assert!(first.partly);
    }

    /// The ring gives consecutive places, wraps to its start when the end is
    /// taken, and never gives what is not given back.
    #[test]
    fn the_ring_gives_places_back_in_order() {
        let mut ring = Ring { head: 0, live: VecDeque::new() };
        let a = ring.take(RING / 2).unwrap();
        let b = ring.take(RING / 4).unwrap();
        assert_eq!((a, b), (0, RING / 2));
        // Not past the end, and not over `a`.
        assert_eq!(ring.take(RING / 2), None);
        ring.give(a);
        let c = ring.take(RING / 4).unwrap();
        assert_eq!(c, RING * 3 / 4);
        let d = ring.take(RING / 4).unwrap();
        assert_eq!(d, 0);
        // Up to `b` and not onto it.
        assert_eq!(ring.take(RING / 4), None);
        ring.give(c);
        ring.give(b);
        ring.give(d);
        assert!(ring.live.is_empty());
        assert_eq!(ring.take(RING - 1), Some(0));
        assert_eq!(ring.take(0), None);
    }

    #[test]
    fn a_pass_reads_its_pieces_within_the_buffer() {
        const P: u64 = 65536;
        assert_eq!(pass_bytes(0, 1, 0, 10 * P), P);
        assert_eq!(pass_bytes(2, 3, 0, 10 * P), 3 * P);
        // A copy 100 bytes into its first piece: that piece is 100 short.
        assert_eq!(pass_bytes(0, 1, 100, 10 * P), P - 100);
        assert_eq!(pass_bytes(1, 1, 100, 10 * P), P);
        // The buffer's end cuts the last piece.
        assert_eq!(pass_bytes(9, 2, 0, 10 * P - 16), P - 16);
        assert_eq!(pass_bytes(12, 1, 0, 10 * P), 0);
    }

    /// On a host device that takes timestamp queries: a list's marks are
    /// resolved and read once its submission has landed, and the kinds it
    /// ran have runs (structure only: no magnitude is asserted).
    #[test]
    fn a_submission_on_the_host_folds_its_runs() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        if !on() || objects().is_none() {
            return; // the split is off or the driver refuses the queries
        }
        let before = [Kind::Draws, Kind::TitleOther].map(|k| measured(k).unwrap_or_default());
        // (No present in the tests: the frame's budget starts here.)
        USED.store(0, Ordering::Relaxed);
        let list = host.new_list().unwrap();
        let (a, _) = host.shared_buffer(1 << 20).unwrap();
        let (b, _) = host.shared_buffer(1 << 20).unwrap();
        list.gpu_kind(Kind::TitleOther);
        // SAFETY: an open list; live buffers of that size.
        unsafe { list.list.CopyBufferRegion(&b, 0, &a, 0, 1 << 20) };
        list.gpu_kind(Kind::Draws);
        // (A run of the same kind adds no mark.)
        list.gpu_kind(Kind::Draws);
        // SAFETY: as above.
        unsafe { list.list.CopyBufferRegion(&a, 0, &b, 0, 1 << 20) };
        assert_eq!(list.gpu_marks.inner.lock().unwrap().slots.len(), 2);
        list.close();
        host.execute(&[&list]);
        let after = [Kind::Draws, Kind::TitleOther].map(|k| measured(k).unwrap_or_default());
        for (b, a) in before.iter().zip(&after) {
            assert!(a.1 > b.1, "no run counted: {before:?} {after:?}");
            assert!(a.0 >= b.0);
        }
        list.reset();
        assert!(list.gpu_marks.inner.lock().unwrap().slots.is_empty());
        list.close();
    }
}
