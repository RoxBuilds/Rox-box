//! Console wave64 compute programs whose thread group is several console waves, on a host
//! of 32-lane waves: the two host waves of one console wave meet by themselves
//! (`lower::with_pair_meetings`), where a group barrier would wait for console waves that
//! went another way.

use std::collections::HashSet;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{HwStage, NativeProgram, RootType, StageIo};
use crate::{ir, resource, structure};

/// Every container (`*.cdxil`) of the folder `VICTORY_CENSUS_DIR` translated for a host of
/// 32-lane waves, one line each in the file `VICTORY_CENSUS_OUT`: its name, the wave size
/// its translation needs, how many of its group barriers and pair meetings some thread may
/// miss, and a hash of its DXIL (the same translation, the same line). Not run unless asked
/// for (`--ignored`): it is how a change to the wave forms is shown to leave every other
/// program of a title as it was.
#[test]
#[ignore = "needs VICTORY_CENSUS_DIR, a folder of containers"]
fn census_of_a_folder_of_containers() {
    let Some(dir) = std::env::var_os("VICTORY_CENSUS_DIR") else {
        eprintln!("VICTORY_CENSUS_DIR is not set; nothing translated");
        return;
    };
    let out = std::env::var_os("VICTORY_CENSUS_OUT").expect("VICTORY_CENSUS_OUT names the file to write");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("the folder")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "cdxil"))
        .collect();
    files.sort();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let lines = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            std::thread::Builder::new()
                .stack_size(256 << 20)
                .spawn_scoped(s, || {
                    lower::TEST_HOST_WAVE.set(Some(32));
                    loop {
                        let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(path) = files.get(n) else { break };
                        let bytes = std::fs::read(path).expect("a container");
                        let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                        let line = match super::translate(&bytes) {
                            Ok(shader) => {
                                let key = super::dxil_key(&shader.dxil);
                                let got = super::EMITTED.with_borrow(|m| m.get(&key).copied());
                                let form = super::LAST_FORM.take().unwrap_or_else(|| "first".to_owned());
                                // (`VICTORY_CENSUS_WAITS`: what its barriers may wait for, after the
                                // form, which earlier censuses end their lines with)
                                let waits = super::WAITS.with_borrow(|m| m.get(&key).cloned()).unwrap_or_default().0;
                                let waits = if std::env::var_os("VICTORY_CENSUS_WAITS").is_some() {
                                    format!(
                                        "\twaits cuts={} after_cut={} in_loops={} deepest={} loops={}",
                                        waits.cuts.len(),
                                        waits.after_cut.len(),
                                        waits.in_loops.len(),
                                        waits.in_loops.iter().map(|x| x.1).max().unwrap_or(0),
                                        waits.loops
                                    )
                                } else {
                                    String::new()
                                };
                                format!("{name}\tok {got:?} dxil={key:016x} bytes={} form={form}{waits}", shader.dxil.len())
                            }
                            Err(e) => format!("{name}\terr {e}"),
                        };
                        super::EMITTED.with_borrow_mut(|m| m.clear());
                        super::WAITS.with_borrow_mut(|m| m.clear());
                        lines.lock().unwrap_or_else(|e| e.into_inner()).push(line);
                    }
                })
                .expect("a thread");
        }
    });
    let mut lines = lines.into_inner().unwrap_or_else(|e| e.into_inner());
    lines.sort();
    let unique: HashSet<&String> = lines.iter().collect();
    assert_eq!(unique.len(), lines.len());
    std::fs::write(out, lines.join("\n") + "\n").expect("the census file");
    eprintln!("{} containers translated", lines.len());
}

/// One console wave's part of [`several_waves`]: a loop of `count` rounds (a scalar register)
/// in which the lanes whose number plus the round is below s2 read word 0 of their wave's
/// LDS and add it to word 1 (a read and an add in one lane branch, as
/// a particle visibility compute program has them), then the thread index of the wave's lane 63
/// (a lane of the upper half) added to the scalar s23: a wave that ran both parts would hold
/// the sum of two, so the branches around the parts cannot be left out.
fn waves_part(count: u32) -> [u32; 17] {
    [
        0xBE94_0380, // s_mov_b32 s20, 0
        // loop:
        0x4A08_0414, // v_add_nc_u32 v4, s20, v2
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
        0xBF06_0014 | count << 8, // s_cmp_eq_u32 s20, s<count>
        0xBF84_FFF3, // s_cbranch_scc0 loop
        0xD760_0016,
        0x0001_7F00, // v_readlane_b32 s22, v0, 63
        0x8117_1617, // s_add_i32 s23, s23, s22
    ]
}

/// The shape of a defragmenting compute program: a thread
/// group of two console waves (128 threads, the thread index in v0), of which the first runs
/// one loop and the second another, each behind a branch only its own lanes take, with LDS
/// read and written inside and a lane of the upper half read by all. Each wave has two words
/// of LDS: lane 0 sets word 0 to s0 and clears word 1; the first wave runs
/// [`waves_part`] for s1 rounds, the second for s3 rounds. Then the lanes of each wave's
/// lower half go [`LOWER_ROUNDS`] times round a loop of their own (each lane leaves by
/// itself; nothing in it needs another lane) while the upper half's go once, so the upper
/// host wave is at the next meeting long before the lower. Lane 0 (of the lower half) then
/// adds one to word 1, and every thread stores its wave's word 1 to dword t, s23 (what its
/// wave read of lane 63) to dword 128 + t and what its own loop made to dword 256 + t: an
/// upper half that did not wait for the lower one would store word 1 without lane 0's one.
fn several_waves() -> Vec<u32> {
    let mut words = vec![
        0x2C02_0086, // v_lshrrev_b32 v1, 6, v0: the console wave
        0x3402_0283, // v_lshlrev_b32 v1, 3, v1: its two words of LDS
        0x3604_00BF, // v_and_b32 v2, 63, v0: the lane
        0xBE97_0380, // s_mov_b32 s23, 0
        0xBE8C_047E, // s_mov_b64 s[12:13], exec
        0x7D84_0480, // v_cmp_eq_u32 vcc, 0, v2
        0xBE92_246A, // s_and_saveexec_b64 s[18:19], vcc
        0x7E06_0200, // v_mov_b32 v3, s0
        0xD834_0000,
        0x0000_0301, // ds_write_b32 v1, v3: word 0 = s0
        0x7E06_0280, // v_mov_b32 v3, 0
        0xD834_0004,
        0x0000_0301, // ds_write_b32 v1, v3 offset:4: word 1 = 0
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0x7DA8_00C0, // v_cmpx_gt_u32 exec, 64, v0: the first console wave
        0xBF88_0011, // s_cbranch_execz second
    ];
    words.extend_from_slice(&waves_part(1));
    words.extend_from_slice(&[
        // second:
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0x7DA6_00C0, // v_cmpx_le_u32 exec, 64, v0: the second console wave
        0xBF88_0011, // s_cbranch_execz both
    ]);
    words.extend_from_slice(&waves_part(3));
    words.extend_from_slice(&[
        // both:
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0x7E18_02FF,
        LOWER_ROUNDS, // v_mov_b32 v12, LOWER_ROUNDS
        0x7E1A_0280, // v_mov_b32 v13, 0
        0x7D88_04A0, // v_cmp_gt_u32 vcc, 32, v2
        0x0216_190D, // v_cndmask_b32 v11, v13, v12, vcc: the lane's rounds
        0x7E1C_0280, // v_mov_b32 v14, 0
        0x7E1E_0280, // v_mov_b32 v15, 0
        0xBE8E_047E, // s_mov_b64 s[14:15], exec: the lanes in the loop
        // lanes:
        0x4A1C_1C81, // v_add_nc_u32 v14, 1, v14
        0x341E_1E81, // v_lshlrev_b32 v15, 1, v15
        0x3A1E_1F0E, // v_xor_b32 v15, v14, v15
        0xBE90_047E, // s_mov_b64 s[16:17], exec
        0x7DAC_170E, // v_cmpx_ge_u32 exec, v14, v11: the lanes that leave
        0x8A8E_7E0E, // s_andn2_b64 s[14:15], s[14:15], exec
        0xBF84_0002, // s_cbranch_scc0 left: no lane left
        0x87FE_0E10, // s_and_b64 exec, s[16:17], s[14:15]
        0xBF82_FFF7, // s_branch lanes
        // left:
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0x7D84_0480, // v_cmp_eq_u32 vcc, 0, v2
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
        0x7E14_0217, // v_mov_b32 v10, s23
        0x340C_0082, // v_lshlrev_b32 v6, 2, v0
        0xE070_1000,
        0x8002_0906, // buffer_store_dword v9, v6, s[8:11], 0 offen
        0x4A0C_0CFF,
        0x0000_0200, // v_add_nc_u32 v6, 512, v6
        0xE070_1000,
        0x8002_0A06, // buffer_store_dword v10, v6, s[8:11], 0 offen
        0x4A0C_0CFF,
        0x0000_0200, // v_add_nc_u32 v6, 512, v6
        0xE070_1000,
        0x8002_0F06, // buffer_store_dword v15, v6, s[8:11], 0 offen
    ]);
    words
}

/// The rounds of the lower halves' own loop in [`several_waves`] (under the translation's
/// limit of 4,096 rounds a loop).
const LOWER_ROUNDS: u32 = 3000;

/// How many dwords [`several_waves`] stores.
pub(super) const STORED: u32 = 384;

/// [`several_waves`] as the console runs it: the dwords it stores.
pub(super) fn several_waves_on_the_console(s0: u32, s1: u32, s2: u32, s3: u32) -> Vec<u32> {
    let mut out = vec![0u32; STORED as usize];
    for wave in 0..2u32 {
        let rounds = if wave == 0 { s1 } else { s3 };
        let adds = (0..rounds).map(|round| (0..64u32).filter(|&l| l + round < s2).count() as u32).sum::<u32>();
        for lane in 0..64 {
            let t = (wave * 64 + lane) as usize;
            out[t] = s0.wrapping_mul(adds).wrapping_add(1);
            out[128 + t] = wave * 64 + 63;
            let rounds = if lane < 32 { LOWER_ROUNDS } else { 1 };
            out[256 + t] = (1..=rounds).fold(0u32, |made, round| made << 1 ^ round);
        }
    }
    out
}

/// [`several_waves`] as a program of 128 threads a group.
pub(super) fn several_waves_native() -> NativeProgram {
    let mut native = super::meetings_tests::native_lds(&several_waves());
    let StageIo::Cs(cs) = &mut native.io else { unreachable!() };
    cs.threads = [128, 1, 1];
    native
}

/// What `emit` gives `native` on a host of 32-lane waves, validated by `dxil.dll` when
/// present, with the wave size it needs and how many of its group barriers some thread may
/// miss.
fn emitted(native: &NativeProgram) -> Result<(HostShader, Option<(Option<u32>, usize)>), crate::ShaderError> {
    let bytes = crate::dxbc::build(&[]);
    let c = crate::Container::parse(&bytes).expect("container");
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = crate::io::raise(&p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let shader = super::emit(&c, native, &p, &st, &io, &res);
    lower::TEST_HOST_WAVE.set(was);
    let shader = shader?;
    let got = super::EMITTED.with_borrow(|m| m.get(&super::dxil_key(&shader.dxil)).copied());
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    Ok((shader, got))
}

/// The hardware device's name and the lanes of its waves (least, most), for a test's line.
fn the_card(host: &Host) -> String {
    use windows::Win32::Graphics::Direct3D12::{D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory4};
    // SAFETY: plain D3D12/DXGI queries with valid out pointers.
    unsafe {
        let mut waves = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        let _ = host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS1,
            (&raw mut waves).cast(),
            size_of_val(&waves) as u32,
        );
        let name = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|f| f.EnumAdapterByLuid::<IDXGIAdapter1>(host.device.GetAdapterLuid()))
            .and_then(|a| a.GetDesc1())
            .map(|d| String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_owned())
            .unwrap_or_else(|_| "an adapter without a name".to_owned());
        format!("{name} (waves of {} to {} lanes)", waves.WaveLaneCountMin, waves.WaveLaneCountMax)
    }
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
            groups: [groups, 1, 1],
        })
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][..dwords as usize].to_vec())
}

/// A group of two console waves that go different ways, with LDS shared inside each and a
/// lane of the upper half read by all: no form but the last gives it waves of 32 (with the
/// switch off it is refused), the last one's DXIL validates, every group barrier and every
/// meeting of two host waves is reached alike, and on a card of 32-lane waves every thread
/// stores what the console's does: whatever the two waves' counts of rounds, and with many
/// groups at once.
#[test]
fn two_console_waves_that_go_different_ways_run_as_on_the_console() {
    let native = several_waves_native();
    let was = super::WAVE_PAIR_TEST.replace(Some(false));
    // (nor as one host thread a console wave, which `serial_tests` asks for)
    super::WAVE_SERIAL_TEST.set(Some(false));
    let refused = emitted(&native);
    super::WAVE_PAIR_TEST.set(Some(true));
    let translated = emitted(&native);
    super::WAVE_PAIR_TEST.set(was);
    super::WAVE_SERIAL_TEST.set(None);
    assert!(
        matches!(&refused, Err(crate::ShaderError::Unsupported { what, .. }) if what.starts_with("no wave32 lowering")),
        "without the form the program needs waves of 64: {:?}",
        refused.map(|r| r.1)
    );
    let (shader, got) = translated.expect("emit");
    assert_eq!(got, Some((Some(32), 0)), "waves of 32, no barrier some thread misses");
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // (s0, s1 rounds of the first wave, s2, s3 rounds of the second): the same count, one
    // wave far ahead of the other, lanes of the lower half only, one lane of the upper half
    // too, none
    let cases = [(3u32, 4u32, 64u32, 4u32), (5, 1, 40, 9), (7, 200, 70, 2), (1, 3, 32, 5), (2, 2, 34, 1), (9, 2, 0, 3)];
    for (s0, s1, s2, s3) in cases {
        let want = several_waves_on_the_console(s0, s1, s2, s3);
        let Some(out) = on_the_card(&shader, [s0, s1, s2, s3], STORED, 1) else { return };
        assert_eq!(out, want, "s0 {s0} s1 {s1} s2 {s2} s3 {s3}");
    }
    // many groups at once: each stores the same, and none waits for another's waves
    for _ in 0..8 {
        let want = several_waves_on_the_console(11, 37, 50, 3);
        let Some(out) = on_the_card(&shader, [11, 37, 50, 3], STORED, 4096) else { return };
        assert_eq!(out, want, "4096 groups");
    }
    let card = Host::new(false).map(|h| the_card(&h)).unwrap_or_default();
    eprintln!("ran on {card}: every thread stored what the console's does");
    // What the waits are for: the same program whose halves give up after one look at the
    // other's count stores something else (the upper halves read word 1 too soon).
    let was = (super::WAVE_PAIR_TEST.replace(Some(true)), lower::TEST_PAIR_ROUNDS.replace(Some(1)));
    let unmet = emitted(&native);
    super::WAVE_PAIR_TEST.set(was.0);
    lower::TEST_PAIR_ROUNDS.set(was.1);
    let (unmet, _) = unmet.expect("emit");
    let want = several_waves_on_the_console(11, 37, 50, 3);
    let differs = |_: &i32| on_the_card(&unmet, [11, 37, 50, 3], STORED, 64).is_some_and(|out| out != want);
    let wrong = (0..8).filter(differs).count();
    eprintln!("without the waits {wrong} of 8 runs stored something else");
    assert!(wrong > 0, "the upper halves get to their meetings first: without the waits they read too soon");
}

/// The limit of the waits has room: with a sixteenth of the rounds a translation has
/// unless told otherwise (`lower.rs` `PAIR_ROUNDS`) every thread of
/// [`two_console_waves_that_go_different_ways_run_as_on_the_console`]'s program still stores
/// what the console's does, one wave far ahead of the other and many groups at once. (At a
/// limit that only just passes, a card or a driver a little slower stores something else.)
#[test]
fn two_console_waves_meet_at_a_sixteenth_of_the_default_limit() {
    let native = several_waves_native();
    let was = (
        super::WAVE_PAIR_TEST.replace(Some(true)),
        super::WAVE_SERIAL_TEST.replace(Some(false)),
        lower::TEST_PAIR_ROUNDS.replace(Some(lower::PAIR_ROUNDS / 16)),
    );
    let translated = emitted(&native);
    super::WAVE_PAIR_TEST.set(was.0);
    super::WAVE_SERIAL_TEST.set(was.1);
    lower::TEST_PAIR_ROUNDS.set(was.2);
    let (shader, got) = translated.expect("emit");
    assert_eq!(got, Some((Some(32), 0)), "waves of 32, no barrier some thread misses");
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let cases = [(3u32, 4u32, 64u32, 4u32), (5, 1, 40, 9), (7, 200, 70, 2), (1, 3, 32, 5), (2, 2, 34, 1), (9, 2, 0, 3)];
    for (s0, s1, s2, s3) in cases {
        let want = several_waves_on_the_console(s0, s1, s2, s3);
        let Some(out) = on_the_card(&shader, [s0, s1, s2, s3], STORED, 1) else { return };
        assert_eq!(out, want, "s0 {s0} s1 {s1} s2 {s2} s3 {s3}");
    }
    for _ in 0..8 {
        let want = several_waves_on_the_console(11, 37, 50, 3);
        let Some(out) = on_the_card(&shader, [11, 37, 50, 3], STORED, 4096) else { return };
        assert_eq!(out, want, "4096 groups");
    }
    let rounds = lower::PAIR_ROUNDS / 16;
    eprintln!("ran on the hardware device at {rounds} rounds: every thread stored what the console's does");
}

/// The shape of a trail compute program: the title's own group barrier inside a branch on a
/// word of LDS that every
/// thread of the group reads alike. Thread 0 sets word 0 to s0 and clears word 1; after a
/// barrier every lane reads word 0, and unless s1 is above it (a branch on a lane mask), lane
/// 63 of each console wave adds one to word 1, the group meets at a barrier, and every thread
/// reads word 1 and adds the thread index of its wave's lane 63 (a lane of the upper half).
/// Every thread stores the sum (0 where the branch was taken) to dword t.
pub(super) const A_BARRIER_IN_A_BRANCH: [u32; 36] = [
    0x7E02_0280, // v_mov_b32 v1, 0
    0x3604_00BF, // v_and_b32 v2, 63, v0: the lane
    0x7E12_0280, // v_mov_b32 v9, 0
    0xBE8C_047E, // s_mov_b64 s[12:13], exec
    0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
    0xBE92_246A, // s_and_saveexec_b64 s[18:19], vcc
    0x7E06_0200, // v_mov_b32 v3, s0
    0xD834_0000,
    0x0000_0301, // ds_write_b32 v1, v3: word 0 = s0
    0x7E06_0280, // v_mov_b32 v3, 0
    0xD834_0004,
    0x0000_0301, // ds_write_b32 v1, v3 offset:4: word 1 = 0
    0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    0xBF8A_0000, // s_barrier
    0xD8D8_0000,
    0x0500_0001, // ds_read_b32 v5, v1: word 0
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x7D88_0A01, // v_cmp_gt_u32 vcc, s1, v5
    0xBF87_000E, // s_cbranch_vccnz skip
    0xBE90_047E, // s_mov_b64 s[16:17], exec
    0x7DA4_04BF, // v_cmpx_eq_u32 exec, 63, v2: lane 63 of each console wave
    0x7E0A_0281, // v_mov_b32 v5, 1
    0xD880_0004,
    0x0700_0501, // ds_add_rtn_u32 v7, v1, v5 offset:4: word 1 += 1
    0xBEFE_0410, // s_mov_b64 exec, s[16:17]
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xBF8A_0000, // s_barrier
    0xD8D8_0004,
    0x0900_0001, // ds_read_b32 v9, v1 offset:4
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xD760_0016,
    0x0001_7F00, // v_readlane_b32 s22, v0, 63
    0x4A12_1216, // v_add_nc_u32 v9, s22, v9
    // skip:
    0x340C_0082, // v_lshlrev_b32 v6, 2, v0
    0xE070_1000,
    0x8002_0906, // buffer_store_dword v9, v6, s[8:11], 0 offen
];

/// The title's group barrier inside a branch that the whole group takes or leaves together
/// (its lanes all read the same word of LDS): the branch is taken wave-wide for the LDS
/// write and read inside it, the halves' exchanged answers are seen to be the group's
/// (`Module::divergent_barriers_with`), so the barrier stays one every thread reaches, and
/// on a card of 32-lane waves every thread of four console waves reads the count of waves
/// and its own wave's lane 63.
#[test]
fn the_titles_barrier_in_a_branch_the_whole_group_takes_is_kept() {
    let mut native = super::meetings_tests::native_lds(&A_BARRIER_IN_A_BRANCH);
    let StageIo::Cs(cs) = &mut native.io else { unreachable!() };
    cs.threads = [256, 1, 1];
    let was = super::WAVE_PAIR_TEST.replace(Some(true));
    let translated = emitted(&native);
    super::WAVE_PAIR_TEST.set(was);
    let (shader, got) = translated.expect("emit");
    assert_eq!(got, Some((Some(32), 0)), "waves of 32, no barrier some thread misses");
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

/// How long a dispatch of `groups` groups of `shader` takes on the hardware device, its
/// pipeline made and run once before; None (and a line) without a device that takes it.
fn dispatch_time(shader: &HostShader, constants: [u32; 4], groups: u32) -> Option<std::time::Duration> {
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
    let mut last = None;
    for _ in 0..2 {
        let start = std::time::Instant::now();
        host.dispatch(&root, &pso, &Dispatch {
            roots: vec![(0, Root::Constants(ud.clone()))],
            views: &[View::RawUav(STORED)],
            samplers: &[],
            groups: [groups, 1, 1],
        })
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        last = Some(start.elapsed());
    }
    last
}

/// A half that never comes is waited for once, for the limit's rounds, and never again. With
/// the upper halves' counts held back (a test's switch) every lower half waits at its first
/// meeting until the limit, and the dispatch still ends: in a time that grows with the limit
/// and not with the number of meetings (400 rounds of the program's loops are some 2,400
/// meetings a host wave). What such a run stores is not looked at: its halves did not meet.
#[test]
fn a_half_that_never_comes_is_waited_for_once() {
    let native = several_waves_native();
    let was = (super::WAVE_PAIR_TEST.replace(Some(true)), lower::TEST_PAIR_ONE_HALF.replace(true));
    let limits = [16u32, 1 << 16, 1 << 20];
    let shaders: Vec<HostShader> = limits
        .iter()
        .map(|&limit| {
            lower::TEST_PAIR_ROUNDS.set(Some(limit));
            let (shader, got) = emitted(&native).expect("emit");
            assert_eq!(got.map(|g| g.0), Some(Some(32)));
            shader
        })
        .collect();
    lower::TEST_PAIR_ROUNDS.set(None);
    super::WAVE_PAIR_TEST.set(was.0);
    lower::TEST_PAIR_ONE_HALF.set(was.1);
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let groups: u32 = std::env::var("VICTORY_TEST_PAIR_GROUPS").ok().and_then(|v| v.parse().ok()).unwrap_or(64);
    let mut times = Vec::new();
    for (shader, limit) in shaders.iter().zip(limits) {
        for rounds in [1u32, 400] {
            let Some(time) = dispatch_time(shader, [3, rounds, 64, rounds], groups) else { return };
            eprintln!("a limit of {limit} rounds, {rounds} rounds of the program, {groups} groups: {time:?}");
            times.push(time);
        }
    }
    // the waits of 2,400 meetings at a limit of 2^20 would be minutes
    assert!(times[5] < times[4] * 4 + std::time::Duration::from_secs(1), "a wait at every meeting: {times:?}");
}

/// With `VICTORY_CENSUS_DIR` set (a folder of the title's containers): every wave64 compute
/// program of a group of several console waves in it that `emit` gives waves of 32 has DXIL
/// that `dxil.dll` takes and a pipeline the hardware device makes. Says so and checks
/// nothing without the folder.
#[test]
#[ignore = "needs VICTORY_CENSUS_DIR, a folder of containers"]
fn the_cards_pipelines_of_a_folder_of_containers() {
    let Some(dir) = std::env::var_os("VICTORY_CENSUS_DIR") else {
        eprintln!("VICTORY_CENSUS_DIR is not set; nothing made");
        return;
    };
    let only = std::env::var("VICTORY_CENSUS_ONLY").unwrap_or_default();
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let host = Host::new(false).expect("a hardware device");
    let mut validator = validator::Validator::load(validator::Validator::default_path().as_deref()).expect("dxil.dll");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("the folder")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "cdxil"))
        .filter(|p| only.split(',').any(|o| p.to_string_lossy().contains(o)))
        .collect();
    files.sort();
    let (mut made, mut failed) = (0, Vec::new());
    lower::TEST_HOST_WAVE.set(Some(32));
    for path in &files {
        let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = std::fs::read(path).expect("a container");
        let start = std::time::Instant::now();
        let translated = super::translate(&bytes);
        let took = start.elapsed();
        let shader = match translated {
            Ok(s) if s.stage == HwStage::Cs => s,
            Ok(_) => continue,
            Err(e) => {
                failed.push(format!("{name}: {e}"));
                continue;
            }
        };
        let mut copy = shader.dxil.clone();
        if let Err(e) = validator.validate(&mut copy) {
            failed.push(format!("{name}: dxil.dll: {e}"));
            continue;
        }
        let start = std::time::Instant::now();
        match host.compute_pipeline(&shader) {
            Ok(_) => made += 1,
            Err(e) => failed.push(format!("{name}: the card: {e}")),
        }
        // (a line a program whose pipeline takes the card long: the lane-serial ones)
        if start.elapsed().as_secs() >= 2 {
            eprintln!("{name}: translated in {took:.1?}, {} bytes of DXIL, its pipeline made in {:.1?}", shader.dxil.len(), start.elapsed());
        }
    }
    lower::TEST_HOST_WAVE.set(None);
    eprintln!("{made} of {} pipelines validated and made on the hardware device", files.len());
    assert!(failed.is_empty(), "{failed:#?}");
}
