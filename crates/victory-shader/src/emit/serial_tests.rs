//! Console wave64 compute programs as one host thread a console wave, the wave's lanes one
//! after another (`lower::with_serial`, `bitcode::serial`): the shapes of the programs whose
//! two host waves cannot meet where they must (LDS shared within a wave behind branches and
//! in loops the lanes leave one by one, lanes read across the wave's halves, several console
//! waves a group that go different ways, the title's own group barriers). Each is lowered,
//! validated by `dxil.dll` when present, run on the host GPU, and compared with the console's
//! results lane by lane (the models of `meetings_tests` and `pair_tests`).

use super::host::{Dispatch, Host, Root, View};
use super::meetings_tests::{COUNTS, FLAG_LOOP, FLAG_LOOP_IN_BRANCH, counts_on_the_console, flag_loop_on_the_console, native_lds};
use super::pair_tests::{A_BARRIER_IN_A_BRANCH, STORED, several_waves_native, several_waves_on_the_console};
use super::{HostShader, lower, validator};
use crate::native::{NativeProgram, RootType, StageIo};
use crate::{ir, resource, structure};

/// `native` (a console wave64 compute program) as one host thread a console wave: the form
/// `emit` gives a program none of its other exact forms takes, here asked for by itself.
/// Validated by `dxil.dll` when present; its thread group must be one thread a console wave,
/// and it needs no wave size of the host.
pub(super) fn serial(native: &NativeProgram) -> HostShader {
    let bytes = crate::dxbc::build(&[]);
    let c = crate::Container::parse(&bytes).expect("container");
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let made = lower::with_serial(|| super::widen(&c, native, &p, false, true));
    lower::TEST_HOST_WAVE.set(was);
    let (shader, wave) = made.expect("a wave a thread");
    assert_eq!(wave, None, "no wave size asked of the host");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// Runs `shader` on the hardware device with root constants `constants` (s0..s3), `groups`
/// thread groups, and gives the UAV's first `dwords` dwords; None (and a line) without a
/// device that takes it.
fn on_the_card(shader: &HostShader, constants: [u32; 4], dwords: u32, groups: u32) -> Option<Vec<u32>> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => panic!("the hardware device does not take a program of one thread a wave: {e}"),
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=3) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let out = host
        .dispatch(&root, &pso, &Dispatch {
            roots: vec![(0, Root::Constants(ud))],
            views: &[View::RawUav(dwords)],
            samplers: &[],
            groups: [groups, 1, 1],
        })
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][..dwords as usize].to_vec())
}

/// A flag in LDS set by some lanes and read by the others in one round of a loop the lanes
/// leave one by one, behind a lane branch and not (a group of one console wave: no barrier in
/// the console's code): every lane leaves in the round the console's does.
#[test]
fn a_flag_set_and_read_in_a_loop_lanes_leave() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (words, belows) in [(&FLAG_LOOP_IN_BRANCH[..], &[64u32, 40, 33, 32, 20, 0][..]), (&FLAG_LOOP[..], &[64][..])] {
        let shader = serial(&native_lds(words));
        for &below in belows {
            for (s0, s1) in [(75u32, 7u32), (70, 40), (12, 25), (63, 0), (200, 1)] {
                let (rounds, flag) = flag_loop_on_the_console(s0, s1, below);
                let Some(out) = on_the_card(&shader, [s0, s1, below, 0], 65, 1) else { return };
                assert_eq!(out[0], flag, "flag, s0 {s0} s1 {s1}, lanes below {below}");
                assert_eq!(&out[1..], &rounds[..], "rounds per lane, s0 {s0} s1 {s1}, lanes below {below}");
            }
        }
    }
    eprintln!("ran on the hardware device: every lane left in the console's round");
}

/// LDS adds where some lanes run, beside a read of another word, in a loop of a scalar count,
/// and a word read, changed and written back by one lane and then read by all: every lane
/// reads the console's sum.
#[test]
fn lds_adds_and_one_lanes_update_around_a_uniform_loop() {
    let shader = serial(&native_lds(&COUNTS));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (s0, s1, s2) in [(3u32, 4u32, 64u32), (5, 7, 40), (1, 3, 32), (2, 1, 34), (9, 2, 0)] {
        let want = counts_on_the_console(s0, s1, s2);
        for groups in [1, 300] {
            let Some(out) = on_the_card(&shader, [s0, s1, s2, 0], 64, groups) else { return };
            assert!(out.iter().all(|&w| w == want), "s0 {s0} s1 {s1} s2 {s2}: {out:?}, the console's {want}");
        }
    }
    eprintln!("ran on the hardware device: every lane read the console's sum");
}

/// A group of two console waves that go different ways (two host threads), with LDS shared
/// inside each, a lane of the upper half read by all, and a loop each lane leaves by itself:
/// every thread stores what the console's does, whatever the two waves' counts of rounds,
/// and with many groups at once.
#[test]
fn two_console_waves_that_go_different_ways() {
    let shader = serial(&several_waves_native());
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let cases = [(3u32, 4u32, 64u32, 4u32), (5, 1, 40, 9), (7, 200, 70, 2), (1, 3, 32, 5), (2, 2, 34, 1), (9, 2, 0, 3)];
    for (s0, s1, s2, s3) in cases {
        let want = several_waves_on_the_console(s0, s1, s2, s3);
        let Some(out) = on_the_card(&shader, [s0, s1, s2, s3], STORED, 1) else { return };
        assert_eq!(out, want, "s0 {s0} s1 {s1} s2 {s2} s3 {s3}");
    }
    let want = several_waves_on_the_console(11, 37, 50, 3);
    let Some(out) = on_the_card(&shader, [11, 37, 50, 3], STORED, 4096) else { return };
    assert_eq!(out, want, "4096 groups");
    eprintln!("ran on the hardware device: every thread stored what the console's does");
}

/// The title's own group barrier inside a branch that the whole group takes or leaves
/// together (four console waves: four host threads, which all read the same word of LDS):
/// the barrier stays a group barrier every host thread reaches, and every thread reads the
/// count of waves and its own wave's lane 63.
#[test]
fn the_titles_barrier_in_a_branch_the_whole_group_takes() {
    let mut native = native_lds(&A_BARRIER_IN_A_BRANCH);
    let StageIo::Cs(cs) = &mut native.io else { unreachable!() };
    cs.threads = [256, 1, 1];
    let shader = serial(&native);
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (s0, s1) in [(5u32, 5u32), (5, 6), (0, 0), (7, 1), (1, 7)] {
        let want: Vec<u32> = (0..256u32).map(|t| if s1 > s0 { 0 } else { 4 + (t | 63) }).collect();
        for groups in [1, 512] {
            let Some(out) = on_the_card(&shader, [s0, s1, 0, 0], 256, groups) else { return };
            assert_eq!(out, want, "s0 {s0} s1 {s1}, {groups} groups");
        }
    }
    eprintln!("ran on the hardware device: every thread read the count of console waves and its wave's lane 63");
}

/// `emit` gives a program whose other forms a host of 32-lane waves cannot run this form
/// (with the switch off it is refused as before): a group of two console waves whose halves
/// would meet by themselves, with that form off.
#[test]
fn emit_gives_a_program_nothing_else_translates_this_form() {
    let native = several_waves_native();
    let bytes = crate::dxbc::build(&[]);
    let c = crate::Container::parse(&bytes).expect("container");
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = crate::io::raise(&p, &native).expect("io::raise");
    let was = (lower::TEST_HOST_WAVE.replace(Some(32)), super::WAVE_PAIR_TEST.replace(Some(false)));
    super::WAVE_SERIAL_TEST.set(Some(false));
    let refused = super::emit(&c, &native, &p, &st, &io, &res);
    super::WAVE_SERIAL_TEST.set(Some(true));
    let taken = super::emit(&c, &native, &p, &st, &io, &res);
    let form = super::LAST_FORM.take();
    super::WAVE_SERIAL_TEST.set(None);
    lower::TEST_HOST_WAVE.set(was.0);
    super::WAVE_PAIR_TEST.set(was.1);
    assert!(
        matches!(&refused, Err(crate::ShaderError::Unsupported { what, .. }) if what.starts_with("no wave32 lowering")),
        "without the form the program needs waves of 64"
    );
    let shader = taken.expect("emit");
    assert_eq!(form.as_deref(), Some("a console wave a host thread, its lanes one after another"));
    let got = super::EMITTED.with_borrow(|m| m.get(&super::dxil_key(&shader.dxil)).copied());
    assert_eq!(got, Some((None, 0)), "no wave size, no barrier some thread misses");
}

/// A wave OR inside `if (t < s0)` (`wave64_tests::program`): the lanes that took the branch
/// are stepped through it together, and the OR is over them alone, as EXEC has it on the
/// console; whichever half's lanes take the branch.
#[test]
fn a_wave_or_inside_a_lane_branch() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (skip, name) in [(0xBF88_0000, "s_cbranch_execz"), (0xBF84_0000, "s_cbranch_scc0")] {
        let shader = serial(&super::wave64_tests::program(skip));
        // (threads below s0 take the branch; thread t gives bit t >> 1)
        for (below, expected) in [(64u32, u32::MAX), (40, 0x000F_FFFF), (33, 0x0001_FFFF), (10, 0x1F), (1, 1)] {
            let Some(out) = on_the_card(&shader, [below, 0, 0, 0], 1, 1) else { return };
            assert_eq!(out[0], expected, "{name}: {below} threads in the branch");
        }
    }
    eprintln!("ran on the hardware device: the OR of the lanes in the branch in every case");
}

/// The structure builder's prefix of wave totals (`wave64_tests::wave_prefix_program`): four
/// console waves a group, each of which loops over the totals of the waves before it with a
/// first-lane read every round, behind the title's barrier.
#[test]
fn a_prefix_of_wave_totals_over_four_console_waves() {
    let shader = serial(&super::wave64_tests::wave_prefix_program(false));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for groups in [1, 200] {
        let Some(out) = on_the_card(&shader, [0; 4], 256, groups) else { return };
        for t in 0..256u32 {
            assert_eq!(out[t as usize], (0..(t >> 6)).map(|w| 10 + w).sum::<u32>(), "thread {t}, {groups} groups: {out:?}");
        }
    }
    eprintln!("ran on the hardware device: every console wave got the sum of the waves before it");
}

/// A tiled shadow program's gate (`wave64_tests::far_early_out`) with its early-out left
/// in: the lanes out of bounds leave the program, and the others count the failing lanes of
/// their whole console wave (two console waves a group).
#[test]
fn a_gate_behind_an_early_out_counts_the_lanes_of_its_wave() {
    let shader = serial(&super::wave64_tests::far_early_out(&[], false));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (below, failing) in [(128u32, 40u32), (128, 100), (128, 63), (40, 50), (40, 10), (33, 32), (1, 0), (0, 5)] {
        let Some(got) = on_the_card(&shader, [below, failing, 0, 0], 128, 1) else { return };
        assert_eq!(got, super::wave64_tests::gate_expected(below, failing), "{below} threads in bounds, thread {failing} fails");
    }
    eprintln!("ran on the hardware device: the gate's count over each console wave");
}

/// How long a dispatch of [`several_waves_native`] takes the hardware device as one host
/// thread a console wave, and with the two host waves of each console wave meeting by
/// themselves (the form `emit` gives it first): a line each, for 16,384 groups of 128
/// threads. Not run unless asked for (`--ignored`); it checks only that both store the same.
#[test]
#[ignore = "a measurement"]
fn how_long_a_dispatch_takes_a_wave_a_thread() {
    let native = several_waves_native();
    let lanes = serial(&native);
    let bytes = crate::dxbc::build(&[]);
    let c = crate::Container::parse(&bytes).expect("container");
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = crate::io::raise(&p, &native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let halves = super::emit(&c, &native, &p, &st, &io, &res).expect("emit");
    lower::TEST_HOST_WAVE.set(was);
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let mut stored = Vec::new();
    for (name, shader) in [("a wave a thread", &lanes), ("two host waves a console wave", &halves)] {
        let (root, pso) = host.compute_pipeline(shader).expect("a pipeline");
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, d @ 0..=3) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = [11, 37, 50, 3][d as usize];
            }
        }
        let mut best = std::time::Duration::MAX;
        let mut out = Vec::new();
        for _ in 0..5 {
            let start = std::time::Instant::now();
            out = host
                .dispatch(&root, &pso, &Dispatch {
                    roots: vec![(0, Root::Constants(ud.clone()))],
                    views: &[View::RawUav(STORED)],
                    samplers: &[],
                    groups: [16384, 1, 1],
                })
                .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
            best = best.min(start.elapsed());
        }
        eprintln!("{name}: {best:.2?} for 16,384 groups ({} bytes of DXIL)", shader.dxil.len());
        stored.push(out[0][..STORED as usize].to_vec());
    }
    assert_eq!(stored[0], stored[1]);
}
