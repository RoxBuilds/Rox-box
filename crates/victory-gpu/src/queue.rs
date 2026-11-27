//! Command queues. Submitted lists run on the host GPU and are waited for,
//! so fence signals complete at once.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUEUE, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

#[cfg(windows)]
mod behind;

const QUEUE_IIDS: [Iid; 4] = [
    guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDesc {
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

/// The title reads the list type right after the header.
#[repr(C)]
struct Queue {
    object: Object,
    kind: u32,
    desc: QueueDesc,
    /// What the queue was given after a wait for a fence that had not been
    /// reached, in order; it runs when the fence gets there.
    #[cfg(windows)]
    held: std::sync::Mutex<Vec<Held>>,
}
const _: () = assert!(std::mem::offset_of!(Queue, kind) == 0x10);

/// One thing a waiting queue holds back.
#[cfg(windows)]
enum Held {
    /// The queue waits for the fence (its address) to reach the value.
    Wait(usize, u64),
    /// A list of the next submission waits for a memory value.
    Value(crate::list::ValueWait),
    /// A submission: the lists' host sides, taken from the title's lists
    /// (which it may reset meanwhile), and the order they run in.
    Run(Vec<crate::list::Detached>, Vec<usize>),
    /// A signal of the queue's, due after what it holds before it.
    Signal(usize, u64),
}

/// The queues that hold something (their addresses).
#[cfg(windows)]
static WAITING: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

#[cfg(windows)]
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether queues wait for fences. `VICTORY_QUEUE_WAIT=0`: they do not,
/// and every submission runs when it is made (as before).
#[cfg(windows)]
fn waits() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_QUEUE_WAIT").is_none_or(|v| v != "0"))
}

/// Whether a submission is held list by list: the lists before the first
/// one whose wait for a memory value is not met run at once, and each list
/// that waits is held with the lists after it, behind its own waits.
/// Holding a submission whole made the title's queues wait for each other:
/// a graphics submission was held for a value a compute list writes, and
/// that compute list for a value an earlier list of the same graphics
/// submission writes. Nothing moved until the present forced it, and the
/// title reached its present only after waiting seconds, every frame
/// (against tens of milliseconds before waits were honoured; the frame's
/// main graphics submissions ran forced, the compute lists before them).
/// On by default; `VICTORY_SPLIT_WAITS=0` holds whole submissions, as
/// before.
#[cfg(windows)]
fn split() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SPLIT_WAITS").is_none_or(|v| v != "0"))
}

/// Whether a list is cut into host lists at its waits for memory values
/// (`crate::list::cut`), and a queue runs a submission part by part: it
/// stands at each wait until the value is in memory, with everything
/// recorded before the wait run and its bottom-of-pipe writes done, as on
/// the console. Held list by list ([`split`]) a wait still held its whole
/// list: the title's lists are chains of jobs, each behind a wait for a
/// value another queue's list writes in the middle of its own chain
/// (a compute list of hundreds of commands with over ten waits; graphics
/// lists of the form "dispatches, wait, draws"), so two lists
/// each waited for a write behind the other's wait and only the present
/// moved them, in the wrong order.
/// On by default where waits are honoured list by list;
/// `VICTORY_CUT_WAITS=0` holds whole lists, as before.
#[cfg(windows)]
pub(crate) fn cuts() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    waits() && split() && *ON.get_or_init(|| std::env::var_os("VICTORY_CUT_WAITS").is_none_or(|v| v != "0"))
}

/// Whether a present runs what queues still hold (on by default: a wait
/// is not kept across frames). `VICTORY_WAIT_FORCE=0` keeps it held: for
/// finding out whether the title's waits reach across its presents; a
/// wait nothing ever meets then stops the title for good.
#[cfg(windows)]
fn forces() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WAIT_FORCE").is_none_or(|v| v != "0"))
}

/// A submission to a queue whose lists are cut at their waits ([`cuts`]).
/// The lists before the first one that waits run at once if the queue
/// holds nothing; the rest goes behind what the queue holds, part by part
/// (`crate::list::detach_parts`): the waits of a part, then its host
/// lists. [`release_held`] runs the queue's line as far as the values are
/// there, now and whenever a submission has run.
#[cfg(windows)]
fn submit_cut(this: This, lists: &[This]) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let first = if lock(&queue(this).held).is_empty() {
        lists
            .iter()
            .position(|&l| crate::list::cut_list(l))
            .unwrap_or(lists.len())
    } else {
        0
    };
    #[cfg(feature = "diagnostics")]
    crate::capture::op(this as usize, || {
        let named: Vec<String> = lists
            .iter()
            .map(|&l| match crate::list::host_list(l) {
                Some(h) => format!("{:#x}", std::ptr::from_ref(h) as usize),
                None => format!("title {l:?}"),
            })
            .collect();
        format!(
            "SUBMIT queue kind {:?} held {} lists {named:?} (the first {first} run at once, the rest part by part)",
            Some(queue(this).kind),
            first < lists.len()
        )
    });
    if first > 0 && crate::host::late() {
        // Sent to the card and not waited for: their host sides are taken
        // from the title's lists (which it may reset before they have run)
        // and come back, with their tables' slots, when they are settled.
        let (taken, order) = crate::list::detach(&lists[..first]);
        host.execute_late(taken, order);
    } else if first > 0 {
        let now: Vec<_> = lists[..first]
            .iter()
            .filter_map(|&l| crate::list::host_list(l))
            .collect();
        host.execute(&now);
        // They have run: their descriptor tables' slots are free again, as
        // on the path without cuts ([`execute`]). This path replaced that
        // one without the call, so the lists that run at once kept their
        // slots until the title's reset and the ring ran out as before
        // (a fifth of a frame's tables refused).
        crate::list::tables_done(&lists[..first]);
    }
    if first < lists.len() {
        let parts = crate::list::detach_parts(&lists[first..]);
        static SEEN: AtomicU64 = AtomicU64::new(0);
        if SEEN.fetch_add(1, Ordering::Relaxed) < 8 {
            tracing::info!(
                kind = queue(this).kind,
                lists = lists.len(),
                run_at_once = first,
                parts = parts.len(),
                waits = parts.iter().map(|p| p.0.len()).sum::<usize>(),
                "a submission is given to its queue part by part (its lists wait for memory values)"
            );
        }
        {
            let mut held = lock(&queue(this).held);
            for (waits, taken, order) in parts {
                held.extend(waits.into_iter().map(Held::Value));
                held.push(Held::Run(taken, order));
            }
        }
        let mut waiting = lock(&WAITING);
        if !waiting.contains(&(this as usize)) {
            waiting.push(this as usize);
        }
    }
    // What ran may be what another queue waits for, and what was just
    // given to the queue may be free to run already.
    release_held(false);
}

/// A submission to a queue that honours waits, list by list ([`split`]).
/// shortcut: a wait still holds its whole list, not only the commands
/// after it; and lists named twice among those held stay in one piece.
#[cfg(windows)]
fn submit_split(this: This, lists: &[This]) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let by = crate::list::waits_by_list(lists);
    // Behind what the queue already holds, nothing runs now.
    let cut = if lock(&queue(this).held).is_empty() {
        (0..lists.len())
            .find(|&i| by[i].iter().any(|w| !w.met()))
            .unwrap_or(lists.len())
    } else {
        0
    };
    #[cfg(feature = "diagnostics")]
    crate::capture::op(this as usize, || {
        let named: Vec<String> = lists
            .iter()
            .map(|&l| match crate::list::host_list(l) {
                Some(h) => format!("{:#x}", std::ptr::from_ref(h) as usize),
                None => format!("title {l:?}"),
            })
            .collect();
        format!(
            "SUBMIT queue kind {:?} held {} lists {named:?} (the first {cut} run at once)",
            Some(queue(this).kind),
            cut < lists.len()
        )
    });
    if cut > 0 {
        let now: Vec<_> = lists[..cut]
            .iter()
            .filter_map(|&l| crate::list::host_list(l))
            .collect();
        host.execute(&now);
        // They have run: their descriptor tables' slots are free again, as
        // `submit_cut` gives them back after the same call (after fxceless
        // #1320, af23c65). These lists are not detached, so nothing else gave
        // their fresh tables' slots back before the title reset them, and with
        // `VICTORY_CUT_WAITS=0` every such submission held its first lists'
        // slots: the ring of 2^18 on a host heap capped at 1,000,000 views
        // (NVIDIA) runs dry, and tables are refused. A list named again among
        // the ones held keeps them: it runs again, and its slots go back with
        // its held part. `VICTORY_SPLIT_TABLES=0`: kept until the reset, as
        // before.
        if split_tables() {
            crate::list::tables_done(&ran_once(lists, cut));
        }
    }
    if cut < lists.len() {
        static SEEN: AtomicU64 = AtomicU64::new(0);
        if SEEN.fetch_add(1, Ordering::Relaxed) < 8 {
            tracing::info!(
                kind = queue(this).kind,
                lists = lists.len(),
                run_at_once = cut,
                waits = ?by[cut],
                "lists of a submission are held for a memory value"
            );
        }
        let rest = &lists[cut..];
        // A list's host side can be taken once.
        let twice = rest.iter().enumerate().any(|(i, l)| rest[..i].contains(l));
        let mut pieces: Vec<Held> = Vec::new();
        let mut from = cut;
        while from < lists.len() {
            // Up to the next list that waits for anything (met or not: it
            // is looked at again when its turn comes).
            let to = if twice {
                lists.len()
            } else {
                (from + 1..lists.len())
                    .find(|&i| !by[i].is_empty())
                    .unwrap_or(lists.len())
            };
            let waits: Vec<crate::list::ValueWait> = by[from..to].concat();
            pieces.extend(waits.into_iter().map(Held::Value));
            let (taken, order) = crate::list::detach(&lists[from..to]);
            pieces.push(Held::Run(taken, order));
            from = to;
        }
        lock(&queue(this).held).extend(pieces);
        let mut waiting = lock(&WAITING);
        if !waiting.contains(&(this as usize)) {
            waiting.push(this as usize);
        }
    }
    // What ran may be what another queue waits for, and what was just held
    // may be free to run already.
    release_held(false);
}

/// Whether [`submit_split`] gives back the descriptor tables' slots of the
/// lists it runs at once (`VICTORY_SPLIT_TABLES`, on unless `0`, read once).
#[cfg(windows)]
fn split_tables() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SPLIT_TABLES").is_none_or(|v| v != "0"))
}

/// Of the first `cut` of `lists` (run at once), those not named again in the
/// rest (held, to run later): the ones done with their tables.
#[cfg(windows)]
fn ran_once<T: PartialEq + Copy>(lists: &[T], cut: usize) -> Vec<T> {
    let (now, later) = lists.split_at(cut.min(lists.len()));
    now.iter().copied().filter(|l| !later.contains(l)).collect()
}

/// Adds `what()` to what the queue holds, if it holds anything (it waits
/// for a fence): whether it was added.
#[cfg(windows)]
fn hold(this: This, what: impl FnOnce() -> Held) -> bool {
    if this.is_null() {
        return false;
    }
    let mut held = lock(&queue(this).held);
    if held.is_empty() {
        return false;
    }
    held.push(what());
    true
}

/// Runs what waiting queues hold as far as their fences have been reached
/// (`all`: all of it), each queue's in the order it was given. Called
/// where a fence may have moved: after a signal and after a submission (a
/// list's bottom-of-pipe write may be into a fence), never from inside
/// one.
#[cfg(windows)]
pub(crate) fn release_held(all: bool) {
    if behind::on() {
        // What queues stand at is looked at by the thread that runs them.
        behind::wake();
        return;
    }
    // One caller at a time, so a queue's submissions keep their order.
    static RELEASING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    if lock(&WAITING).is_empty() {
        return;
    }
    let _looking = crate::timeline::span("looking at what queues hold");
    let _one = lock(&RELEASING);
    loop {
        let queues = lock(&WAITING).clone();
        let mut moved = false;
        for q in queues {
            // SAFETY: a live queue: one released is taken off the list.
            let held = unsafe { &(*(q as *const Queue)).held };
            let mut held = lock(held);
            loop {
                match held.first() {
                    None => break,
                    Some(&Held::Wait(fence, value))
                        if !all && crate::fence::completed(fence as This) < value =>
                    {
                        break;
                    }
                    Some(Held::Value(wait)) if !all && !value_met(wait) => break,
                    Some(_) => {}
                }
                match held.remove(0) {
                    Held::Wait(..) => {}
                    Held::Value(wait) => {
                        // Passed at a present without being met: what the
                        // queue stood at.
                        if all && !value_met(&wait) {
                            static SEEN: AtomicU64 = AtomicU64::new(0);
                            if SEEN.fetch_add(1, Ordering::Relaxed) < 16 {
                                tracing::warn!(
                                    queue = format_args!("{q:#x}"),
                                    ?wait,
                                    "a wait for a memory value was not met at a present: the queue runs on"
                                );
                            }
                        }
                    }
                    Held::Run(lists, order) => {
                        #[cfg(feature = "diagnostics")]
                        crate::capture::op(q, || {
                            format!(
                                "RUN held submission of {} lists (forced {all})",
                                order.len()
                            )
                        });
                        // Diagnostics: the part behind this one stands at
                        // waits that are met already, so the two could have
                        // been one host submission (one wait for the card,
                        // each range shaders write compared once).
                        #[cfg(feature = "diagnostics")]
                        {
                            let waits = held.iter().take_while(|h| matches!(h, Held::Value(_))).count();
                            if waits > 0
                                && matches!(held.get(waits), Some(Held::Run(..)))
                                && held[..waits].iter().all(|h| matches!(h, Held::Value(w) if w.met()))
                            {
                                crate::copystat::add(crate::copystat::C::PartsTogether, 1);
                            }
                        }
                        if crate::host::late()
                            && let Some(host) = crate::host::host()
                        {
                            // Not waited for: the host keeps the lists
                            // until they are settled.
                            host.execute_late(lists, order);
                        } else {
                            let ran = {
                                let runs: Vec<&crate::host::HostList> =
                                    order.iter().map(|&i| lists[i].host()).collect();
                                match crate::host::host() {
                                    Some(host) => {
                                        host.execute(&runs);
                                        true
                                    }
                                    None => false,
                                }
                            };
                            // They have run: their host lists are used again.
                            if ran {
                                crate::list::recycle(lists, &order);
                            }
                        }
                        if all {
                            #[cfg(feature = "diagnostics")]
                            crate::copystat::add(crate::copystat::C::Forced, 1);
                            static ONCE: AtomicBool = AtomicBool::new(false);
                            if !ONCE.swap(true, Ordering::Relaxed) {
                                tracing::warn!(
                                    "a queue still held a submission at a present: its fence was not reached"
                                );
                            }
                        }
                    }
                    // Behind what was sent to the card and not waited
                    // for, if anything was.
                    Held::Signal(fence, value) => {
                        if !crate::host::late_signal(fence, value) {
                            crate::fence::set(fence as This, value);
                        }
                    }
                }
                moved = true;
            }
            if held.is_empty() {
                lock(&WAITING).retain(|&w| w != q);
            }
        }
        if !moved {
            break;
        }
    }
}

/// Before a submission of ours that is waited for, or a wait for flights by
/// a thread of the title's: what the title gave its queues before goes to the
/// card first ([`behind`]).
#[cfg(windows)]
pub(crate) fn before_sync() {
    if behind::on() {
        behind::drain();
    }
}

/// Whether submissions go into the line of a thread of ours ([`behind`]).
#[cfg(windows)]
pub(crate) fn line_on() -> bool {
    behind::on()
}

/// Lists in the line may have been translated ([`crate::list::thin`]).
#[cfg(windows)]
pub(crate) fn line_moved() {
    behind::wake();
}

/// This thread does work the line waits for: it does not wait for the line.
#[cfg(windows)]
pub(crate) fn no_drain_here() {
    behind::no_drain_here();
}

/// Submissions the title made that the host has not begun ([`behind`]).
#[cfg(windows)]
pub(crate) fn queued() -> u64 {
    behind::queued()
}

/// For the frame-rate line.
#[cfg(windows)]
pub(crate) fn behind_line(frames: f64) {
    if behind::on() {
        crate::list::thin::line(frames);
        behind::line(frames);
    }
}

/// Whether a queue may pass `wait`: memory holds the value, or, with
/// submissions sent to the card without a wait (`crate::host::late`), it
/// will once those in the air have run.
#[cfg(windows)]
fn value_met(wait: &crate::list::ValueWait) -> bool {
    wait.met() || (crate::host::late() && crate::host::late_met(wait))
}

fn queue<'a>(this: This) -> &'a Queue {
    // SAFETY: called from queue methods only.
    unsafe { &*this.cast::<Queue>() }
}

pub(crate) extern "system" fn create(
    _: This,
    desc: *const QueueDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "command queue");
    let queue = Box::new(Queue {
        object: if crate::device::newer() {
            Object::new(&NEWER_FUNCTIONS)
        } else {
            Object::new(&FUNCTIONS)
        },
        kind: desc.kind,
        desc,
        #[cfg(windows)]
        held: Default::default(),
    });
    object::hand_out(QUEUE, queue, iid, &QUEUE_IIDS, out)
}

/// The console's queue description: an engine or pipe and a queue on it
/// in place of PC's priority and node mask.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDescX {
    kind: u32,
    flags: u32,
    engine_or_pipe: u32,
    queue: u32,
}

/// Device method `CreateCommandQueueX`. The host picks engines itself, so
/// the queue is created as on PC.
pub(crate) extern "system" fn create_x(
    this: This,
    desc: *const QueueDescX,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&x) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(desc = ?x, "console command queue");
    let desc = QueueDesc {
        kind: x.kind,
        priority: 0,
        flags: x.flags,
        nodes: 1,
    };
    create(this, &desc, iid, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUEUE,
        iid,
        object::is(iid, &QUEUE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    let left = object::release_boxed::<Queue>(this);
    #[cfg(windows)]
    if left == 0 {
        lock(&WAITING).retain(|&q| q != this as usize);
    }
    left
}

/// Runs the lists' host lists and waits for them. A queue
/// that waits for a fence keeps them until the fence is reached.
extern "system" fn execute(this: This, count: u32, lists: *const This) {
    if lists.is_null() {
        return;
    }
    victory_pipeline::report::bump(victory_pipeline::report::Stat::Submissions);
    let _called = crate::timeline::span("the title's ExecuteCommandLists");
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::timeline::title_thread();
    #[cfg(windows)]
    if let Some(host) = crate::host::host() {
        // SAFETY: the title passes `count` lists.
        let lists = unsafe { std::slice::from_raw_parts(lists, count as usize) };
        if behind::on() && !this.is_null() {
            behind::submit(this, lists);
            return;
        }
        if cuts() && !this.is_null() {
            submit_cut(this, lists);
            return;
        }
        if waits() && split() && !this.is_null() {
            submit_split(this, lists);
            return;
        }
        // A list that waits for a memory value not there yet: the queue
        // holds the submission, as it does behind a fence.
        if waits() && !this.is_null() {
            let unmet = crate::list::unmet_waits(lists);
            if !unmet.is_empty() {
                static SEEN: AtomicU64 = AtomicU64::new(0);
                if SEEN.fetch_add(1, Ordering::Relaxed) < 8 {
                    tracing::info!(kind = queue(this).kind, ?unmet, "a submission is held for a memory value");
                }
                lock(&queue(this).held).extend(unmet.into_iter().map(Held::Value));
                let mut waiting = lock(&WAITING);
                if !waiting.contains(&(this as usize)) {
                    waiting.push(this as usize);
                }
            }
        }
        let kept = hold(this, || {
            let (taken, order) = crate::list::detach(lists);
            Held::Run(taken, order)
        });
        // Diagnostics: the order submissions are made in, next to the
        // commands of the frame's `ops` file (which are in recording order).
        #[cfg(feature = "diagnostics")]
        crate::capture::op(this as usize, || {
            let kind = (!this.is_null()).then(|| queue(this).kind);
            // The lists as the frame's other lines name them (their host sides).
            let named: Vec<String> = lists
                .iter()
                .map(|&l| match crate::list::host_list(l) {
                    Some(h) => format!("{:#x}", std::ptr::from_ref(h) as usize),
                    None => format!("title {l:?}"),
                })
                .collect();
            format!("SUBMIT queue kind {kind:?} held {kept} lists {named:?}")
        });
        if kept {
            return;
        }
        let host_lists: Vec<_> = lists
            .iter()
            .filter_map(|&l| crate::list::host_list(l))
            .collect();
        host.execute(&host_lists);
        crate::list::tables_done(lists);
        // What the lists wrote at the bottom of the pipe may be a fence
        // another queue waits for.
        release_held(false);
        return;
    }
    let _ = this;
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::Relaxed) {
        tracing::warn!(count, "command lists submitted; no host GPU runs them");
    }
}

/// The start of a present plane: frame token, then the plane's resources.
#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const This,
}

/// Bytes from one plane's parameters to the next where the second plane's
/// token does not say ([`plane_stride`]): `D3D12XBOX_PRESENT_PLANE_PARAMETERS`
/// over up to two planes (0x90 bytes zeroed), with frame token at 0, resource
/// count at 8, resources at 0x10, source rect at 0x18, destination rect at
/// 0x20 (`{0.0, 0.0, 1.0, 1.0}`), colour space at 0x28, zeros to 0x48.
const PLANE_STRIDE: usize = 0x48;
/// Bytes of the planes' parameters read, at the least (and logged).
const PLANE_BYTES: usize = 160;
/// Bytes of a plane's parameters up to the words after its colour space.
const PLANE_HEAD: usize = 0x2c;
/// Planes of a present looked at, at most.
const MOST_PLANES: u32 = 4;

/// A plane's parameters as the title gives them ([`PLANE_STRIDE`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PlaneDesc {
    token: u64,
    count: u32,
    resources: usize,
    sources: usize,
    placement: usize,
    colour_space: u32,
    /// The words after the colour space, up to the next plane (scale
    /// filter, flags: zeros from this title).
    rest: [u32; 7],
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// The bytes from one plane's parameters to the next', found where the
/// second plane begins: every plane of a present carries the same token,
/// and a plane there names at least one resource (so a word that happens to
/// equal the token does not count: tokens are frame numbers). `None` with
/// one plane, or where no place fits every plane.
fn plane_stride(bytes: &[u8], planes: usize) -> Option<usize> {
    if planes < 2 {
        return None;
    }
    let token = u64_at(bytes, 0)?;
    (PLANE_HEAD.next_multiple_of(8)..=bytes.len()).step_by(8).find(|&stride| {
        (1..planes).all(|k| {
            let at = k * stride;
            u64_at(bytes, at) == Some(token)
                && u32_at(bytes, at + 8).is_some_and(|n| (1..=16).contains(&n))
                && u64_at(bytes, at + 0x10).is_some_and(|r| r != 0)
        })
    })
}

/// The parameters of `planes` planes `stride` bytes apart in `bytes` (those
/// that `bytes` holds up to their colour space).
fn planes_of(bytes: &[u8], planes: usize, stride: usize) -> Vec<PlaneDesc> {
    (0..planes)
        .map_while(|k| {
            let at = k.checked_mul(stride)?;
            let mut rest = [0; 7];
            for (i, word) in rest.iter_mut().enumerate() {
                let o = PLANE_HEAD + 4 * i;
                if o + 4 > stride {
                    break;
                }
                *word = u32_at(bytes, at + o).unwrap_or(0);
            }
            Some(PlaneDesc {
                token: u64_at(bytes, at)?,
                count: u32_at(bytes, at + 8)?,
                resources: u64_at(bytes, at + 0x10)? as usize,
                sources: u64_at(bytes, at + 0x18)? as usize,
                placement: u64_at(bytes, at + 0x20)? as usize,
                colour_space: u32_at(bytes, at + 0x28)?,
                rest,
            })
        })
        .collect()
}

/// The destination of a plane that covers the whole picture, as fractions of
/// it: left, top, right, bottom.
pub(crate) const WHOLE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// Whether `p` is a destination (fractions of the picture: left, top, right,
/// bottom): within the picture and not empty.
pub(crate) fn placed(p: [f32; 4]) -> bool {
    p.iter().all(|v| (0.0..=1.0).contains(v)) && p[0] < p[2] && p[1] < p[3]
}

/// Whether `source` (pixels: left, top, right, bottom) lies within a texture
/// of that size and is not empty.
pub(crate) fn inside(source: [i32; 4], (width, height): (u64, u32)) -> bool {
    let [l, t, r, b] = source.map(i64::from);
    0 <= l && l < r && r as u64 <= width && 0 <= t && t < b && b as u64 <= u64::from(height)
}

/// What a present knows of a plane: its first resource (0: none, or not one
/// of ours), that resource's size if it is a 2D texture, and the source and
/// destination the title gives it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Seen {
    resource: usize,
    size: Option<(u64, u32)>,
    source: Option<[i32; 4]>,
    placement: Option<[f32; 4]>,
}

/// Whether the planes are put together (`Host::present_planes`) instead of
/// the first one's texture being copied whole, as before: more than one
/// names a resource, or the one that does is not the first, shows a part of
/// its texture or is placed on a part of the picture. (A source or a
/// destination that is not one counts as the whole.)
fn composes(planes: &[Seen]) -> bool {
    let named: Vec<(usize, &Seen)> = planes.iter().enumerate().filter(|(_, p)| p.resource != 0).collect();
    match named[..] {
        [] => false,
        [(k, one)] => {
            let part = match (one.source, one.size) {
                (Some(s), Some(size)) => inside(s, size) && s != [0, 0, size.0 as i32, size.1 as i32],
                _ => false,
            };
            k != 0 || part || one.placement.is_some_and(|p| placed(p) && p != WHOLE)
        }
        _ => true,
    }
}
/// Submission tagged with the frame it belongs to `(count, lists, frame
/// token, flags)`; frames are paced elsewhere, so it runs as a plain one.
extern "system" fn execute_x(this: This, count: u32, lists: *const This, _token: u64, _flags: u32) {
    execute(this, count, lists);
}
/// Signal with sync settings for the top and bottom of the pipe; our
/// submissions are complete when they return, so it signals at once.
extern "system" fn signal2_x(
    this: This,
    fence: This,
    value: u64,
    _bottom: *const u8,
    _top: *const u8,
) -> HRESULT {
    signal(this, fence, value)
}
/// Markers and events only matter to profilers.
extern "system" fn marker(_: This) {}
extern "system" fn signal(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    let _called = crate::timeline::span("the title's queue Signal");
    #[cfg(windows)]
    if behind::on() && !this.is_null() {
        behind::signal(this, fence, value);
        return S_OK;
    }
    // A queue that waits signals after what it holds.
    #[cfg(windows)]
    if hold(this, || Held::Signal(fence as usize, value)) {
        return S_OK;
    }
    // Behind the submissions sent to the card and not waited for: the
    // fence is set when they are settled.
    #[cfg(windows)]
    if crate::host::late_signal(fence as usize, value) {
        return S_OK;
    }
    let _ = this;
    crate::fence::signal(fence, value)
}
/// From here on the queue runs nothing it is given until `fence` reaches
/// `value`: how the console orders work between queues (the title's
/// compute queue waits for the depth the graphics queue draws). Our
/// submissions run when they are made, so a wait for a value already
/// reached is nothing; otherwise what the queue is given next is held
/// ([`Held`]) until a signal or a list's bottom-of-pipe write gets the
/// fence there.
extern "system" fn wait(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    let _called = crate::timeline::span("the title's queue Wait");
    #[cfg(windows)]
    if !this.is_null() {
        let now = crate::fence::completed(fence);
        // Whether the title uses this at all (the first calls).
        {
            static CALLS: AtomicU64 = AtomicU64::new(0);
            if CALLS.fetch_add(1, Ordering::Relaxed) < 8 {
                tracing::info!(kind = queue(this).kind, now, value, "a queue is told to wait for a fence");
            }
        }
        #[cfg(feature = "diagnostics")]
        crate::capture::op(this as usize, || format!("WAIT queue kind {} for fence {fence:?} value {value} (now {now})", queue(this).kind));
        if behind::on() && waits() {
            behind::wait(this, fence, value);
            return S_OK;
        }
        let behind = {
            let mut held = lock(&queue(this).held);
            let behind = !held.is_empty() || now < value;
            if behind && waits() {
                held.push(Held::Wait(fence as usize, value));
            }
            behind
        };
        if behind {
            static SEEN: AtomicU64 = AtomicU64::new(0);
            if SEEN.fetch_add(1, Ordering::Relaxed) < 8 {
                tracing::info!(
                    kind = queue(this).kind,
                    now,
                    value,
                    held = waits(),
                    "a queue waits for a fence not reached yet"
                );
            }
        }
        if behind && waits() {
            {
                let mut waiting = lock(&WAITING);
                if !waiting.contains(&(this as usize)) {
                    waiting.push(this as usize);
                }
            }
            // The fence may have got there meanwhile.
            release_held(false);
        }
    }
    let _ = (this, value);
    S_OK
}
/// Ticks per second of GPU timestamps (see [`Written::GpuTimestamp`]).
extern "system" fn timestamp_frequency(_: This, out: *mut u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(TIMESTAMP_FREQUENCY) };
    S_OK
}
const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

/// What a list's bottom-of-pipe write stores when the list has run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Written {
    Value(u64),
    /// The GPU clock: [`TIMESTAMP_FREQUENCY`] ticks per second since the
    /// process started (our GPU work is done on the CPU's timeline).
    GpuTimestamp,
    /// The CPU's performance counter.
    SystemTimestamp,
}

impl Written {
    /// Stores the low `bytes` (4 or 8) of the value at `at`.
    pub(crate) fn write(self, at: *mut u8, bytes: usize) {
        let value = match self {
            Written::Value(v) => v,
            Written::GpuTimestamp => {
                static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
                let now = (elapsed.as_nanos() / (1_000_000_000 / u128::from(TIMESTAMP_FREQUENCY))) as u64;
                #[cfg(all(windows, feature = "diagnostics"))]
                crate::host::spans::stamped(at as usize, now);
                as_loaded(now, at as usize, gpu_load())
            }
            Written::SystemTimestamp => system_timestamp(),
        };
        #[cfg(all(windows, feature = "diagnostics"))]
        crate::timeline::told("written when its list had run", at as usize, value);
        // SAFETY: the title names writable GPU memory (unified with ours).
        unsafe {
            if bytes == 4 {
                at.cast::<u32>().write_unaligned(value as u32);
            } else {
                at.cast::<u64>().write_unaligned(value);
            }
        }
    }
}

/// What the title's GPU time is made to look like, in ticks
/// (`VICTORY_GPU_LOAD_MS`; unset: as it is).
///
/// The title asks for four GPU timestamps a frame, in pairs of 16 bytes, the
/// start in the second eight and the end in the first. All of them are written
/// when the frame's last submissions land, within microseconds, so every pair
/// says the card did nothing.
fn gpu_load() -> u64 {
    static LOAD: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *LOAD.get_or_init(|| {
        let ms = std::env::var("VICTORY_GPU_LOAD_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        ms * (TIMESTAMP_FREQUENCY / 1000)
    })
}

/// The timestamp written at `at` when the clock says `now`, if a pair's start
/// is to lie `load` ticks before its end.
fn as_loaded(now: u64, at: usize, load: u64) -> u64 {
    if at & 8 != 0 {
        now.saturating_sub(load).max(1)
    } else {
        now
    }
}

fn system_timestamp() -> u64 {
    #[cfg(windows)]
    {
        let mut count = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
        count as u64
    }
    #[cfg(not(windows))]
    0
}

/// Frames presented so far.
static FRAMES: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
pub(crate) fn presented() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

/// Shows the first plane's first resource in our window: copied whole, as
/// it always was, where the present has one plane shown whole on the whole
/// picture; else the planes put together ([`showing`],
/// `Host::present_planes`), or the one plane `VICTORY_PRESENT_PLANE` names.
///
/// shortcut: a plane's first resource only (the title names one).
extern "system" fn present(
    this: This,
    planes: u32,
    plane_parameters: *const Plane,
    _parameters: *const u8,
) -> HRESULT {
    #[cfg(not(windows))]
    let _ = this;
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::copystat::present_begins();
    let called = crate::timeline::span("the title's Present");
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::timeline::title_thread();
    // Submissions sent to the card and not waited for are settled first,
    // and what that frees runs in its own order, before anything is forced.
    #[cfg(windows)]
    if behind::on() {
        // The present goes into the line of what the queues were given.
    } else if crate::host::late() && !crate::host::late_free_present() {
        crate::host::settle_for_present();
        release_held(false);
        crate::host::settle_for_present();
    } else if crate::host::late() {
        // The present is sent ahead too: what waits for a value a flight
        // will write runs behind that flight, and nothing is settled here.
        release_held(false);
    }
    // shortcut: what a queue still holds at a present runs now, whatever
    // its fence: a wait is not kept across frames.
    #[cfg(windows)]
    if !behind::on() {
        release_held(forces());
    }
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    #[cfg(windows)]
    crate::runreport::presented();
    if frame.is_power_of_two() {
        let (draws, dispatches) = crate::list::take_counts();
        #[cfg(windows)]
        let (drawn, dispatched) = {
            static DRAWN: AtomicU64 = AtomicU64::new(0);
            static DISPATCHED: AtomicU64 = AtomicU64::new(0);
            let since = |last: &AtomicU64, now: u64| now - last.swap(now, Ordering::Relaxed);
            (
                since(&DRAWN, crate::draw::drawn()),
                since(&DISPATCHED, crate::draw::dispatched()),
            )
        };
        #[cfg(not(windows))]
        let (drawn, dispatched) = (0, 0);
        tracing::info!(
            frame,
            draws,
            drawn,
            dispatches,
            dispatched,
            "frame presented (since the last line: draws and dispatches, and those run on the host)"
        );
        #[cfg(windows)]
        if frame >= 256 {
            let (hits, misses) = crate::shader_cache::counts();
            tracing::info!(hits, misses, "shaders from the cache and translated so far");
        }
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        crate::capture::frame_drew(frame, crate::draw::drawn());
        if crate::capture::listed(frame) {
            before_sync();
            crate::capture::dump_targets(frame);
        }
    }
    #[cfg(windows)]
    let shows = showing(planes, plane_parameters, frame);
    #[cfg(windows)]
    if behind::on() && !this.is_null() {
        const COPY_SOURCE: u32 = 0x800;
        // The planes put together, each in the state the lists recorded so
        // far leave it in (as below); the first goes as the texture shown.
        let layers: Vec<Layer> = shows
            .layers
            .iter()
            .filter_map(|&(r, source, placement)| layer(r, source, placement, crate::list::thin::wanted()))
            .collect();
        let texture = if let Some(first) = layers.first() {
            Some((shows.resource as usize, first.texture.clone(), first.state))
        } else if !shows.resource.is_null() {
            let resource = shows.resource;
            // The state the lists recorded so far leave it in (the next
            // frame's, recorded before this is shown, go on from there).
            crate::resource::host_copy(resource).map(|copy| {
                let state = crate::resource::host_transition(resource, COPY_SOURCE).map_or(COPY_SOURCE, |(_, before)| before);
                crate::resource::host_transition(resource, state);
                // (Lists not translated yet leave it in no state yet: the
                // one it is in when its turn in the line comes.)
                let state = if crate::list::thin::wanted() { u32::MAX } else { state };
                (resource as usize, copy, state)
            })
        } else {
            None
        };
        let resource = texture.as_ref().map_or(0, |t| t.0);
        behind::present(this, texture.map(|(_, copy, state)| (copy, state)), Shown { frame, resource, layers });
        called.end();
        #[cfg(feature = "diagnostics")]
        frame_ends();
        return S_OK;
    }
    #[cfg(windows)]
    let layers: Vec<Layer> =
        shows.layers.iter().filter_map(|&(r, source, placement)| layer(r, source, placement, false)).collect();
    #[cfg(windows)]
    if let Some(first) = layers.first()
        && crate::host::host().is_some()
    {
        let (texture, state) = (first.texture.clone(), first.state);
        show(&texture.texture, state, Shown { frame, resource: shows.resource as usize, layers });
    } else if planes > 0
        && let resource = shows.resource
        && !resource.is_null()
        && let Some(texture) = crate::resource::host_resource(resource)
        && let Some(host) = crate::host::host()
    {
        const COPY_SOURCE: u32 = 0x800;
        let state = crate::resource::host_transition(resource, COPY_SOURCE)
            .map_or(COPY_SOURCE, |(_, before)| before);
        show(texture, state, Shown { frame, resource: resource as usize, layers: Vec::new() });
        crate::resource::host_transition(resource, state);
        let _ = host;
    }
    #[cfg(not(windows))]
    let _ = (planes, plane_parameters);
    called.end();
    #[cfg(all(windows, feature = "diagnostics"))]
    frame_ends();
    S_OK
}

/// Diagnostics: the title's present is over.
#[cfg(all(windows, feature = "diagnostics"))]
fn frame_ends() {
    crate::timeline::presented();
    crate::faultbook::frame();
    crate::ab::presented(|switch, was, will| {
        if switch == "VICTORY_ASYNC_SUBMIT" && was && !will {
            behind::leave();
        }
    });
}

/// The frame a present shows, and the title's resource it shows.
#[cfg(windows)]
struct Shown {
    frame: u64,
    resource: usize,
    /// The planes put together ([`composes`]); empty: the texture given
    /// beside this is copied whole, as before.
    layers: Vec<Layer>,
}

/// A plane of a present put together with the others (`Host::present_planes`):
/// its texture's host copy, the host state the lists leave it in
/// (`u32::MAX`: not known yet; the texture's own when it is shown), the part
/// of the texture shown (pixels: left, top, right, bottom) and where on the
/// picture (fractions of it, the same order); `None`: all of it.
#[cfg(windows)]
pub(crate) struct Layer {
    pub(crate) texture: std::sync::Arc<crate::resource::HostTexture>,
    pub(crate) state: u32,
    pub(crate) source: Option<[i32; 4]>,
    pub(crate) placement: Option<[f32; 4]>,
}

/// Shows `texture`, in host state `state`, in our window; or, with
/// `what.layers`, those put together (the texture is the first of them).
#[cfg(windows)]
fn show(texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource, state: u32, what: Shown) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let shown = if what.layers.is_empty() {
        host.present(texture, state, what.frame)
    } else {
        let mut layers = what.layers;
        for layer in &mut layers {
            // (Where its lists were not translated when the title
            // presented: the state they left it in.)
            if layer.state == u32::MAX {
                layer.state = layer.texture.state.load(Ordering::Relaxed);
            }
        }
        host.present_planes(&layers, what.frame)
    };
    #[cfg(feature = "diagnostics")]
    blinks(what.frame, what.resource as This, state, shown);
    #[cfg(not(feature = "diagnostics"))]
    let _ = (shown, what.resource);
}

/// Whether the planes' parameters are logged (`VICTORY_PRESENT_LOG`; on
/// unless `0`): those of the first presents, and of each present whose
/// number of planes changes or that shows a resource its plane has not shown
/// before (a few lines a run, at most [`LOGGED_MOST`] presents).
#[cfg(windows)]
fn present_log() -> bool {
    crate::toggled!("VICTORY_PRESENT_LOG")
}

/// The plane shown alone (`VICTORY_PRESENT_PLANE=<n>`; unset: the first,
/// put together with the others where [`present_planes`] says so).
#[cfg(windows)]
fn present_plane() -> Option<usize> {
    static PLANE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *PLANE.get_or_init(|| std::env::var("VICTORY_PRESENT_PLANE").ok()?.trim().parse().ok())
}

/// Whether the planes of a present are put together where there is more
/// than one, or where the one is not shown whole on the whole picture
/// ([`composes`]; on unless `VICTORY_PRESENT_PLANES=0`, which shows the
/// first plane's first resource whole, as before). One plane shown whole is
/// copied as before either way.
#[cfg(windows)]
fn present_planes() -> bool {
    crate::toggled!("VICTORY_PRESENT_PLANES")
}

/// Presents whose planes are logged, at most.
#[cfg(windows)]
const LOGGED_MOST: u64 = 48;

/// What a present shows, from its planes' parameters.
#[cfg(windows)]
struct Shows {
    /// The resource copied whole: the first of its plane's (null: none).
    resource: This,
    /// The planes put together ([`composes`]): each one's first resource,
    /// source and destination; empty: `resource` is copied.
    layers: Vec<(This, Option<[i32; 4]>, Option<[f32; 4]>)>,
}

/// What present `frame`, of `planes` planes described at `parameters`,
/// shows. With [`present_log`], [`present_plane`] and [`present_planes`] off,
/// the first plane's first resource, read as before.
#[cfg(windows)]
fn showing(planes: u32, parameters: *const Plane, frame: u64) -> Shows {
    let none = Shows { resource: std::ptr::null(), layers: Vec::new() };
    if planes == 0 || parameters.is_null() {
        return none;
    }
    if !present_log() && present_plane().is_none() && !present_planes() {
        // SAFETY: the title passes `planes` plane descriptions.
        let Some(plane) = (unsafe { parameters.as_ref() }) else {
            return none;
        };
        if plane.count == 0 || plane.resources.is_null() {
            return none;
        }
        // SAFETY: the plane lists `count` resources.
        return Shows { resource: unsafe { *plane.resources }, layers: Vec::new() };
    }
    let n = planes.min(MOST_PLANES) as usize;
    let bytes = title_bytes(parameters as usize, PLANE_BYTES.max(n * PLANE_STRIDE));
    let stride = plane_stride(&bytes, n);
    // Planes after the first only where the second's place was found: what
    // lies past the first is not taken for resources otherwise.
    let descs = planes_of(&bytes, if stride.is_some() { n } else { 1 }, stride.unwrap_or(PLANE_STRIDE));
    let first = descs.first().map_or(0, |d| first_resource(d, None));
    let seen: Vec<Seen> = descs
        .iter()
        .enumerate()
        .map(|(k, d)| seen_of(d, if k == 0 { first } else { first_resource(d, Some(first)) }))
        .collect();
    let alone = present_plane();
    let composed = alone.is_none() && present_planes() && composes(&seen);
    if present_log() {
        log_planes(frame, planes, stride, &descs, &seen, &bytes, composed, first);
    }
    let resource_of = |k: usize| seen.get(k).map_or(std::ptr::null(), |s| s.resource as This);
    if let Some(k) = alone {
        if k >= seen.len() {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(plane = k, planes, "VICTORY_PRESENT_PLANE names a plane the present does not have: the first is shown")
            });
        }
        return Shows { resource: resource_of(if k < seen.len() { k } else { 0 }), layers: Vec::new() };
    }
    if composed {
        let layers = seen.iter().filter(|s| s.resource != 0).map(|s| (s.resource as This, s.source, s.placement)).collect();
        return Shows { resource: resource_of(0), layers };
    }
    Shows { resource: resource_of(0), layers: Vec::new() }
}

/// The first resource plane `d` names (0: none). Past the first plane, only
/// an object with the function table of `like` (the first plane's first
/// resource): one of our resources.
#[cfg(windows)]
fn first_resource(d: &PlaneDesc, like: Option<usize>) -> usize {
    if d.count == 0 || d.resources == 0 {
        return 0;
    }
    let resource = title_u64(d.resources).unwrap_or(0) as usize;
    match like {
        Some(like) if !same_table(resource, like) => 0,
        _ => resource,
    }
}

/// Whether the objects at `a` and `b` have one function table (`b` being
/// one of our resources, `a` is one).
#[cfg(windows)]
fn same_table(a: usize, b: usize) -> bool {
    let table = |at: usize| (at != 0).then(|| title_u64(at)).flatten();
    table(a).is_some_and(|t| Some(t) == table(b))
}

/// What a present knows of plane `d`, whose first resource is `resource`.
#[cfg(windows)]
fn seen_of(d: &PlaneDesc, resource: usize) -> Seen {
    Seen {
        resource,
        size: (resource != 0).then(|| crate::resource::texture_kind(resource as This).map(|(w, h, _)| (w, h))).flatten(),
        source: title_rect(d.sources).map(|r| r.map(|v| v as i32)),
        placement: title_rect(d.placement).map(|r| r.map(f32::from_bits)),
    }
}

/// Plane `resource`'s host copy as a layer, in the state the lists recorded
/// so far leave it in, as for the texture shown alone (`thin`: lists not
/// translated yet leave it in none: the one it is in when the present's turn
/// in the line comes).
#[cfg(windows)]
fn layer(resource: This, source: Option<[i32; 4]>, placement: Option<[f32; 4]>, thin: bool) -> Option<Layer> {
    const COPY_SOURCE: u32 = 0x800;
    let texture = crate::resource::host_copy(resource)?;
    let state = crate::resource::host_transition(resource, COPY_SOURCE).map_or(COPY_SOURCE, |(_, before)| before);
    crate::resource::host_transition(resource, state);
    Some(Layer { texture, state: if thin { u32::MAX } else { state }, source, placement })
}

/// Logs present `frame`'s planes if it is due ([`present_log`]).
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn log_planes(
    frame: u64,
    planes: u32,
    stride: Option<usize>,
    descs: &[PlaneDesc],
    seen: &[Seen],
    bytes: &[u8],
    composed: bool,
    first: usize,
) {
    // Presents logged, the number of planes of the last present, the first
    // resources each plane has shown.
    type Logged = (u64, u32, Vec<std::collections::BTreeSet<usize>>);
    static LOGGED: std::sync::Mutex<Logged> = std::sync::Mutex::new((0, 0, Vec::new()));
    let mut logged = lock(&LOGGED);
    let (count, last, known) = &mut *logged;
    if *count >= LOGGED_MOST {
        return;
    }
    let fresh = seen.iter().enumerate().any(|(k, s)| known.get(k).is_none_or(|set| !set.contains(&s.resource)));
    let due = *count < 4 || planes != *last || fresh;
    *last = planes;
    known.resize_with(known.len().max(seen.len()), Default::default);
    for (k, s) in seen.iter().enumerate() {
        known[k].insert(s.resource);
    }
    if !due {
        return;
    }
    *count += 1;
    tracing::info!(
        "present planes={planes} frame={frame} stride={0} ({0:#x}, {1}) put together={composed} (VICTORY_PRESENT_PLANES) \
         shown alone={2:?} (VICTORY_PRESENT_PLANE){3}",
        stride.unwrap_or(PLANE_STRIDE),
        match (stride, planes) {
            (Some(_), _) => "found at the second plane's token",
            (None, 0 | 1) => "one plane",
            (None, _) => "not found: the planes after the first are left out",
        },
        present_plane(),
        if *count == LOGGED_MOST { "; the last present logged" } else { "" }
    );
    for (k, d) in descs.iter().enumerate() {
        let resources: Vec<String> = (0..d.count.min(MOST_PLANES) as usize)
            .map(|i| {
                let at = if d.resources == 0 { 0 } else { title_u64(d.resources + 8 * i).unwrap_or(0) as usize };
                // Only what is known to be a resource of ours is asked about.
                resource_text(at, (k, i) == (0, 0) || same_table(at, first))
            })
            .collect();
        let rect = |at: usize, floats: bool| match (at, title_rect(at)) {
            (0, _) => "none".to_owned(),
            (_, None) => format!("{at:#x} (not readable)"),
            (_, Some(r)) if floats => format!("{at:#x} {:?}", r.map(f32::from_bits)),
            (_, Some(r)) => format!("{at:#x} {:?}", r.map(|v| v as i32)),
        };
        tracing::info!(
            "present plane {k}: token {} resources {} at {:#x} [{}]; source rects {}; destination {}; colour space {}; \
             then {:?}; put together as {:?}",
            d.token,
            d.count,
            d.resources,
            resources.join(", "),
            rect(d.sources, false),
            rect(d.placement, true),
            d.colour_space,
            d.rest,
            seen.get(k)
        );
    }
    let hex: Vec<String> = bytes.iter().take(PLANE_BYTES).map(|b| format!("{b:02x}")).collect();
    tracing::info!("present plane parameters, {} bytes: {}", hex.len(), hex.join(" "));
}

/// A resource of a plane, for the log: its address and, if it is known to be
/// one of ours (`ours`), its size and format, whether it has a host copy (of
/// what size and format) and the texture it shows through a placed view
/// (`crate::resource::alias_of`).
#[cfg(windows)]
fn resource_text(at: usize, ours: bool) -> String {
    if at == 0 || !ours {
        return format!("{at:#x} (not looked at)");
    }
    let r = at as This;
    let kind = match crate::resource::texture_kind(r) {
        Some((w, h, f)) => format!("{w}x{h} f{f}"),
        None => "not a 2D texture of one sample".to_owned(),
    };
    let copy = match crate::resource::made_copy(r) {
        Some(c) => {
            // SAFETY: a live resource.
            let d = unsafe { c.texture.GetDesc() };
            format!("host copy made ({}x{} f{} state {:#x})", d.Width, d.Height, d.Format.0, c.state.load(Ordering::Relaxed))
        }
        None => "no host copy made".to_owned(),
    };
    format!("{at:#x} {kind}, {copy}, alias {:?}", crate::resource::alias_of(r))
}

/// Eight bytes of title memory at `at`, if they can be read.
#[cfg(windows)]
fn title_u64(at: usize) -> Option<u64> {
    u64_at(&title_bytes(at, 8), 0)
}

/// Sixteen bytes of title memory at `at` as four words (a rectangle), if
/// `at` is not null and they can be read.
#[cfg(windows)]
fn title_rect(at: usize) -> Option<[u32; 4]> {
    if at == 0 {
        return None;
    }
    let b = title_bytes(at, 16);
    Some([u32_at(&b, 0)?, u32_at(&b, 4)?, u32_at(&b, 8)?, u32_at(&b, 12)?])
}

/// `len` bytes of title memory from `at`; up to the end of its page if a page
/// after it cannot be read, nothing if that one cannot.
#[cfg(windows)]
fn title_bytes(at: usize, len: usize) -> Vec<u8> {
    let mut out = vec![0; len];
    if read_title(at, &mut out) {
        return out;
    }
    out.truncate((4096 - at % 4096).min(len));
    if read_title(at, &mut out) { out } else { Vec::new() }
}

/// Copies title memory from `at` into `out`: whether all of it could be read.
#[cfg(windows)]
fn read_title(at: usize, out: &mut [u8]) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    if at == 0 || out.is_empty() {
        return false;
    }
    if crate::safecopy::on() {
        // SAFETY: our buffer of that length; a page of the source that cannot
        // be read fails the copy.
        return unsafe { crate::safecopy::copy(out.as_mut_ptr(), at as *const u8, out.len()) };
    }
    let mut done = 0;
    // SAFETY: as above; the kernel checks the source.
    unsafe { ReadProcessMemory(GetCurrentProcess(), at as *const _, out.as_mut_ptr().cast(), out.len(), &mut done) != 0 }
}

/// Diagnostics: logs a presented frame that is black after one that was
/// not, with what was presented and how many draws ran since.
#[cfg(all(windows, feature = "diagnostics"))]
fn blinks(frame: u64, resource: This, state: u32, shown: Option<bool>) {
    static LAST: std::sync::Mutex<(bool, u64, u64)> = std::sync::Mutex::new((false, 0, 0));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let drawn = crate::draw::drawn();
    let black = shown.unwrap_or(false);
    let draws = crate::draw::take_frame();
    if black && !last.0 {
        last.2 += 1;
        tracing::warn!(
            frame,
            resource = ?resource,
            state = format_args!("{state:#x}"),
            draws = drawn - last.1,
            copied = shown.is_some(),
            black_frames = last.2,
            levels = ?crate::capture::recent_levels(),
            "a black frame after a frame that was not"
        );
        for d in draws {
            tracing::warn!("black frame {frame}: {d}");
        }
        AFTER.store(frame + 6, Ordering::Relaxed);
    }
    // Six frames on: the levels around it (a fade or a single drop).
    static AFTER: AtomicU64 = AtomicU64::new(0);
    if AFTER.load(Ordering::Relaxed) == frame {
        tracing::warn!(frame, levels = ?crate::capture::recent_levels(), "levels after the black frame");
    }
    *last = (black, drawn, last.2);
}

/// A run of destination pages: how they are mapped (0 pool pages from
/// `start` on, 1 pool page `start` each, 2 unmapped, 3 left as they are)
/// and how many.
#[repr(C)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

/// The ranges mapped from a destination address on.
#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

/// Bytes per mapped page.
const PAGE: u64 = 64 << 10;

/// `CopyPageMappingsBatchX(count, batches, pool, pool pages, flags)`: maps
/// pages of a pool (memory the title mapped physical pages into) at the
/// batches' addresses. Titles stream textures into pool pages and map them
/// into tiled heaps (including single-page broadcast fills and unmaps). Done
/// at once after earlier submissions complete.
extern "system" fn copy_page_mappings(
    _: This,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
    _flags: u32,
) -> HRESULT {
    let Some(map) = crate::resource::page_mapper() else {
        return E_INVALIDARG;
    };
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    let _called = crate::timeline::span("the title's CopyPageMappings");
    let batches = (count > 0).then(|| {
        // SAFETY: the title passes `count` batches.
        unsafe { std::slice::from_raw_parts(batches, count as usize) }
    });
    let mut ok = true;
    let many = crate::resource::many_page_mapper();
    let mut gathered: Vec<(u64, u64, u64)> = Vec::new();
    let mut map = |to: u64, from: u64, len: u64| match many {
        Some(_) => {
            gathered.push((to, from, len));
            true
        }
        None => map(to, from, len),
    };
    for b in batches.into_iter().flatten() {
        if b.ranges.is_null() {
            continue;
        }
        // SAFETY: each batch holds `count` ranges.
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            let len = u64::from(r.count) * PAGE;
            let inside = |pages: u32| r.start.checked_add(pages).is_some_and(|e| e <= pool_pages);
            let source = pool + u64::from(r.start) * PAGE;
            ok &= match r.kind {
                0 if inside(r.count) => map(at, source, len),
                1 if inside(1) => (0..u64::from(r.count)).all(|k| map(at + k * PAGE, source, PAGE)),
                2 => map(at, 0, len),
                3 => true,
                _ => false,
            };
            at += len;
        }
    }
    if let Some(many) = many {
        ok &= many(&gathered);
    }
    if !ok {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(pool = format_args!("{pool:#x}"), "a page mapping failed"));
    }
    S_OK
}

extern "system" fn desc(this: This, out: *mut QueueDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(queue(this).desc) };
    }
}

/// The queue methods by their slot in older titles' queues (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, queue 26 (12 as on PC,
/// then console ones).
const METHODS: [(usize, *const ()); 15] = [
    (0, query as *const ()),
    (2, release as *const ()),
    (7, object::parent_device as *const ()), // 0x38
    (10, execute as *const ()),              // 0x50
    (11, marker as *const ()),               // 0x58 set marker
    (12, marker as *const ()),               // 0x60 begin event
    (13, marker as *const ()),               // 0x68 end event
    (14, signal as *const ()),               // 0x70
    (15, wait as *const ()),                 // 0x78
    (16, timestamp_frequency as *const ()),  // 0x80
    (26, desc as *const ()),                 // 0xd0
    (29, copy_page_mappings as *const ()),   // 0xe8 page mapping batch
    (30, present as *const ()),              // 0xf0
    (33, execute_x as *const ()),            // 0x108
    (32, signal2_x as *const ()),            // 0x100
];

/// Newer titles' queues have two more methods before Signal, which moved
/// from 0x70 to 0x80: every slot from 0x70 on is two later (taken here as
/// starting at 0x70): Signal 0x80 `(fence, value)`, timestamp frequency 0x90,
/// suspend/resume 0xb8/0xc0, PresentX 0x100, and frame-token submission 0x118
/// `(count, lists, token, flags)`.
const fn newer_slot(slot: usize) -> usize {
    if slot >= 14 { slot + 2 } else { slot }
}

const fn queue_table(newer: bool) -> [*const (); 36] {
    let mut f = object::d3d12_object(object::missing_table::<QUEUE, 36>());
    let mut i = 0;
    while i < METHODS.len() {
        let (slot, method) = METHODS[i];
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    f
}

static FUNCTIONS: Table<36> = Table(queue_table(false));
static NEWER_FUNCTIONS: Table<36> = Table(queue_table(true));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    /// Where a queue's signal is: newer layouts have it two slots later.
    /// Another test may switch layouts while this one runs, so the queue's
    /// own table decides.
    fn signal_slot(q: This) -> usize {
        // SAFETY: one of our queues; its first field is the table.
        let table = unsafe { *q.cast::<*const *const ()>() };
        if table == NEWER_FUNCTIONS.0.as_ptr() {
            0x80
        } else {
            0x70
        }
    }

    #[test]
    fn queue_signals_complete_at_once() {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        // SAFETY: the title reads the type at +0x10.
        assert_eq!(unsafe { q.cast::<u8>().add(0x10).cast::<u32>().read() }, 0);
        let mut fence = null;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let signal = slot::<extern "system" fn(This, This, u64) -> HRESULT>(q, signal_slot(q));
        assert_eq!(signal(q, fence, 7), S_OK);
        assert_eq!(
            slot::<extern "system" fn(This) -> u64>(fence, 0x40)(fence),
            7
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    /// The title's compute queue: `{COMPUTE, 0, pipe 1, queue 0}`.
    #[test]
    fn console_queues_are_created_as_on_pc() {
        let null = std::ptr::null();
        let desc = QueueDescX {
            kind: 2,
            flags: 0,
            engine_or_pipe: 1,
            queue: 0,
        };
        let mut q = null;
        assert_eq!(create_x(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        assert_eq!(queue(q).desc.kind, 2);
        let none = std::ptr::null();
        assert_eq!(create_x(null, none, &QUEUE_IIDS[0], &mut q), E_INVALIDARG);
    }

    #[test]
    fn newer_queues_have_their_methods_two_slots_later() {
        let (old, new) = (queue_table(false), queue_table(true));
        assert_eq!(old[10], new[10]); // submission stays at 0x50
        assert_eq!(old[14], new[16]); // signal 0x70 → 0x80
        assert_eq!(old[30], new[32]); // present 0xf0 → 0x100
        assert_eq!(new[35], execute_x as *const ()); // 0x118
    }

    #[test]
    fn a_pair_of_timestamps_shows_the_load_it_is_given() {
        let (start, end) = (0x000d_0419_6698, 0x000d_0419_6690);
        assert_eq!(as_loaded(5_000_000, end, 4_000_000) - as_loaded(5_000_000, start, 4_000_000), 4_000_000);
        assert_eq!(as_loaded(5_000_000, start, 0), 5_000_000);
        assert_eq!(as_loaded(5, start, 100), 1);
    }

    /// Planes' parameters as the title lays them out: (token, resource count,
    /// resources, source rects, destination, colour space) each, `stride`
    /// bytes apart, in at least [`PLANE_BYTES`].
    fn plane_bytes(planes: &[(u64, u32, u64, u64, u64, u32)], stride: usize) -> Vec<u8> {
        let mut b = vec![0u8; PLANE_BYTES.max(stride * planes.len())];
        for (k, &(token, count, resources, sources, placement, colour)) in planes.iter().enumerate() {
            let at = k * stride;
            b[at..at + 8].copy_from_slice(&token.to_le_bytes());
            b[at + 8..at + 12].copy_from_slice(&count.to_le_bytes());
            b[at + 0x10..at + 0x18].copy_from_slice(&resources.to_le_bytes());
            b[at + 0x18..at + 0x20].copy_from_slice(&sources.to_le_bytes());
            b[at + 0x20..at + 0x28].copy_from_slice(&placement.to_le_bytes());
            b[at + 0x28..at + 0x2c].copy_from_slice(&colour.to_le_bytes());
        }
        b
    }

    fn desc(token: u64, count: u32, resources: usize, sources: usize, placement: usize) -> PlaneDesc {
        PlaneDesc { token, count, resources, sources, placement, ..Default::default() }
    }

    /// One plane: read where the title puts its fields; no second plane to
    /// tell the stride by.
    #[test]
    fn one_plane_is_read_where_the_title_puts_it() {
        let b = plane_bytes(&[(7, 1, 0x1000, 0x2000, 0x3000, 2)], PLANE_STRIDE);
        assert_eq!(plane_stride(&b, 1), None);
        let planes = planes_of(&b, 1, PLANE_STRIDE);
        assert_eq!(planes, [PlaneDesc { colour_space: 2, ..desc(7, 1, 0x1000, 0x2000, 0x3000) }]);
        // Without rectangles: null pointers.
        let b = plane_bytes(&[(7, 1, 0x1000, 0, 0, 0)], PLANE_STRIDE);
        assert_eq!(planes_of(&b, 1, PLANE_STRIDE), [desc(7, 1, 0x1000, 0, 0)]);
        // Too few bytes for a plane: none.
        assert!(planes_of(&b[..0x20], 1, PLANE_STRIDE).is_empty());
        assert_eq!(plane_stride(&[], 2), None);
    }

    /// Two planes: the second found where the token comes again with a
    /// resource, whatever the stride; a word that happens to equal the token
    /// is passed over, and planes past the bytes are left out.
    #[test]
    fn the_second_plane_is_found_by_its_token() {
        let two = [(3, 1, 0x1000, 0x2000, 0x3000, 0), (3, 1, 0x5000, 0x6000, 0x3000, 0)];
        let b = plane_bytes(&two, PLANE_STRIDE);
        assert_eq!(plane_stride(&b, 2), Some(0x48));
        let planes = planes_of(&b, 2, 0x48);
        assert_eq!(planes, [desc(3, 1, 0x1000, 0x2000, 0x3000), desc(3, 1, 0x5000, 0x6000, 0x3000)]);
        // Another layout, 0x40 apart.
        let b = plane_bytes(&two, 0x40);
        assert_eq!(plane_stride(&b, 2), Some(0x40));
        assert_eq!(planes_of(&b, 2, 0x40)[1], desc(3, 1, 0x5000, 0x6000, 0x3000));
        // Token 1, and words of the first plane that read 1 (with no count,
        // then with a count and no resources after it).
        let mut b = plane_bytes(&[(1, 1, 0x1000, 0, 0, 0), (1, 1, 0x5000, 0, 0, 0)], PLANE_STRIDE);
        b[0x30] = 1;
        assert_eq!(plane_stride(&b, 2), Some(0x48));
        b[0x38] = 1;
        assert_eq!(plane_stride(&b, 2), Some(0x48));
        // A second plane without a resource, or a third that is not there.
        let b = plane_bytes(&[(3, 1, 0x1000, 0, 0, 0), (3, 1, 0, 0, 0, 0)], PLANE_STRIDE);
        assert_eq!(plane_stride(&b, 2), None);
        let b = plane_bytes(&two, PLANE_STRIDE);
        assert_eq!(plane_stride(&b, 3), None);
        assert_eq!(planes_of(&b, 3, 0x48).len(), 2);
    }

    /// One plane shown whole on the whole picture is copied as before;
    /// anything else is put together.
    #[test]
    fn planes_are_put_together_only_where_a_copy_would_not_do() {
        let whole = Seen {
            resource: 0x10,
            size: Some((1920, 1080)),
            source: Some([0, 0, 1920, 1080]),
            placement: Some(WHOLE),
        };
        let none = Seen::default();
        assert!(!composes(&[]));
        assert!(!composes(&[whole]));
        assert!(!composes(&[Seen { source: None, placement: None, ..whole }]));
        assert!(!composes(&[whole, none]), "a second plane without a resource");
        assert!(composes(&[whole, Seen { resource: 0x20, ..whole }]));
        assert!(composes(&[none, whole]), "the one plane with a resource is not the first");
        assert!(composes(&[Seen { source: Some([0, 0, 1600, 900]), ..whole }]));
        assert!(composes(&[Seen { placement: Some([0.0, 0.0, 0.5, 0.5]), ..whole }]));
        // What is not a source or a destination counts as the whole; a
        // texture of no known size is taken as shown whole.
        assert!(!composes(&[Seen { source: Some([0, 0, 4000, 1080]), ..whole }]));
        assert!(!composes(&[Seen { placement: Some([f32::NAN, 0.0, 1.0, 1.0]), ..whole }]));
        assert!(!composes(&[Seen { placement: Some([0.0, 0.0, 2.0, 1.0]), ..whole }]));
        assert!(!composes(&[Seen { size: None, source: Some([0, 0, 1600, 900]), ..whole }]));
        assert!(placed(WHOLE) && !placed([0.5, 0.0, 0.5, 1.0]));
        assert!(inside([0, 0, 1920, 1080], (1920, 1080)) && !inside([0, 0, 1921, 1080], (1920, 1080)));
        assert!(!inside([i32::MIN, 0, 10, 10], (1920, 1080)));
    }

    #[test]
    fn timestamps_and_values_are_written() {
        let mut at = [0u64; 2];
        Written::Value(0x1_0000_0002).write(at.as_mut_ptr().cast(), 4);
        assert_eq!(at[0], 2);
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        let first = at[1];
        std::thread::sleep(std::time::Duration::from_millis(2));
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        assert!(at[1] > first);
    }

    /// The lists a split submission ran at once give their tables back,
    /// except one named again among those held, which runs again (after
    /// fxceless #1320, af23c65, which has no test of its own).
    #[cfg(windows)]
    #[test]
    fn lists_run_at_once_and_not_held_give_their_tables_back() {
        assert_eq!(ran_once(&[1, 2, 3, 4], 2), [1, 2]);
        assert_eq!(ran_once(&[1, 2, 3, 1], 2), [2]);
        // Named twice among those run: both times (giving back is once).
        assert_eq!(ran_once(&[5, 5, 6], 2), [5, 5]);
        assert_eq!(ran_once(&[1, 2], 2), [1, 2]);
        assert!(ran_once(&[1, 2], 0).is_empty());
        assert!(ran_once::<u8>(&[], 3).is_empty());
    }
}
