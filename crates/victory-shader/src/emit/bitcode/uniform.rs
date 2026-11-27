//! Which group barriers of a finished module a thread of the group may reach where another
//! thread of the same group does not, or reach a different number of times.
//!
//! A group barrier (`dx.op.barrier` with bit 0 of its mode, the wait for the group) must be
//! reached by every thread of the group the same number of times: D3D12 leaves any other use
//! undefined, NVIDIA's wait is for every thread (`emit::group_sync`), and threads that went
//! different ways meet at the wrong barriers or not at all. The wave-wide form
//! `VICTORY_WAVE_WIDE=1` makes of one tiling compute program put the barriers its
//! two host waves meet at in a loop each lane leaves on its own.
//! [`Module::divergent_barriers`] finds the blocks of such barriers: those behind a branch
//! whose condition need not be the same in every thread of the group, a loop such a branch
//! leaves included.
//!
//! A value is the same in every thread ("uniform") when it is a constant, the group's id, a
//! read of a constant buffer, a resource or group-shared memory at a uniform place, or made of
//! uniform values by an instruction every thread of the group runs; a value one wave agrees
//! on (a first-lane read, a wave's "some lane") counts only when the group is one host wave,
//! or when what the wave reads is uniform. Anything an instruction makes in a block some
//! threads do not run, and a join after such a branch, is not.
//!
//! The blocks a lane leaves the program through at the translation's loop limit
//! (`lower.rs` `abort_block`, 4,096 rounds in a compute program) are left out, as if no lane
//! got there: a lane stopped by the limit has left a loop the console would still be running.
//! shortcut: in a loop every lane runs alike the limit stops every lane alike; a lane of a
//! loop the lanes leave one by one that does run into it leaves the others standing at the
//! next barrier, as it does without this check.
//!
//! shortcut: a read of memory at a uniform place counts as uniform. Another thread's store
//! may fall between two threads' reads of the same place; the reads the lowering branches on
//! (the halves' answers it exchanges through group-shared memory) are made after a barrier
//! from words written before it.
//!
//! A group of several console waves, each run as two host waves, has a second kind of
//! meeting: the two host waves of one console wave meet by themselves (`lower.rs`
//! `Lower::pair_meeting`), each waiting for the other's count of arrivals. Such a meeting
//! must be reached alike by the threads of one console wave, whatever the group's other
//! console waves do. [`Module::divergent_meetings`] is the same check with "the same in
//! every thread of one console wave" for "the same in every thread of the group": the
//! number of the thread's console wave (which the lowering names) counts as the same, and
//! so does what is read from shared memory at a place made of it (the words the two halves
//! exchange through).

use super::{Module, Op, Term, V, for_each_operand, successors};
#[cfg(test)]
use super::{Bin, Cast, Const};

/// What [`Module::waits`] finds of a finished module's group barriers and meetings.
///
/// A lane that reaches the translation's loop limit in a loop closed by a plain jump leaves
/// the program (`lower.rs` `abort_block`). Where the lanes of a group go round such a loop
/// different numbers of times, some leave there and the others go on, and a group barrier
/// they go on to waits for threads that have returned: [`Module::divergent_barriers`] takes
/// no lane to get to the limit, so it passes such a barrier.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Waits {
    /// The blocks ending in a loop limit's test that some threads of the group may fail
    /// where others do not, with a group barrier or a meeting after them.
    pub cuts: Vec<usize>,
    /// The blocks of the group barriers and meetings that can be reached after one of `cuts`.
    pub after_cut: Vec<usize>,
    /// The blocks of the group barriers and meetings inside loops, each with the number of
    /// loops around it.
    pub in_loops: Vec<(usize, u32)>,
    /// How many loops the module has.
    pub loops: u32,
}

impl Module {
    /// The blocks of `main` holding a group barrier that some thread of the group may reach
    /// where another does not (see the module notes); `waves` is the number of host waves a
    /// group runs as (1: a value every lane of a wave agrees on is the group's), `aborts` the
    /// blocks no lane is taken to reach (the loop limit's). Empty when every group barrier is
    /// reached by every thread alike.
    #[cfg(test)]
    pub(crate) fn divergent_barriers(&self, waves: u32, aborts: &[usize]) -> Vec<usize> {
        self.divergent_barriers_with(waves, aborts, &[])
    }

    /// [`Module::divergent_barriers`] for a group of several console waves whose halves
    /// exchange through words of their own: `echoes` are (what a thread reads back of an
    /// exchange, what its host wave gave to it). The words are at a place of the thread's
    /// console wave, so what is read counts as varying across the group; but where every
    /// thread of the group gives the same, every thread reads the same back (each half reads
    /// its own word and the other half's, written at the meeting before the read).
    pub(crate) fn divergent_barriers_with(&self, waves: u32, aborts: &[usize], echoes: &[(V, V)]) -> Vec<usize> {
        let (reached, region) = self.split_regions(waves, aborts, &[], echoes);
        (0..self.blocks.len())
            .filter(|&b| reached[b] && region[b] && self.blocks[b].body.iter().any(|&i| self.group_barrier(i)))
            .collect()
    }

    /// Those of the blocks `meetings` (where the two host waves of a console wave meet by
    /// themselves, see the module notes) that some thread of a console wave may reach where
    /// another thread of the same console wave does not; `same` are the values the lowering
    /// knows to be the same in every thread of one console wave (the wave's number in its
    /// group), `aborts` as for [`Module::divergent_barriers`]. Empty when both halves of every
    /// console wave reach every meeting alike.
    pub(crate) fn divergent_meetings(&self, meetings: &[usize], same: &[V], aborts: &[usize]) -> Vec<usize> {
        // (two host waves a console wave: what one host wave agrees on is not the pair's)
        let (reached, region) = self.split_regions(2, aborts, same, &[]);
        meetings.iter().copied().filter(|&b| reached.get(b).copied().unwrap_or(false) && region[b]).collect()
    }

    /// What the group barriers of `main` (and the blocks `meetings`, where two host waves
    /// meet by themselves) may wait for that [`Module::divergent_barriers`] leaves out, and
    /// what they may cost: see [`Waits`]. `waves`, `aborts` and `echoes` as there.
    pub(crate) fn waits(&self, waves: u32, aborts: &[usize], echoes: &[(V, V)], meetings: &[usize]) -> Waits {
        let n = self.blocks.len();
        let mut w = Waits::default();
        if n == 0 {
            return w;
        }
        let (reached, region) = self.split_regions(waves, aborts, &[], echoes);
        let succ = |b: usize| -> Vec<usize> {
            successors(self.blocks[b].term).iter().copied().filter(|&s| reached[s]).collect()
        };
        let stop: Vec<bool> = (0..n)
            .map(|b| reached[b] && (meetings.contains(&b) || self.blocks[b].body.iter().any(|&i| self.group_barrier(i))))
            .collect();
        // the loop limit's tests that some threads may fail where others do not, and the
        // barriers the others can go on to
        let mut after = vec![false; n];
        for b in (0..n).filter(|&b| reached[b] && region[b]) {
            if !successors(self.blocks[b].term).iter().any(|s| aborts.contains(s)) {
                continue;
            }
            let mut seen = vec![false; n];
            let mut stack = succ(b);
            let mut found = false;
            while let Some(x) = stack.pop() {
                if std::mem::replace(&mut seen[x], true) {
                    continue;
                }
                if stop[x] {
                    after[x] = true;
                    found = true;
                }
                stack.extend(succ(x));
            }
            if found {
                w.cuts.push(b);
            }
        }
        w.after_cut = (0..n).filter(|&b| after[b]).collect();
        // the loops (a way back to a block that dominates its start) around each barrier
        let dom = self.dominators(&reached);
        let preds = {
            let mut p = vec![Vec::new(); n];
            for b in (0..n).filter(|&b| reached[b]) {
                for s in succ(b) {
                    p[s].push(b);
                }
            }
            p
        };
        let mut bodies: std::collections::BTreeMap<usize, Vec<bool>> = std::collections::BTreeMap::new();
        for b in (0..n).filter(|&b| reached[b]) {
            for h in succ(b) {
                if dom[b][h / 64] >> (h % 64) & 1 == 0 {
                    continue;
                }
                let body = bodies.entry(h).or_insert_with(|| vec![false; n]);
                body[h] = true;
                let mut stack = vec![b];
                while let Some(x) = stack.pop() {
                    if std::mem::replace(&mut body[x], true) {
                        continue;
                    }
                    stack.extend(preds[x].iter().copied());
                }
            }
        }
        w.loops = bodies.len() as u32;
        for b in (0..n).filter(|&b| stop[b]) {
            let depth = bodies.values().filter(|body| body[b]).count() as u32;
            if depth > 0 {
                w.in_loops.push((b, depth));
            }
        }
        w
    }

    /// Per block, the blocks that dominate it (bitsets; every block dominates itself), among
    /// the blocks `reached`.
    fn dominators(&self, reached: &[bool]) -> Vec<Vec<u64>> {
        let n = self.blocks.len();
        let words = n.div_ceil(64);
        let mut preds = vec![Vec::new(); n];
        for b in (0..n).filter(|&b| reached[b]) {
            for &s in successors(self.blocks[b].term).iter().filter(|&&s| reached[s]) {
                preds[s].push(b);
            }
        }
        let mut dom: Vec<Vec<u64>> = (0..n)
            .map(|b| {
                if b == 0 {
                    let mut v = vec![0u64; words];
                    v[0] = 1;
                    v
                } else {
                    vec![u64::MAX; words]
                }
            })
            .collect();
        let mut changed = true;
        while changed {
            changed = false;
            for b in (1..n).filter(|&b| reached[b]) {
                let mut v = vec![u64::MAX; words];
                for &p in &preds[b] {
                    for (w, x) in v.iter_mut().zip(&dom[p]) {
                        *w &= x;
                    }
                }
                v[b / 64] |= 1 << (b % 64);
                if v != dom[b] {
                    dom[b] = v;
                    changed = true;
                }
            }
        }
        dom
    }

    /// Per block: whether it is reached at all (`aborts` are not), and whether some threads
    /// may run it while others do not, or run it more often (see the module notes): the
    /// regions of the branches on values that vary. `same` are instructions taken as the
    /// same in every thread whatever they are made of; `echoes` (an instruction, another)
    /// those that vary only where the other does.
    fn split_regions(&self, waves: u32, aborts: &[usize], same: &[V], echoes: &[(V, V)]) -> (Vec<bool>, Vec<bool>) {
        let n = self.blocks.len();
        if n == 0 {
            return (Vec::new(), Vec::new());
        }
        let mut reached = self.reached();
        for &a in aborts {
            if let Some(r) = reached.get_mut(a) {
                *r = false;
            }
        }
        let reached = reached;
        let succ = |b: usize| -> Vec<usize> {
            successors(self.blocks[b].term).iter().copied().filter(|&s| reached[s]).collect()
        };
        let mut home = vec![usize::MAX; self.insts.len()];
        for (b, blk) in self.blocks.iter().enumerate() {
            for &i in blk.phis.iter().chain(&blk.body) {
                home[i as usize] = b;
            }
        }
        let pdom = self.post_dominators(&reached);
        let preds = {
            let mut p = vec![Vec::new(); n];
            for b in (0..n).filter(|&b| reached[b]) {
                for s in succ(b) {
                    p[s].push(b);
                }
            }
            p
        };
        let mut divergent = vec![false; self.insts.len()];
        let mut given = vec![false; self.insts.len()];
        for &v in same {
            if let V::Inst(i) = v {
                given[i as usize] = true;
            }
        }
        let mut echo = vec![u32::MAX; if echoes.is_empty() { 0 } else { self.insts.len() }];
        for &(read, gave) in echoes {
            if let (V::Inst(read), V::Inst(gave)) = (read, gave) {
                echo[read as usize] = gave;
            } else if let V::Inst(read) = read {
                // (a constant given: the same everywhere)
                given[read as usize] = true;
            }
        }
        // Blocks some threads of the group may run while others do not (or run more often).
        let mut region = vec![false; n];
        loop {
            let mut changed = false;
            for (i, inst) in self.insts.iter().enumerate() {
                let b = home[i];
                if divergent[i] || given[i] || b == usize::MAX || !reached[b] {
                    continue;
                }
                let is = |v: V| matches!(v, V::Inst(j) if divergent[j as usize]);
                let echoed = echo.get(i).copied().filter(|&gave| gave != u32::MAX);
                let d = region[b]
                    || match &inst.op {
                        _ if echoed.is_some() => echoed.is_some_and(|gave| divergent[gave as usize]),
                        // a join after a branch some threads took the other way
                        Op::Phi(inc) => {
                            inc.iter().any(|&(v, _)| is(v))
                                || preds[b].iter().any(|&p| region[p] || self.splits(p, &divergent, &reached))
                        }
                        Op::AtomicRmw(..) | Op::CmpXchg(..) => true,
                        Op::Call(f, args) => self.call_varies(*f, args, &divergent, waves),
                        op => {
                            let mut any = false;
                            for_each_operand(op, |v| any |= is(v));
                            any
                        }
                    };
                if d {
                    divergent[i] = true;
                    changed = true;
                }
            }
            // what a branch on a value that varies decides: the blocks reachable from it that
            // do not post-dominate it, the branch's own block too when a loop comes back to it
            for d in (0..n).filter(|&d| reached[d] && self.splits(d, &divergent, &reached)) {
                let mut seen = vec![false; n];
                let mut stack: Vec<usize> = succ(d);
                while let Some(b) = stack.pop() {
                    if seen[b] || b != d && pdom[d][b / 64] >> (b % 64) & 1 == 1 {
                        continue;
                    }
                    seen[b] = true;
                    if !region[b] {
                        region[b] = true;
                        changed = true;
                    }
                    stack.extend(succ(b));
                }
            }
            if !changed {
                break;
            }
        }
        (reached, region)
    }

    /// Whether block `b` ends in a branch on a value that may vary across the group, both of
    /// whose ways are `reached`.
    fn splits(&self, b: usize, divergent: &[bool], reached: &[bool]) -> bool {
        matches!(self.blocks[b].term, Some(Term::CondBr(V::Inst(c), t, f))
            if divergent[c as usize] && reached[t] && reached[f])
    }

    /// Whether instruction `i` is a barrier that waits for the group (bit 0 of its mode).
    fn group_barrier(&self, i: u32) -> bool {
        let Op::Call(f, args) = &self.insts[i as usize].op else {
            return false;
        };
        self.funcs[*f as usize].name == "dx.op.barrier"
            && args.get(1).and_then(|&m| self.int_value(m)).is_none_or(|mode| mode & 1 != 0)
    }

    /// Whether a call of function `f` with `args` may give threads of one group different
    /// results.
    fn call_varies(&self, f: u32, args: &[V], divergent: &[bool], waves: u32) -> bool {
        let is = |v: &V| matches!(*v, V::Inst(j) if divergent[j as usize]);
        let name = &self.funcs[f as usize].name;
        let Some(op) = name.strip_prefix("dx.op.") else {
            return true;
        };
        let op = op.split('.').next().unwrap_or(op);
        match op {
            // what the operands give: the same operands, the same result in every thread
            "unary" | "unaryBits" | "binary" | "tertiary" | "dot2" | "dot3" | "dot4" | "isSpecialFloat"
            | "legacyF16ToF32" | "legacyF32ToF16" | "makeDouble" | "splitDouble" | "groupId"
            | "cbufferLoadLegacy" | "createHandle" | "createHandleFromBinding" | "createHandleFromHeap"
            | "annotateHandle" | "getDimensions" | "bufferLoad" | "rawBufferLoad" | "textureLoad"
            | "sampleLevel" | "sampleCmpLevelZero" | "sampleCmpLevel" | "sampleGrad" | "textureGather"
            | "textureGatherCmp" | "waveGetLaneCount" => args.iter().any(is),
            // one value in every lane of a wave: the group's when the group is one wave, or when
            // what is read is uniform
            "waveReadLaneFirst" | "waveReadLaneAt" | "waveAnyTrue" | "waveAllTrue" => {
                if waves <= 1 {
                    // (a lane read at a lane that varies is still one lane's value)
                    false
                } else {
                    args.get(1).is_none_or(is)
                }
            }
            // a wave's ballot, sum, bits...: the same in every lane of one wave, and of the group
            // when it is one wave; across waves they count different lanes
            "waveActiveBallot" | "waveActiveOp" | "waveActiveBit" | "waveActiveAllEqual" => waves > 1,
            // the thread's own: ids, lane numbers, prefixes, quads, atomics, and anything not
            // named above
            _ => true,
        }
    }

    /// Per block, the blocks that post-dominate it (bitsets; every block post-dominates
    /// itself), with a virtual exit after every return.
    pub(super) fn post_dominators(&self, reached: &[bool]) -> Vec<Vec<u64>> {
        let n = self.blocks.len();
        let words = n.div_ceil(64);
        let own = |b: usize| {
            let mut v = vec![0u64; words];
            v[b / 64] |= 1 << (b % 64);
            v
        };
        let live = |b: usize| -> Vec<usize> {
            successors(self.blocks[b].term).iter().copied().filter(|&s| reached[s]).collect()
        };
        let mut pdom: Vec<Vec<u64>> = (0..n)
            .map(|b| if live(b).is_empty() { own(b) } else { vec![u64::MAX; words] })
            .collect();
        let mut changed = true;
        while changed {
            changed = false;
            for b in (0..n).rev().filter(|&b| reached[b]) {
                let succ = live(b);
                if succ.is_empty() {
                    continue;
                }
                let mut v = vec![u64::MAX; words];
                for &s in &succ {
                    for (w, x) in v.iter_mut().zip(&pdom[s]) {
                        *w &= x;
                    }
                }
                v[b / 64] |= 1 << (b % 64);
                if v != pdom[b] {
                    pdom[b] = v;
                    changed = true;
                }
            }
        }
        pdom
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Attr, Bin, Module, Term, V, pred};

    /// `main` and the calls a lowering makes: a group barrier, the thread's lane, a wave's
    /// first-lane read and a constant-buffer read.
    struct Kit {
        m: Module,
        entry: usize,
        barrier: V,
        lane: V,
        first: V,
        cbuffer: V,
    }

    fn kit() -> Kit {
        let mut m = Module::default();
        let (i32t, void) = (m.int(32), m.void());
        let unary = m.func_ty(i32t, vec![i32t]);
        let binary = m.func_ty(i32t, vec![i32t, i32t]);
        let barrier_ty = m.func_ty(void, vec![i32t, i32t]);
        for t in [unary, binary, barrier_ty] {
            m.ptr(t);
        }
        let barrier = m.function("dx.op.barrier", barrier_ty, Some(Attr::NoDuplicate), false);
        let lane = m.function("dx.op.waveGetLaneIndex", unary, Some(Attr::ReadOnly), false);
        let first = m.function("dx.op.waveReadLaneFirst.i32", binary, Some(Attr::NoUnwind), false);
        let cbuffer = m.function("dx.op.cbufferLoadLegacy.i32", binary, Some(Attr::ReadOnly), false);
        let main_ty = m.func_ty(void, vec![]);
        m.ptr(main_ty);
        m.function("main", main_ty, None, true);
        let entry = m.new_block();
        Kit { m, entry, barrier, lane, first, cbuffer }
    }

    fn wait(k: &mut Kit, b: usize) {
        let (op, mode) = (k.m.i32(80), k.m.i32(9));
        k.m.call(b, k.barrier, vec![op, mode]);
    }

    /// A barrier behind a branch on a constant-buffer word is reached by every thread; behind
    /// a branch on the lane, by some; a barrier that only fences waits for no one.
    #[test]
    fn barriers_behind_uniform_branches_are_reached_by_every_thread() {
        for (lane_branch, expect_flagged) in [(false, false), (true, true)] {
            let mut k = kit();
            let (then, join) = (k.m.new_block(), k.m.new_block());
            let (op_lane, op_cb, zero, five) = (k.m.i32(111), k.m.i32(59), k.m.i32(0), k.m.i32(5));
            let x = if lane_branch {
                k.m.call(k.entry, k.lane, vec![op_lane])
            } else {
                k.m.call(k.entry, k.cbuffer, vec![op_cb, zero])
            };
            let c = k.m.cmp(k.entry, pred::IULT, x, five);
            k.m.terminate(k.entry, Term::CondBr(c, then, join));
            wait(&mut k, then);
            // (a fence alone in the same block is no wait)
            let (op, fence) = (k.m.i32(80), k.m.i32(8));
            k.m.call(join, k.barrier, vec![op, fence]);
            k.m.terminate(then, Term::Br(join));
            k.m.terminate(join, Term::Ret);
            let flagged = k.m.divergent_barriers(2, &[]);
            assert_eq!(flagged.is_empty(), !expect_flagged, "lane branch {lane_branch}: {flagged:?}");
            if expect_flagged {
                assert_eq!(flagged, [then]);
            }
        }
    }

    /// A loop each lane leaves on its own: a barrier in the block that tests the exit is
    /// reached more often by the threads that stay (the block of the test post-dominates
    /// itself, and is still the loop's). With a uniform test, every thread goes round alike.
    #[test]
    fn a_barrier_in_a_loop_lanes_leave_on_their_own_is_divergent() {
        for lane_exit in [false, true] {
            let mut k = kit();
            let i32t = k.m.int(32);
            let (head, exit) = (k.m.new_block(), k.m.new_block());
            let (op_lane, op_cb, zero, one) = (k.m.i32(111), k.m.i32(59), k.m.i32(0), k.m.i32(1));
            let limit = if lane_exit {
                k.m.call(k.entry, k.lane, vec![op_lane])
            } else {
                k.m.call(k.entry, k.cbuffer, vec![op_cb, zero])
            };
            k.m.terminate(k.entry, Term::Br(head));
            let count = k.m.phi(head, i32t);
            wait(&mut k, head);
            let next = k.m.bin(head, Bin::Add, count, one);
            let more = k.m.cmp(head, pred::IULT, next, limit);
            k.m.terminate(head, Term::CondBr(more, head, exit));
            k.m.set_phi(count, vec![(zero, k.entry), (next, head)]);
            // after the loop, a barrier every thread reaches once
            wait(&mut k, exit);
            k.m.terminate(exit, Term::Ret);
            let flagged = k.m.divergent_barriers(2, &[]);
            if lane_exit {
                assert_eq!(flagged, [head]);
            } else {
                assert!(flagged.is_empty(), "{flagged:?}");
            }
        }
    }

    /// A wave's first-lane read of a lane's value is the same in every lane of the wave: the
    /// group's when the group is one host wave, not when it is two (a console wave64 program
    /// in two host waves of 32). Of a uniform value it is uniform either way.
    #[test]
    fn a_wave_value_is_the_groups_only_in_a_group_of_one_wave() {
        for (of_lane, waves, expect_flagged) in [(true, 1, false), (true, 2, true), (false, 2, false)] {
            let mut k = kit();
            let (then, join) = (k.m.new_block(), k.m.new_block());
            let (op_lane, op_first, op_cb, zero, five) =
                (k.m.i32(111), k.m.i32(118), k.m.i32(59), k.m.i32(0), k.m.i32(5));
            let x = if of_lane {
                k.m.call(k.entry, k.lane, vec![op_lane])
            } else {
                k.m.call(k.entry, k.cbuffer, vec![op_cb, zero])
            };
            let f = k.m.call(k.entry, k.first, vec![op_first, x]);
            let c = k.m.cmp(k.entry, pred::IULT, f, five);
            k.m.terminate(k.entry, Term::CondBr(c, then, join));
            wait(&mut k, then);
            k.m.terminate(then, Term::Br(join));
            k.m.terminate(join, Term::Ret);
            let flagged = k.m.divergent_barriers(waves, &[]);
            assert_eq!(!flagged.is_empty(), expect_flagged, "of lane {of_lane}, {waves} waves: {flagged:?}");
        }
    }

    /// What a branch on a lane decides reaches past its join through a join's values: a
    /// later branch on the joined value varies too, and so does a value made inside the
    /// branch and read after it.
    #[test]
    fn a_join_after_a_lane_branch_varies() {
        let mut k = kit();
        let i32t = k.m.int(32);
        let (then, join, after, end) = (k.m.new_block(), k.m.new_block(), k.m.new_block(), k.m.new_block());
        let (op_lane, zero, one, five) = (k.m.i32(111), k.m.i32(0), k.m.i32(1), k.m.i32(5));
        let lane = k.m.call(k.entry, k.lane, vec![op_lane]);
        let c = k.m.cmp(k.entry, pred::IULT, lane, five);
        k.m.terminate(k.entry, Term::CondBr(c, then, join));
        k.m.terminate(then, Term::Br(join));
        // both incoming values are constants; the join is not
        let joined = k.m.phi(join, i32t);
        k.m.set_phi(joined, vec![(one, then), (zero, k.entry)]);
        let again = k.m.cmp(join, pred::EQ, joined, one);
        k.m.terminate(join, Term::CondBr(again, after, end));
        wait(&mut k, after);
        k.m.terminate(after, Term::Br(end));
        k.m.terminate(end, Term::Ret);
        assert_eq!(k.m.divergent_barriers(2, &[]), [after]);
    }

    /// A loop each lane leaves on its own, with the translation's limit on its rounds: a
    /// barrier after the loop is reached by every lane once the limit's block is left out (no
    /// lane is taken to run into the limit), and not otherwise (a lane stopped there leaves
    /// the program before it).
    #[test]
    fn a_barrier_after_a_loop_lanes_leave_on_their_own_waits_for_all_but_the_limit() {
        let mut k = kit();
        let i32t = k.m.int(32);
        let (head, latch, abort, exit) = (k.m.new_block(), k.m.new_block(), k.m.new_block(), k.m.new_block());
        let (op_lane, zero, one, limit) = (k.m.i32(111), k.m.i32(0), k.m.i32(1), k.m.i32(1 << 20));
        let lane = k.m.call(k.entry, k.lane, vec![op_lane]);
        k.m.terminate(k.entry, Term::Br(head));
        let count = k.m.phi(head, i32t);
        let next = k.m.bin(head, Bin::Add, count, one);
        let more = k.m.cmp(head, pred::IULT, next, lane);
        k.m.terminate(head, Term::CondBr(more, latch, exit));
        let ok = k.m.cmp(latch, pred::IULT, next, limit);
        k.m.terminate(latch, Term::CondBr(ok, head, abort));
        k.m.set_phi(count, vec![(zero, k.entry), (next, latch)]);
        k.m.terminate(abort, Term::Ret);
        wait(&mut k, exit);
        k.m.terminate(exit, Term::Ret);
        assert!(k.m.divergent_barriers(2, &[abort]).is_empty());
        assert_eq!(k.m.divergent_barriers(2, &[]), [exit]);
    }

    /// A meeting of the two host waves of one console wave is reached by both alike behind a
    /// branch on what they exchanged (a word of shared memory at a place made of their
    /// console wave's number, which the lowering names), where a group barrier is not: the
    /// group's other console waves read other words. Behind a branch on the lane, or on what
    /// one host wave agrees on, one half may miss it; and the wait itself (a loop each lane
    /// leaves on an atomic read) keeps what follows it reached by all.
    #[test]
    fn a_meeting_behind_what_the_halves_exchanged_is_reached_by_both() {
        for (on, expect_missed) in [("exchanged", false), ("lane", true), ("first", true)] {
            let mut k = kit();
            let i32t = k.m.int(32);
            let words = k.m.array(8, i32t);
            let lds = k.m.global("lds", words, 3);
            let (then, spin, after, join) = (k.m.new_block(), k.m.new_block(), k.m.new_block(), k.m.new_block());
            let (op_lane, op_first, zero, one, five, six) =
                (k.m.i32(111), k.m.i32(118), k.m.i32(0), k.m.i32(1), k.m.i32(5), k.m.i32(6));
            let lane = k.m.call(k.entry, k.lane, vec![op_lane]);
            // (the thread's console wave, as the lowering makes it of a count the waves took)
            let slot = k.m.element(k.entry, lds, zero);
            let base = k.m.atomic(k.entry, super::super::Rmw::Add, slot, zero);
            let base = k.m.call(k.entry, k.first, vec![op_first, base]);
            let thread = k.m.bin(k.entry, Bin::Add, base, lane);
            let wave = k.m.bin(k.entry, Bin::LShr, thread, six);
            let place = k.m.element(k.entry, lds, wave);
            let x = match on {
                "exchanged" => k.m.load(k.entry, place),
                "lane" => lane,
                _ => k.m.call(k.entry, k.first, vec![op_first, lane]),
            };
            let c = k.m.cmp(k.entry, pred::IULT, x, five);
            k.m.terminate(k.entry, Term::CondBr(c, then, join));
            // the meeting: `then` counts, `spin` waits, `after` goes on to a second meeting
            k.m.atomic(then, super::super::Rmw::Add, place, one);
            k.m.terminate(then, Term::Br(spin));
            let seen = k.m.atomic(spin, super::super::Rmw::Or, place, zero);
            let here = k.m.cmp(spin, pred::IUGE, seen, one);
            k.m.terminate(spin, Term::CondBr(here, after, spin));
            wait(&mut k, after);
            k.m.terminate(after, Term::Br(join));
            k.m.terminate(join, Term::Ret);
            let missed = k.m.divergent_meetings(&[then, after], &[wave], &[]);
            assert_eq!(!missed.is_empty(), expect_missed, "a branch on {on}: {missed:?}");
            // unless the lowering names the console wave's number, nothing is known to be the pair's
            if on == "exchanged" {
                assert_eq!(k.m.divergent_meetings(&[then, after], &[], &[]), [then, after]);
                // and the group's other console waves go their own way: no barrier of the group
                assert_eq!(k.m.divergent_barriers(4, &[]), [after]);
                // unless what the halves gave to the exchange is the same in the whole group
                // (a constant-buffer word): then every thread read the same back
                let (op_cb, given) = (k.m.i32(59), k.m.i32(0));
                let uniform = k.m.call(k.entry, k.cbuffer, vec![op_cb, given]);
                assert!(k.m.divergent_barriers_with(4, &[], &[(x, uniform)]).is_empty());
                assert_eq!(k.m.divergent_barriers_with(4, &[], &[(x, lane)]), [after]);
            }
        }
    }

    /// A block cut in two before an instruction: the new block takes the instruction, what
    /// follows and the exit, and the joins the old block went to name the new one.
    #[test]
    fn a_block_cut_in_two_keeps_its_joins() {
        let mut k = kit();
        let i32t = k.m.int(32);
        let (head, exit) = (k.m.new_block(), k.m.new_block());
        let (op_cb, zero, one) = (k.m.i32(59), k.m.i32(0), k.m.i32(1));
        let limit = k.m.call(k.entry, k.cbuffer, vec![op_cb, zero]);
        k.m.terminate(k.entry, Term::Br(head));
        let count = k.m.phi(head, i32t);
        let next = k.m.bin(head, Bin::Add, count, one);
        let more = k.m.cmp(head, pred::IULT, next, limit);
        k.m.terminate(head, Term::CondBr(more, head, exit));
        k.m.set_phi(count, vec![(zero, k.entry), (next, head)]);
        let last = k.m.phi(exit, i32t);
        k.m.set_phi(last, vec![(next, head)]);
        k.m.terminate(exit, Term::Ret);
        let (old, new) = k.m.split_before(more).expect("an instruction of a block");
        assert_eq!(old, head);
        assert!(k.m.blocks[old].term.is_none() && k.m.blocks[old].body.len() == 1);
        assert!(matches!(k.m.blocks[new].term, Some(Term::CondBr(_, t, f)) if t == head && f == exit));
        let incoming = |m: &Module, phi: V| match (phi, m) {
            (V::Inst(i), m) => match &m.insts[i as usize].op {
                super::super::Op::Phi(inc) => inc.iter().map(|x| x.1).collect::<Vec<_>>(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        assert_eq!(incoming(&k.m, count), [k.entry, new]);
        assert_eq!(incoming(&k.m, last), [new]);
        assert!(k.m.split_before(zero).is_none());
    }
}

/// A descriptor a finished module takes from a heap ([`Module::heap_views`]).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HeapView {
    /// Its index with the user data `heap_views` was given; None where it is not made of
    /// constants and user data alone.
    pub index: Option<u32>,
    pub sampler: bool,
    /// The two words the handle is annotated with (the kind of resource and its type).
    pub props: (u32, u32),
}

#[cfg(test)]
impl Module {
    /// The descriptors `main` takes from the heaps, for a run with made-up resources: each
    /// `createHandleFromHeap` with its index worked out from constants and the user-data
    /// dwords `user` gives (the constant buffer at `b0`), and what its handle is annotated as.
    pub(crate) fn heap_views(&self, user: &dyn Fn(u32) -> u32) -> Vec<HeapView> {
        let name = |f: u32| self.funcs[f as usize].name.as_str();
        let mut out = Vec::new();
        for (i, inst) in self.insts.iter().enumerate() {
            let Op::Call(f, args) = &inst.op else { continue };
            if !name(*f).starts_with("dx.op.annotateHandle") {
                continue;
            }
            let (Some(&V::Inst(h)), Some(&V::Const(p))) = (args.get(1), args.get(2)) else { continue };
            let Op::Call(hf, hargs) = &self.insts[h as usize].op else { continue };
            if !name(*hf).starts_with("dx.op.createHandleFromHeap") {
                continue;
            }
            let props = match &self.consts[p as usize].1 {
                Const::Aggregate(ids) => {
                    let word = |k: usize| ids.get(k).and_then(|&c| self.int_value(V::Const(c))).unwrap_or(0) as u32;
                    (word(0), word(1))
                }
                _ => (0, 0),
            };
            let _ = i;
            out.push(HeapView {
                index: hargs.get(1).and_then(|&v| self.folded(v, user, 0)),
                sampler: hargs.get(2).and_then(|&v| self.int_value(v)) == Some(1),
                props,
            });
        }
        out.sort();
        out.dedup();
        out
    }

    /// The 32-bit value of `v` where it is made of constants and user-data dwords alone.
    fn folded(&self, v: V, user: &dyn Fn(u32) -> u32, depth: u32) -> Option<u32> {
        if let Some(x) = self.int_value(v) {
            return Some(x as u32);
        }
        let V::Inst(i) = v else { return None };
        if depth > 64 {
            return None;
        }
        let f = |x: V| self.folded(x, user, depth + 1);
        match &self.insts[i as usize].op {
            Op::Bin(op, a, b) => {
                let (a, b) = (f(*a)?, f(*b)?);
                Some(match op {
                    Bin::Add => a.wrapping_add(b),
                    Bin::Sub => a.wrapping_sub(b),
                    Bin::Mul => a.wrapping_mul(b),
                    Bin::UDiv => a.checked_div(b)?,
                    Bin::URem => a.checked_rem(b)?,
                    Bin::Shl => a.checked_shl(b)?,
                    Bin::LShr => a.checked_shr(b)?,
                    Bin::And => a & b,
                    Bin::Or => a | b,
                    Bin::Xor => a ^ b,
                    Bin::SDiv | Bin::AShr => return None,
                })
            }
            Op::Cast(Cast::ZExt | Cast::Trunc | Cast::Bitcast, a) => f(*a),
            Op::Select(c, a, b) => {
                if f(*c)? & 1 != 0 {
                    f(*a)
                } else {
                    f(*b)
                }
            }
            Op::Extract(agg, k) => {
                // a dword of the user data: `cbufferLoadLegacy` of the handle bound at `b0`
                let V::Inst(load) = *agg else { return None };
                let Op::Call(lf, largs) = &self.insts[load as usize].op else { return None };
                if !self.funcs[*lf as usize].name.starts_with("dx.op.cbufferLoadLegacy") {
                    return None;
                }
                let mut h = *largs.get(1)?;
                // (through the handle's annotation to what made it)
                while let V::Inst(x) = h {
                    match &self.insts[x as usize].op {
                        Op::Call(af, aargs) if self.funcs[*af as usize].name.starts_with("dx.op.annotateHandle") => {
                            h = *aargs.get(1)?;
                        }
                        Op::Call(bf, bargs) if self.funcs[*bf as usize].name.starts_with("dx.op.createHandleFromBinding") => {
                            // (register 0 of space 0, not a root view's)
                            let Some(&V::Const(bind)) = bargs.get(1) else { return None };
                            let Const::Aggregate(ids) = &self.consts[bind as usize].1 else { return None };
                            if ids.iter().take(3).any(|&c| self.int_value(V::Const(c)) != Some(0)) {
                                return None;
                            }
                            let row = self.int_value(*largs.get(2)?)? as u32;
                            return Some(user(row * 4 + k));
                        }
                        _ => return None,
                    }
                }
                None
            }
            Op::Call(cf, cargs) if self.funcs[*cf as usize].name.starts_with("dx.op.binary") => {
                let (a, b) = (f(*cargs.get(1)?)?, f(*cargs.get(2)?)?);
                match self.int_value(*cargs.first()?)? {
                    39 => Some(a.max(b)),
                    40 => Some(a.min(b)),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}
