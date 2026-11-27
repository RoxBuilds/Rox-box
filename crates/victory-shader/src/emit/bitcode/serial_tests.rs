//! The lane-serial rewrite against a model of the console's waves, on random programs: a
//! module for one thread a lane is run with its threads in step (every thread at a block runs
//! each instruction before any runs the next, the blocks in program order, a wave operation
//! over the lanes of the wave that are at the block), the rewritten module one thread after
//! another, and both must leave the same in the output buffer and in shared memory.

use super::super::{Attr, Bin, Cast, Const, Module, Op, Rmw, Term, Ty, TypeDef, V, pred};
use super::{Refusal, Serial};

#[derive(Debug, Clone, PartialEq)]
enum Val {
    S(u64),
    A(Vec<u64>),
    /// A place: shared memory (0) or an array of the thread's own (instruction + 1), and the
    /// element.
    P(u32, u64),
}

impl Val {
    fn s(&self) -> u64 {
        match self {
            Val::S(x) => *x,
            v => panic!("{v:?} as a number"),
        }
    }
}

struct Thread {
    vals: Vec<Option<Val>>,
    own: std::collections::HashMap<u32, Vec<u64>>,
    block: usize,
    from: usize,
    at: usize,
    done: bool,
    waits: bool,
}

/// What a run leaves: the output buffer and shared memory.
#[derive(Debug, PartialEq)]
struct Left {
    out: Vec<u32>,
    lds: Vec<u32>,
}

struct Machine<'m> {
    m: &'m Module,
    /// The thread group, and the lanes of a wave (0: no waves, a thread after another).
    threads: [u32; 3],
    wave: u32,
    inputs: &'m [u32],
    t: Vec<Thread>,
    left: Left,
}

fn bits(m: &Module, ty: Ty) -> Option<u32> {
    match m.types[ty.0 as usize] {
        TypeDef::Int(b) => Some(b),
        _ => None,
    }
}

fn mask(b: u32) -> u64 {
    if b >= 64 { u64::MAX } else { (1 << b) - 1 }
}

fn sext(b: u32, x: u64) -> i64 {
    ((x << (64 - b)) as i64) >> (64 - b)
}

/// The lanes' floats added as the console's compiler adds them.
fn tree(mut v: Vec<f32>) -> f32 {
    v.resize(64, 0.0);
    let mut step = 1;
    while step < 64 {
        for i in (0..64).step_by(step * 2) {
            v[i] += v[i + step];
        }
        step *= 2;
    }
    v[0]
}

impl Machine<'_> {
    fn eval(&self, t: usize, v: V) -> Val {
        match v {
            V::Inst(i) => self.t[t].vals[i as usize].clone().unwrap_or_else(|| panic!("value {i} read before it was made")),
            V::Const(c) => match (&self.m.consts[c as usize].1, bits(self.m, self.m.consts[c as usize].0)) {
                (Const::Int(x), Some(b)) => Val::S(*x as u64 & mask(b)),
                (Const::Float(b), _) => Val::S(u64::from(*b)),
                _ => Val::S(0),
            },
            V::Global(_) => Val::P(0, 0),
            V::Func(_) => panic!("a function as a value"),
        }
    }

    fn read(&self, t: usize, p: &Val) -> u64 {
        match *p {
            Val::P(0, k) => u64::from(self.left.lds[k as usize % self.left.lds.len()]),
            Val::P(a, k) => self.t[t].own[&a][k as usize],
            _ => panic!("a read of no place"),
        }
    }

    fn write(&mut self, t: usize, p: &Val, x: u64) {
        match *p {
            Val::P(0, k) => {
                let n = self.left.lds.len();
                self.left.lds[k as usize % n] = x as u32;
            }
            Val::P(a, k) => self.t[t].own.get_mut(&a).expect("an array")[k as usize] = x,
            _ => panic!("a store to no place"),
        }
    }

    /// Instruction `i` in the threads `set` (all at one block), together.
    fn step(&mut self, set: &[usize], i: u32) {
        let inst = &self.m.insts[i as usize];
        let b = bits(self.m, inst.ty);
        if let Op::Call(f, args) = &inst.op {
            let name = self.m.funcs[*f as usize].name.as_str();
            if name.starts_with("dx.op.wave") && name != "dx.op.waveGetLaneIndex" {
                return self.wave_op(set, i, name, args);
            }
        }
        for &t in set {
            let e = |v: V| self.eval(t, v);
            let r = match &inst.op {
                Op::Bin(op, x, y) => {
                    let (x, y) = (e(*x).s(), e(*y).s());
                    Val::S(match b {
                        Some(b) => {
                            (match op {
                                Bin::Add => x.wrapping_add(y),
                                Bin::Sub => x.wrapping_sub(y),
                                Bin::Mul => x.wrapping_mul(y),
                                Bin::And => x & y,
                                Bin::Or => x | y,
                                Bin::Xor => x ^ y,
                                Bin::Shl => x << (y & 63),
                                Bin::LShr => x >> (y & 63),
                                Bin::AShr => (sext(b, x) >> (y & 63)) as u64,
                                Bin::UDiv => x.checked_div(y).unwrap_or(0),
                                Bin::URem => x.checked_rem(y).unwrap_or(0),
                                Bin::SDiv => panic!("no signed divisions here"),
                            }) & mask(b)
                        }
                        None => {
                            let (x, y) = (f32::from_bits(x as u32), f32::from_bits(y as u32));
                            u64::from(
                                (match op {
                                    Bin::Add => x + y,
                                    Bin::Sub => x - y,
                                    _ => x * y,
                                })
                                .to_bits(),
                            )
                        }
                    })
                }
                Op::Cmp(p, x, y) => {
                    let wide = bits(self.m, self.m.type_of(*x)).unwrap_or(32);
                    let (x, y) = (e(*x).s(), e(*y).s());
                    let (sx, sy) = (sext(wide, x), sext(wide, y));
                    Val::S(u64::from(match *p {
                        pred::EQ => x == y,
                        pred::NE => x != y,
                        pred::IUGT => x > y,
                        pred::IUGE => x >= y,
                        pred::IULT => x < y,
                        pred::IULE => x <= y,
                        pred::SGT => sx > sy,
                        pred::SGE => sx >= sy,
                        pred::SLT => sx < sy,
                        _ => sx <= sy,
                    }))
                }
                Op::Select(c, x, y) => {
                    if e(*c).s() != 0 {
                        e(*x)
                    } else {
                        e(*y)
                    }
                }
                Op::Cast(op, x) => {
                    let from = bits(self.m, self.m.type_of(*x));
                    let x = e(*x).s();
                    Val::S(match (op, from, b) {
                        (Cast::Bitcast | Cast::ZExt, _, _) => x,
                        (Cast::SExt, Some(f), Some(t)) => sext(f, x) as u64 & mask(t),
                        (Cast::Trunc, _, Some(t)) => x & mask(t),
                        (Cast::UiToFp, _, _) => u64::from((x as f32).to_bits()),
                        _ => panic!("a cast the generator does not make"),
                    })
                }
                Op::Extract(a, k) => match e(*a) {
                    Val::A(f) => Val::S(f[*k as usize]),
                    v => panic!("a field of {v:?}"),
                },
                Op::Gep(_, p, idx) => match e(*p) {
                    Val::P(a, _) => Val::P(a, e(idx[1]).s()),
                    v => panic!("an element of {v:?}"),
                },
                Op::Alloca(_) => {
                    self.t[t].own.insert(i + 1, vec![0xDEAD_0000 + u64::from(i); 64]);
                    Val::P(i + 1, 0)
                }
                Op::Load(p) => Val::S(self.read(t, &e(*p))),
                Op::Store(p, x) => {
                    let (p, x) = (e(*p), e(*x).s());
                    self.write(t, &p, x);
                    Val::S(0)
                }
                Op::AtomicRmw(op, p, x) => {
                    let (p, x) = (e(*p), e(*x).s());
                    let old = self.read(t, &p);
                    let new = match op {
                        Rmw::Add => old.wrapping_add(x) & mask(32),
                        Rmw::Or => old | x,
                        Rmw::UMax => old.max(x),
                        _ => x,
                    };
                    self.write(t, &p, new);
                    Val::S(old)
                }
                Op::CmpXchg(..) | Op::Phi(_) => panic!("not made here"),
                Op::Call(f, args) => {
                    let a: Vec<u64> = args.iter().map(|&v| e(v).s()).collect();
                    let [x, y, _] = self.threads;
                    let flat = t as u64;
                    match self.m.funcs[*f as usize].name.as_str() {
                        "dx.op.waveGetLaneIndex" => Val::S(flat % u64::from(self.wave.max(1))),
                        "dx.op.flattenedThreadIdInGroup.i32" => Val::S(flat),
                        "dx.op.groupId.i32" => Val::S(0),
                        "dx.op.threadIdInGroup.i32" => Val::S(match a[1] {
                            0 => flat % u64::from(x),
                            1 => flat / u64::from(x) % u64::from(y),
                            _ => flat / u64::from(x * y),
                        }),
                        "dx.op.loadInput.i32" => Val::S(u64::from(self.inputs[(a[0] ^ a[1]) as usize % self.inputs.len()])),
                        "dx.op.bufferLoad.i32" => Val::S(u64::from(self.left.out[(a[0] ^ a[1]) as usize % 4096])),
                        "dx.op.binary.f32" => {
                            let (p, q) = (f32::from_bits(a[1] as u32), f32::from_bits(a[2] as u32));
                            Val::S(u64::from(if a[0] == 35 { p.max(q) } else { p.min(q) }.to_bits()))
                        }
                        "dx.op.barrier" => {
                            self.t[t].waits = a[1] & 1 != 0;
                            Val::S(0)
                        }
                        "dx.op.bufferStore.i32" => {
                            self.left.out[a[0] as usize % 4096] = a[1] as u32;
                            Val::S(0)
                        }
                        name => panic!("{name} is not run here"),
                    }
                }
            };
            self.t[t].vals[i as usize] = Some(r);
        }
    }

    /// A wave operation over the lanes of each wave that are in `set`.
    fn wave_op(&mut self, set: &[usize], i: u32, name: &str, args: &[V]) {
        let wave = self.wave as usize;
        assert!(wave > 0, "a wave operation in a program without waves");
        let ty = self.m.insts[i as usize].ty;
        let float = self.m.types[ty.0 as usize] == TypeDef::Float;
        let mut waves: Vec<usize> = set.iter().map(|t| t / wave).collect();
        waves.dedup();
        for w in waves {
            let lanes: Vec<usize> = set.iter().copied().filter(|t| t / wave == w).collect();
            let x: Vec<u64> = lanes.iter().map(|&t| self.eval(t, args[1]).s()).collect();
            let k = |n: usize| self.eval(lanes[0], args[n]).s();
            let op = name.strip_prefix("dx.op.").unwrap_or(name).split('.').next().unwrap_or("");
            let one = match op {
                "waveReadLaneFirst" => Some(Val::S(x[0])),
                "waveAnyTrue" => Some(Val::S(u64::from(x.iter().any(|&b| b != 0)))),
                "waveAllTrue" => Some(Val::S(u64::from(x.iter().all(|&b| b != 0)))),
                "waveActiveBallot" => {
                    let mut m = 0u64;
                    for (&t, &b) in lanes.iter().zip(&x) {
                        m |= (b & 1) << (t % wave);
                    }
                    Some(Val::A(vec![m & 0xFFFF_FFFF, m >> 32, 0, 0]))
                }
                "waveActiveBit" => Some(Val::S(match k(2) {
                    0 => x.iter().fold(u64::from(u32::MAX), |a, b| a & b),
                    1 => x.iter().fold(0, |a, b| a | b),
                    _ => x.iter().fold(0, |a, b| a ^ b),
                })),
                "waveActiveOp" if float => {
                    let f: Vec<f32> = x.iter().map(|&b| f32::from_bits(b as u32)).collect();
                    Some(Val::S(u64::from(
                        match k(2) {
                            0 => {
                                let mut all = vec![0.0; 64];
                                for (&t, &v) in lanes.iter().zip(&f) {
                                    all[t % wave] = v;
                                }
                                tree(all)
                            }
                            2 => f.iter().copied().fold(f[0], f32::min),
                            _ => f.iter().copied().fold(f[0], f32::max),
                        }
                        .to_bits(),
                    )))
                }
                "waveActiveOp" => Some(Val::S(match (k(2), k(3) & 1) {
                    (0, _) => x.iter().fold(0u64, |a, b| a.wrapping_add(*b)) & mask(32),
                    (2, 1) => x.iter().copied().min().unwrap_or(0),
                    (3, 1) => x.iter().copied().max().unwrap_or(0),
                    (2, _) => x.iter().copied().min_by_key(|&v| sext(32, v)).unwrap_or(0),
                    _ => x.iter().copied().max_by_key(|&v| sext(32, v)).unwrap_or(0),
                })),
                _ => None,
            };
            let mut sum = 0u64;
            for (n, &t) in lanes.iter().enumerate() {
                let r = match (&one, op) {
                    (Some(v), _) => v.clone(),
                    (None, "waveReadLaneAt") => {
                        let at = (self.eval(t, args[2]).s() & 63) as usize;
                        Val::S(lanes.iter().position(|&l| l % wave == at).map_or(0, |p| x[p]))
                    }
                    (None, "wavePrefixOp") => {
                        let r = Val::S(sum);
                        sum = sum.wrapping_add(x[n]) & mask(32);
                        r
                    }
                    (None, op) => panic!("{op} is not run here"),
                };
                self.t[t].vals[i as usize] = Some(r);
            }
        }
    }

    /// The joins of thread `t`'s block take their values, together.
    fn joins(&mut self, t: usize) {
        let b = self.t[t].block;
        let from = self.t[t].from;
        let joined: Vec<(u32, Val)> = self.m.blocks[b]
            .phis
            .iter()
            .map(|&p| {
                let Op::Phi(incoming) = &self.m.insts[p as usize].op else { panic!("a join") };
                let v = incoming
                    .iter()
                    .find(|x| x.1 == from)
                    .unwrap_or_else(|| panic!("join {p} of block {b} has no value from block {from}"));
                (p, self.eval(t, v.0))
            })
            .collect();
        for (p, x) in joined {
            self.t[t].vals[p as usize] = Some(x);
        }
    }

    /// Thread `t` takes the exit of its block.
    fn exit(&mut self, t: usize) {
        let b = self.t[t].block;
        self.t[t].from = b;
        self.t[t].at = 0;
        match self.m.blocks[b].term.expect("a terminator") {
            Term::Ret => self.t[t].done = true,
            Term::Br(to) => self.t[t].block = to,
            Term::CondBr(c, yes, no) => self.t[t].block = if self.eval(t, c).s() != 0 { yes } else { no },
        }
    }
}

/// Runs `m` for one thread group of `threads`; `wave`: the lanes of its waves, whose threads
/// run in step, or 0 for a thread after another between group barriers.
fn run(m: &Module, threads: [u32; 3], wave: u32, inputs: &[u32]) -> Left {
    let n = threads.iter().product::<u32>() as usize;
    let mut k = Machine {
        m,
        threads,
        wave,
        inputs,
        t: (0..n)
            .map(|_| Thread {
                vals: vec![None; m.insts.len()],
                own: Default::default(),
                block: 0,
                from: usize::MAX,
                at: 0,
                done: false,
                waits: false,
            })
            .collect(),
        left: Left { out: vec![0; 4096], lds: vec![0; 64] },
    };
    for _ in 0..2_000_000 {
        if k.t.iter().all(|t| t.done) {
            return k.left;
        }
        if wave > 0 {
            // in step: the first block in program order that a thread is at
            let b = k.t.iter().filter(|t| !t.done).map(|t| t.block).min().unwrap_or(0);
            let set: Vec<usize> = (0..n).filter(|&t| !k.t[t].done && k.t[t].block == b).collect();
            for &t in &set {
                k.joins(t);
            }
            for &i in &m.blocks[b].body {
                k.step(&set, i);
            }
            for &t in &set {
                k.exit(t);
            }
            continue;
        }
        // a thread after another, each until it waits for the group or ends (a block with a
        // barrier is cut at it: the rest runs after the others have come)
        for t in 0..n {
            while !k.t[t].done && !k.t[t].waits {
                let b = k.t[t].block;
                if k.t[t].at == 0 {
                    k.joins(t);
                }
                while k.t[t].at < m.blocks[b].body.len() && !k.t[t].waits {
                    let i = m.blocks[b].body[k.t[t].at];
                    k.t[t].at += 1;
                    k.step(&[t], i);
                }
                if !k.t[t].waits {
                    k.exit(t);
                } else if k.t[t].at == 0 {
                    unreachable!();
                }
            }
        }
        let waiting = k.t.iter().filter(|t| t.waits).count();
        let live = k.t.iter().filter(|t| !t.done).count();
        assert!(waiting == 0 || waiting == live && live == n, "a group barrier {waiting} of {n} threads reached");
        for t in &mut k.t {
            t.waits = false;
        }
    }
    panic!("still running");
}

struct Fns {
    input: V,
    load: V,
    store: V,
    lane: V,
    flat: V,
    tid: V,
    first: V,
    at: V,
    any: V,
    all: V,
    ballot: V,
    bit: V,
    active: V,
    active_f: V,
    prefix: V,
    barrier: V,
}

/// A random structured program for one thread a lane: registers that vary by lane and
/// registers the whole wave holds alike, wave operations where the whole wave is, shared
/// memory (in a group of one wave), branches and loops on both kinds of condition.
struct Gen {
    m: Module,
    rng: u64,
    /// Registers that vary, then registers every lane holds alike.
    regs: Vec<V>,
    varying: usize,
    cur: usize,
    /// Whether the whole wave runs the code being made.
    whole: bool,
    /// Whether the code being made is run once (no loop or branch around it).
    once: bool,
    lds: Option<V>,
    waves: u32,
    /// The lanes every wave of the group has, less one (a power of two less one): a read of
    /// a lane is of one of these.
    lanes: u32,
    f: Fns,
}

impl Gen {
    fn next(&mut self, n: u64) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) % n
    }

    fn k(&mut self) -> V {
        let k = [0u32, 1, 2, 3, 5, 31, 63, u32::MAX, 0x8000_0000, 7, 100][self.next(11) as usize];
        self.m.i32(k)
    }

    /// A register that varies (or any register, or a constant).
    fn vary(&mut self) -> V {
        match self.next(8) {
            0 => self.k(),
            1 | 2 => self.alike(),
            _ => {
                let n = self.next(self.varying as u64) as usize;
                self.regs[n]
            }
        }
    }

    /// A register every lane holds alike, or a constant.
    fn alike(&mut self) -> V {
        match self.next(4) {
            0 => self.k(),
            _ => {
                let n = self.varying + self.next((self.regs.len() - self.varying) as u64) as usize;
                self.regs[n]
            }
        }
    }

    fn arith(&mut self, x: V, y: V) -> V {
        let c = self.cur;
        match self.next(8) {
            0..=4 => {
                let op = [Bin::Add, Bin::Sub, Bin::Mul, Bin::And, Bin::Or, Bin::Xor][self.next(6) as usize];
                self.m.bin(c, op, x, y)
            }
            5 => {
                let by = self.next(32) as u32;
                let by = self.m.i32(by);
                let op = [Bin::Shl, Bin::LShr, Bin::AShr][self.next(3) as usize];
                self.m.bin(c, op, x, by)
            }
            6 => {
                let p = pred::EQ + self.next(10) as u8;
                let t = self.m.cmp(c, p, x, y);
                let z = self.k();
                self.m.select(c, t, y, z)
            }
            _ => {
                // small floats, added
                let (f32t, i32t) = (self.m.float(), self.m.int(32));
                let k7 = self.m.i32(7);
                let (a, b) = (self.m.bin(c, Bin::And, x, k7), self.m.bin(c, Bin::And, y, k7));
                let (a, b) = (self.m.cast(c, Cast::UiToFp, a, f32t), self.m.cast(c, Cast::UiToFp, b, f32t));
                let s = self.m.bin(c, Bin::Add, a, b);
                self.m.cast(c, Cast::Bitcast, s, i32t)
            }
        }
    }

    fn cond(&mut self, alike: bool) -> V {
        let (x, y) = if alike { (self.alike(), self.alike()) } else { (self.vary(), self.vary()) };
        let p = pred::EQ + self.next(10) as u8;
        self.m.cmp(self.cur, p, x, y)
    }

    /// The element of shared memory at `index` (its low six bits).
    fn word(&mut self, index: V) -> V {
        let k63 = self.m.i32(63);
        let at = self.m.bin(self.cur, Bin::And, index, k63);
        let lds = self.lds.expect("shared memory");
        self.m.element(self.cur, lds, at)
    }

    /// `index` as the number of a lane every wave has.
    fn a_lane(&mut self, index: V) -> V {
        let k = self.m.i32(self.lanes);
        self.m.bin(self.cur, Bin::And, index, k)
    }

    fn is_alike(&self, v: V) -> bool {
        matches!(v, V::Const(_)) || self.regs[self.varying..].contains(&v)
    }

    fn statement(&mut self) {
        let c = self.cur;
        let (i32t, f32t) = (self.m.int(32), self.m.float());
        let d = self.next(self.varying as u64) as usize;
        let u = self.varying + self.next((self.regs.len() - self.varying) as u64) as usize;
        let (k0, k1) = (self.m.i8(0), self.m.i8(1));
        let code = self.m.i32(0);
        match self.next(30) {
            8 | 9 if self.whole => {
                let (x, y) = (self.alike(), self.alike());
                self.regs[u] = self.arith(x, y);
            }
            10 | 20 => {
                // each thread's own words of the output
                let flat = self.m.call(c, self.f.flat, vec![code]);
                let slot = self.next(4) as u32;
                let (k16, slot) = (self.m.i32(16), self.m.i32(slot));
                let at = self.m.bin(c, Bin::Mul, flat, k16);
                let at = self.m.bin(c, Bin::Add, at, slot);
                if self.next(3) == 0 {
                    self.regs[d] = self.m.call(c, self.f.load, vec![at, code]);
                } else {
                    let x = self.vary();
                    self.m.call(c, self.f.store, vec![at, x]);
                }
            }
            11 => {
                let n = self.next(3) as u32;
                let n = self.m.i32(n);
                self.regs[d] = match self.next(3) {
                    0 => self.m.call(c, self.f.lane, vec![code]),
                    1 => self.m.call(c, self.f.flat, vec![code]),
                    _ => self.m.call(c, self.f.tid, vec![code, n]),
                };
            }
            // a wave operation: where the whole wave is, or (the wave's lanes that are there)
            // where the lanes went different ways; what it gives is alike only in the first
            12 | 13 | 21 | 26 | 27 if self.whole || self.next(3) == 0 => {
                let x = self.vary();
                let to = if self.whole { u } else { d };
                self.regs[to] = match self.next(6) {
                    0 => self.m.call(c, self.f.first, vec![code, x]),
                    1 if !self.whole => {
                        let x = self.vary();
                        self.m.call(c, self.f.prefix, vec![code, x, k0, k1])
                    }
                    1 => {
                        let at = self.alike();
                        let at = self.a_lane(at);
                        self.m.call(c, self.f.at, vec![code, x, at])
                    }
                    2 => {
                        let kind = self.next(3) as u8;
                        let kind = self.m.i8(kind);
                        self.m.call(c, self.f.bit, vec![code, x, kind])
                    }
                    3 => {
                        let (op, sign) = ([0u8, 2, 3][self.next(3) as usize], self.next(2) as u8);
                        let (op, sign) = (self.m.i8(op), self.m.i8(sign));
                        self.m.call(c, self.f.active, vec![code, x, op, sign])
                    }
                    4 => {
                        let k = self.m.i32(0xFF);
                        let x = self.m.bin(c, Bin::And, x, k);
                        let f = self.m.cast(c, Cast::UiToFp, x, f32t);
                        let k3 = self.m.f32(3.3f32.to_bits());
                        let f = self.m.bin(c, Bin::Mul, f, k3);
                        let op = [0u8, 2, 3][self.next(3) as usize];
                        let op = self.m.i8(op);
                        let r = self.m.call(c, self.f.active_f, vec![code, f, op, k0]);
                        self.m.cast(c, Cast::Bitcast, r, i32t)
                    }
                    _ => {
                        let t = self.cond(false);
                        match self.next(3) {
                            0 => {
                                let r = self.m.call(c, self.f.any, vec![code, t]);
                                self.m.cast(c, Cast::ZExt, r, i32t)
                            }
                            1 => {
                                let r = self.m.call(c, self.f.all, vec![code, t]);
                                self.m.cast(c, Cast::ZExt, r, i32t)
                            }
                            _ => {
                                let r = self.m.call(c, self.f.ballot, vec![code, t]);
                                let (lo, hi) = (self.m.extract(c, r, 0), self.m.extract(c, r, 1));
                                let k3 = self.m.i32(3);
                                let hi = self.m.bin(c, Bin::Mul, hi, k3);
                                self.m.bin(c, Bin::Xor, lo, hi)
                            }
                        }
                    }
                };
            }
            14 | 22 if self.whole => {
                let x = self.vary();
                self.regs[d] = if self.next(2) == 0 {
                    self.m.call(c, self.f.prefix, vec![code, x, k0, k1])
                } else {
                    let at = self.vary();
                    let at = self.a_lane(at);
                    self.m.call(c, self.f.at, vec![code, x, at])
                };
            }
            // shared memory, any word
            15 | 16 | 23 if self.lds.is_some() => {
                let at = if self.next(2) == 0 { self.vary() } else { self.alike() };
                let p = self.word(at);
                let x = self.m.load(self.cur, p);
                if self.whole && self.is_alike(at) && self.next(2) == 0 {
                    self.regs[u] = x;
                } else {
                    self.regs[d] = x;
                }
            }
            17 | 18 | 24 if self.lds.is_some() && (self.whole || self.next(2) == 0) => {
                let at = if self.next(2) == 0 { self.vary() } else { self.alike() };
                let p = self.word(at);
                let x = self.vary();
                match self.next(3) {
                    0 => self.regs[d] = self.m.atomic(self.cur, Rmw::Add, p, x),
                    _ => self.m.store(self.cur, p, x),
                }
            }
            // the group meets
            19 | 25 if self.whole && self.once && self.waves > 1 => {
                let (mode, code) = (self.m.i32(9), self.m.i32(80));
                self.m.call(c, self.f.barrier, vec![code, mode]);
            }
            _ => {
                let (x, y) = (self.vary(), self.vary());
                self.regs[d] = self.arith(x, y);
            }
        }
    }

    fn code(&mut self, depth: u32) {
        for _ in 0..1 + self.next(6) {
            match self.next(9) {
                0 | 1 if depth > 0 => self.branch(depth - 1),
                2 | 3 if depth > 0 => self.repeat(depth - 1),
                _ => self.statement(),
            }
        }
    }

    fn branch(&mut self, depth: u32) {
        let alike = self.whole && self.next(2) == 0;
        let c = self.cond(alike);
        let i32t = self.m.int(32);
        let then = self.m.new_block();
        let (was_whole, was_once) = (self.whole, self.once);
        self.whole &= alike;
        self.once = false;
        let before = self.regs.clone();
        let head = self.cur;
        self.cur = then;
        self.code(depth);
        let (then_regs, then_end) = (std::mem::replace(&mut self.regs, before), self.cur);
        let other = (self.next(2) == 0).then(|| self.m.new_block());
        let mut other_end = head;
        if let Some(o) = other {
            self.cur = o;
            self.code(depth);
            other_end = self.cur;
        }
        let join = self.m.new_block();
        self.m.terminate(head, Term::CondBr(c, then, other.unwrap_or(join)));
        self.m.terminate(then_end, Term::Br(join));
        if other.is_some() {
            self.m.terminate(other_end, Term::Br(join));
        }
        for (r, &t) in then_regs.iter().enumerate() {
            if t != self.regs[r] {
                let p = self.m.phi(join, i32t);
                self.m.set_phi(p, vec![(t, then_end), (self.regs[r], other_end)]);
                self.regs[r] = p;
            }
        }
        self.cur = join;
        (self.whole, self.once) = (was_whole, was_once);
    }

    fn repeat(&mut self, depth: u32) {
        let alike = self.whole && self.next(2) == 0;
        let i32t = self.m.int(32);
        let head = self.m.new_block();
        let before = self.cur;
        self.m.terminate(before, Term::Br(head));
        let entering = self.regs.clone();
        // (a loop the lanes leave one by one changes no register the wave holds alike)
        let joined = if alike { self.regs.len() } else { self.varying };
        let joins: Vec<V> = (0..joined).map(|_| self.m.phi(head, i32t)).collect();
        let count = self.m.phi(head, i32t);
        self.regs[..joined].copy_from_slice(&joins);
        let (was_whole, was_once) = (self.whole, self.once);
        self.whole &= alike;
        self.once = false;
        self.cur = head;
        self.code(depth);
        let one = self.m.i32(1);
        let next = self.m.bin(self.cur, Bin::Add, count, one);
        let limit = 1 + self.next(3) as u32;
        let limit = self.m.i32(limit);
        let mut more = self.m.cmp(self.cur, pred::IULT, next, limit);
        if !alike {
            // each lane leaves on its own
            let t = self.cond(false);
            more = self.m.bin(self.cur, Bin::And, more, t);
        } else if self.next(2) == 0 {
            let t = self.cond(true);
            more = self.m.bin(self.cur, Bin::And, more, t);
        }
        let latch = self.cur;
        let exit = self.m.new_block();
        self.m.terminate(latch, Term::CondBr(more, head, exit));
        for (r, &p) in joins.iter().enumerate() {
            self.m.set_phi(p, vec![(entering[r], before), (self.regs[r], latch)]);
        }
        let zero = self.m.i32(0);
        self.m.set_phi(count, vec![(zero, before), (next, latch)]);
        self.cur = exit;
        (self.whole, self.once) = (was_whole, was_once);
    }
}

/// The program of `seed` for a group of `threads`, and its shared memory.
fn random_program(seed: u64, threads: [u32; 3]) -> (Module, Option<V>) {
    let mut m = Module::default();
    let (i32t, i1t, i8t, f32t, void) = (m.int(32), m.int(1), m.int(8), m.float(), m.void());
    let main_ty = m.func_ty(void, vec![]);
    m.ptr(main_ty);
    m.function("main", main_ty, None, true);
    let f = |m: &mut Module, name: &str, ret, params: Vec<Ty>, attr| {
        let fty = m.func_ty(ret, params);
        m.ptr(fty);
        m.function(name, fty, Some(attr), false)
    };
    let four = m.named_struct("dx.types.fouri32", vec![i32t; 4]);
    let fns = Fns {
        input: f(&mut m, "dx.op.loadInput.i32", i32t, vec![i32t, i32t], Attr::ReadNone),
        load: f(&mut m, "dx.op.bufferLoad.i32", i32t, vec![i32t, i32t], Attr::ReadOnly),
        store: f(&mut m, "dx.op.bufferStore.i32", void, vec![i32t, i32t], Attr::NoUnwind),
        lane: f(&mut m, "dx.op.waveGetLaneIndex", i32t, vec![i32t], Attr::ReadOnly),
        flat: f(&mut m, "dx.op.flattenedThreadIdInGroup.i32", i32t, vec![i32t], Attr::ReadNone),
        tid: f(&mut m, "dx.op.threadIdInGroup.i32", i32t, vec![i32t, i32t], Attr::ReadNone),
        first: f(&mut m, "dx.op.waveReadLaneFirst.i32", i32t, vec![i32t, i32t], Attr::NoUnwind),
        at: f(&mut m, "dx.op.waveReadLaneAt.i32", i32t, vec![i32t, i32t, i32t], Attr::NoUnwind),
        any: f(&mut m, "dx.op.waveAnyTrue", i1t, vec![i32t, i1t], Attr::NoUnwind),
        all: f(&mut m, "dx.op.waveAllTrue", i1t, vec![i32t, i1t], Attr::NoUnwind),
        ballot: f(&mut m, "dx.op.waveActiveBallot", four, vec![i32t, i1t], Attr::NoUnwind),
        bit: f(&mut m, "dx.op.waveActiveBit.i32", i32t, vec![i32t, i32t, i8t], Attr::NoUnwind),
        active: f(&mut m, "dx.op.waveActiveOp.i32", i32t, vec![i32t, i32t, i8t, i8t], Attr::NoUnwind),
        active_f: f(&mut m, "dx.op.waveActiveOp.f32", f32t, vec![i32t, f32t, i8t, i8t], Attr::NoUnwind),
        prefix: f(&mut m, "dx.op.wavePrefixOp.i32", i32t, vec![i32t, i32t, i8t, i8t], Attr::NoUnwind),
        barrier: f(&mut m, "dx.op.barrier", void, vec![i32t, i32t], Attr::NoDuplicate),
    };
    let b = m.new_block();
    let waves = threads.iter().product::<u32>().div_ceil(64);
    let lds = (waves == 1).then(|| {
        let arr = m.array(64, i32t);
        m.global("lds", arr, 3)
    });
    let mut g = Gen {
        m,
        rng: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        regs: Vec::new(),
        varying: 4,
        cur: b,
        whole: true,
        once: true,
        lds,
        waves,
        lanes: {
            let total: u32 = threads.iter().product();
            let least = if total.is_multiple_of(64) { 64 } else { total % 64 };
            (1 << least.ilog2()) - 1
        },
        f: fns,
    };
    for r in 0..7u32 {
        let (x, y) = (g.m.i32(r), g.m.i32(r * 3));
        let mut v = g.m.call(b, g.f.input, vec![x, y]);
        if r < 4 {
            let code = g.m.i32(0);
            let flat = g.m.call(b, g.f.flat, vec![code]);
            let k = g.m.i32(0x9E37_79B9u32.rotate_left(r));
            let t = g.m.bin(b, Bin::Mul, flat, k);
            v = g.m.bin(b, Bin::Xor, v, t);
        }
        g.regs.push(v);
    }
    g.code(3);
    g.code(2);
    g.code(2);
    // every register is read at the end: each thread's, at words of its own
    let code = g.m.i32(0);
    let flat = g.m.call(g.cur, g.f.flat, vec![code]);
    let k16 = g.m.i32(16);
    let base = g.m.bin(g.cur, Bin::Mul, flat, k16);
    for r in 0..g.regs.len() {
        let n = g.m.i32(4 + r as u32);
        let at = g.m.bin(g.cur, Bin::Add, base, n);
        g.m.call(g.cur, g.f.store, vec![at, g.regs[r]]);
    }
    g.m.terminate(g.cur, Term::Ret);
    (g.m, g.lds)
}

fn inputs_of(seed: u64, n: u32) -> Vec<u32> {
    (0..9u32)
        .map(|i| match n {
            0 => 0,
            1 => i,
            2 => u32::MAX - i,
            _ => (seed as u32 ^ n).wrapping_mul(0x9E37_79B9).rotate_left(i * 5) ^ (i << 28),
        })
        .collect()
}

/// Random programs, rewritten, leave what the console's waves leave: in groups of one wave
/// (full and not), of several, of several with a last one that is not full.
#[test]
fn a_wave_a_thread_leaves_what_the_wave_leaves() {
    let (mut made, mut loops, mut arrays) = (0, 0, 0);
    let seeds: u64 = std::env::var("VICTORY_TEST_SERIAL_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    let only: Option<u64> = std::env::var("VICTORY_TEST_SERIAL_ONLY").ok().and_then(|v| v.parse().ok());
    for seed in (1..=seeds).filter(|&s| only.is_none_or(|o| o == s)) {
        let threads = [[64, 1, 1], [8, 8, 1], [4, 4, 2], [128, 1, 1], [16, 8, 2], [100, 1, 1], [5, 3, 1]][seed as usize % 7];
        let (plain, lds) = random_program(seed, threads);
        let total: u32 = threads.iter().product();
        let (mut m, _) = random_program(seed, threads);
        let cfg = Serial { threads, aborts: Vec::new(), lds, rank: Vec::new() };
        let stats = match m.serialise(&cfg) {
            Ok(s) => s,
            Err(e) => panic!("seed {seed}: {e:?}"),
        };
        made += 1;
        loops += stats.loops;
        arrays += stats.arrays;
        let host = [total.div_ceil(64), 1, 1];
        let want: Vec<Left> = (0..4).map(|n| run(&plain, threads, 64, &inputs_of(seed, n))).collect();
        for (n, want) in want.iter().enumerate() {
            let got = run(&m, host, 0, &inputs_of(seed, n as u32));
            assert!(got == *want, "seed {seed}, inputs {n}, threads {threads:?}: {}", differs(&got, want));
        }
        // and after the clean-up passes
        m.optimise(super::super::opt::MODULE);
        for (n, want) in want.iter().enumerate() {
            let got = run(&m, host, 0, &inputs_of(seed, n as u32));
            assert!(got == *want, "seed {seed}, inputs {n}, threads {threads:?}, cleaned up: {}", differs(&got, want));
        }
        m.write();
        let wave_left = m.blocks.iter().flat_map(|b| &b.body).any(|&i| {
            matches!(&m.insts[i as usize].op, Op::Call(f, _) if m.funcs[*f as usize].name.starts_with("dx.op.wave"))
        });
        assert!(!wave_left, "seed {seed}: a wave operation left");
    }
    eprintln!("{made} programs: {loops} loops over the lanes, {arrays} arrays");
    assert!(loops > made * 3 && arrays > made);
}

/// Where two runs first differ.
fn differs(got: &Left, want: &Left) -> String {
    if let Some(k) = (0..got.out.len()).find(|&k| got.out[k] != want.out[k]) {
        return format!("output word {k} (thread {}, word {}): {:#x}, the wave leaves {:#x}", k / 16, k % 16, got.out[k], want.out[k]);
    }
    let k = (0..got.lds.len()).find(|&k| got.lds[k] != want.lds[k]).unwrap_or(0);
    format!("shared word {k}: {:#x}, the wave leaves {:#x}", got.lds[k], want.lds[k])
}

/// The title's group barrier in a block only some lanes run is refused by its block.
#[test]
fn a_group_barrier_where_lanes_went_different_ways_names_its_block() {
    let seed = (1..200).find(|&s| {
        let (m, _) = random_program(s, [128, 1, 1]);
        m.blocks[0].body.iter().any(|&i| matches!(&m.insts[i as usize].op, Op::Call(f, _) if m.funcs[*f as usize].name == "dx.op.barrier"))
    });
    let (mut m, _) = random_program(seed.expect("a program with a barrier"), [128, 1, 1]);
    // a new entry: the lanes below 7 go on to the program, the others return
    let i32t = m.int(32);
    let (entry, rest, out) = (0, m.new_block(), m.new_block());
    m.blocks[rest].body = std::mem::take(&mut m.blocks[entry].body);
    m.blocks[rest].term = m.blocks[entry].term.take();
    for inst in &mut m.insts {
        if let Op::Phi(incoming) = &mut inst.op {
            for (_, from) in incoming.iter_mut().filter(|(_, from)| *from == entry) {
                *from = rest;
            }
        }
    }
    let fty = m.func_ty(i32t, vec![i32t]);
    let lane = m.function("dx.op.waveGetLaneIndex", fty, Some(Attr::ReadOnly), false);
    let code = m.i32(111);
    let l = m.call(entry, lane, vec![code]);
    let k7 = m.i32(7);
    let c = m.cmp(entry, pred::IULT, l, k7);
    m.terminate(entry, Term::CondBr(c, rest, out));
    m.terminate(out, Term::Ret);
    let cfg = Serial { threads: [128, 1, 1], aborts: Vec::new(), lds: None, rank: Vec::new() };
    match m.serialise(&cfg) {
        Err(Refusal::Needs(blocks, _)) => assert_eq!(blocks, [rest]),
        other => panic!("{other:?}"),
    }
}
