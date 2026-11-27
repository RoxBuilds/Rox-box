//! A finished compute module, written for one host thread a console lane with the console's
//! waves of 64 lanes, made "lane-serial": one host thread a console wave, which runs the
//! wave's lanes one after another.
//!
//! A host of 32-lane waves runs a console wave of 64 lanes as two host waves, which must
//! meet (at group barriers, or by themselves) wherever a lane reads another's. Where such a
//! meeting would stand in code that only some threads of the group reach, D3D12 leaves the
//! barrier undefined. This form has no meetings: a host thread is a whole console wave, so
//! what one lane reads of another is in the thread's own memory.
//!
//! How the module is rewritten ([`Module::serialise`]):
//!
//! - A value is **uniform** when every lane of the wave holds the same: constants, the
//!   group's id, what a wave operation gives the whole wave (a ballot, a sum, a first-lane
//!   read), a read of memory at a uniform place, and whatever an instruction every lane runs
//!   makes of uniform values. Everything else **varies**: the thread's ids and lane number,
//!   what stores and atomics give, joins after a branch the lanes took differently, and all
//!   that is made in a block only some lanes run.
//! - A branch on a value that varies splits the wave: the blocks from it to where its ways
//!   meet again (the blocks reachable from it that do not post-dominate it) are run by each
//!   lane on its own, the whole region in one loop over the lanes. Every other block is run
//!   by the wave together: its uniform instructions once, each run of instructions that
//!   vary in a loop over the lanes.
//! - What varies and is read outside the loop that made it lives in an array of 64 (one
//!   element a lane) in the function's memory, or is made again where it is read when it is
//!   cheap to (a thread id is the loop's counter).
//! - A wave operation is a loop over the lanes' elements, where the whole wave is.
//! - Shared memory of a group that is one console wave: the console runs the wave's lanes in
//!   step, so the compiler leaves the group barriers out, and a lane's read still sees every
//!   other lane's earlier write. A loop over the lanes therefore ends before a store to
//!   shared memory that follows another access of it, and before a read that follows a
//!   store. With several console waves a group the title's barriers are in the code, and
//!   they are group barriers every host thread reaches.
//! - A region of lanes that holds a wave operation, or a store to such shared memory beside
//!   another access of it, cannot run lane after lane: it is **stepped**. Every lane has the
//!   number of the block it is at in an array; again and again the first block in program
//!   order (a loop's blocks before what follows the loop) that some lane is at is run, for
//!   the lanes that are at it, as a whole-wave block is: its runs in loops over those lanes,
//!   its wave operations over those lanes' values. That is how the console runs the region:
//!   its code in order, each instruction for the lanes EXEC has there.
//! - The title's group barrier in a region of lanes is refused, naming the block, so that
//!   the translation takes the branches around it wave-wide and tries again.
//!
//! What a lane-serial program does between two such points is one of the orders the lanes
//! of a wave may run in under HLSL's rules (lane 0 to the end, then lane 1, ...), which is
//! all the title's source could rely on.
//!
//! shortcut: a lane that runs into the translation's loop limit ends its whole host thread
//! (the console would still be in the loop); the blocks it leaves through are given.

use std::collections::{HashMap, HashSet};

use super::opt::{Effect, map_operands};
use super::{Attr, Bin, Cast, Module, Op, Term, Ty, TypeDef, V, for_each_operand, pred, successors};

/// What [`Module::serialise`] needs to know of the program.
pub(crate) struct Serial {
    /// The console's thread group.
    pub threads: [u32; 3],
    /// The blocks a lane leaves the program through at the loop limit.
    pub aborts: Vec<usize>,
    /// The group's shared memory, where its accesses must keep the order the console's wave
    /// gives them (a group of one console wave; see the module notes).
    pub lds: Option<V>,
    /// Per block, its place in the console's code (the more, the later); a block past the
    /// end has its own number. It settles which of two blocks neither of which leads to the
    /// other a stepped region runs first.
    pub rank: Vec<usize>,
}

/// Why a module was not made lane-serial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// These blocks (as they were numbered before the call) hold what needs the whole wave,
    /// and only some lanes run them.
    Needs(Vec<usize>, &'static str),
    /// Something the rewrite has no form for.
    Cannot(&'static str),
}

/// What a rewrite made, for a line of the log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Made {
    /// Arrays of 64 in the function's memory.
    pub arrays: u32,
    /// Loops over the lanes.
    pub loops: u32,
    /// Of those, the ones that run a region of lanes.
    pub regions: u32,
    /// Regions of lanes run block by block.
    pub stepped: u32,
}

/// The lanes of a console wave.
const LANES: u32 = 64;
const NONE: u32 = u32::MAX;
/// The "loop" of an instruction that reads another lane's value: never the one that made it.
const FOREIGN: u32 = u32::MAX - 1;
/// How many instructions are made again rather than kept in an array.
const REMAKE: u32 = 6;
/// In a stepped region's array of the block each lane is at: the lane has left the region,
/// and the lane ran into the loop limit.
const LEFT: u32 = 0x7FFF_FFFF;
const STOPPED: u32 = 0x7FFF_FFFE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seed {
    Lane,
    Flat,
    InGroup(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sync {
    First,
    At,
    Any,
    All,
    Ballot,
    /// 0 and, 1 or, 2 xor.
    Bit(u8),
    /// (0 sum, 2 min, 3 max; unsigned).
    Active(u8, bool),
    Prefix,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Phi,
    /// The operands' values alone.
    Pure,
    /// Memory a store may change.
    Reads,
    /// Stores, atomics, fences: each lane's own.
    Writes,
    Seed(Seed),
    Sync(Sync),
    /// A barrier that waits for the group.
    Barrier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kept {
    No,
    /// The array, and whether an i1 is kept as an i32.
    Array(V, bool),
    Remade,
}

#[derive(Debug, Clone, Copy)]
enum El {
    Inst(u32),
    /// A join that became an array: its read.
    Join(u32),
    /// The stores of the joins a block's exit leads to.
    Copies,
    /// In a stepped region: the block the lane goes to.
    Next,
}

#[derive(Debug, Clone, Copy)]
enum Item {
    Once(u32),
    Run(usize),
}

#[derive(Debug, Default)]
struct Run {
    els: Vec<El>,
    id: u32,
    writes: bool,
    /// 0: no shared memory; 1: reads; 2: a store.
    lds: u8,
}

#[derive(Debug, Default)]
struct Plan {
    items: Vec<Item>,
    /// The run a region of lanes takes into its loop.
    tail: Option<usize>,
}

#[derive(Debug)]
struct Region {
    blocks: Vec<usize>,
    /// The block whose end (its last run and its exit) is in the loop.
    merged: Option<usize>,
    /// Without `merged`: where the loop starts.
    entry: usize,
    id: u32,
    lds: u8,
    head: usize,
    latch: usize,
    after: usize,
    lane: V,
    exit: Option<usize>,
    /// Without `merged`: the blocks the loop is entered from.
    from: Vec<usize>,
    /// Run block by block: the array of the block each lane is at, and the block that picks
    /// the next block to run.
    stepped: Option<(V, usize)>,
}

/// A loop over the lanes: its first block (with the counter), the block of its body, its
/// last block, the counter, and the block after it.
#[derive(Clone, Copy)]
struct Loop {
    head: usize,
    body: usize,
    end: usize,
    lane: V,
    after: usize,
}

/// Where a value is wanted: the loop's lane, the block, the loop.
#[derive(Clone, Copy)]
struct Cx {
    lane: V,
    block: usize,
    id: u32,
}

struct Pass<'m> {
    m: &'m mut Module,
    cfg: &'m Serial,
    kind: Vec<Kind>,
    var: Vec<bool>,
    div: Vec<bool>,
    reached: Vec<bool>,
    /// The loop each varying instruction is in.
    loop_of: Vec<u32>,
    kept: Vec<Kept>,
    /// The instructions as they were before the rewrite (one made again is made of these:
    /// the rewrite gives an instruction the values of its own loop).
    was: Vec<Op>,
    /// The blocks' exits as they were before the rewrite.
    exits: Vec<Option<Term>>,
    copies: HashMap<usize, Vec<(u32, V)>>,
    prefix: HashMap<u32, V>,
    ballots: HashMap<u32, (V, V)>,
    memo: HashMap<(u32, V), V>,
    memo_block: usize,
    /// The lanes of this thread's wave, and the wave's first thread index in its group.
    count: V,
    base: V,
    whole: bool,
    scratch: HashMap<Ty, V>,
    made: Made,
    /// Per block: in a stepped region.
    hard: Vec<bool>,
    /// The stepped block being rewritten: the array of the lanes' blocks, and its number.
    step: Option<(V, V)>,
    /// What the wave operations of stepped blocks gave.
    results: HashMap<u32, V>,
    /// A stepped block's number, and its region's array.
    index: HashMap<usize, (u32, V)>,
    /// The loop a stepped block's lanes take their next block in.
    next_loop: HashMap<usize, u32>,
}

impl Module {
    /// Rewrites `main` (a compute program for one thread a console lane in host waves of 64)
    /// to run a console wave a host thread; see the module notes. The thread group is then
    /// `ceil(threads / 64)` by 1 by 1. A refused module is left in pieces: not to be written.
    pub(crate) fn serialise(&mut self, cfg: &Serial) -> Result<Made, Refusal> {
        if self.blocks.is_empty() {
            return Err(Refusal::Cannot("a program without code"));
        }
        self.tidy();
        let n = self.insts.len();
        let zero = self.i32(0);
        let mut pass = Pass {
            m: self,
            cfg,
            kind: Vec::new(),
            var: vec![false; n],
            div: Vec::new(),
            reached: Vec::new(),
            loop_of: vec![NONE; n],
            kept: vec![Kept::No; n],
            was: Vec::new(),
            exits: Vec::new(),
            copies: HashMap::new(),
            prefix: HashMap::new(),
            ballots: HashMap::new(),
            memo: HashMap::new(),
            memo_block: usize::MAX,
            count: zero,
            base: zero,
            whole: true,
            scratch: HashMap::new(),
            made: Made::default(),
            hard: Vec::new(),
            step: None,
            results: HashMap::new(),
            index: HashMap::new(),
            next_loop: HashMap::new(),
        };
        pass.run()?;
        Ok(pass.made)
    }

    /// Before the rewrite: a branch both of whose ways are one is a jump, and a field taken
    /// from a call's result stands right behind the call (so the two are never apart).
    fn tidy(&mut self) {
        for b in &mut self.blocks {
            if let Some(Term::CondBr(_, t, f)) = b.term
                && t == f
            {
                b.term = Some(Term::Br(t));
            }
        }
        let mut fields: HashMap<u32, Vec<u32>> = HashMap::new();
        for b in 0..self.blocks.len() {
            let mut k = 0;
            while k < self.blocks[b].body.len() {
                let i = self.blocks[b].body[k];
                if let Op::Extract(V::Inst(a), _) = self.insts[i as usize].op
                    && matches!(self.insts[a as usize].op, Op::Call(..) | Op::CmpXchg(..))
                    && (k == 0 || self.blocks[b].body[k - 1] != a && !fields.get(&a).is_some_and(|f| f.contains(&self.blocks[b].body[k - 1])))
                {
                    fields.entry(a).or_default().push(i);
                    self.blocks[b].body.remove(k);
                    continue;
                }
                if let Op::Extract(V::Inst(a), _) = self.insts[i as usize].op
                    && matches!(self.insts[a as usize].op, Op::Call(..) | Op::CmpXchg(..))
                {
                    // (already behind its call: it stays, and the moved ones follow it)
                    fields.entry(a).or_default();
                }
                k += 1;
            }
        }
        if fields.values().all(Vec::is_empty) {
            return;
        }
        for b in 0..self.blocks.len() {
            let old = std::mem::take(&mut self.blocks[b].body);
            let mut body = Vec::with_capacity(old.len());
            let mut k = 0;
            while k < old.len() {
                let i = old[k];
                body.push(i);
                k += 1;
                if let Some(moved) = fields.get(&i) {
                    // behind the fields that were already there
                    while k < old.len() && matches!(self.insts[old[k] as usize].op, Op::Extract(V::Inst(a), _) if a == i) {
                        body.push(old[k]);
                        k += 1;
                    }
                    body.extend(moved);
                }
            }
            self.blocks[b].body = body;
        }
    }
}

impl Pass<'_> {
    fn run(&mut self) -> Result<(), Refusal> {
        self.classify()?;
        self.spread();
        self.stepped()?;
        let (joins, slots) = self.joins()?;
        let (mut regions, region_of, merged_into) = self.regions()?;
        // (the arrays and loop counters made so far are instructions too: uniform, in no loop)
        let n = self.m.insts.len();
        self.kind.resize(n, Kind::Pure);
        self.var.resize(n, false);
        self.loop_of.resize(n, NONE);
        self.kept.resize(n, Kept::No);
        let (plans, mut runs) = self.plan(&joins, &regions, &region_of, &merged_into)?;
        self.keep(&joins, &slots, &plans, &runs, &regions, &region_of, &merged_into)?;
        self.share(&joins);
        self.was = self.m.insts.iter().map(|i| i.op.clone()).collect();
        self.exits = self.m.blocks.iter().map(|b| b.term).collect();
        self.emit(&joins, &plans, &mut runs, &mut regions, &region_of, &merged_into)
    }

    fn is_var(&self, v: V) -> bool {
        matches!(v, V::Inst(i) if self.var.get(i as usize).copied().unwrap_or(false))
    }

    fn name(&self, f: u32) -> &str {
        let name = self.m.funcs[f as usize].name.as_str();
        let name = name.strip_prefix("dx.op.").unwrap_or(name);
        name.split('.').next().unwrap_or(name)
    }

    /// What each instruction is to the rewrite.
    fn classify(&mut self) -> Result<(), Refusal> {
        let effects = self.m.effects();
        // (an instruction the clean-up took out of its block is not looked at: the functions
        // it names may have been renumbered since)
        let mut placed = vec![false; self.m.insts.len()];
        for b in &self.m.blocks {
            for &i in b.phis.iter().chain(&b.body) {
                placed[i as usize] = true;
            }
        }
        let mut kind = Vec::with_capacity(self.m.insts.len());
        for (i, inst) in self.m.insts.iter().enumerate() {
            if !placed[i] {
                kind.push(Kind::Pure);
                continue;
            }
            let k = match &inst.op {
                Op::Phi(_) => Kind::Phi,
                Op::Bin(..) | Op::Cast(..) | Op::Cmp(..) | Op::Select(..) | Op::Extract(..) | Op::Gep(..) => Kind::Pure,
                Op::Load(p) => {
                    let base = match *p {
                        V::Inst(g) => match &self.m.insts[g as usize].op {
                            Op::Gep(_, base, _) => *base,
                            _ => *p,
                        },
                        _ => *p,
                    };
                    match base {
                        V::Global(g) if self.m.globals[g as usize].init.is_some() => Kind::Pure,
                        _ => Kind::Reads,
                    }
                }
                Op::Store(..) | Op::AtomicRmw(..) | Op::CmpXchg(..) => Kind::Writes,
                Op::Alloca(_) => return Err(Refusal::Cannot("a program with memory of its own")),
                Op::Call(f, args) => {
                    let k8 = |n: usize| args.get(n).and_then(|&a| self.m.int_value(a));
                    match self.name(*f) {
                        "waveGetLaneIndex" => Kind::Seed(Seed::Lane),
                        "flattenedThreadIdInGroup" => Kind::Seed(Seed::Flat),
                        "threadIdInGroup" => match k8(1) {
                            Some(c @ 0..=2) => Kind::Seed(Seed::InGroup(c as u32)),
                            _ => return Err(Refusal::Cannot("a thread id of no known component")),
                        },
                        "threadId" => return Err(Refusal::Cannot("a dispatch thread id")),
                        "waveGetLaneCount" => Kind::Sync(Sync::Count),
                        "waveReadLaneFirst" => Kind::Sync(Sync::First),
                        "waveReadLaneAt" => Kind::Sync(Sync::At),
                        "waveAnyTrue" => Kind::Sync(Sync::Any),
                        "waveAllTrue" => Kind::Sync(Sync::All),
                        "waveActiveBallot" => Kind::Sync(Sync::Ballot),
                        "waveActiveBit" => match k8(2) {
                            Some(k @ 0..=2) => Kind::Sync(Sync::Bit(k as u8)),
                            _ => return Err(Refusal::Cannot("a wave bit operation of no known kind")),
                        },
                        "waveActiveOp" => match (k8(2), k8(3)) {
                            (Some(op @ (0 | 2 | 3)), Some(sign)) => Kind::Sync(Sync::Active(op as u8, sign & 1 != 0)),
                            _ => return Err(Refusal::Cannot("a wave reduction of no known kind")),
                        },
                        "wavePrefixOp" => match k8(2) {
                            Some(0) => Kind::Sync(Sync::Prefix),
                            _ => return Err(Refusal::Cannot("a wave prefix of an operation other than a sum")),
                        },
                        "barrier" => match k8(1) {
                            Some(mode) if mode & 1 == 0 => Kind::Writes,
                            _ => Kind::Barrier,
                        },
                        _ => match effects[*f as usize] {
                            Effect::Pure => Kind::Pure,
                            Effect::Reads => Kind::Reads,
                            Effect::Writes => Kind::Writes,
                            Effect::Lanes => return Err(Refusal::Cannot("a wave or quad operation with no loop over the lanes")),
                        },
                    }
                }
            };
            kind.push(k);
        }
        self.kind = kind;
        Ok(())
    }

    fn succ(&self, b: usize) -> Vec<usize> {
        successors(self.m.blocks[b].term).iter().copied().filter(|&s| self.reached[s]).collect()
    }

    fn preds(&self) -> Vec<Vec<usize>> {
        let n = self.m.blocks.len();
        let mut p = vec![Vec::new(); n];
        for b in (0..n).filter(|&b| self.reached[b]) {
            for s in self.succ(b) {
                if !p[s].contains(&b) {
                    p[s].push(b);
                }
            }
        }
        p
    }

    fn splits(&self, b: usize) -> bool {
        matches!(self.m.blocks[b].term, Some(Term::CondBr(c, t, f))
            if self.is_var(c) && self.reached[t] && self.reached[f] && t != f)
    }

    /// Which values vary across the lanes of a wave, and which blocks only some lanes run
    /// (or run a different number of times): the regions of the branches on values that
    /// vary, as `uniform.rs` finds a group's.
    fn spread(&mut self) {
        let n = self.m.blocks.len();
        let mut reached = self.m.reached();
        for &a in &self.cfg.aborts {
            if let Some(r) = reached.get_mut(a) {
                *r = false;
            }
        }
        self.reached = reached;
        self.div = vec![false; n];
        let pdom = self.m.post_dominators(&self.reached);
        let preds = self.preds();
        loop {
            let mut changed = false;
            for b in (0..n).filter(|&b| self.reached[b]) {
                let insts: Vec<u32> = self.m.blocks[b].phis.iter().chain(&self.m.blocks[b].body).copied().collect();
                for i in insts {
                    if self.var[i as usize] {
                        continue;
                    }
                    let inst = &self.m.insts[i as usize];
                    let mut any = false;
                    for_each_operand(&inst.op, |v| any |= self.is_var(v));
                    let v = self.div[b]
                        || match self.kind[i as usize] {
                            Kind::Phi => any || preds[b].iter().any(|&p| self.div[p] || self.splits(p)),
                            Kind::Writes | Kind::Seed(_) | Kind::Sync(Sync::Prefix) => true,
                            Kind::Sync(Sync::At) => match &inst.op {
                                Op::Call(_, args) => self.is_var(args[1]) && self.is_var(args[2]),
                                _ => true,
                            },
                            Kind::Sync(_) | Kind::Barrier => false,
                            Kind::Pure | Kind::Reads => any,
                        };
                    if v {
                        self.var[i as usize] = true;
                        changed = true;
                    }
                }
            }
            for d in 0..n {
                if !(self.reached[d] && self.splits(d)) {
                    continue;
                }
                let mut seen = vec![false; n];
                let mut stack = self.succ(d);
                while let Some(b) = stack.pop() {
                    if seen[b] || b != d && pdom[d][b / 64] >> (b % 64) & 1 == 1 {
                        continue;
                    }
                    seen[b] = true;
                    if !self.div[b] {
                        self.div[b] = true;
                        changed = true;
                    }
                    stack.extend(self.succ(b));
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Which regions of lanes are run block by block ([`Pass::hard`]): those with a wave
    /// operation, and those whose accesses of shared memory need the console's order. A
    /// group barrier in a region of lanes is refused by its block.
    fn stepped(&mut self) -> Result<(), Refusal> {
        let n = self.m.blocks.len();
        let preds = self.preds();
        let (group, ids) = self.groups(&preds);
        let mut barriers = Vec::new();
        let mut hard: HashSet<usize> = HashSet::new();
        for b in (0..n).filter(|&b| self.reached[b] && self.div[b]) {
            for &i in &self.m.blocks[b].body {
                match self.kind[i as usize] {
                    Kind::Sync(Sync::Count) => {}
                    Kind::Sync(_) => {
                        hard.insert(group[b]);
                    }
                    Kind::Barrier if !barriers.contains(&b) => barriers.push(b),
                    _ => {}
                }
            }
        }
        if !barriers.is_empty() {
            return Err(Refusal::Needs(barriers, "a group barrier where not every lane runs"));
        }
        for g in ids {
            let blocks: Vec<usize> = (0..n).filter(|&b| self.reached[b] && self.div[b] && group[b] == g).collect();
            if self.shared(&blocks) == 3 {
                hard.insert(g);
            }
        }
        self.hard = (0..n).map(|b| self.reached[b] && self.div[b] && hard.contains(&group[b])).collect();
        Ok(())
    }

    /// The accesses of shared memory in a region of lanes: none (0), reads only (1), one
    /// store that no lane repeats (2), or more (3: the console's order of them counts).
    fn shared(&self, blocks: &[usize]) -> u8 {
        let mut lds = 0u8;
        for &b in blocks {
            for &i in &self.m.blocks[b].body {
                lds = match (lds, self.lds(i)) {
                    (l, 0) => l,
                    (0, 1) | (1, 1) => 1,
                    (0, 2) if !self.cycles(b, blocks) => 2,
                    _ => 3,
                };
            }
        }
        lds
    }

    /// The array element a value of type `ty` is kept in, and whether it is an i1 kept as an
    /// i32. None: a handle, a pointer, a structure.
    fn element(&mut self, ty: Ty) -> Option<(Ty, bool)> {
        match self.m.types[ty.0 as usize] {
            TypeDef::Int(1) => Some((self.m.int(32), true)),
            TypeDef::Int(8 | 16 | 32 | 64) | TypeDef::Float | TypeDef::Double => Some((ty, false)),
            _ => None,
        }
    }

    fn array(&mut self, ty: Ty) -> Option<(V, bool)> {
        let (elem, wide) = self.element(ty)?;
        let arr = self.m.array(LANES, elem);
        self.made.arrays += 1;
        Some((self.m.alloca(arr), wide))
    }

    /// The joins that become arrays: those that vary in a block the whole wave runs, and
    /// those of a region's block that lanes enter from outside the region's loop. Each
    /// way in then stores the value it brings ([`Pass::copies`], by the block the stores
    /// stand at the end of: the block it comes from when that only jumps, else a new block
    /// on the way). Returns the joins by block, and of each whether it is an i1.
    #[allow(clippy::type_complexity)]
    fn joins(&mut self) -> Result<(HashMap<usize, Vec<u32>>, HashMap<u32, bool>), Refusal> {
        let n = self.m.blocks.len();
        let preds = self.preds();
        // the one block a region of lanes is entered from, when that block's end is in its loop
        let sole = self.sole_sources(&preds);
        let mut joins: HashMap<usize, Vec<u32>> = HashMap::new();
        let mut slots = HashMap::new();
        for x in 0..n {
            if !self.reached[x] {
                continue;
            }
            // (a stepped region's joins all are: its blocks run apart)
            let outside = self.div[x]
                && (self.hard[x] || preds[x].iter().any(|&p| !self.div[p] && sole.get(&x) != Some(&p)));
            let phis = self.m.blocks[x].phis.clone();
            let mut keep = Vec::new();
            for p in phis {
                if self.div[x] && !outside || !self.div[x] && !self.var[p as usize] {
                    keep.push(p);
                    continue;
                }
                let ty = self.m.insts[p as usize].ty;
                let (_, wide) = self.element(ty).ok_or(Refusal::Cannot("a join of handles the lanes bring differently"))?;
                slots.insert(p, wide);
                self.var[p as usize] = true;
                joins.entry(x).or_default().push(p);
                let Op::Phi(incoming) = self.m.insts[p as usize].op.clone() else { unreachable!() };
                for (v, from) in incoming {
                    if self.reached.get(from).copied().unwrap_or(false) {
                        self.copies.entry(from * n + x).or_default().push((p, v));
                    }
                }
            }
            self.m.blocks[x].phis = keep;
        }
        // (keyed by edge so far: now by the block the stores stand in)
        let mut by_edge: Vec<(usize, Vec<(u32, V)>)> = self.copies.drain().collect();
        by_edge.sort_by_key(|e| e.0);
        for (edge, list) in by_edge {
            let (from, x) = (edge / n, edge % n);
            let jumps = matches!(self.m.blocks[from].term, Some(Term::Br(_)));
            if jumps {
                self.copies.insert(from, list);
                continue;
            }
            let e = self.m.new_block();
            self.m.blocks[e].term = Some(Term::Br(x));
            match &mut self.m.blocks[from].term {
                Some(Term::CondBr(_, t, f)) => {
                    if *t == x {
                        *t = e;
                    }
                    if *f == x {
                        *f = e;
                    }
                }
                _ => unreachable!("an edge from a block that returns"),
            }
            // the joins of x that stay joins name the new block
            for &q in &self.m.blocks[x].phis.clone() {
                if let Op::Phi(incoming) = &mut self.m.insts[q as usize].op {
                    for (_, b) in incoming.iter_mut().filter(|(_, b)| *b == from) {
                        *b = e;
                    }
                }
            }
            self.reached.push(true);
            self.div.push(self.div[from] || self.splits_to(from));
            self.hard.push(false);
            self.copies.insert(e, list);
        }
        Ok((joins, slots))
    }

    /// Whether `b` ends in a branch on a value that varies (its ways are then lanes' ways).
    fn splits_to(&self, b: usize) -> bool {
        matches!(self.m.blocks[b].term, Some(Term::CondBr(c, ..)) if self.is_var(c))
    }

    /// Per first block of a region of lanes entered from one block only, whose end can go
    /// into the region's loop (it only jumps, or branches on a value that varies): that block.
    fn sole_sources(&self, preds: &[Vec<usize>]) -> HashMap<usize, usize> {
        let n = self.m.blocks.len();
        let (group, _) = self.groups(preds);
        let mut sources: HashMap<usize, Vec<usize>> = HashMap::new();
        for x in (0..n).filter(|&x| self.reached[x] && self.div[x]) {
            for &p in preds[x].iter().filter(|&&p| !self.div[p]) {
                let s = sources.entry(group[x]).or_default();
                if !s.contains(&p) {
                    s.push(p);
                }
            }
        }
        let mut sole = HashMap::new();
        for x in (0..n).filter(|&x| self.reached[x] && self.div[x]) {
            if let Some([u]) = sources.get(&group[x]).map(Vec::as_slice)
                && preds[x].contains(u)
                && (self.splits(*u) || matches!(self.m.blocks[*u].term, Some(Term::Br(_))))
            {
                sole.insert(x, *u);
            }
        }
        sole
    }

    /// The regions of lanes: the blocks only some lanes run, joined where one leads to
    /// another, and where one branch on a varying value leads to both. Per block its
    /// region's number (its lowest block), and the numbers.
    fn groups(&self, preds: &[Vec<usize>]) -> (Vec<usize>, Vec<usize>) {
        let n = self.m.blocks.len();
        let mut group: Vec<usize> = (0..n).collect();
        fn find(g: &mut [usize], mut b: usize) -> usize {
            while g[b] != b {
                g[b] = g[g[b]];
                b = g[b];
            }
            b
        }
        let join = |g: &mut Vec<usize>, a: usize, b: usize| {
            let (a, b) = (find(g, a), find(g, b));
            if a != b {
                let (lo, hi) = (a.min(b), a.max(b));
                g[hi] = lo;
            }
        };
        for b in (0..n).filter(|&b| self.reached[b]) {
            let succ = self.succ(b);
            if self.div[b] {
                for &s in succ.iter().filter(|&&s| self.div[s]) {
                    join(&mut group, b, s);
                }
            } else if self.splits(b) {
                let inside: Vec<usize> = succ.iter().copied().filter(|&s| self.div[s]).collect();
                for w in inside.windows(2) {
                    join(&mut group, w[0], w[1]);
                }
            }
        }
        let _ = preds;
        let group: Vec<usize> = (0..n).map(|b| find(&mut group, b)).collect();
        let mut ids: Vec<usize> = (0..n).filter(|&b| self.reached[b] && self.div[b]).map(|b| group[b]).collect();
        ids.sort_unstable();
        ids.dedup();
        (group, ids)
    }

    /// Whether instruction `i` reads (1) or writes (2) the group's shared memory.
    fn lds(&self, i: u32) -> u8 {
        let Some(lds) = self.cfg.lds else { return 0 };
        let at = |p: V| match p {
            V::Inst(g) => matches!(&self.m.insts[g as usize].op, Op::Gep(_, base, _) if *base == lds),
            p => p == lds,
        };
        match &self.m.insts[i as usize].op {
            Op::Load(p) if at(*p) => 1,
            Op::Store(p, _) | Op::AtomicRmw(_, p, _) | Op::CmpXchg(p, ..) if at(*p) => 2,
            _ => 0,
        }
    }

    #[allow(clippy::type_complexity)]
    fn regions(&mut self) -> Result<(Vec<Region>, Vec<usize>, HashMap<usize, usize>), Refusal> {
        let n = self.m.blocks.len();
        let preds = self.preds();
        let (group, ids) = self.groups(&preds);
        let mut region_of = vec![usize::MAX; n];
        let mut regions = Vec::new();
        let mut merged_into = HashMap::new();
        for (k, &g) in ids.iter().enumerate() {
            let blocks: Vec<usize> = (0..n).filter(|&b| self.reached[b] && self.div[b] && group[b] == g).collect();
            let mut sources: Vec<usize> = Vec::new();
            let mut entries: Vec<usize> = Vec::new();
            for &x in &blocks {
                region_of[x] = k;
                for &p in preds[x].iter().filter(|&&p| !self.div[p]) {
                    if !sources.contains(&p) {
                        sources.push(p);
                    }
                    if !entries.contains(&x) {
                        entries.push(x);
                    }
                }
            }
            let merged = match sources[..] {
                [u] if self.splits(u) || matches!(self.m.blocks[u].term, Some(Term::Br(_))) => Some(u),
                _ => None,
            };
            if merged.is_none() && (entries.len() != 1 || sources.iter().any(|&u| self.splits(u))) {
                return Err(Refusal::Cannot("a region of lanes entered from several blocks"));
            }
            if let Some(u) = merged
                && merged_into.insert(u, k).is_some()
            {
                return Err(Refusal::Cannot("a block that leads to two regions of lanes"));
            }
            let lds = self.shared(&blocks);
            let hard = lds == 3 || blocks.iter().any(|&b| self.hard[b]);
            for &b in &blocks {
                self.hard[b] = hard;
            }
            // where the wave goes on once every lane is through the region
            let mut exit = None;
            for &b in blocks.iter().chain(merged.iter()) {
                for &t in successors(self.m.blocks[b].term).iter() {
                    if self.reached[t] && !self.div[t] && exit.replace(t).is_some_and(|j| j != t) {
                        return Err(Refusal::Cannot("a region of lanes that ends in two places"));
                    }
                }
            }
            let head = self.m.new_block();
            let latch = self.m.new_block();
            let after = self.m.new_block();
            let i32t = self.m.int(32);
            let lane = self.m.phi(head, i32t);
            self.reached.extend([true; 3]);
            self.div.extend([false; 3]);
            self.hard.extend([false; 3]);
            let stepped = if hard {
                let (pc, _) = self.array(i32t).expect("an array of numbers");
                let pick = self.m.new_block();
                self.reached.push(true);
                self.div.push(false);
                self.hard.push(false);
                self.made.stepped += 1;
                Some((pc, pick))
            } else {
                None
            };
            regions.push(Region {
                blocks,
                merged,
                entry: entries.first().copied().unwrap_or(usize::MAX),
                id: k as u32,
                lds,
                head,
                latch,
                after,
                lane,
                exit,
                from: if merged.is_some() { Vec::new() } else { sources },
                stepped,
            });
        }
        // the blocks of stepped regions, numbered in the order they are run in
        if regions.iter().any(|r| r.stepped.is_some()) {
            let order = self.order();
            for r in &mut regions {
                let Some((pc, _)) = r.stepped else { continue };
                r.blocks.sort_by_key(|&b| order[b]);
                for (j, &b) in r.blocks.iter().enumerate() {
                    self.index.insert(b, (j as u32, pc));
                }
            }
        }
        Ok((regions, region_of, merged_into))
    }

    /// Whether a lane that ran `b` can run it again without leaving `blocks`.
    fn cycles(&self, b: usize, blocks: &[usize]) -> bool {
        let mut seen = HashSet::new();
        let mut stack = self.succ(b);
        while let Some(s) = stack.pop() {
            if s == b {
                return true;
            }
            if blocks.contains(&s) && seen.insert(s) {
                stack.extend(self.succ(s));
            }
        }
        false
    }

    /// The order a stepped region's blocks are run in: per block its place in a reverse
    /// postorder of the program in which a loop's blocks all come before what follows the
    /// loop (the way out of a loop is walked first, so it ends up last), and of two ways
    /// neither of which leaves a loop the one earlier in the console's code comes first.
    fn order(&self) -> Vec<usize> {
        let n = self.m.blocks.len();
        let words = n.div_ceil(64);
        let has = |set: &[u64], b: usize| set[b / 64] >> (b % 64) & 1 == 1;
        let preds = self.preds();
        // dominators
        let mut dom: Vec<Vec<u64>> = vec![vec![u64::MAX; words]; n];
        dom[0] = vec![0; words];
        dom[0][0] = 1;
        let mut changed = true;
        while changed {
            changed = false;
            for b in (1..n).filter(|&b| self.reached[b]) {
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
        // the loops: per block that a branch comes back to, the blocks that get back to it
        let mut loops: Vec<Vec<u64>> = Vec::new();
        let mut heads: HashMap<usize, usize> = HashMap::new();
        for t in (0..n).filter(|&t| self.reached[t]) {
            for h in self.succ(t) {
                if !has(&dom[t], h) {
                    continue;
                }
                let k = *heads.entry(h).or_insert_with(|| {
                    let mut body = vec![0u64; words];
                    body[h / 64] |= 1 << (h % 64);
                    loops.push(body);
                    loops.len() - 1
                });
                let mut stack = vec![t];
                while let Some(x) = stack.pop() {
                    if !has(&loops[k], x) {
                        loops[k][x / 64] |= 1 << (x % 64);
                        stack.extend(preds[x].iter().copied());
                    }
                }
            }
        }
        let rank = |b: usize| self.cfg.rank.get(b).copied().unwrap_or(b);
        // how many loops the way from `b` to `s` leaves
        let leaves = |b: usize, s: usize| loops.iter().filter(|body| has(body, b) && !has(body, s)).count();
        let mut seen = vec![false; n];
        let mut post = Vec::new();
        let ways = |b: usize| -> Vec<usize> {
            let mut s = self.succ(b);
            // (walked first: ends up last)
            s.sort_by_key(|&t| (std::cmp::Reverse(leaves(b, t)), std::cmp::Reverse(rank(t)), std::cmp::Reverse(t)));
            s
        };
        let mut stack = vec![(0usize, ways(0), 0usize)];
        seen[0] = true;
        while let Some(top) = stack.last_mut() {
            let k = top.2;
            top.2 += 1;
            match top.1.get(k).copied() {
                Some(t) if !seen[t] => {
                    seen[t] = true;
                    let w = ways(t);
                    stack.push((t, w, 0));
                }
                Some(_) => {}
                None => {
                    post.push(top.0);
                    stack.pop();
                }
            }
        }
        post.reverse();
        let mut order = vec![usize::MAX; n];
        for (k, &b) in post.iter().enumerate() {
            order[b] = k;
        }
        order
    }

    /// Each whole-wave block, and each block of a stepped region, as what runs once and what
    /// runs in loops over the lanes.
    #[allow(clippy::type_complexity)]
    fn plan(
        &mut self,
        joins: &HashMap<usize, Vec<u32>>,
        regions: &[Region],
        region_of: &[usize],
        merged_into: &HashMap<usize, usize>,
    ) -> Result<(HashMap<usize, Plan>, Vec<Run>), Refusal> {
        let mut plans = HashMap::new();
        let mut runs: Vec<Run> = Vec::new();
        let first = regions.len() as u32;
        // (the blocks of the program: not the loops' own, made since)
        for u in (0..region_of.len()).filter(|&u| self.reached[u] && (!self.div[u] || self.hard[u])) {
            let hard = self.hard[u];
            let mut items: Vec<Item> = Vec::new();
            let mut open: Option<usize> = None;
            let add = |items: &mut Vec<Item>, open: &mut Option<usize>, runs: &mut Vec<Run>, el: El, lds: u8, writes: bool| {
                if let Some(k) = *open
                    && let Item::Run(r) = items[k]
                    && (lds == 2 && runs[r].lds != 0 || lds == 1 && runs[r].lds == 2)
                {
                    *open = None;
                }
                if open.is_none() {
                    runs.push(Run { id: first + runs.len() as u32, ..Run::default() });
                    items.push(Item::Run(runs.len() - 1));
                    *open = Some(items.len() - 1);
                }
                let Some(Item::Run(r)) = open.map(|k| items[k]) else { unreachable!() };
                runs[r].els.push(el);
                runs[r].writes |= writes;
                runs[r].lds = runs[r].lds.max(lds);
            };
            let hoist = |items: &mut Vec<Item>, open: &mut Option<usize>, i: u32| match open {
                Some(k) => {
                    items.insert(*k, Item::Once(i));
                    *k += 1;
                }
                None => items.push(Item::Once(i)),
            };
            for &p in joins.get(&u).map_or(&[][..], Vec::as_slice) {
                add(&mut items, &mut open, &mut runs, El::Join(p), 0, false);
            }
            for &i in &self.m.blocks[u].body.clone() {
                if matches!(self.m.insts[i as usize].op, Op::Alloca(_)) {
                    continue; // (they stay the entry's first instructions)
                }
                let var = self.var[i as usize];
                let args_vary = {
                    let mut any = false;
                    for_each_operand(&self.m.insts[i as usize].op, |v| any |= self.is_var(v));
                    any
                };
                match self.kind[i as usize] {
                    Kind::Sync(Sync::Prefix) => {
                        open = None;
                        items.push(Item::Once(i));
                        add(&mut items, &mut open, &mut runs, El::Inst(i), 0, false);
                    }
                    Kind::Sync(Sync::At) if var => {
                        open = None;
                        add(&mut items, &mut open, &mut runs, El::Inst(i), 0, false);
                    }
                    Kind::Sync(Sync::Count) if var => add(&mut items, &mut open, &mut runs, El::Inst(i), 0, false),
                    // in a stepped block: over the lanes that are at it, which then each take
                    // the result (a ballot's fields are taken by the instructions after it)
                    Kind::Sync(s) if hard => {
                        open = None;
                        items.push(Item::Once(i));
                        if s != Sync::Ballot {
                            add(&mut items, &mut open, &mut runs, El::Inst(i), 0, false);
                        }
                    }
                    Kind::Sync(_) if !args_vary => hoist(&mut items, &mut open, i),
                    Kind::Sync(_) | Kind::Barrier => {
                        open = None;
                        items.push(Item::Once(i));
                    }
                    _ if var => {
                        let writes = self.kind[i as usize] == Kind::Writes;
                        add(&mut items, &mut open, &mut runs, El::Inst(i), self.lds(i), writes);
                    }
                    Kind::Reads => {
                        if let Some(k) = open
                            && let Item::Run(r) = items[k]
                            && runs[r].writes
                        {
                            open = None;
                        }
                        hoist(&mut items, &mut open, i);
                    }
                    _ => hoist(&mut items, &mut open, i),
                }
            }
            if self.copies.contains_key(&u) {
                add(&mut items, &mut open, &mut runs, El::Copies, 0, false);
            }
            if hard {
                add(&mut items, &mut open, &mut runs, El::Next, 0, false);
            }
            let mut tail = None;
            if let Some(&k) = merged_into.get(&u)
                && regions[k].stepped.is_none()
                && let Some(Item::Run(r)) = open.map(|k| items[k])
            {
                let (a, b) = (runs[r].lds, regions[k].lds);
                if a == 0 || b == 0 || a == 1 && b == 1 {
                    tail = Some(r);
                    runs[r].id = regions[k].id;
                }
            }
            plans.insert(u, Plan { items, tail });
        }
        Ok((plans, runs))
    }

    /// Which varying values are read outside the loop that makes them, and how each is had
    /// there: from an array, or made again.
    #[allow(clippy::too_many_arguments)]
    fn keep(
        &mut self,
        joins: &HashMap<usize, Vec<u32>>,
        slots: &HashMap<u32, bool>,
        plans: &HashMap<usize, Plan>,
        runs: &[Run],
        regions: &[Region],
        region_of: &[usize],
        merged_into: &HashMap<usize, usize>,
    ) -> Result<(), Refusal> {
        // the loop of every varying instruction, and of every block's stores of joins
        let mut copies_loop: HashMap<usize, u32> = HashMap::new();
        for (&u, plan) in plans {
            for item in &plan.items {
                let Item::Run(r) = *item else { continue };
                for el in &runs[r].els {
                    match *el {
                        El::Inst(i) | El::Join(i) => self.loop_of[i as usize] = runs[r].id,
                        El::Copies => {
                            copies_loop.insert(u, runs[r].id);
                        }
                        El::Next => {
                            self.next_loop.insert(u, runs[r].id);
                        }
                    }
                }
            }
        }
        for r in regions.iter().filter(|r| r.stepped.is_none()) {
            for &b in &r.blocks {
                for &i in self.m.blocks[b].phis.iter().chain(&self.m.blocks[b].body) {
                    self.loop_of[i as usize] = r.id;
                }
                for &p in joins.get(&b).map_or(&[][..], Vec::as_slice) {
                    self.loop_of[p as usize] = r.id;
                }
                copies_loop.insert(b, r.id);
            }
        }
        // (the arrays themselves are given out once every kept value is known: `share`)
        let unset = self.m.i32(0);
        for (&p, &wide) in slots {
            self.kept[p as usize] = Kept::Array(unset, wide);
        }
        // every read, with the loop it is made in
        let mut uses: Vec<(V, u32)> = Vec::new();
        for b in (0..self.m.blocks.len()).filter(|&b| self.reached[b]) {
            let region = match (self.div[b], self.next_loop.get(&b)) {
                (true, Some(&at)) => Some(at),
                (true, None) => Some(regions[region_of[b]].id),
                (false, _) => None,
            };
            for &i in &self.m.blocks[b].body {
                let at = self.loop_of[i as usize];
                match (&self.m.insts[i as usize].op, self.kind[i as usize]) {
                    (Op::Call(_, args), Kind::Sync(Sync::At)) if self.var[i as usize] => {
                        uses.push((args[1], FOREIGN));
                        uses.push((args[2], at));
                    }
                    // (a ballot's fields are made with it)
                    (Op::Extract(V::Inst(a), _), _) if self.kind[*a as usize] == Kind::Sync(Sync::Ballot) => {}
                    (op, Kind::Sync(_)) => for_each_operand(op, |v| uses.push((v, FOREIGN))),
                    (op, _) => for_each_operand(op, |v| uses.push((v, at))),
                }
            }
            for &q in &self.m.blocks[b].phis {
                if let (Op::Phi(incoming), Some(id)) = (&self.m.insts[q as usize].op, region) {
                    uses.extend(incoming.iter().map(|&(v, _)| (v, id)));
                }
            }
            if let Some(Term::CondBr(c, ..)) = self.m.blocks[b].term {
                // (a stepped region is entered through a loop of its own)
                let enters = || merged_into.get(&b).map(|&k| if regions[k].stepped.is_some() { FOREIGN } else { regions[k].id });
                if let Some(at) = region.or_else(enters) {
                    uses.push((c, at));
                }
            }
            if let Some(list) = self.copies.get(&b) {
                let at = copies_loop.get(&b).copied().unwrap_or(FOREIGN);
                uses.extend(list.iter().map(|&(_, v)| (v, at)));
            }
        }
        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        while let Some((v, at)) = uses.pop() {
            let V::Inst(i) = v else { continue };
            if !self.var[i as usize] || self.loop_of[i as usize] == at || !seen.insert((i, at)) {
                continue;
            }
            if self.kept[i as usize] == Kept::No {
                let ty = self.m.insts[i as usize].ty;
                let again = matches!(self.kind[i as usize], Kind::Pure | Kind::Seed(_));
                self.kept[i as usize] = if again && (self.element(ty).is_none() || self.cost(v, REMAKE) <= REMAKE) {
                    Kept::Remade
                } else {
                    let (_, wide) = self.element(ty).ok_or(Refusal::Cannot("a handle kept from one loop over the lanes to another"))?;
                    Kept::Array(unset, wide)
                };
            }
            if self.kept[i as usize] == Kept::Remade {
                for_each_operand(&self.m.insts[i as usize].op, |o| uses.push((o, at)));
            }
        }
        Ok(())
    }

    /// Gives every kept value its array, one array for values no lane holds at the same time.
    ///
    /// An array's element belongs to its lane, and a lane runs the program the module was
    /// before the rewrite, so two values can share an array when neither is live where the
    /// other is made, in that program: the liveness of the kept values over its blocks (a
    /// value made again where it is read reads the arrays it is made of there; a join that
    /// became an array is made at the end of each block that leads to it, and its values are
    /// read before any is stored). A wave operation reads the other lanes' elements where
    /// every lane it is over stands at that operation, with the value live.
    fn share(&mut self, joins: &HashMap<usize, Vec<u32>>) {
        let kept: Vec<u32> = (0..self.kept.len() as u32).filter(|&i| matches!(self.kept[i as usize], Kept::Array(..))).collect();
        if kept.is_empty() {
            return;
        }
        let number: HashMap<u32, usize> = kept.iter().enumerate().map(|(k, &i)| (i, k)).collect();
        let words = kept.len().div_ceil(64);
        let n = self.m.blocks.len();
        // the kept values an instruction's read of `v` reads
        fn leaves(pass: &Pass, v: V, number: &HashMap<u32, usize>, out: &mut Vec<usize>) {
            let V::Inst(i) = v else { return };
            match pass.kept.get(i as usize) {
                Some(Kept::Array(..)) => out.push(number[&i]),
                Some(Kept::Remade) => for_each_operand(&pass.m.insts[i as usize].op, |o| leaves(pass, o, number, out)),
                _ => {}
            }
        }
        // per block, from its end to its start: (the values made, the values read)
        let mut steps: Vec<Vec<(Vec<usize>, Vec<usize>)>> = vec![Vec::new(); n];
        for b in (0..n).filter(|&b| self.reached[b]) {
            let mut list = Vec::new();
            let mut read = Vec::new();
            if let Some(Term::CondBr(c, ..)) = self.m.blocks[b].term {
                leaves(self, c, &number, &mut read);
            }
            // the joins of the blocks this one leads to: those that became arrays are made
            // here, the others read their values here
            for t in self.succ(b) {
                for &q in &self.m.blocks[t].phis {
                    if let Op::Phi(incoming) = &self.m.insts[q as usize].op {
                        for &(v, from) in incoming {
                            if from == b {
                                leaves(self, v, &number, &mut read);
                            }
                        }
                    }
                }
            }
            list.push((Vec::new(), std::mem::take(&mut read)));
            if let Some(copies) = self.copies.get(&b) {
                let made: Vec<usize> = copies.iter().filter_map(|(p, _)| number.get(p).copied()).collect();
                for &(_, v) in copies {
                    leaves(self, v, &number, &mut read);
                }
                list.push((made, std::mem::take(&mut read)));
            }
            for &i in self.m.blocks[b].body.iter().rev() {
                for_each_operand(&self.m.insts[i as usize].op, |o| leaves(self, o, &number, &mut read));
                list.push((number.get(&i).copied().into_iter().collect(), std::mem::take(&mut read)));
            }
            // the joins that stayed joins are made at the block's start; those that became
            // arrays are read there
            let made: Vec<usize> = self.m.blocks[b].phis.iter().filter_map(|q| number.get(q).copied()).collect();
            let read: Vec<usize> = joins.get(&b).map_or(&[][..], Vec::as_slice).iter().filter_map(|p| number.get(p).copied()).collect();
            list.push((made, Vec::new()));
            list.push((Vec::new(), read));
            steps[b] = list;
        }
        let set = |bits: &mut [u64], k: usize| bits[k / 64] |= 1 << (k % 64);
        let clear = |bits: &mut [u64], k: usize| bits[k / 64] &= !(1 << (k % 64));
        let mut live_in: Vec<Vec<u64>> = vec![vec![0; words]; n];
        let through = |b: usize, mut live: Vec<u64>, clash: Option<&mut Vec<Vec<u64>>>| -> Vec<u64> {
            let mut clash = clash;
            for (made, read) in &steps[b] {
                for &k in made {
                    if let Some(clash) = clash.as_deref_mut() {
                        // (made here: against everything live across this point)
                        for (w, &l) in live.iter().enumerate() {
                            clash[k][w] |= l;
                        }
                        for (w, &l) in live.iter().enumerate() {
                            let mut bits = l;
                            while bits != 0 {
                                let other = w * 64 + bits.trailing_zeros() as usize;
                                bits &= bits - 1;
                                clash[other][k / 64] |= 1 << (k % 64);
                            }
                        }
                    }
                }
                for &k in made {
                    clear(&mut live, k);
                }
                for &k in read {
                    set(&mut live, k);
                }
            }
            live
        };
        let mut changed = true;
        while changed {
            changed = false;
            for b in (0..n).rev().filter(|&b| self.reached[b]) {
                let mut out = vec![0u64; words];
                for t in self.succ(b) {
                    for (w, x) in out.iter_mut().zip(&live_in[t]) {
                        *w |= x;
                    }
                }
                let live = through(b, out, None);
                if live != live_in[b] {
                    live_in[b] = live;
                    changed = true;
                }
            }
        }
        let mut clash: Vec<Vec<u64>> = vec![vec![0; words]; kept.len()];
        for b in (0..n).filter(|&b| self.reached[b]) {
            let mut out = vec![0u64; words];
            for t in self.succ(b) {
                for (w, x) in out.iter_mut().zip(&live_in[t]) {
                    *w |= x;
                }
            }
            // values made together at one point (a block's joins) hold their arrays together
            for (made, _) in &steps[b] {
                for &x in made {
                    for &y in made {
                        if x != y {
                            clash[x][y / 64] |= 1 << (y % 64);
                        }
                    }
                }
            }
            through(b, out, Some(&mut clash));
        }
        // What a lane reads of another lane at a lane of its own choosing stays whole while the
        // loop with the read runs (the other lane may be past it there): an array to itself.
        let mut alone = vec![false; kept.len()];
        for b in (0..n).filter(|&b| self.reached[b]) {
            for &i in &self.m.blocks[b].body {
                if let (Kind::Sync(Sync::At), Op::Call(_, args), true) =
                    (self.kind[i as usize], &self.m.insts[i as usize].op, self.var[i as usize])
                {
                    let mut read = Vec::new();
                    leaves(self, args[1], &number, &mut read);
                    for k in read {
                        alone[k] = true;
                    }
                }
            }
        }
        // an array for each value: the first of its element's arrays that no clashing value has
        let mut arrays: Vec<(Ty, V, Vec<u64>, bool)> = Vec::new();
        for (k, &i) in kept.iter().enumerate() {
            let Kept::Array(_, wide) = self.kept[i as usize] else { unreachable!() };
            let ty = self.m.insts[i as usize].ty;
            let (elem, _) = self.element(ty).expect("checked where it was kept");
            let free = arrays
                .iter()
                .position(|(e, _, users, own)| !alone[k] && !own && *e == elem && users.iter().zip(&clash[k]).all(|(u, c)| u & c == 0));
            let at = match free {
                Some(at) => at,
                None => {
                    let (a, _) = self.array(ty).expect("checked where it was kept");
                    arrays.push((elem, a, vec![0; words], alone[k]));
                    arrays.len() - 1
                }
            };
            set(&mut arrays[at].2, k);
            self.kept[i as usize] = Kept::Array(arrays[at].1, wide);
        }
    }

    /// How many instructions making `v` again takes, from uniform values and thread ids
    /// alone; more than `limit` when it takes more or cannot be done.
    fn cost(&self, v: V, limit: u32) -> u32 {
        let V::Inst(i) = v else { return 0 };
        if !self.var[i as usize] {
            return 0;
        }
        match self.kind[i as usize] {
            Kind::Seed(_) => 1,
            Kind::Pure => {
                let mut total = 1;
                for_each_operand(&self.m.insts[i as usize].op, |o| {
                    if total <= limit {
                        total += self.cost(o, limit - total.min(limit));
                    }
                });
                total
            }
            _ => limit + 1,
        }
    }

    // ---- the rewrite ----

    /// `v` as the lane of `cx` holds it, in `cx.block`.
    fn value(&mut self, v: V, cx: Cx) -> Result<V, Refusal> {
        let V::Inst(i) = v else { return Ok(v) };
        if !self.var.get(i as usize).copied().unwrap_or(false) || self.loop_of[i as usize] == cx.id {
            return Ok(v);
        }
        if self.memo_block != cx.block {
            self.memo.clear();
            self.memo_block = cx.block;
        }
        if let Some(&got) = self.memo.get(&(i, cx.lane)) {
            return Ok(got);
        }
        let got = match self.kept[i as usize] {
            Kept::Array(a, wide) => {
                let p = self.m.element(cx.block, a, cx.lane);
                let x = self.m.load(cx.block, p);
                if wide {
                    let zero = self.m.i32(0);
                    self.m.cmp(cx.block, pred::NE, x, zero)
                } else {
                    x
                }
            }
            Kept::Remade => match self.kind[i as usize] {
                Kind::Seed(s) => self.seed(s, cx),
                _ => {
                    let mut op = self.was[i as usize].clone();
                    let mut reads = Vec::new();
                    for_each_operand(&op, |o| reads.push(o));
                    let mut made = Vec::with_capacity(reads.len());
                    for o in reads {
                        made.push(self.value(o, cx)?);
                    }
                    let mut made = made.into_iter();
                    map_operands(&mut op, |o| made.next().unwrap_or(o));
                    let ty = self.m.insts[i as usize].ty;
                    self.m.push(cx.block, ty, op)
                }
            },
            Kept::No => return Err(Refusal::Cannot("a value read in a loop over the lanes that did not make it")),
        };
        self.memo.insert((i, cx.lane), got);
        Ok(got)
    }

    /// A thread id of the lane of `cx`.
    fn seed(&mut self, s: Seed, cx: Cx) -> V {
        let b = cx.block;
        if s == Seed::Lane {
            return cx.lane;
        }
        let flat = if self.m.int_value(self.base) == Some(0) { cx.lane } else { self.m.bin(b, Bin::Add, self.base, cx.lane) };
        let [x, y, z] = self.cfg.threads;
        let (xk, yk, xy) = (self.m.i32(x), self.m.i32(y), self.m.i32(x * y));
        match s {
            Seed::Lane | Seed::Flat => flat,
            Seed::InGroup(0) if y * z == 1 => flat,
            Seed::InGroup(0) => self.m.bin(b, Bin::URem, flat, xk),
            Seed::InGroup(1) => {
                let t = if x == 1 { flat } else { self.m.bin(b, Bin::UDiv, flat, xk) };
                if z == 1 { t } else { self.m.bin(b, Bin::URem, t, yk) }
            }
            Seed::InGroup(_) => self.m.bin(b, Bin::UDiv, flat, xy),
        }
    }

    /// Makes instruction `i` stand for `v`.
    fn alias(&mut self, i: u32, v: V) {
        let yes = self.m.bool(true);
        self.m.insts[i as usize].op = Op::Select(yes, v, v);
    }

    /// The lane's element of the array `i` is kept in gets its value.
    fn spill(&mut self, i: u32, cx: Cx) {
        let Kept::Array(a, wide) = self.kept[i as usize] else { return };
        self.store(a, wide, V::Inst(i), cx);
    }

    fn store(&mut self, a: V, wide: bool, v: V, cx: Cx) {
        let v = if wide {
            let i32t = self.m.int(32);
            self.m.cast(cx.block, Cast::ZExt, v, i32t)
        } else {
            v
        };
        let p = self.m.element(cx.block, a, cx.lane);
        self.m.store(cx.block, p, v);
    }

    /// One instruction of a loop over the lanes, into `cx.block`.
    fn lane_inst(&mut self, i: u32, cx: Cx) -> Result<(), Refusal> {
        match (self.kind[i as usize], self.m.insts[i as usize].op.clone()) {
            (Kind::Seed(s), _) => {
                let v = self.seed(s, cx);
                self.alias(i, v);
            }
            (Kind::Sync(Sync::Count), _) => {
                let k = self.m.i32(LANES);
                self.alias(i, k);
            }
            (Kind::Sync(Sync::At), Op::Call(_, args)) => {
                let at = self.value(args[2], cx)?;
                let k63 = self.m.i32(LANES - 1);
                let lane = self.m.bin(cx.block, Bin::And, at, k63);
                let x = self.value(args[1], Cx { lane, id: FOREIGN, ..cx })?;
                self.alias(i, x);
            }
            (Kind::Sync(Sync::Prefix), _) => {
                let arr = self.prefix[&i];
                let p = self.m.element(cx.block, arr, cx.lane);
                self.m.insts[i as usize].op = Op::Load(p);
            }
            // (a stepped block's: what its loop over the lanes at the block gave)
            (Kind::Sync(_), _) => {
                let r = *self.results.get(&i).ok_or(Refusal::Cannot("a wave operation read before it was made"))?;
                self.alias(i, r);
            }
            (_, Op::Extract(V::Inst(a), k)) if self.ballots.contains_key(&a) => {
                let (lo, hi) = self.ballots[&a];
                let v = match k {
                    0 => lo,
                    1 => hi,
                    _ => self.m.i32(0),
                };
                self.alias(i, v);
            }
            (_, mut op) => {
                let mut reads = Vec::new();
                for_each_operand(&op, |o| reads.push(o));
                let mut made = Vec::with_capacity(reads.len());
                for o in reads {
                    made.push(self.value(o, cx)?);
                }
                let mut made = made.into_iter();
                map_operands(&mut op, |o| made.next().unwrap_or(o));
                self.m.insts[i as usize].op = op;
            }
        }
        self.m.blocks[cx.block].body.push(i);
        self.spill(i, cx);
        Ok(())
    }

    /// The read of a join that became an array.
    fn join(&mut self, p: u32, cx: Cx) {
        let Kept::Array(a, wide) = self.kept[p as usize] else { unreachable!("a join without its array") };
        let at = self.m.element(cx.block, a, cx.lane);
        if wide {
            let x = self.m.load(cx.block, at);
            let zero = self.m.i32(0);
            self.m.insts[p as usize].op = Op::Cmp(pred::NE, x, zero);
        } else {
            self.m.insts[p as usize].op = Op::Load(at);
        }
        self.m.blocks[cx.block].body.push(p);
    }

    /// The stores of the joins that the exit of block `b` leads to: every value read first.
    fn stores(&mut self, b: usize, cx: Cx) -> Result<(), Refusal> {
        let Some(list) = self.copies.get(&b).cloned() else { return Ok(()) };
        let mut values = Vec::with_capacity(list.len());
        for &(_, v) in &list {
            values.push(self.value(v, cx)?);
        }
        for (&(p, _), v) in list.iter().zip(values) {
            let Kept::Array(a, wide) = self.kept[p as usize] else { unreachable!("a join without its array") };
            self.store(a, wide, v, cx);
        }
        Ok(())
    }

    /// The number, in its region's array, of the block a lane of the stepped block `b` goes
    /// to by the way to `t`.
    fn number(&mut self, t: usize) -> V {
        let k = match self.index.get(&t) {
            Some(&(j, _)) => j,
            None if !self.reached.get(t).copied().unwrap_or(false) => STOPPED,
            None => LEFT,
        };
        self.m.i32(k)
    }

    /// Where the lane of `cx` goes by the exit `term`, stored in the array `pc`.
    fn next(&mut self, term: Option<Term>, pc: V, cx: Cx) -> Result<(), Refusal> {
        let to = match term {
            Some(Term::Br(t)) => self.number(t),
            Some(Term::CondBr(c, t, f)) => {
                let c = self.value(c, cx)?;
                let (t, f) = (self.number(t), self.number(f));
                self.m.select(cx.block, c, t, f)
            }
            // (the lane has left the program)
            _ => self.m.i32(LEFT),
        };
        let p = self.m.element(cx.block, pc, cx.lane);
        self.m.store(cx.block, p, to);
        Ok(())
    }

    /// A loop over the lanes from `cur`. `stepped`: in a stepped block, only the lanes that
    /// are at the block run the loop's body. The caller fills `body` and calls
    /// [`Pass::finish`].
    fn begin(&mut self, cur: usize, stepped: bool) -> Loop {
        let head = self.m.new_block();
        let after = self.m.new_block();
        let i32t = self.m.int(32);
        let lane = self.m.phi(head, i32t);
        self.m.blocks[cur].term = Some(Term::Br(head));
        self.made.loops += 1;
        match self.step {
            Some((pc, k)) if stepped => {
                let (body, end) = (self.m.new_block(), self.m.new_block());
                let p = self.m.element(head, pc, lane);
                let at = self.m.load(head, p);
                let here = self.m.cmp(head, pred::EQ, at, k);
                self.m.blocks[head].term = Some(Term::CondBr(here, body, end));
                Loop { head, body, end, lane, after }
            }
            _ => Loop { head, body: head, end: head, lane, after },
        }
    }

    /// Ends the loop `l` of [`Pass::begin`] from `cur`: `bound` lanes.
    fn finish(&mut self, cur: usize, l: &Loop, bound: V) {
        if l.body != l.end {
            self.m.blocks[l.body].term = Some(Term::Br(l.end));
        }
        let (one, zero) = (self.m.i32(1), self.m.i32(0));
        let next = self.m.bin(l.end, Bin::Add, l.lane, one);
        let more = self.m.cmp(l.end, pred::IULT, next, bound);
        self.m.blocks[l.end].term = Some(Term::CondBr(more, l.head, l.after));
        self.m.set_phi(l.lane, vec![(zero, cur), (next, l.end)]);
    }

    /// In a stepped block: whether the lane `lane` is at the block (in `block`).
    fn here(&mut self, block: usize, lane: V) -> Option<V> {
        let (pc, k) = self.step?;
        let p = self.m.element(block, pc, lane);
        let at = self.m.load(block, p);
        Some(self.m.cmp(block, pred::EQ, at, k))
    }

    /// `yes` where the lane is at the stepped block (`here`), else `no`.
    fn pick(&mut self, block: usize, here: Option<V>, yes: V, no: V) -> V {
        match here {
            Some(h) => self.m.select(block, h, yes, no),
            None => yes,
        }
    }

    /// The first lane a wave operation at `*cur` is over: lane 0, or in a stepped block the
    /// first lane that is at the block (there is one: the block is run for its lanes).
    fn first(&mut self, cur: &mut usize) -> V {
        if self.step.is_none() {
            return self.m.i32(0);
        }
        let i32t = self.m.int(32);
        let pre = *cur;
        let l = self.begin(pre, false);
        let f = self.m.phi(l.body, i32t);
        let here = self.here(l.body, l.lane).expect("a stepped block");
        let none = self.m.i32(LANES);
        let unset = self.m.cmp(l.body, pred::EQ, f, none);
        let takes = self.m.bin(l.body, Bin::And, here, unset);
        let next = self.m.select(l.body, takes, l.lane, f);
        self.m.set_phi(f, vec![(none, pre), (next, l.end)]);
        let count = self.count;
        self.finish(pre, &l, count);
        *cur = l.after;
        let k63 = self.m.i32(LANES - 1);
        self.m.bin(*cur, Bin::And, next, k63)
    }

    fn function(&mut self, name: &str, ret: Ty, params: Vec<Ty>, attr: Attr) -> V {
        let fty = self.m.func_ty(ret, params);
        self.m.ptr(fty);
        self.m.function(name, fty, Some(attr), false)
    }

    /// An instruction a block runs once, at `*cur`.
    fn once(&mut self, i: u32, cur: &mut usize) -> Result<(), Refusal> {
        let op = self.m.insts[i as usize].op.clone();
        match (self.kind[i as usize], op) {
            (Kind::Sync(s), Op::Call(_, args)) => {
                if let Some(r) = self.sync(i, s, &args, cur)? {
                    if self.step.is_some() {
                        // (each lane at the block takes it in the loop that follows)
                        self.results.insert(i, r);
                    } else {
                        self.alias(i, r);
                        self.m.blocks[*cur].body.push(i);
                    }
                }
                Ok(())
            }
            (Kind::Barrier, Op::Call(f, mut args)) => {
                // one host thread a group: nothing to wait for, the fences stay
                if self.whole {
                    let mode = self.m.int_value(args[1]).unwrap_or(1) & !1;
                    if mode == 0 {
                        return Ok(());
                    }
                    args[1] = self.m.i32(mode as u32);
                    self.m.insts[i as usize].op = Op::Call(f, args);
                }
                self.m.blocks[*cur].body.push(i);
                Ok(())
            }
            (_, Op::Extract(V::Inst(a), k)) if self.ballots.contains_key(&a) => {
                let (lo, hi) = self.ballots[&a];
                let v = match k {
                    0 => lo,
                    1 => hi,
                    _ => self.m.i32(0),
                };
                self.alias(i, v);
                self.m.blocks[*cur].body.push(i);
                Ok(())
            }
            _ => {
                self.m.blocks[*cur].body.push(i);
                Ok(())
            }
        }
    }

    /// The wave operation `i` as a loop over the lanes' values, at `*cur` (in a stepped
    /// block: over the lanes that are at the block). Gives its result; none for a ballot
    /// ([`Pass::ballots`]) and a prefix ([`Pass::prefix`]).
    fn sync(&mut self, i: u32, s: Sync, args: &[V], cur: &mut usize) -> Result<Option<V>, Refusal> {
        let ty = self.m.insts[i as usize].ty;
        let zero = self.m.i32(0);
        let count = self.count;
        if s == Sync::Count {
            return Ok(Some(self.m.i32(LANES)));
        }
        let x = args.get(1).copied().ok_or(Refusal::Cannot("a wave operation without its value"))?;
        // what every lane holds alike, one lane holds
        if !self.is_var(x) && matches!(s, Sync::First | Sync::At | Sync::Any | Sync::All | Sync::Bit(0 | 1) | Sync::Active(2 | 3, _)) {
            return Ok(Some(x));
        }
        let float = matches!(self.m.types[ty.0 as usize], TypeDef::Float | TypeDef::Double);
        match s {
            Sync::First | Sync::At => {
                let lane = if s == Sync::First {
                    self.first(cur)
                } else {
                    let k63 = self.m.i32(LANES - 1);
                    self.m.bin(*cur, Bin::And, args[2], k63)
                };
                Ok(Some(self.value(x, Cx { lane, block: *cur, id: FOREIGN })?))
            }
            Sync::Ballot => {
                let i32t = self.m.int(32);
                let pre = *cur;
                let l = self.begin(pre, false);
                let (lo, hi) = (self.m.phi(l.body, i32t), self.m.phi(l.body, i32t));
                let here = self.here(l.body, l.lane);
                let bit = self.value(x, Cx { lane: l.lane, block: l.body, id: FOREIGN })?;
                let bit = self.m.cast(l.body, Cast::ZExt, bit, i32t);
                let bit = self.pick(l.body, here, bit, zero);
                let (k31, k32) = (self.m.i32(31), self.m.i32(32));
                let sh = self.m.bin(l.body, Bin::And, l.lane, k31);
                let bit = self.m.bin(l.body, Bin::Shl, bit, sh);
                let low = self.m.cmp(l.body, pred::IULT, l.lane, k32);
                let to_lo = self.m.select(l.body, low, bit, zero);
                let to_hi = self.m.select(l.body, low, zero, bit);
                let lo1 = self.m.bin(l.body, Bin::Or, lo, to_lo);
                let hi1 = self.m.bin(l.body, Bin::Or, hi, to_hi);
                self.m.set_phi(lo, vec![(zero, pre), (lo1, l.end)]);
                self.m.set_phi(hi, vec![(zero, pre), (hi1, l.end)]);
                self.finish(pre, &l, count);
                self.ballots.insert(i, (lo1, hi1));
                *cur = l.after;
                Ok(None)
            }
            Sync::Prefix => {
                let (arr, _) = self.array(ty).ok_or(Refusal::Cannot("a wave prefix of values with no place in memory"))?;
                let start = if float { self.fzero(ty) } else { self.m.int_const(ty, 0) };
                let pre = *cur;
                let l = self.begin(pre, false);
                let acc = self.m.phi(l.body, ty);
                let here = self.here(l.body, l.lane);
                let p = self.m.element(l.body, arr, l.lane);
                self.m.store(l.body, p, acc);
                let v = self.value(x, Cx { lane: l.lane, block: l.body, id: FOREIGN })?;
                let sum = self.m.bin(l.body, Bin::Add, acc, v);
                let next = self.pick(l.body, here, sum, acc);
                self.m.set_phi(acc, vec![(start, pre), (next, l.end)]);
                self.finish(pre, &l, count);
                self.prefix.insert(i, arr);
                *cur = l.after;
                Ok(None)
            }
            Sync::Active(0, _) if float => Ok(Some(self.float_sum(x, ty, cur)?)),
            Sync::Any | Sync::All | Sync::Bit(_) | Sync::Active(..) => {
                // a least or greatest starts from the first lane's own value (taking it twice
                // changes nothing); the others from what changes nothing
                let start = match s {
                    Sync::Active(2 | 3, _) => {
                        let lane = self.first(cur);
                        self.value(x, Cx { lane, block: *cur, id: FOREIGN })?
                    }
                    Sync::All | Sync::Bit(0) => self.m.int_const(ty, -1),
                    _ => self.m.int_const(ty, 0),
                };
                let pre = *cur;
                let l = self.begin(pre, false);
                let acc = self.m.phi(l.body, ty);
                let here = self.here(l.body, l.lane);
                let v = self.value(x, Cx { lane: l.lane, block: l.body, id: FOREIGN })?;
                let with = match s {
                    Sync::Any | Sync::Bit(1) => self.m.bin(l.body, Bin::Or, acc, v),
                    Sync::All | Sync::Bit(0) => self.m.bin(l.body, Bin::And, acc, v),
                    Sync::Bit(_) => self.m.bin(l.body, Bin::Xor, acc, v),
                    Sync::Active(0, _) => self.m.bin(l.body, Bin::Add, acc, v),
                    Sync::Active(op, _) if float => {
                        // FMax 35, FMin 36, as the host's wave operation combines two lanes
                        let sfx = if self.m.types[ty.0 as usize] == TypeDef::Float { "f32" } else { "f64" };
                        let i32t = self.m.int(32);
                        let f = self.function(&format!("dx.op.binary.{sfx}"), ty, vec![i32t, ty, ty], Attr::ReadNone);
                        let code = self.m.i32(if op == 3 { 35 } else { 36 });
                        self.m.call(l.body, f, vec![code, acc, v])
                    }
                    Sync::Active(op, unsigned) => {
                        let p = match (op, unsigned) {
                            (2, true) => pred::IULT,
                            (2, false) => pred::SLT,
                            (_, true) => pred::IUGT,
                            (_, false) => pred::SGT,
                        };
                        let takes = self.m.cmp(l.body, p, v, acc);
                        self.m.select(l.body, takes, v, acc)
                    }
                    _ => unreachable!(),
                };
                let next = self.pick(l.body, here, with, acc);
                self.m.set_phi(acc, vec![(start, pre), (next, l.end)]);
                self.finish(pre, &l, count);
                *cur = l.after;
                Ok(Some(next))
            }
            Sync::Count => unreachable!(),
        }
    }

    fn fzero(&mut self, ty: Ty) -> V {
        self.m.constant(ty, super::Const::Float(0))
    }

    /// The sum of the lanes' floats `x` as the console's compiler adds them: lanes 2k and
    /// 2k + 1, then the pairs, ... then the wave's two halves (`wave::buddies`); a lane the
    /// wave does not have, or that is elsewhere, adds +0.
    fn float_sum(&mut self, x: V, ty: Ty, cur: &mut usize) -> Result<V, Refusal> {
        if self.m.types[ty.0 as usize] != TypeDef::Float {
            // (a double's zero is not one of this module's constants)
            return self.float_sum_in_order(x, ty, cur);
        }
        let arr = match self.scratch.get(&ty) {
            Some(&a) => a,
            None => {
                let (a, _) = self.array(ty).ok_or(Refusal::Cannot("a wave sum of values with no place in memory"))?;
                self.scratch.insert(ty, a);
                a
            }
        };
        let all = self.m.i32(LANES);
        let fzero = self.fzero(ty);
        let pre = *cur;
        let l = self.begin(pre, false);
        let here = self.here(l.body, l.lane);
        let v = self.value(x, Cx { lane: l.lane, block: l.body, id: FOREIGN })?;
        let v = self.pick(l.body, here, v, fzero);
        let v = if self.m.int_value(self.count) == Some(i64::from(LANES)) {
            v
        } else {
            let has = self.m.cmp(l.body, pred::IULT, l.lane, self.count);
            self.m.select(l.body, has, v, fzero)
        };
        let p = self.m.element(l.body, arr, l.lane);
        self.m.store(l.body, p, v);
        self.finish(pre, &l, all);
        *cur = l.after;
        for step in 0..LANES.trailing_zeros() {
            let pre = *cur;
            let l = self.begin(pre, false);
            let (shift, half) = (self.m.i32(step + 1), self.m.i32(1 << step));
            let at = self.m.bin(l.body, Bin::Shl, l.lane, shift);
            let other = self.m.bin(l.body, Bin::Add, at, half);
            let (pa, pb) = (self.m.element(l.body, arr, at), self.m.element(l.body, arr, other));
            let (a, b) = (self.m.load(l.body, pa), self.m.load(l.body, pb));
            let sum = self.m.bin(l.body, Bin::Add, a, b);
            self.m.store(l.body, pa, sum);
            let pairs = self.m.i32(LANES >> (step + 1));
            self.finish(pre, &l, pairs);
            *cur = l.after;
        }
        let zero = self.m.i32(0);
        let p = self.m.element(*cur, arr, zero);
        Ok(self.m.load(*cur, p))
    }

    /// A sum of doubles, lane after lane from +0 (as an i64's bits, which this module has).
    fn float_sum_in_order(&mut self, x: V, ty: Ty, cur: &mut usize) -> Result<V, Refusal> {
        let i64t = self.m.int(64);
        let k0 = self.m.int_const(i64t, 0);
        let start = self.m.cast(*cur, Cast::Bitcast, k0, ty);
        let count = self.count;
        let pre = *cur;
        let l = self.begin(pre, false);
        let acc = self.m.phi(l.body, ty);
        let here = self.here(l.body, l.lane);
        let v = self.value(x, Cx { lane: l.lane, block: l.body, id: FOREIGN })?;
        let sum = self.m.bin(l.body, Bin::Add, acc, v);
        let next = self.pick(l.body, here, sum, acc);
        self.m.set_phi(acc, vec![(start, pre), (next, l.end)]);
        self.finish(pre, &l, count);
        *cur = l.after;
        Ok(next)
    }

    /// The items of block `u` from `*cur` on: what runs once, and each run in a loop over the
    /// lanes (in a stepped block: over the lanes at the block). Not the run `skip`.
    fn items(&mut self, u: usize, plan: &Plan, runs: &[Run], cur: &mut usize) -> Result<(), Refusal> {
        for item in &plan.items {
            match *item {
                Item::Once(i) => self.once(i, cur)?,
                Item::Run(r) if plan.tail == Some(r) => {}
                Item::Run(r) => {
                    let pre = *cur;
                    let l = self.begin(pre, true);
                    let cx = Cx { lane: l.lane, block: l.body, id: runs[r].id };
                    self.els(u, &runs[r].els, cx)?;
                    let count = self.count;
                    self.finish(pre, &l, count);
                    *cur = l.after;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        joins: &HashMap<usize, Vec<u32>>,
        plans: &HashMap<usize, Plan>,
        runs: &mut [Run],
        regions: &mut [Region],
        region_of: &[usize],
        merged_into: &HashMap<usize, usize>,
    ) -> Result<(), Refusal> {
        let total: u32 = self.cfg.threads.iter().product();
        let waves = total.div_ceil(LANES);
        self.whole = waves == 1;
        // the block each whole-wave block's exit ends up in
        let mut ends: HashMap<usize, usize> = HashMap::new();
        let mut order: Vec<usize> = plans.keys().copied().filter(|&u| !self.div[u]).collect();
        order.sort_unstable();
        debug_assert_eq!(order.first(), Some(&0), "the entry is a block the whole wave runs");
        for &u in &order {
            let old = std::mem::take(&mut self.m.blocks[u].body);
            let term = self.m.blocks[u].term;
            let mut cur = u;
            if u == 0 {
                // (the arrays made so far are the entry's first instructions)
                self.m.blocks[0].body = old.iter().copied().filter(|&i| matches!(self.m.insts[i as usize].op, Op::Alloca(_))).collect();
                self.prologue(waves, total);
            }
            let plan = &plans[&u];
            self.items(u, plan, runs, &mut cur)?;
            match merged_into.get(&u).map(|&k| (k, regions[k].stepped)) {
                Some((_, Some((pc, pick)))) => {
                    // into a stepped region: every lane's first block, by this block's exit
                    let pre = cur;
                    let l = self.begin(pre, false);
                    self.next(term, pc, Cx { lane: l.lane, block: l.body, id: FOREIGN })?;
                    let count = self.count;
                    self.finish(pre, &l, count);
                    self.m.blocks[l.after].term = Some(Term::Br(pick));
                    ends.insert(u, l.after);
                }
                Some((k, None)) => {
                    let tail: Vec<El> = plan.tail.map(|r| std::mem::take(&mut runs[r].els)).unwrap_or_default();
                    let (head, lane, id) = (regions[k].head, regions[k].lane, regions[k].id);
                    self.m.blocks[cur].term = Some(Term::Br(head));
                    regions[k].from = vec![cur];
                    let cx = Cx { lane, block: head, id };
                    self.els(u, &tail, cx)?;
                    let term = match term {
                        Some(Term::CondBr(c, t, f)) => {
                            let c = self.value(c, cx)?;
                            Term::CondBr(c, self.inside(&regions[k], t), self.inside(&regions[k], f))
                        }
                        Some(Term::Br(t)) => Term::Br(self.inside(&regions[k], t)),
                        _ => unreachable!("a region entered from a block that returns"),
                    };
                    self.m.blocks[head].term = Some(term);
                    ends.insert(u, head);
                }
                None => {
                    let to = |t: usize| if self.div.get(t).copied().unwrap_or(false) { regions[region_of[t]].head } else { t };
                    self.m.blocks[cur].term = Some(match term {
                        Some(Term::CondBr(c, t, f)) => Term::CondBr(c, to(t), to(f)),
                        Some(Term::Br(t)) => Term::Br(to(t)),
                        _ => Term::Ret,
                    });
                    ends.insert(u, cur);
                }
            }
        }
        for k in 0..regions.len() {
            match regions[k].stepped {
                Some(_) => self.steps(k, plans, runs, regions)?,
                None => self.region(k, joins, regions, &ends)?,
            }
        }
        // the joins that stayed joins of whole-wave blocks: from where their blocks' exits are now
        for &u in &order {
            for &q in &self.m.blocks[u].phis.clone() {
                if let Op::Phi(incoming) = &mut self.m.insts[q as usize].op {
                    for (_, b) in incoming.iter_mut() {
                        if let Some(&e) = ends.get(b) {
                            *b = e;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The elements of a loop over the lanes, for block `u`.
    fn els(&mut self, u: usize, els: &[El], cx: Cx) -> Result<(), Refusal> {
        for el in els {
            match *el {
                El::Inst(i) => self.lane_inst(i, cx)?,
                El::Join(p) => self.join(p, cx),
                El::Copies => self.stores(u, cx)?,
                El::Next => {
                    let (_, pc) = self.index[&u];
                    self.next(self.exits[u], pc, cx)?;
                }
            }
        }
        Ok(())
    }

    /// Where a way out of a block of region `r` leads within its loop: the block itself
    /// while it is the region's, the block that returns for the loop limit's, else the
    /// loop's end (the lane is done with the region).
    fn inside(&self, r: &Region, t: usize) -> usize {
        if !self.reached.get(t).copied().unwrap_or(false) || r.blocks.contains(&t) { t } else { r.latch }
    }

    /// The stepped region `k`: the block that picks, again and again, the first block in
    /// the region's order that a lane is at, and each block run for the lanes at it.
    fn steps(&mut self, k: usize, plans: &HashMap<usize, Plan>, runs: &[Run], regions: &[Region]) -> Result<(), Refusal> {
        let r = &regions[k];
        let (pc, pick) = r.stepped.expect("a stepped region");
        let i32t = self.m.int(32);
        let count = self.count;
        // entered from several blocks: every lane starts at the region's first block
        if r.merged.is_none() {
            let (at, _) = self.index[&r.entry];
            let at = self.m.i32(at);
            let l = self.begin(r.head, false);
            let p = self.m.element(l.body, pc, l.lane);
            self.m.store(l.body, p, at);
            self.finish(r.head, &l, count);
            self.m.blocks[l.after].term = Some(Term::Br(pick));
        }
        for &b in &r.blocks {
            let (j, _) = self.index[&b];
            let number = self.m.i32(j);
            self.step = Some((pc, number));
            self.m.blocks[b].body.clear();
            let mut cur = b;
            self.items(b, &plans[&b], runs, &mut cur)?;
            self.m.blocks[cur].term = Some(Term::Br(pick));
        }
        self.step = None;
        // the least number a lane holds
        let left = self.m.i32(LEFT);
        let l = self.begin(pick, false);
        let least = self.m.phi(l.body, i32t);
        let p = self.m.element(l.body, pc, l.lane);
        let at = self.m.load(l.body, p);
        let less = self.m.cmp(l.body, pred::IULT, at, least);
        let next = self.m.select(l.body, less, at, least);
        self.m.set_phi(least, vec![(left, pick), (next, l.end)]);
        self.finish(pick, &l, count);
        let mut cur = l.after;
        for &b in &r.blocks {
            let (j, _) = self.index[&b];
            let number = self.m.i32(j);
            let is = self.m.cmp(cur, pred::EQ, next, number);
            let other = self.m.new_block();
            self.m.blocks[cur].term = Some(Term::CondBr(is, b, other));
            cur = other;
        }
        // every lane has left the region, or one ran into the loop limit (which ends the thread)
        let stopped = self.m.i32(STOPPED);
        let is = self.m.cmp(cur, pred::EQ, next, stopped);
        let out = self.m.new_block();
        self.m.blocks[out].term = Some(Term::Ret);
        self.m.blocks[cur].term = Some(Term::CondBr(is, out, r.after));
        self.m.blocks[r.after].term = Some(match r.exit {
            Some(j) => Term::Br(j),
            None => Term::Ret,
        });
        Ok(())
    }

    /// The loop over the lanes that runs region `k`.
    fn region(&mut self, k: usize, joins: &HashMap<usize, Vec<u32>>, regions: &mut [Region], ends: &HashMap<usize, usize>) -> Result<(), Refusal> {
        self.made.loops += 1;
        self.made.regions += 1;
        let (head, latch, after, lane, id) = (regions[k].head, regions[k].latch, regions[k].after, regions[k].lane, regions[k].id);
        let merged = regions[k].merged;
        if merged.is_none() {
            self.m.blocks[head].term = Some(Term::Br(regions[k].entry));
            regions[k].from = regions[k].from.iter().map(|u| ends[u]).collect();
        }
        for b in regions[k].blocks.clone() {
            let cx = Cx { lane, block: b, id };
            let old = std::mem::take(&mut self.m.blocks[b].body);
            for &p in joins.get(&b).map_or(&[][..], Vec::as_slice) {
                self.join(p, cx);
            }
            // (a join that stayed one and is read outside the loop: its array's element)
            for q in self.m.blocks[b].phis.clone() {
                self.spill(q, cx);
            }
            for i in old {
                self.lane_inst(i, cx)?;
            }
            self.stores(b, cx)?;
            let term = match self.m.blocks[b].term {
                Some(Term::CondBr(c, t, f)) => {
                    let c = self.value(c, cx)?;
                    Term::CondBr(c, self.inside(&regions[k], t), self.inside(&regions[k], f))
                }
                Some(Term::Br(t)) => Term::Br(self.inside(&regions[k], t)),
                // the lane has left the program: the next lane
                _ => Term::Br(latch),
            };
            self.m.blocks[b].term = Some(term);
        }
        // the joins inside the region: each value as the lane holds it at the end of the
        // block it comes from (the entering block's end is the loop's first block)
        for b in regions[k].blocks.clone() {
            for &q in &self.m.blocks[b].phis.clone() {
                let Op::Phi(mut incoming) = self.m.insts[q as usize].op.clone() else { continue };
                for (v, from) in &mut incoming {
                    if Some(*from) == merged {
                        *from = head;
                    }
                    *v = self.value(*v, Cx { lane, block: *from, id })?;
                }
                self.m.insts[q as usize].op = Op::Phi(incoming);
            }
        }
        let (one, zero) = (self.m.i32(1), self.m.i32(0));
        let next = self.m.bin(latch, Bin::Add, lane, one);
        let more = self.m.cmp(latch, pred::IULT, next, self.count);
        self.m.blocks[latch].term = Some(Term::CondBr(more, head, after));
        let mut incoming: Vec<(V, usize)> = regions[k].from.iter().map(|&b| (zero, b)).collect();
        incoming.push((next, latch));
        self.m.set_phi(lane, incoming);
        self.m.blocks[after].term = Some(match regions[k].exit {
            Some(j) => Term::Br(j),
            None => Term::Ret,
        });
        Ok(())
    }

    /// At the start of the entry: which console wave of its group this host thread is, and
    /// how many lanes that wave has.
    fn prologue(&mut self, waves: u32, total: u32) {
        let i32t = self.m.int(32);
        if waves == 1 {
            self.base = self.m.i32(0);
            self.count = self.m.i32(total.min(LANES));
            self.unknown();
            return;
        }
        let f = self.function("dx.op.threadIdInGroup.i32", i32t, vec![i32t, i32t], Attr::ReadNone);
        let (code, zero, six) = (self.m.i32(95), self.m.i32(0), self.m.i32(6));
        let wave = self.m.call(0, f, vec![code, zero]);
        self.base = self.m.bin(0, Bin::Shl, wave, six);
        self.count = if total.is_multiple_of(LANES) {
            self.m.i32(LANES)
        } else {
            let (all, k) = (self.m.i32(total), self.m.i32(LANES));
            let left = self.m.bin(0, Bin::Sub, all, self.base);
            let last = self.m.cmp(0, pred::IULT, left, k);
            self.m.select(0, last, left, k)
        };
        self.unknown();
    }

    /// The count of lanes less the top bit of the group's id (which is clear: a dispatch has
    /// at most 65,535 groups a side), so that the host's compiler does not know how often
    /// the loops over the lanes run. Knowing it, NVIDIA's unrolls them: the pipeline of
    /// one compute program with hundreds of such loops took a 32-lane GPU minutes to make, and
    /// a seventh of that with this. `VICTORY_WAVE_SERIAL_UNROLL=1`: the count is the constant
    /// it is.
    fn unknown(&mut self) {
        static PLAIN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *PLAIN.get_or_init(|| std::env::var_os("VICTORY_WAVE_SERIAL_UNROLL").is_some_and(|v| v == "1")) {
            return;
        }
        let i32t = self.m.int(32);
        let f = self.function("dx.op.groupId.i32", i32t, vec![i32t, i32t], Attr::ReadNone);
        let (code, zero, k31) = (self.m.i32(94), self.m.i32(0), self.m.i32(31));
        let group = self.m.call(0, f, vec![code, zero]);
        let top = self.m.bin(0, Bin::LShr, group, k31);
        self.count = self.m.bin(0, Bin::Sub, self.count, top);
    }
}

#[cfg(test)]
#[path = "serial_tests.rs"]
mod tests;
