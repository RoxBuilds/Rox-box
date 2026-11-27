//! Console wave64 compute programs that share LDS within their one wave (a thread group of 64)
//! and that no form but the last one of `emit` gives waves of 32: their two host waves meet
//! inside the branches and loops around the LDS accesses, which the last form takes
//! wave-wide, and every group barrier is checked to be reached by every thread alike
//! (`lower::with_checked_meetings`). Each program is the shape of one the title's 32-lane GPU
//! runs refused ("host pipeline refused ... The parameter is incorrect"): lowered, validated by
//! `dxil.dll` when present, run on the host GPU when it takes waves of 32, and compared with
//! the console's results lane by lane.

use std::collections::HashSet;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// A wave64 compute program of 64 threads (one console wave; the thread index in v0) with 4
/// root constants in s0..s3, a raw UAV's descriptor in s[8:11] and 512 bytes of LDS, then
/// `body`.
pub(super) fn native_lds(body: &[u32]) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            // 5 user SGPRs (s0..s4), one 512-byte LDS granule
            rsrc2: 5 << 1 | 1 << 15,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// A tiling compute program's loop: each round, the lanes that
/// find what they look for set a flag in LDS and the others read it, and every lane that has
/// the flag leaves; the loop ends when no lane is left or after four rounds. On the console
/// the wave's writes come before its reads, so a flag set by any lane in a round sends every
/// lane out in that round. Lane l looks for `l + round * s1 == s0`. Each lane stores its
/// rounds to dword 1 + l; lane 0 stores the flag to dword 0.
pub(super) const FLAG_LOOP: [u32; 42] = [
    0x7E02_0280, // v_mov_b32 v1, 0
    0xBE8C_047E, // s_mov_b64 s[12:13], exec
    0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
    0xBE92_246A, // s_and_saveexec_b64 s[18:19], vcc
    0xD834_0000,
    0x0000_0101, // ds_write_b32 v1, v1: the flag is clear
    0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    0xBE94_0380, // s_mov_b32 s20, 0: the round
    0x7E06_0280, // v_mov_b32 v3, 0: the lane's rounds
    0xBE8E_047E, // s_mov_b64 s[14:15], exec: the lanes in the loop
    // loop:
    0x9315_0114, // s_mul_i32 s21, s20, s1
    0x4A08_0015, // v_add_nc_u32 v4, s21, v0
    0x4A06_0681, // v_add_nc_u32 v3, 1, v3
    0xBE90_047E, // s_mov_b64 s[16:17], exec
    0x7DA4_0800, // v_cmpx_eq_u32 exec, s0, v4: the lanes that find it
    0x7E0A_0281, // v_mov_b32 v5, 1
    0xD834_0000,
    0x0000_0501, // ds_write_b32 v1, v5: set the flag
    0x8AFE_7E10, // s_andn2_b64 exec, s[16:17], exec: the others
    0xD8D8_0000,
    0x0500_0001, // ds_read_b32 v5, v1: read it
    0xBEFE_0410, // s_mov_b64 exec, s[16:17]
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x7DAA_0A80, // v_cmpx_ne_u32 exec, 0, v5: the lanes that leave
    0x8A8E_7E0E, // s_andn2_b64 s[14:15], s[14:15], exec
    0xBF84_0004, // s_cbranch_scc0 done: no lane left
    0x87FE_0E10, // s_and_b64 exec, s[16:17], s[14:15]
    0x8114_8114, // s_add_i32 s20, s20, 1
    0xBF02_8314, // s_cmp_gt_i32 s20, 3
    0xBF84_FFEC, // s_cbranch_scc0 loop
    // done:
    0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    0x340C_0082, // v_lshlrev_b32 v6, 2, v0
    0x4A0C_0C84, // v_add_nc_u32 v6, 4, v6
    0xE070_1000,
    0x8002_0306, // buffer_store_dword v3, v6, s[8:11], 0 offen
    0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
    0xBE92_246A, // s_and_saveexec_b64 s[18:19], vcc
    0xD8D8_0000,
    0x0700_0001, // ds_read_b32 v7, v1
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xE070_0000,
    0x8002_0700, // buffer_store_dword v7, off, s[8:11], 0
];

/// [`FLAG_LOOP`] behind a lane branch, as the title's program has it (0x94: the pixels of the
/// tile on the screen): the lanes below s2 run the loop, the others keep 0 rounds.
pub(super) const FLAG_LOOP_IN_BRANCH: [u32; 44] = [
    0x7E02_0280, // v_mov_b32 v1, 0
    0xBE8C_047E, // s_mov_b64 s[12:13], exec
    0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
    0xBE92_246A, // s_and_saveexec_b64 s[18:19], vcc
    0xD834_0000,
    0x0000_0101, // ds_write_b32 v1, v1: the flag is clear
    0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    0x7E06_0280, // v_mov_b32 v3, 0: the lane's rounds
    0x7DA8_0002, // v_cmpx_gt_u32 exec, s2, v0: the lanes below s2
    0xBF88_0016, // s_cbranch_execz done
    0xBE94_0380, // s_mov_b32 s20, 0: the round
    0xBE8E_047E, // s_mov_b64 s[14:15], exec: the lanes in the loop
    // loop:
    0x9315_0114, // s_mul_i32 s21, s20, s1
    0x4A08_0015, // v_add_nc_u32 v4, s21, v0
    0x4A06_0681, // v_add_nc_u32 v3, 1, v3
    0xBE90_047E, // s_mov_b64 s[16:17], exec
    0x7DA4_0800, // v_cmpx_eq_u32 exec, s0, v4: the lanes that find it
    0x7E0A_0281, // v_mov_b32 v5, 1
    0xD834_0000,
    0x0000_0501, // ds_write_b32 v1, v5: set the flag
    0x8AFE_7E10, // s_andn2_b64 exec, s[16:17], exec: the others
    0xD8D8_0000,
    0x0500_0001, // ds_read_b32 v5, v1: read it
    0xBEFE_0410, // s_mov_b64 exec, s[16:17]
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x7DAA_0A80, // v_cmpx_ne_u32 exec, 0, v5: the lanes that leave
    0x8A8E_7E0E, // s_andn2_b64 s[14:15], s[14:15], exec
    0xBF84_0004, // s_cbranch_scc0 done: no lane left
    0x87FE_0E10, // s_and_b64 exec, s[16:17], s[14:15]
    0x8114_8114, // s_add_i32 s20, s20, 1
    0xBF02_8314, // s_cmp_gt_i32 s20, 3
    0xBF84_FFEC, // s_cbranch_scc0 loop
    // done:
    0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    0x340C_0082, // v_lshlrev_b32 v6, 2, v0
    0x4A0C_0C84, // v_add_nc_u32 v6, 4, v6
    0xE070_1000,
    0x8002_0306, // buffer_store_dword v3, v6, s[8:11], 0 offen
    0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
    0xBE92_246A, // s_and_saveexec_b64 s[18:19], vcc
    0xD8D8_0000,
    0x0700_0001, // ds_read_b32 v7, v1
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xE070_0000,
    0x8002_0700, // buffer_store_dword v7, off, s[8:11], 0
];

/// [`FLAG_LOOP`] (every lane, `below` 64) or [`FLAG_LOOP_IN_BRANCH`] (the lanes below
/// `below`) as the console runs it: each lane's rounds and the flag.
pub(super) fn flag_loop_on_the_console(s0: u32, s1: u32, below: u32) -> (Vec<u32>, u32) {
    let mut active: Vec<bool> = (0..64).map(|l| l < below).collect();
    let mut rounds = vec![0u32; 64];
    let mut flag = 0;
    for round in 0..4u32 {
        if !active.iter().any(|&a| a) {
            // (the branch past the loop: no lane is in it)
            break;
        }
        for l in (0..64).filter(|&l| active[l]) {
            rounds[l] += 1;
        }
        let finds = |l: usize| active[l] && (l as u32).wrapping_add(round.wrapping_mul(s1)) == s0;
        // the wave's writes, then its reads
        if (0..64).any(finds) {
            flag = 1;
        }
        let seen: Vec<u32> = (0..64).map(|l| if finds(l) { 1 } else { flag }).collect();
        for l in 0..64 {
            if active[l] && seen[l] != 0 {
                active[l] = false;
            }
        }
        if !active.iter().any(|&a| a) {
            break;
        }
    }
    (rounds, flag)
}

/// A particle visibility compute program's LDS use: lane 0 sets the words, a loop of s1 rounds (a
/// scalar count) adds in LDS where lane l is below s2 - round, reading another word in the
/// same branch, and lane 0 alone reads, adds to and writes back a word that
/// every lane reads afterwards. Every lane stores the last word to dword l.
pub(super) const COUNTS: [u32; 40] = [
    0x7E02_0280, // v_mov_b32 v1, 0
    0xBE8C_047E, // s_mov_b64 s[12:13], exec
    0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
    0xBE92_246A, // s_and_saveexec_b64 s[18:19], vcc
    0x7E04_0200, // v_mov_b32 v2, s0
    0xD834_0000,
    0x0000_0201, // ds_write_b32 v1, v2: word 0 = s0
    0xD834_0004,
    0x0000_0101, // ds_write_b32 v1, v1 offset:4: word 1 = 0
    0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    0xBE94_0380, // s_mov_b32 s20, 0
    // loop:
    0x4A08_0014, // v_add_nc_u32 v4, s20, v0
    0xBE90_047E, // s_mov_b64 s[16:17], exec
    0x7DA8_0802, // v_cmpx_gt_u32 exec, s2, v4
    0xBF88_0005, // s_cbranch_execz skip
    0xD8D8_0000,
    0x0500_0001, // ds_read_b32 v5, v1: word 0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xD880_0004,
    0x0700_0501, // ds_add_rtn_u32 v7, v1, v5 offset:4: word 1 += word 0
    // skip:
    0xBEFE_0410, // s_mov_b64 exec, s[16:17]
    0x8114_8114, // s_add_i32 s20, s20, 1
    0xBF06_0114, // s_cmp_eq_u32 s20, s1
    0xBF84_FFF3, // s_cbranch_scc0 loop
    0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
    0xBE92_246A, // s_and_saveexec_b64 s[18:19], vcc
    0xD8D8_0004,
    0x0800_0001, // ds_read_b32 v8, v1 offset:4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x4A10_1081, // v_add_nc_u32 v8, 1, v8
    0xD834_0004,
    0x0000_0801, // ds_write_b32 v1, v8 offset:4: word 1 + 1
    0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xD8D8_0004,
    0x0900_0001, // ds_read_b32 v9, v1 offset:4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x340C_0082, // v_lshlrev_b32 v6, 2, v0
    0xE070_1000,
    0x8002_0906, // buffer_store_dword v9, v6, s[8:11], 0 offen
];

/// [`COUNTS`] as the console runs it: the last word, which every lane stores.
pub(super) fn counts_on_the_console(s0: u32, s1: u32, s2: u32) -> u32 {
    let pairs = (0..s1).map(|round| (0..64u32).filter(|&l| l + round < s2).count() as u32).sum::<u32>();
    s0.wrapping_mul(pairs).wrapping_add(1)
}

/// The program's plain translation on a host of 32-lane waves keeps waves of 64 (refused by
/// such a host), and its last form gives waves of 32: lowered with the branches the last form
/// takes wave-wide, the shader and how many of its group barriers some thread may miss.
fn last_form(native: &NativeProgram) -> (HostShader, usize) {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let plain = lower::with_checked_meetings(|| lowered(native, &p, &HashSet::new())).expect("lower");
    assert_eq!(plain.0, Some(64), "the plain form needs waves of 64");
    // the branches `widen` takes wave-wide with the meetings checked
    let wide = lower::with_checked_meetings(|| wide_set(native, &p));
    let (wave, shader, divergent) = lower::with_checked_meetings(|| lowered(native, &p, &wide)).expect("lower");
    lower::TEST_HOST_WAVE.set(was);
    assert_eq!(wave, Some(32), "the last form gives waves of 32 ({} branches wave-wide)", wide.len());
    assert_eq!(divergent, 0, "every group barrier is reached by every thread alike");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    (shader, wide.len())
}

/// The branches `widen` (with the meetings checked) ends up taking wave-wide for `p`.
fn wide_set(native: &NativeProgram, p: &ir::Program) -> HashSet<usize> {
    let mut wide = HashSet::new();
    for _ in 0..16 {
        lower::take_needs();
        let r = lowered(native, p, &wide);
        let needs = lower::take_needs();
        let stuck = matches!(&r, Err(crate::ShaderError::Unsupported { what, .. }) if lower::needs_every_lane(what))
            || matches!(&r, Ok((Some(64), ..))) && !needs.is_empty();
        if !stuck {
            return wide;
        }
        let st = structure::structure_wide(p, &wide).expect("structure");
        let more: Vec<usize> = lower::branches_around(&st, &needs).unwrap_or_default().into_iter().filter(|b| !wide.contains(b)).collect();
        assert!(!more.is_empty(), "a branch around {needs:?} could be taken wave-wide");
        wide.extend(more);
    }
    panic!("the wave-wide branches settle");
}

/// `native` lowered for the current test host with the branches closing `wide` taken
/// wave-wide: the wave size it needs, the shader, and its group barriers some thread of the
/// group may miss (`bitcode::uniform`).
fn lowered(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
) -> Result<(Option<u32>, HostShader, usize), crate::ShaderError> {
    let st = structure::structure_wide(p, wide).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = crate::io::raise(p, native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native,
        p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })?;
    Ok((
        lowered.wave_size,
        HostShader {
            stage: HwStage::Cs,
            dxil: lowered.dxil,
            user_data: lowered.user_data,
            samplers: lowered.samplers,
            static_samplers: Vec::new(),
            root_cbvs: lowered.root_cbvs,
            root_srvs: lowered.root_srvs,
            root_uavs: lowered.root_uavs,
            root_indexed_views: res.root_indexed_views,
        },
        lowered.divergent_barriers,
    ))
}

/// Runs `shader` on the hardware device with root constants `constants` (s0..s3) and gives
/// the UAV's first `dwords` dwords; None (and a line) without a device that takes it.
fn on_the_card(shader: &HostShader, constants: [u32; 4], dwords: u32) -> Option<Vec<u32>> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program: {e}; not run");
            return None;
        }
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
            groups: [1, 1, 1],
        })
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][..dwords as usize].to_vec())
}

/// A flag set by some lanes and read by the others in one round of a loop the lanes leave one
/// by one, behind a lane branch and not: the last form takes the loop wave-wide (its test's
/// block is the loop's own), and on a host of 32-lane waves every lane leaves in the round the
/// console's does, whichever half set the flag.
#[test]
fn a_flag_set_and_read_in_a_loop_lanes_leave_runs_as_on_the_console() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (words, belows) in [(&FLAG_LOOP_IN_BRANCH[..], &[64u32, 40, 33, 32, 20, 0][..]), (&FLAG_LOOP[..], &[64][..])] {
        let native = native_lds(words);
        let (shader, wide) = last_form(&native);
        assert!(wide >= 1, "the loop is taken wave-wide");
        // (s0, s1): lane 61 finds it in round 2 (the upper half sets the flag the lower half
        // reads), lane 30 in round 1, lane 12 at once, lane 63 every round, none at all
        for &below in belows {
            for (s0, s1) in [(75u32, 7u32), (70, 40), (12, 25), (63, 0), (200, 1)] {
                let (rounds, flag) = flag_loop_on_the_console(s0, s1, below);
                let Some(out) = on_the_card(&shader, [s0, s1, below, 0], 65) else { return };
                assert_eq!(out[0], flag, "flag, s0 {s0} s1 {s1}, lanes below {below}");
                assert_eq!(&out[1..], &rounds[..], "rounds per lane, s0 {s0} s1 {s1}, lanes below {below}");
            }
        }
    }
    eprintln!("ran on the hardware device: every lane left in the console's round");
}

/// LDS adds where some lanes run, beside a read of another word, in a loop of a scalar count,
/// and a word read, changed and written back by one lane and then read by all: on a host of
/// 32-lane waves the halves meet around both, and every lane reads the console's sum.
#[test]
fn lds_adds_and_one_lanes_update_around_a_uniform_loop_run_as_on_the_console() {
    let native = native_lds(&COUNTS);
    let (shader, _) = last_form(&native);
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // (s0, s1 rounds, s2): every lane every round, some lanes, the lower half only, a lane
    // of the upper half only, none
    for (s0, s1, s2) in [(3u32, 4u32, 64u32), (5, 7, 40), (1, 3, 32), (2, 1, 34), (9, 2, 0)] {
        let want = counts_on_the_console(s0, s1, s2);
        let Some(out) = on_the_card(&shader, [s0, s1, s2, 0], 64) else { return };
        assert!(out.iter().all(|&w| w == want), "s0 {s0} s1 {s1} s2 {s2}: {out:?}, the console's {want}");
    }
    eprintln!("ran on the hardware device: every lane read the console's sum");
}

/// The hole the last form's rules close. Without them a loop's own test counts as a block
/// every lane runs: [`FLAG_LOOP`]'s plain form already gets waves of 32 with a barrier before
/// each LDS access in that block, and the branches `widen` takes wave-wide for
/// [`FLAG_LOOP_IN_BRANCH`] leave the same test per lane (the form `VICTORY_WAVE_WIDE=1`
/// gives). In both, those barriers stand in a loop each lane leaves on its own, and the check
/// finds them.
#[test]
fn the_forms_without_the_rules_have_barriers_some_threads_miss() {
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    // (counted, not refused: the check as it was before it was on for every program)
    lower::TEST_BARRIER_CHECK.set(Some(false));
    let plain = native_lds(&FLAG_LOOP);
    let p = ir::build(&plain.code, plain.wave64, plain.vgprs).expect("ir::build");
    let (wave, _, divergent) = lowered(&plain, &p, &HashSet::new()).expect("lower");
    assert_eq!(wave, Some(32));
    assert!(divergent > 0, "the plain form's barriers in the loop are found");
    let branch = native_lds(&FLAG_LOOP_IN_BRANCH);
    let p = ir::build(&branch.code, branch.wave64, branch.vgprs).expect("ir::build");
    let wide = wide_set(&branch, &p);
    let (wave, _, divergent) = lowered(&branch, &p, &wide).expect("lower");
    lower::TEST_BARRIER_CHECK.set(None);
    lower::TEST_HOST_WAVE.set(was);
    assert_eq!(wave, Some(32));
    assert!(divergent > 0, "the old wave-wide form's barriers in the loop are found ({} branches wave-wide)", wide.len());
}

/// What `emit` itself gives a program no other form gives waves of 32 ([`COUNTS`]) on a host
/// of 32-lane waves: its last form, waves of 32 with every group barrier reached by every
/// thread alike (`VICTORY_WAVE_WIDE_LAST`, on by default; with it off the test says so and
/// checks nothing). ([`FLAG_LOOP`]'s plain form, and [`FLAG_LOOP_IN_BRANCH`] without its skip,
/// get waves of 32 before it, through the hole the last form's rules close:
/// `VICTORY_WAVE_LOOP_EXITS=1` closes it for every form, and the barrier check (on unless
/// `VICTORY_WAVE_BARRIER_CHECK=0`) refuses such forms.)
#[test]
fn emit_gives_a_program_nothing_else_translates_its_last_form() {
    if std::env::var_os("VICTORY_WAVE_WIDE_LAST").is_some_and(|v| v == "0") {
        eprintln!("VICTORY_WAVE_WIDE_LAST=0: the last form is off; not checked");
        return;
    }
    let bytes = crate::dxbc::build(&[]);
    let c = crate::Container::parse(&bytes).expect("container");
    let native = native_lds(&COUNTS);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = crate::io::raise(&p, &native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let shader = super::emit(&c, &native, &p, &st, &io, &res);
    lower::TEST_HOST_WAVE.set(was);
    let shader = shader.expect("emit");
    let got = super::EMITTED.with_borrow(|m| m.get(&super::dxil_key(&shader.dxil)).copied());
    assert_eq!(got, Some((Some(32), 0)), "waves of 32, no barrier some thread misses");
}
