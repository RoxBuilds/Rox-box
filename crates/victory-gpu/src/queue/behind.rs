//! Submissions prepared and sent by a thread of ours, in order, instead of by
//! the thread of the title that submits them (`VICTORY_ASYNC_SUBMIT=1`).
//!
//! The idea and the argument are fxceless/victory-fork's, commit f85101b6
//! (GPL-3.0-or-later, as this): a profile had the title's render thread inside
//! `queue::execute` for 202 of 292 ms of a frame while its recording threads
//! and the card stood idle, and the title does nothing with the result of that
//! work: it only has to be done before the lists run, in order. (Here most of
//! a frame went in the submit calls, the card idle behind them for half of
//! it.) What differs from that commit: one line for all queues, kept
//! in the order of the title's calls; the present stands in it too; nobody but
//! our thread runs what is in it.
//!
//! With the switch a submission's lists are taken from the title's lists (as
//! they are for every flight, `crate::list::detach`) and put in the [`Lane`],
//! with what they wait for, the queues' fence waits and signals, and the
//! presents. [`Lane::serve`], on a thread of ours, takes from it whatever is
//! free to run, the oldest first; within a queue the order is the title's,
//! always. What tells the title that work is done (fence values,
//! bottom-of-pipe writes) is told by the flights as before, when the card has
//! run them: later than the title's call by more than before, never earlier.
//!
//! What the title may do between its submit call and the moment our thread
//! prepares the lists, and why that is all right:
//!
//! - Reset or release the lists and allocators: their host sides, upload
//!   memory and draw state were taken out of them.
//! - Write upload memory, descriptors, constants for later lists: those of
//!   lists submitted are the card's to read until a fence behind them is
//!   reached, on the console as here; no fence behind them is reached before
//!   the lists have run ([`signal`] goes into the line behind them).
//! - Signal, wait, present on the queue: in the line behind them.
//! - Write a fence or a value in memory by the CPU: at once; our thread looks
//!   at what it stands at again ([`wake`], and every millisecond).
//! - Map pages (`CopyPageMappingsBatchX`): at once, as before, where the lists
//!   on the card may be reading them too; the bytes of uploads were read when
//!   the lists were recorded.
//! - Ask for something that has to see earlier submissions done (a read-back,
//!   a fence waited for without an event): [`drain`] first.
//! - Release a texture or a view's memory: what is retired waits for what is
//!   in the line too ([`queued`]).
//!
//! The line is bounded: a present returns when the one before it was shown
//! ([`frames_most`]), and a submission when at most [`runs_most`] are in the
//! line.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed, Ordering::SeqCst};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::This;

/// One thing in the line: what it stands at, if it does, and what it does.
pub(super) trait Work: Send {
    /// What has to be met before it goes.
    type Ask;
    fn ask(&self) -> Option<Self::Ask>;
    fn met(ask: &Self::Ask) -> bool;
    /// Whether it is a present: everything before it goes first, met or not.
    fn shows(&self) -> bool;
    /// Whether it is a submission.
    fn runs(&self) -> bool;
    /// Whether it stands for other work, which takes its place in the line
    /// once it is free to go ([`Work::unfold`]).
    fn folded(&self) -> bool {
        false
    }
    fn unfold(self) -> Vec<Self>
    where
        Self: Sized,
    {
        vec![self]
    }
    /// Waits until a folded one is free to go.
    fn settle(_: &Self::Ask) {}
    /// Takes `next`, what its queue was given behind it, along to be done with
    /// it; or gives it back. Whether it is a submission that was taken.
    fn take_along(&mut self, next: Self) -> Result<bool, Self>
    where
        Self: Sized,
    {
        Err(next)
    }
    /// Does it. `forced`: its wait was not met.
    fn go(self, forced: bool);
    /// A mark taken when an entry that runs is put in the line (by the thread
    /// that puts it, before the line's lock): a round of the heaps' records
    /// that begins after that (`crate::heap_marks`). 0: none.
    fn mark_now() -> u64 {
        0
    }
    /// A mark taken when a wait goes or is taken along (on our thread): a
    /// round that begins after.
    fn mark_later() -> u64 {
        0
    }
    /// Whether what its queue has behind this wait may have been written only
    /// just before the wait was met (by the CPU, which writes, then meets the
    /// wait): it was met in memory.
    fn raises(&self) -> bool {
        true
    }
    /// Told just before [`Work::go`] of a submission: the highest mark of what
    /// it is made of and of the waits its queue went past.
    fn marked(&mut self, _mark: u64) {}
}

struct Entry<W> {
    number: u64,
    queue: usize,
    what: W,
    /// [`Work::mark_now`] when it was put in the line.
    mark: u64,
}

struct State<W> {
    /// In the order of the title's calls, whatever queue they were made on.
    items: VecDeque<Entry<W>>,
    /// The number the next entry gets.
    next: u64,
    /// The queue whose entry is being done or unfolded now (it is no longer
    /// in `items`; what that queue is given stays behind it all the same).
    running: Option<usize>,
    /// Times a look at the line was asked for (with the lock; [`Lane::wake`]
    /// counts its own, [`Lane::asked`] adds them), and the last of them that
    /// a whole look has followed.
    asked: u64,
    served: u64,
    /// Presents and submissions in the line or being done.
    presents: usize,
    runs: usize,
    /// Per queue, the highest [`Work::mark_later`] of the waits it went past:
    /// what is behind a wait may have been written only before the wait was
    /// met, not before it was put in the line.
    floors: Vec<(usize, u64)>,
}

impl<W> State<W> {
    fn floor(&self, queue: usize) -> u64 {
        self.floors.iter().find(|f| f.0 == queue).map_or(0, |f| f.1)
    }

    fn raise(&mut self, queue: usize, mark: u64) {
        match self.floors.iter_mut().find(|f| f.0 == queue) {
            Some(f) => f.1 = f.1.max(mark),
            None => self.floors.push((queue, mark)),
        }
    }
}

/// See the module's text.
pub(super) struct Lane<W: Work> {
    state: Mutex<State<W>>,
    /// Something to look at.
    work: Condvar,
    /// Something was done, or a look at the line ended.
    moved: Condvar,
    /// Looks asked for by [`Lane::wake`] without the lock.
    woken: AtomicU64,
    /// Our thread waits with nothing in the line ([`Lane::serve`]): a wake
    /// need not tell it.
    idle: AtomicBool,
    /// Whether the lock is held while an entry unfolds and taken by a wake
    /// (`None`: as [`unfold_locked`] says; tests ask for either).
    locked: Option<bool>,
}

impl<W: Work> Lane<W> {
    pub(super) const fn new() -> Self {
        Self::unfolding(None)
    }

    const fn unfolding(locked: Option<bool>) -> Self {
        Lane {
            state: Mutex::new(State {
                items: VecDeque::new(),
                next: 0,
                running: None,
                asked: 0,
                served: 0,
                presents: 0,
                runs: 0,
                floors: Vec::new(),
            }),
            work: Condvar::new(),
            moved: Condvar::new(),
            woken: AtomicU64::new(0),
            idle: AtomicBool::new(false),
            locked,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<W>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether an entry is unfolded with the lock held, and a wake takes it,
    /// as before ([`unfold_locked`]).
    fn unfold_locked(&self) -> bool {
        self.locked.unwrap_or_else(unfold_locked)
    }

    /// The looks asked for so far: those counted with the lock and the
    /// wakes. (Both only grow, and a look reads them with the lock held, so a
    /// look begun after a count was read sees at least that count.)
    fn asked(&self, state: &State<W>) -> u64 {
        state.asked + self.woken.load(SeqCst)
    }

    fn put(state: &mut State<W>, queue: usize, what: W, mark: u64) {
        state.presents += usize::from(what.shows());
        state.runs += usize::from(what.runs());
        let number = state.next;
        state.next += 1;
        state.items.push_back(Entry { number, queue, what, mark });
    }

    /// Adds to the end of the line, and has it looked at.
    pub(super) fn push(&self, queue: usize, what: impl IntoIterator<Item = W>) {
        let what: Vec<W> = what.into_iter().collect();
        let mark = if what.iter().any(W::runs) { W::mark_now() } else { 0 };
        let mut state = self.lock();
        for what in what {
            Self::put(&mut state, queue, what, mark);
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
    }

    /// Adds `what` if `queue` has anything in the line or being done (or
    /// `always`); else does `now`, with nothing of the queue's able to come
    /// in between.
    pub(super) fn push_behind(&self, queue: usize, always: bool, what: W, now: impl FnOnce()) {
        let mut state = self.lock();
        if always || state.running == Some(queue) || state.items.iter().any(|e| e.queue == queue) {
            Self::put(&mut state, queue, what, 0);
        } else {
            now();
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
    }

    /// Has the line looked at: something an entry stands at may have moved.
    /// A count and a notify, without the lock ([`unfold_locked`]: with it, as
    /// before). Nothing is lost so: a wake during a look has the next look
    /// follow it; one that comes between our thread's last reading of the
    /// count and its wait is seen when that wait ends, within the millisecond
    /// our thread waits at most while anything is in the line; and what was
    /// done before the wake (a recording translated, a fence moved) is seen
    /// by the look that reads the count. While our thread waits with nothing
    /// in the line the notify is left out, as the lock's wake did: nothing
    /// stands at anything then, and what is put in the line has a look by
    /// itself. (The count and that flag are read and written in one order for
    /// all threads, so a wake that finds the flag set as our thread wakes is
    /// in the count it reads next.)
    pub(super) fn wake(&self) {
        if self.unfold_locked() {
            let mut state = self.lock();
            if state.items.is_empty() {
                return;
            }
            state.asked += 1;
        } else {
            self.woken.fetch_add(1, SeqCst);
            if self.idle.load(SeqCst) {
                return;
            }
        }
        self.work.notify_one();
    }

    /// Waits until everything in the line that is free to run has been done
    /// (what comes in after this call may not be).
    pub(super) fn drain(&self) {
        let folded: Vec<W::Ask> =
            self.lock().items.iter().filter(|e| e.what.folded()).filter_map(|e| e.what.ask()).collect();
        folded.iter().for_each(W::settle);
        let mut state = self.lock();
        if state.items.is_empty() && state.running.is_none() {
            return;
        }
        state.asked += 1;
        let ticket = self.asked(&state);
        self.work.notify_one();
        while state.served < ticket {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits until at most `most` presents are in the line.
    pub(super) fn wait_presents(&self, most: usize) {
        let mut state = self.lock();
        while state.presents > most {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits until at most `most` submissions are in the line, or what is
    /// left of it stands at something.
    pub(super) fn wait_runs(&self, most: usize) {
        let mut state = self.lock();
        while state.runs > most && state.served < self.asked(&state) {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Looks at the line for as long as the process runs.
    fn serve(&self) -> ! {
        loop {
            let (asked, limit) = {
                let mut state = self.lock();
                loop {
                    let asked = self.asked(&state);
                    if asked != state.served {
                        break (asked, state.next);
                    }
                    if state.items.is_empty() {
                        self.idle.store(true, SeqCst);
                        state = self.work.wait(state).unwrap_or_else(|e| e.into_inner());
                        self.idle.store(false, SeqCst);
                    } else {
                        // It stands at something the CPU may write without
                        // a call of ours (or a wake came between the reading
                        // of the count and this wait).
                        let standing = Instant::now();
                        let (back, out) =
                            self.work.wait_timeout(state, Duration::from_millis(1)).unwrap_or_else(|e| e.into_inner());
                        state = back;
                        STATS[STOOD].fetch_add(standing.elapsed().as_micros() as u64, Relaxed);
                        if out.timed_out() {
                            STATS[POLLED].fetch_add(1, Relaxed);
                            break (self.asked(&state), state.next);
                        }
                    }
                }
            };
            self.look(limit);
            let mut state = self.lock();
            state.served = state.served.max(asked);
            drop(state);
            self.moved.notify_all();
        }
    }

    /// Does what is free to run of the entries before number `limit`.
    pub(super) fn look(&self, limit: u64) {
        while self.step(limit) {}
    }

    /// The parts of `entry`, which stands for them and was taken out of the
    /// line, put in its place: at `at`, where it was, or without one before
    /// the first entry of a higher number. Returns with the lock held.
    ///
    /// The lock is free while it unfolds ([`unfold_locked`]: held, as
    /// before). The place is the one there was before all the same: what
    /// comes in meanwhile goes to the end with higher numbers, and nobody else
    /// takes anything out. Meanwhile `running` names its queue, so what the
    /// queue is given goes behind it (`push_behind`), and a drain waits for
    /// a look that finds the parts.
    fn unfold(&self, at: Option<usize>, entry: Entry<W>) -> MutexGuard<'_, State<W>> {
        let Entry { number, queue, what, mark } = entry;
        let runs = usize::from(what.runs());
        let (mut state, parts) = if self.unfold_locked() {
            let state = self.lock();
            (state, what.unfold())
        } else {
            let parts = what.unfold();
            (self.lock(), parts)
        };
        let at = at.unwrap_or_else(|| state.items.iter().position(|e| e.number > number).unwrap_or(state.items.len()));
        state.runs -= runs;
        state.runs += parts.iter().filter(|p| p.runs()).count();
        for (i, what) in parts.into_iter().enumerate() {
            state.items.insert(at + i, Entry { number, queue, what, mark });
        }
        state
    }

    /// Does one entry: the oldest that is the first of its queue and free to
    /// run. Whether there was one. Only one thread calls this.
    fn step(&self, limit: u64) -> bool {
        // Each queue's first entry, oldest first.
        let mut heads: Vec<(u64, usize, Option<W::Ask>, bool)> = Vec::new();
        // Whether one of them stands for others not known yet.
        let mut folded = false;
        {
            let state = self.lock();
            for e in state.items.iter().take_while(|e| e.number < limit) {
                if !heads.iter().any(|h| h.1 == e.queue) {
                    heads.push((e.number, e.queue, e.what.ask(), e.what.shows()));
                    folded |= e.what.folded();
                }
            }
        }
        let Some(&(first, ..)) = heads.first() else {
            return false;
        };
        let ready = heads.iter().find(|(number, _, ask, shows)| match ask {
            Some(ask) => W::met(ask),
            // A present comes behind everything given before it.
            None => !shows || *number == first || !super::forces(),
        });
        let (number, forced) = match ready {
            Some(head) => (head.0, false),
            None => {
                // Nothing is free to run. With a present in the line the
                // oldest goes on regardless: a wait is not kept across
                // frames (`super::present`). (Not while there is work whose
                // parts are not known: it may be what is waited for.)
                if folded {
                    return false;
                }
                let state = self.lock();
                if !super::forces() || !state.items.iter().any(|e| e.number < limit && e.what.shows()) {
                    return false;
                }
                (first, true)
            }
        };
        let mut entry = {
            let mut state = self.lock();
            let Some(at) = state.items.iter().position(|e| e.number == number) else {
                return false;
            };
            let Some(entry) = state.items.remove(at) else {
                return false;
            };
            state.running = Some(entry.queue);
            entry
        };
        let (shows, mut runs) = (entry.what.shows(), usize::from(entry.what.runs()));
        // The highest mark of what it is made of (`crate::heap_marks`).
        let mut mark = entry.mark;
        if entry.what.ask().is_some() && !entry.what.folded() && entry.what.raises() {
            // A wait that goes: what its queue has behind it was written
            // before it was met, which is now at the latest.
            let later = W::mark_later();
            self.lock().raise(entry.queue, later);
        }
        if entry.what.folded() {
            // Its parts take its place: behind everything before it, before
            // everything after it (they share its number; the first of them
            // is found first). Its queue stays `running` until they are in.
            let mut state = self.unfold(None, entry);
            state.running = None;
            drop(state);
            self.moved.notify_all();
            return true;
        }
        // What follows it in its queue, if that can go with it. (Nobody else
        // takes anything out, and what comes in goes to the end.)
        while runs > 0 && !forced {
            let next = {
                let mut state = self.lock();
                let at = state.items.iter().position(|e| e.queue == entry.queue && e.number < limit);
                at.and_then(|at| Some((at, state.items.remove(at)?)))
            };
            let Some((at, next)) = next else {
                break;
            };
            // (Back where it was: parts of one submission share a number. Its
            // queue is `running` meanwhile: it is the queue of `entry`.)
            if next.what.folded() {
                if next.what.ask().is_none_or(|ask| W::met(&ask)) {
                    drop(self.unfold(Some(at), next));
                    continue;
                }
                self.lock().items.insert(at, next);
                break;
            }
            let Entry { number, queue, what, mark: theirs } = next;
            let waits = what.ask().is_some() && what.raises();
            match entry.what.take_along(what) {
                Ok(run) => {
                    runs += usize::from(run);
                    mark = mark.max(theirs);
                    if waits {
                        let later = W::mark_later();
                        self.lock().raise(queue, later);
                    }
                }
                Err(what) => {
                    self.lock().items.insert(at, Entry { number, queue, what, mark: theirs });
                    break;
                }
            }
        }
        if runs > 0 {
            let floor = self.lock().floor(entry.queue);
            entry.what.marked(mark.max(floor));
        }
        entry.what.go(forced);
        {
            let mut state = self.lock();
            state.running = None;
            state.presents -= usize::from(shows);
            state.runs -= runs;
        }
        self.moved.notify_all();
        true
    }
}

type Recordings = std::sync::Arc<[std::sync::Arc<crate::list::thin::Recording>]>;

/// What the title's queues put in the line.
enum Job {
    /// The queue waits for the fence (its address) to reach the value.
    Wait(usize, u64),
    /// A part of a submission waits for a memory value.
    Value(crate::list::ValueWait),
    /// A submission, or a part of one: the lists' host sides and the order
    /// they run in.
    Run(usize, Vec<crate::list::Detached>, Vec<usize>),
    /// A submission whose lists are being translated ([`crate::list::thin`]):
    /// the parts of it are known when they are.
    Lists(usize, Recordings),
    /// Parts taken along with a submission ([`crate::host::joins`]): where they
    /// begin in the order, and the signals given behind them.
    Joined(usize, Vec<crate::list::Detached>, Vec<usize>, Vec<usize>, Vec<(usize, u64)>),
    /// A signal of the queue's.
    Signal(usize, u64),
    /// A frame is shown: the texture and the state its lists leave it in.
    Present(Option<(std::sync::Arc<crate::resource::HostTexture>, u32)>, super::Shown),
}

// SAFETY: D3D12 objects are free-threaded; a job is touched by whoever put it
// in the line and then, once it is taken out, by our thread alone (the flights
// it becomes are handed to other threads the same way).
unsafe impl Send for Job {}

enum Ask {
    Wait(usize, u64),
    Value(crate::list::ValueWait),
    Lists(Recordings),
}

impl Work for Job {
    type Ask = Ask;

    fn ask(&self) -> Option<Ask> {
        match self {
            &Job::Wait(fence, value) => Some(Ask::Wait(fence, value)),
            Job::Value(wait) => Some(Ask::Value(*wait)),
            Job::Lists(_, lists) => Some(Ask::Lists(lists.clone())),
            _ => None,
        }
    }

    fn folded(&self) -> bool {
        matches!(self, Job::Lists(..))
    }

    fn unfold(self) -> Vec<Self> {
        let Job::Lists(queue, recordings) = self else {
            return vec![self];
        };
        let began = Instant::now();
        let lists: Vec<This> = recordings.iter().map(|r| r.back()).collect();
        let jobs = jobs_of(queue as This, &lists);
        STATS[UNFOLDING].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
        // It stood for one submission not begun.
        let runs = jobs.iter().filter(|j| j.runs()).count() as u64;
        QUEUED.fetch_add(runs, Relaxed);
        QUEUED.fetch_sub(1, Relaxed);
        STATS[RUNS].fetch_add(runs, Relaxed);
        jobs
    }

    fn settle(ask: &Ask) {
        if let Ask::Lists(recordings) = ask {
            recordings.iter().for_each(|r| r.wait());
        }
    }

    fn met(ask: &Ask) -> bool {
        match ask {
            &Ask::Wait(fence, value) => crate::fence::completed(fence as This) >= value,
            Ask::Value(wait) => super::value_met(wait),
            Ask::Lists(recordings) => recordings.iter().all(|r| r.done()),
        }
    }

    fn shows(&self) -> bool {
        matches!(self, Job::Present(..))
    }

    fn runs(&self) -> bool {
        matches!(self, Job::Run(..) | Job::Joined(..) | Job::Lists(..))
    }

    fn take_along(&mut self, next: Job) -> Result<bool, Job> {
        let (most, most_lists) = crate::host::joins();
        if most < 2 {
            return Err(next);
        }
        // Nothing but waits and signals that a submission may follow.
        let (count, lists, signals) = match self {
            Job::Run(_, lists, order) => (1, (&*lists, &*order), &[][..]),
            Job::Joined(_, lists, order, parts, signals) => (1 + parts.len(), (&*lists, &*order), &signals[..]),
            _ => return Err(next),
        };
        let hosts = || lists.1.iter().map(|&i| lists.0[i].host());
        match &next {
            Job::Value(wait) => {
                let met = wait.met()
                    || crate::host::late_met_known(wait) == Some(true)
                    || hosts().any(|list| crate::host::will_meet(list, wait));
                if !met {
                    return Err(next);
                }
            }
            &Job::Wait(fence, value) => {
                if crate::fence::completed(fence as This) < value && !signals.iter().any(|&(f, v)| f == fence && v >= value) {
                    return Err(next);
                }
            }
            Job::Signal(..) => {}
            Job::Run(_, more, order) => {
                let runs: Vec<&crate::host::HostList> = order.iter().map(|&i| more[i].host()).collect();
                if count >= most || lists.1.len() + runs.len() > most_lists || !crate::host::joinable(hosts(), &runs) {
                    return Err(next);
                }
            }
            Job::Joined(..) | Job::Present(..) | Job::Lists(..) => return Err(next),
        }
        let taken = std::mem::replace(self, Job::Wait(0, 0));
        let (queue, mut lists, mut order, mut parts, mut signals) = match taken {
            Job::Run(queue, lists, order) => (queue, lists, order, Vec::new(), Vec::new()),
            Job::Joined(queue, lists, order, parts, signals) => (queue, lists, order, parts, signals),
            other => {
                *self = other;
                return Err(next);
            }
        };
        let run = matches!(next, Job::Run(..));
        match next {
            Job::Run(_, more, then) => {
                parts.push(order.len());
                order.extend(then.into_iter().map(|i| i + lists.len()));
                lists.extend(more);
                STATS[JOINED].fetch_add(1, Relaxed);
            }
            Job::Signal(fence, value) => signals.push((fence, value)),
            _ => {}
        }
        *self = Job::Joined(queue, lists, order, parts, signals);
        Ok(run)
    }

    fn mark_now() -> u64 {
        if crate::heap_marks::on() { crate::descriptor::round_after_now() } else { 0 }
    }

    fn mark_later() -> u64 {
        // (Asked for now: the round runs while the line does what is before
        // the part behind the wait.)
        if crate::heap_marks::on() { crate::descriptor::round_after_now() } else { 0 }
    }

    fn raises(&self) -> bool {
        // (Without the switch no mark is taken: the memory is not read.)
        if !crate::heap_marks::on() {
            return true;
        }
        match self {
            Job::Value(wait) => wait.met(),
            _ => true,
        }
    }

    fn marked(&mut self, mark: u64) {
        crate::heap_marks::set_mark(mark);
    }

    fn go(self, forced: bool) {
        let began = Instant::now();
        let _doing = crate::timeline::span(match self {
            Job::Run(..) => "the line: a submission",
            Job::Joined(..) => "the line: submissions joined",
            Job::Present(..) => "the line: a present",
            Job::Signal(..) => "the line: a signal",
            _ => "the line: a wait that is met",
        });
        if forced {
            STATS[FORCED].fetch_add(1, Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Forced, 1);
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("a queue still stood at a wait at a present: it was not met"));
        }
        match self {
            // (A folded one is unfolded, never done.)
            Job::Wait(..) | Job::Value(..) | Job::Lists(..) => {}
            Job::Run(queue, lists, order) => {
                let _ = queue;
                #[cfg(feature = "diagnostics")]
                crate::capture::op(queue, || format!("RUN submission of {} lists (its thread's)", order.len()));
                match crate::host::host() {
                    Some(host) => host.execute_late(lists, order),
                    None => drop(lists),
                }
                QUEUED.fetch_sub(1, Relaxed);
            }
            Job::Joined(queue, lists, order, parts, signals) => {
                let _ = queue;
                #[cfg(feature = "diagnostics")]
                crate::capture::op(queue, || {
                    format!("RUN {} parts of submissions in one, {} lists (its thread's)", 1 + parts.len(), order.len())
                });
                match crate::host::host() {
                    Some(host) => host.execute_parts(lists, order, &parts),
                    None => drop(lists),
                }
                // (A part that had no list is one all the same.)
                QUEUED.fetch_sub(1 + parts.len() as u64, Relaxed);
                for (fence, value) in signals {
                    if !crate::host::late_signal(fence, value) {
                        crate::fence::set(fence as This, value);
                    }
                }
            }
            Job::Signal(fence, value) => {
                if !crate::host::late_signal(fence, value) {
                    crate::fence::set(fence as This, value);
                }
            }
            Job::Present(texture, shown) => {
                if let Some((texture, state)) = texture {
                    // (Where its lists were not translated when the title
                    // presented: the state they left it in.)
                    let state = if state == u32::MAX { texture.state.load(Relaxed) } else { state };
                    super::show(&texture.texture, state, shown);
                }
                QUEUED.fetch_sub(1, Relaxed);
            }
        }
        // (The mark was this submission's alone.)
        crate::heap_marks::set_mark(0);
        STATS[BUSY].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
}

static LANE: Lane<Job> = Lane::new();

/// Submissions and presents in the line that have not been given to the host
/// yet.
static QUEUED: AtomicU64 = AtomicU64::new(0);

/// Since the last frame-rate line: submissions put in the line, presents, the
/// times they found things in it (the sum of their numbers) and the most; what
/// threads of the title waited, in microseconds, for room in the line, at a
/// present, and for a drain, and the drains; what our thread worked; waits
/// not met at a present; parts taken along; and what our thread took to
/// unfold submissions whose lists were translated; and the looks our thread
/// took when its wait of a millisecond ran out with work standing in the line
/// (nothing woke it), with the microseconds it stood so.
static STATS: [AtomicU64; 14] = [const { AtomicU64::new(0) }; 14];
const RUNS: usize = 0;
const PRESENTS: usize = 1;
const DEPTH: usize = 2;
const DEEPEST: usize = 3;
const ROOM: usize = 4;
const AT_PRESENT: usize = 5;
const DRAINING: usize = 6;
const DRAINS: usize = 7;
const BUSY: usize = 8;
const FORCED: usize = 9;
/// Parts prepared together with the one before them.
const JOINED: usize = 10;
/// Microseconds of [`Job::unfold`] (`crate::list::detach`): with
/// [`unfold_locked`] all of it with the line's lock held.
const UNFOLDING: usize = 11;
/// Looks after a millisecond's wait with work standing that no wake ended,
/// and the microseconds of all waits with work standing.
const POLLED: usize = 12;
const STOOD: usize = 13;

/// How many submissions may be in the line when a submit call returns
/// (`VICTORY_SUBMIT_DEPTH`; 64: about two frames of the title's).
fn runs_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| number("VICTORY_SUBMIT_DEPTH").unwrap_or(64).max(1))
}

/// How many presents may be in the line when a present call returns
/// (`VICTORY_SUBMIT_FRAMES`; 1: the frame before is sent, this one may not be;
/// 0: both are).
fn frames_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| number("VICTORY_SUBMIT_FRAMES").unwrap_or(1))
}

fn number(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// Whether our thread holds the line's lock while it unfolds a submission
/// whose lists were translated, and a [`wake`] takes the lock
/// (`VICTORY_UNFOLD_LOCKED=1`: both, as before; read once). Unfolding is
/// `crate::list::detach`, which closes host lists and, without lazy host
/// lists, makes new ones (`Host::new_list`): that held the lock a tenth of
/// every frame, and meanwhile every wake from a finished translation and every
/// push, signal, wait and
/// present of the title's threads stood at it. Now the lock is free while a
/// submission unfolds ([`Lane::unfold`]), and a wake is a count and a notify
/// ([`Lane::wake`]); what runs, and in what order, is the same.
fn unfold_locked() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_UNFOLD_LOCKED").is_some_and(|v| v == "1");
        tracing::info!(on, "the line's lock held while a submission unfolds and at wakes (VICTORY_UNFOLD_LOCKED)");
        on
    })
}

thread_local! {
    /// This is the thread that does what is in the line.
    static OURS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether submissions are prepared by a thread of ours
/// (`VICTORY_ASYNC_SUBMIT=1`; needs flights with the present sent ahead, as
/// the relay has them, and lists cut at their waits).
pub(super) fn on() -> bool {
    // Diagnostics: on and off while the title runs (`crate::ab`).
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_ASYNC_SUBMIT") {
        return on && started();
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        (std::env::var_os("VICTORY_ASYNC_SUBMIT").is_some_and(|v| v == "1") || crate::list::thin::wanted()) && started()
    })
}

/// The line is about to be left for the way without it: everything in it is
/// done first. (At a present, by the title's thread that submits.)
#[cfg(feature = "diagnostics")]
pub(super) fn leave() {
    LANE.wait_presents(0);
    LANE.drain();
}

/// Starts the thread, once: whether there is one.
fn started() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        if !crate::host::late_free_present() || !super::cuts() {
            tracing::warn!("VICTORY_ASYNC_SUBMIT=1 needs the relay and lists cut at their waits: submissions are prepared by the thread that makes them");
            return false;
        }
        let worker = || {
            use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
            // SAFETY: this thread's own priority.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
            crate::cores::keep_on_fast_cores("victory-submit");
            crate::descriptor::host::no_slot_waits_on_this_thread();
            OURS.set(true);
            let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| LANE.serve()));
            if ended.is_err() {
                // Nothing would ever be sent again, and the title's thread
                // would stand at its next present for good.
                crate::host::fail(format_args!("the thread that prepares and sends submissions panicked"));
            }
        };
        match std::thread::Builder::new().name("victory-submit".into()).spawn(worker) {
            Ok(_) => {
                tracing::info!(
                    frames = frames_most(),
                    runs = runs_most(),
                    "submissions are prepared and sent by a thread of ours, not by the thread that submits (VICTORY_ASYNC_SUBMIT=1)"
                );
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no thread to prepare submissions: the threads that make them do");
                false
            }
        }
    })
}

/// Submissions in the line that the host has not begun
/// (`crate::host::retire` waits for them too).
pub(super) fn queued() -> u64 {
    QUEUED.load(Relaxed)
}

/// Something a queue may stand at moved.
pub(super) fn wake() {
    LANE.wake();
}

/// This thread does work the line waits for.
pub(super) fn no_drain_here() {
    OURS.set(true);
}

/// Before something that has to see the submissions made so far sent: they
/// are, as far as they are free to run. Not on our own thread, which is
/// inside one of them.
pub(super) fn drain() {
    if OURS.get() {
        return;
    }
    let began = Instant::now();
    LANE.drain();
    STATS[DRAINING].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    STATS[DRAINS].fetch_add(1, Relaxed);
}

/// A submission of `lists` to queue `this`.
pub(super) fn submit(this: This, lists: &[This]) {
    let queue = this as usize;
    if crate::list::thin::on() {
        let recordings = crate::list::thin::submitted(lists);
        if recordings.is_empty() {
            return;
        }
        let before = QUEUED.fetch_add(1, Relaxed);
        STATS[DEPTH].fetch_add(before, Relaxed);
        STATS[DEEPEST].fetch_max(before + 1, Relaxed);
        LANE.push(queue, [Job::Lists(queue, recordings)]);
        room(before + 1);
        return;
    }
    let jobs = jobs_of(this, lists);
    let runs = jobs.iter().filter(|j| j.runs()).count() as u64;
    let before = QUEUED.fetch_add(runs, Relaxed);
    STATS[RUNS].fetch_add(runs, Relaxed);
    STATS[DEPTH].fetch_add(before, Relaxed);
    STATS[DEEPEST].fetch_max(before + runs, Relaxed);
    LANE.push(queue, jobs);
    room(before + runs);
}

/// Waits for room in the line, with `queued` submissions in it.
fn room(queued: u64) {
    if queued > runs_most() as u64 {
        let began = Instant::now();
        let _waiting = crate::timeline::span("a submission waits for room in the line");
        LANE.wait_runs(runs_most());
        STATS[ROOM].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
}

/// What a submission of `lists` is in the line.
fn jobs_of(this: This, lists: &[This]) -> Vec<Job> {
    let queue = this as usize;
    // In one piece up to the first list that waits for a memory value, as
    // `super::submit_cut` runs them; the rest part by part.
    let first = lists.iter().position(|&l| crate::list::cut_list(l)).unwrap_or(lists.len());
    #[cfg(feature = "diagnostics")]
    crate::capture::op(queue, || {
        let named: Vec<String> = lists
            .iter()
            .map(|&l| match crate::list::host_list(l) {
                Some(h) => format!("{:#x}", std::ptr::from_ref(h) as usize),
                None => format!("title {l:?}"),
            })
            .collect();
        format!(
            "SUBMIT queue kind {} lists {named:?} to the line (the first {first} in one piece)",
            super::queue(this).kind
        )
    });
    let mut jobs: Vec<Job> = Vec::new();
    if first > 0 {
        let (taken, order) = crate::list::detach(&lists[..first]);
        jobs.push(Job::Run(queue, taken, order));
    }
    if first < lists.len() {
        for (waits, taken, order) in crate::list::detach_parts(&lists[first..]) {
            jobs.extend(waits.into_iter().map(Job::Value));
            jobs.push(Job::Run(queue, taken, order));
        }
    }
    jobs
}

/// A signal of queue `this`: behind what it has in the line; if that is
/// nothing, behind the flights in the air, or now.
pub(super) fn signal(this: This, fence: This, value: u64) {
    let fence = fence as usize;
    LANE.push_behind(this as usize, false, Job::Signal(fence, value), || {
        if !crate::host::late_signal(fence, value) {
            crate::fence::set(fence as This, value);
        }
    });
}

/// Queue `this` waits for `fence`: nothing if it has nothing in the line and
/// the fence is there already.
pub(super) fn wait(this: This, fence: This, value: u64) {
    let always = crate::fence::completed(fence) < value;
    LANE.push_behind(this as usize, always, Job::Wait(fence as usize, value), || {});
}

/// A present on queue `this` of `texture`, in the state the lists recorded so
/// far leave it in.
pub(super) fn present(this: This, texture: Option<(std::sync::Arc<crate::resource::HostTexture>, u32)>, shown: super::Shown) {
    QUEUED.fetch_add(1, Relaxed);
    STATS[PRESENTS].fetch_add(1, Relaxed);
    LANE.push(this as usize, [Job::Present(texture, shown)]);
    let began = Instant::now();
    let _waiting = crate::timeline::span("a present waits for the one before it to be sent");
    LANE.wait_presents(frames_most());
    STATS[AT_PRESENT].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
}

/// For the frame-rate line.
pub(super) fn line(frames: f64) {
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Relaxed) as f64).collect();
    let ms = |i: usize| s[i] / 1000.0 / frames;
    tracing::info!(
        "submissions prepared by a thread of ours, per frame (VICTORY_ASYNC_SUBMIT): {:.1} in {:.1} presents, {:.1} in the line \
         when one was added (at most {:.0}); that thread worked {:.1} ms, and unfolded translated submissions {:.1} ms with the \
         line's lock {} (VICTORY_UNFOLD_LOCKED); threads of the title waited {:.1} ms for room in the \
         line, {:.1} ms at presents, {:.1} ms in {:.1} drains; waits not met at a present {:.2}; {:.1} of the \
         submissions were prepared together with the one before them (VICTORY_JOIN_PARTS); that thread stood {:.1} ms \
         with work in the line that waited for something, and looked again {:.1} times when a millisecond had passed \
         with no word (fewer with VICTORY_LANDING_WAKES=1)",
        s[RUNS] / frames,
        s[PRESENTS] / frames,
        s[DEPTH] / s[RUNS].max(1.0),
        s[DEEPEST],
        ms(BUSY),
        ms(UNFOLDING),
        if unfold_locked() { "held" } else { "free" },
        ms(ROOM),
        ms(AT_PRESENT),
        ms(DRAINING),
        s[DRAINS] / frames,
        s[FORCED] / frames,
        s[JOINED] / frames,
        ms(STOOD),
        s[POLLED] / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    type Log = Arc<Mutex<Vec<String>>>;

    /// A job of a test: what it stands at, and what it does.
    enum T {
        /// Stands until the fence has the value.
        Wait(usize, u64),
        Run(&'static str, Log),
        /// Runs once the gate is open: a submission that takes long.
        Slow(&'static str, Log, Arc<(Mutex<bool>, Condvar)>),
        Signal(usize, u64, Log),
        Present(&'static str, Log),
        /// Takes others of its kind, waits that are met, and signals along.
        Group(Vec<&'static str>, Log, Vec<(usize, u64)>),
        /// Stands for its parts (a submission whose lists are translated),
        /// until the fence has the value if it names one; its unfolding stands
        /// at the gate, if it has one, until the gate is opened.
        Folded(Option<(usize, u64)>, Vec<T>, Option<Arc<Gate>>),
    }

    impl Work for T {
        type Ask = (usize, u64);
        fn ask(&self) -> Option<(usize, u64)> {
            match self {
                &T::Wait(fence, value) => Some((fence, value)),
                T::Folded(ask, ..) => *ask,
                _ => None,
            }
        }
        fn met(ask: &(usize, u64)) -> bool {
            crate::fence::completed(ask.0 as This) >= ask.1
        }
        fn shows(&self) -> bool {
            matches!(self, T::Present(..))
        }
        fn runs(&self) -> bool {
            matches!(self, T::Run(..) | T::Slow(..) | T::Group(..) | T::Folded(..))
        }
        fn folded(&self) -> bool {
            matches!(self, T::Folded(..))
        }
        fn unfold(self) -> Vec<T> {
            let T::Folded(_, parts, gate) = self else {
                return vec![self];
            };
            if let Some(gate) = gate {
                gate.stand();
            }
            parts
        }
        fn take_along(&mut self, next: T) -> Result<bool, T> {
            let T::Group(names, _, signals) = self else {
                return Err(next);
            };
            match next {
                T::Group(more, ..) if names.len() < 3 => {
                    names.extend(more);
                    Ok(true)
                }
                T::Wait(f, v) if crate::fence::completed(f as This) >= v || signals.contains(&(f, v)) => Ok(false),
                T::Signal(f, v, _) => {
                    signals.push((f, v));
                    Ok(false)
                }
                other => Err(other),
            }
        }
        fn go(self, forced: bool) {
            match self {
                T::Wait(..) => {
                    if forced {
                        FORCED_HERE.with(|f| f.set(f.get() + 1));
                    }
                }
                T::Run(name, log) | T::Present(name, log) => log.lock().unwrap().push(name.into()),
                T::Slow(name, log, gate) => {
                    let mut open = gate.0.lock().unwrap();
                    while !*open {
                        open = gate.1.wait(open).unwrap();
                    }
                    log.lock().unwrap().push(name.into());
                }
                T::Signal(fence, value, log) => {
                    log.lock().unwrap().push(format!("signal {value}"));
                    crate::fence::set(fence as This, value);
                }
                T::Group(names, log, signals) => {
                    log.lock().unwrap().push(names.join("+"));
                    for (fence, value) in signals {
                        crate::fence::set(fence as This, value);
                    }
                }
                // (A folded one is unfolded, never done: its parts would be
                // missing from the log.)
                T::Folded(..) => {}
            }
        }
        fn mark_now() -> u64 {
            NOW.get()
        }
        fn mark_later() -> u64 {
            LATER.get()
        }
        fn marked(&mut self, mark: u64) {
            let name = match self {
                T::Run(name, _) | T::Slow(name, ..) => name.to_string(),
                T::Group(names, ..) => names.join("+"),
                _ => "?".into(),
            };
            MARKS.with(|m| m.borrow_mut().push((name, mark)));
        }
    }

    /// Where an unfolding stands until the test opens it: whether one came
    /// to it, and whether it is open.
    #[derive(Default)]
    struct Gate {
        state: Mutex<(bool, bool)>,
        moved: Condvar,
    }

    impl Gate {
        fn stand(&self) {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            self.moved.notify_all();
            while !state.1 {
                state = self.moved.wait(state).unwrap();
            }
        }

        /// Waits until an unfolding stands at it.
        fn reached(&self) {
            let mut state = self.state.lock().unwrap();
            while !state.0 {
                state = self.moved.wait(state).unwrap();
            }
        }

        fn open(&self) {
            self.state.lock().unwrap().1 = true;
            self.moved.notify_all();
        }

        fn is_open(&self) -> bool {
            self.state.lock().unwrap().1
        }
    }

    thread_local! {
        static FORCED_HERE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        /// What [`Work::mark_now`] and [`Work::mark_later`] give in a test.
        static NOW: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        static LATER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        /// The marks submissions were done with, by name.
        static MARKS: std::cell::RefCell<Vec<(String, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// (`VICTORY_HEAP_SYNC_AT_SUBMIT`, ported from trustno1-coder/VIctory-NVIDIA
    /// d2e24b3, from Belobug/VIctory-Belobug-AMD 673be0e.) A submission is done
    /// with the mark taken when it was put in the line, or, behind a wait of
    /// its queue, at least the one taken when that wait went (what is behind a
    /// wait may be written only before it is met); one taken along with
    /// others, the highest of all; a folded one's parts, its mark. Either way
    /// of unfolding.
    #[test]
    fn a_submission_needs_what_was_written_before_it_and_its_queues_waits() {
        for locked in WAYS {
            let lane = Lane::unfolding(Some(locked));
            let log: Log = Default::default();
            let f = fence();
            let run = |name| T::Run(name, log.clone());
            MARKS.take();
            NOW.set(5);
            LATER.set(0);
            lane.push(1, [run("a")]);
            NOW.set(6);
            lane.push(1, [T::Wait(f, 1), run("b")]);
            look(&lane);
            assert_eq!(MARKS.take(), [("a".to_string(), 5)], "b stands at its wait");
            // The wait is met later; what b reads may have been written just
            // before that.
            LATER.set(9);
            crate::fence::set(f as This, 1);
            look(&lane);
            assert_eq!(MARKS.take(), [("b".to_string(), 9)]);
            // Another queue went past no wait; this one did, for good.
            NOW.set(7);
            lane.push(2, [run("c")]);
            NOW.set(3);
            lane.push(1, [run("d")]);
            look(&lane);
            assert_eq!(MARKS.take(), [("c".to_string(), 7), ("d".to_string(), 9)]);
            // Taken along: a met wait between two parts raises them both.
            let part = |name| T::Group(vec![name], log.clone(), Vec::new());
            NOW.set(10);
            LATER.set(12);
            lane.push(3, [part("x"), T::Wait(f, 1), part("y")]);
            look(&lane);
            assert_eq!(MARKS.take(), [("x+y".to_string(), 12)]);
            // A folded one's parts keep the mark it was put in the line with.
            NOW.set(20);
            lane.push(4, [T::Folded(None, vec![run("folded")], None)]);
            look(&lane);
            assert_eq!(MARKS.take(), [("folded".to_string(), 20)]);
            // A present is no submission: it is told no mark.
            lane.push(3, [T::Present("present", log.clone())]);
            look(&lane);
            assert!(MARKS.take().is_empty());
            assert_eq!(*log.lock().unwrap(), ["a", "b", "c", "d", "x+y", "folded", "present"], "locked {locked}");
        }
    }

    /// The two ways a line unfolds: with its lock free (the default) and held
    /// (`VICTORY_UNFOLD_LOCKED=1`, as before). Each test of the order runs
    /// both, with the same expectations.
    const WAYS: [bool; 2] = [false, true];

    fn fence() -> usize {
        let mut f = std::ptr::null();
        assert_eq!(crate::fence::create(std::ptr::null(), 0, 0, &crate::fence::FENCE_IIDS[0], &mut f), super::super::S_OK);
        f as usize
    }

    fn value(f: usize) -> u64 {
        crate::fence::completed(f as This)
    }

    fn look(lane: &Lane<T>) {
        let limit = lane.lock().next;
        lane.look(limit);
    }

    /// A queue's order is kept, a wait for another queue's signal holds only
    /// what is behind it on its own queue, and the oldest goes first.
    #[test]
    fn queues_keep_their_order_and_wait_for_each_others_fences() {
        for locked in WAYS {
            let lane = Lane::unfolding(Some(locked));
            let log: Log = Default::default();
            let f = fence();
            let run = |name| T::Run(name, log.clone());
            // The compute queue waits for what the graphics queue signals later.
            lane.push(2, [T::Wait(f, 1), run("compute")]);
            lane.push(1, [run("draw 1")]);
            lane.push_behind(1, false, T::Signal(f, 1, log.clone()), || panic!("the queue is not empty"));
            lane.push(1, [run("draw 2")]);
            look(&lane);
            assert_eq!(*log.lock().unwrap(), ["draw 1", "signal 1", "compute", "draw 2"], "locked {locked}");
            assert_eq!(value(f), 1);
            // Nothing in the line: a signal is at once.
            let mut done = false;
            lane.push_behind(1, false, T::Signal(f, 2, log.clone()), || done = true);
            assert!(done);
        }
    }

    /// A submission takes what follows it in its queue along, up to what it
    /// cannot take, which stays at its place; the counts come out even.
    #[test]
    fn submissions_take_those_behind_them_along() {
        for locked in WAYS {
            let lane = Lane::unfolding(Some(locked));
            let log: Log = Default::default();
            let f = fence();
            let part = |name| T::Group(vec![name], log.clone(), Vec::new());
            lane.push(1, [part("a"), T::Wait(f, 0), part("b")]);
            lane.push(2, [part("other queue"), T::Wait(f, 1), part("behind a's signal")]);
            lane.push(1, [T::Signal(f, 1, log.clone()), T::Wait(f, 1), part("c"), part("d")]);
            lane.push(1, [T::Wait(f, 9), part("held"), T::Present("present", log.clone()), part("next frame")]);
            // What came after the look was asked for is not touched.
            let limit = lane.lock().next;
            lane.push(1, [part("later")]);
            lane.look(limit);
            assert_eq!(
                *log.lock().unwrap(),
                ["a+b+c", "other queue+behind a's signal", "d", "held", "present", "next frame"],
                "locked {locked}, {:?}",
                lane.lock().items.len()
            );
            look(&lane);
            assert_eq!(log.lock().unwrap().last().unwrap(), "later");
            let state = lane.lock();
            assert_eq!((state.presents, state.runs, state.items.len()), (0, 0, 0));
        }
    }

    /// A wait that nothing meets holds its queue until a present, which
    /// comes behind everything given before it and before what comes after.
    #[test]
    fn a_present_goes_behind_everything_before_it_met_or_not() {
        for locked in WAYS {
            FORCED_HERE.set(0);
            let lane = Lane::unfolding(Some(locked));
            let log: Log = Default::default();
            let f = fence();
            let run = |name| T::Run(name, log.clone());
            lane.push(2, [T::Wait(f, 9), run("held")]);
            lane.push(1, [run("draw")]);
            look(&lane);
            assert_eq!(*log.lock().unwrap(), ["draw"], "the wait holds its queue and no other");
            lane.push(1, [T::Present("present", log.clone())]);
            lane.push(1, [run("next frame")]);
            lane.push(2, [T::Wait(f, 10), run("held again")]);
            look(&lane);
            assert_eq!(*log.lock().unwrap(), ["draw", "held", "present", "next frame"], "locked {locked}");
            assert_eq!(FORCED_HERE.get(), 1);
            // The second wait is met by a signal of the CPU.
            crate::fence::set(f as This, 10);
            look(&lane);
            assert_eq!(log.lock().unwrap().last().unwrap(), "held again");
            assert_eq!(FORCED_HERE.get(), 1);
            let state = lane.lock();
            assert_eq!((state.presents, state.runs, state.items.len()), (0, 0, 0));
        }
    }

    /// Folded jobs give their parts their place: one at the head of its queue
    /// once what it stands at is met, one taken along behind a submission of
    /// its queue once it is free, and one left where it is while it is not
    /// (a present behind it waits for it); the counts come out even.
    #[test]
    fn folded_jobs_unfold_in_their_place() {
        let mut logs = Vec::new();
        for locked in WAYS {
            let lane = Lane::unfolding(Some(locked));
            let log: Log = Default::default();
            let (f, g) = (fence(), fence());
            let part = |name| T::Group(vec![name], log.clone(), Vec::new());
            // Free when the submission before it goes: taken along, parts and all.
            lane.push(1, [part("a"), T::Folded(None, vec![part("b"), T::Wait(f, 0), part("c")], None)]);
            // At the head of its queue until the signal taken along with a.
            lane.push(2, [T::Folded(Some((f, 1)), vec![part("x")], None), part("y")]);
            lane.push(1, [T::Signal(f, 1, log.clone()), part("d")]);
            // Not free when the submission before it goes: left in its place.
            lane.push(3, [part("e"), T::Folded(Some((g, 1)), vec![part("held")], None)]);
            lane.push(1, [T::Present("present", log.clone())]);
            look(&lane);
            assert_eq!(*log.lock().unwrap(), ["a+b+c", "x+y", "d", "e"], "locked {locked}");
            assert_eq!(lane.lock().runs, 1, "the one left stands for one submission");
            crate::fence::set(g as This, 1);
            look(&lane);
            assert_eq!(log.lock().unwrap()[4..], ["held", "present"]);
            let state = lane.lock();
            assert_eq!((state.presents, state.runs, state.items.len(), state.running), (0, 0, 0, None));
            drop(state);
            logs.push(std::mem::take(&mut *log.lock().unwrap()));
        }
        assert_eq!(logs[0], logs[1]);
    }

    /// While our thread unfolds a job (its unfolding stands at a gate, as the
    /// making of host lists did), the line's lock is free: from a thread of
    /// the title a push, a wake, a signal of the queue unfolding and a wait
    /// for at most one present in the line come back, the signal put behind
    /// the job, and nothing runs until it has unfolded; then its parts take
    /// its place, at the head of its queue or taken along, before what came
    /// in meanwhile. With the lock held as it unfolds
    /// (`VICTORY_UNFOLD_LOCKED=1`) those calls wait for it, as before.
    #[test]
    fn the_title_goes_on_while_a_job_unfolds() {
        for along in [false, true] {
            for locked in WAYS {
                let lane: &'static Lane<T> = Box::leak(Box::new(Lane::unfolding(Some(locked))));
                std::thread::spawn(|| lane.serve());
                let log: Log = Default::default();
                let f = fence();
                let gate = Arc::new(Gate::default());
                let folded = T::Folded(None, vec![T::Group(vec!["b"], log.clone(), Vec::new())], Some(gate.clone()));
                if along {
                    lane.push(1, [T::Group(vec!["a"], log.clone(), Vec::new()), folded]);
                } else {
                    lane.push(1, [folded]);
                }
                gate.reached();
                let (told, heard) = std::sync::mpsc::channel();
                let title = {
                    let (log, gate) = (log.clone(), gate.clone());
                    std::thread::spawn(move || {
                        lane.push(2, [T::Run("other queue", log.clone())]);
                        lane.wake();
                        let at_once = || {
                            log.lock().unwrap().push("signal 1 at once".into());
                            crate::fence::set(f as This, 1);
                        };
                        lane.push_behind(1, false, T::Signal(f, 1, log.clone()), at_once);
                        lane.push(1, [T::Present("present", log.clone())]);
                        lane.wait_presents(1);
                        told.send(!gate.is_open()).unwrap();
                    })
                };
                if locked {
                    assert!(heard.recv_timeout(Duration::from_millis(100)).is_err(), "they wait for the lock");
                } else {
                    assert_eq!(heard.recv_timeout(Duration::from_secs(10)), Ok(true), "back while it stands");
                }
                assert!(log.lock().unwrap().is_empty(), "nothing runs while it unfolds");
                assert_eq!(value(f), 0);
                gate.open();
                if locked {
                    assert_eq!(heard.recv_timeout(Duration::from_secs(10)), Ok(false));
                }
                title.join().unwrap();
                lane.drain();
                assert_eq!(value(f), 1);
                let first = if along { "a+b" } else { "b" };
                let log = log.lock().unwrap();
                if locked {
                    // (The title's calls came after the parts were in: the
                    // signal may have found its queue done and been at once.)
                    assert_eq!((log.len(), log[0].as_str()), (4, first), "{log:?}");
                } else {
                    assert_eq!(*log, [first, "other queue", "signal 1", "present"]);
                }
                let state = lane.lock();
                assert_eq!((state.presents, state.runs, state.items.len(), state.running), (0, 0, 0, None));
            }
        }
    }

    /// A wake is a count and a notify: it comes back while the line's lock is
    /// held elsewhere, and is counted; the line then goes on as before (a wait
    /// the CPU meets, a wake, a drain that finds it done). With
    /// `VICTORY_UNFOLD_LOCKED=1` it waits for the lock, as before.
    #[test]
    fn a_wake_does_not_take_the_lock() {
        for locked in WAYS {
            let lane: &'static Lane<T> = Box::leak(Box::new(Lane::unfolding(Some(locked))));
            let log: Log = Default::default();
            let f = fence();
            lane.push(1, [T::Wait(f, 1), T::Run("after the wait", log.clone())]);
            let held = lane.lock();
            let (told, heard) = std::sync::mpsc::channel();
            let waker = std::thread::spawn(move || {
                lane.wake();
                told.send(()).unwrap();
            });
            let back = heard.recv_timeout(Duration::from_millis(if locked { 100 } else { 10_000 }));
            assert_eq!(back.is_ok(), !locked, "locked {locked}");
            drop(held);
            waker.join().unwrap();
            assert_eq!(lane.woken.load(Relaxed), u64::from(!locked));
            // Our thread serves the line; the CPU meets the wait and wakes it.
            std::thread::spawn(|| lane.serve());
            crate::fence::set(f as This, 1);
            lane.wake();
            lane.drain();
            assert_eq!(*log.lock().unwrap(), ["after the wait"]);
            // With nothing in the line our thread waits idle, and a wake
            // leaves it be; what is put in the line next is done all the same.
            while !lane.idle.load(SeqCst) {
                std::thread::yield_now();
            }
            lane.wake();
            lane.push(1, [T::Run("after an idle wake", log.clone())]);
            lane.drain();
            assert_eq!(*log.lock().unwrap(), ["after the wait", "after an idle wake"]);
        }
    }

    /// With the thread: a fence behind a submission is not reached before the
    /// submission has been done, presents hold the title back a frame later,
    /// and a drain comes back when everything was done.
    #[test]
    fn the_title_goes_on_and_is_told_in_order() {
        for locked in WAYS {
            let lane: &'static Lane<T> = Box::leak(Box::new(Lane::unfolding(Some(locked))));
            std::thread::spawn(|| lane.serve());
            let log: Log = Default::default();
            let f = fence();
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            lane.push(1, [T::Slow("slow", log.clone(), gate.clone())]);
            lane.push_behind(1, false, T::Signal(f, 1, log.clone()), || panic!("behind the submission"));
            lane.push(1, [T::Present("present 1", log.clone())]);
            // One present may be in the line: this returns with it there.
            lane.wait_presents(1);
            assert_eq!(value(f), 0);
            assert!(log.lock().unwrap().is_empty());
            // A fence behind a submission being done (no longer in the line).
            while lane.lock().running.is_none() {
                std::thread::yield_now();
            }
            lane.push(1, [T::Run("second", log.clone())]);
            lane.push(1, [T::Present("present 2", log.clone())]);
            let opener = {
                let gate = gate.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(20));
                    *gate.0.lock().unwrap() = true;
                    gate.1.notify_all();
                })
            };
            lane.wait_presents(1);
            assert!(value(f) == 1, "the first frame was done");
            lane.drain();
            let done = ["slow", "signal 1", "present 1", "second", "present 2"];
            assert_eq!(*log.lock().unwrap(), done, "locked {locked}");
            opener.join().unwrap();
            // A wait the CPU meets without telling anybody; a drain does not
            // stand at it.
            lane.push(1, [T::Wait(f, 5), T::Run("after the wait", log.clone())]);
            lane.drain();
            assert_eq!(log.lock().unwrap().len(), 5);
            lane.wait_runs(0);
            crate::fence::set(f as This, 5);
            while log.lock().unwrap().len() < 6 {
                std::thread::yield_now();
            }
        }
    }
}
