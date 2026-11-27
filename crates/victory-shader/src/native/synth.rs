//! Console shader containers made by our own code: the inverse of
//! [`super::native_programs`], for programs we write ourselves from the
//! public GFX10 instruction encodings.
//!
//! Nothing in the repository may come from a title, so whatever exercises
//! the whole translation path (container, `XDAT` index, `XBCH` packing,
//! `FSSM` records, root tables, stage data) has to build its input. This
//! module writes exactly the layout `native.rs` documents and reads, and
//! nothing else: a field the reader ignores is written as zero. The parser's
//! own tests and `victory selftest` (`victory_gpu::selftest`) use it; a
//! container from here goes through `emit::translate` like any other.
//!
//! shortcut: mesh stage data and merged stages are not written (their
//! records get empty stage data); `FSPX` parts are not made.

use super::*;

/// The bytes of stage data written after every record: enough for the
/// largest layout read (the pixel stage's export patches).
pub const STAGE_DATA: usize = PS_PATCHES + MRTS * PATCH_BYTES;

/// The `XDAT` kind of a blob of native programs.
pub const NATIVE_BLOB: u32 = KIND_NATIVE;

/// SGPRs the record says the program uses (read by no one; a plausible
/// value).
const SGPRS_USED: u32 = 104;

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    out[at..at + bytes.len()].copy_from_slice(bytes);
}

fn usage_raw(u: Usage) -> u8 {
    match u {
        Usage::Generic => 6,
        Usage::ClipDistance => 7,
        Usage::Other(n) => n,
    }
}

fn root_raw(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 3,
        RootType::ImmediateSampler => 4,
        RootType::ResourceHeap => 5,
        RootType::SamplerHeap => 6,
        RootType::Other(n) => n,
    }
}

fn resource_raw(k: ResourceType) -> u8 {
    match k {
        ResourceType::Srv => 0,
        ResourceType::Sampler => 1,
        ResourceType::Cbv => 2,
        ResourceType::Uav => 3,
        ResourceType::Other(n) => n,
    }
}

/// Stage data for `io`, [`STAGE_DATA`] bytes: no semantics and every export
/// patch absent unless `io` names them.
fn stage_data(io: &StageIo) -> Vec<u8> {
    let mut data = vec![0; STAGE_DATA];
    for mrt in 0..MRTS {
        put(&mut data, PS_PATCHES + mrt * PATCH_BYTES, &u32::MAX.to_le_bytes());
    }
    match io {
        StageIo::Cs(cs) => {
            put(&mut data, CS_RSRC2, &cs.rsrc2.to_le_bytes());
            for (i, n) in cs.threads.iter().enumerate() {
                put(&mut data, CS_THREADS + 4 * i, &n.to_le_bytes());
            }
        }
        StageIo::Vs { inputs, outputs } => {
            for (i, v) in inputs.iter().enumerate() {
                let highest = (8 - v.mask.leading_zeros() as u8).saturating_sub(1);
                put(&mut data, VS_INPUTS + 5 * i, &[usage_raw(v.usage), v.row, v.mask, v.vgpr, highest]);
            }
            put(&mut data, VS_INPUT_COUNT, &(inputs.len() as u32).to_le_bytes());
            write_outputs(&mut data, outputs);
        }
        StageIo::Ps { inputs, input_addr, db_shader_control, patches } => {
            for (i, p) in inputs.iter().enumerate() {
                let flags = u8::from(p.flat) | u8::from(p.fp16) << 3 | u8::from(p.pack2) << 4;
                put(&mut data, PS_INPUTS + 8 * i, &[usage_raw(p.usage), p.row, p.mask, p.attr, 0, p.row2, 0, flags]);
            }
            put(&mut data, PS_INPUT_COUNT, &(inputs.len() as u32).to_le_bytes());
            put(&mut data, PS_INPUT_ADDR, &input_addr.to_le_bytes());
            put(&mut data, PS_DB_SHADER_CONTROL, &db_shader_control.to_le_bytes());
            let size = patches.iter().map(|p| p.code.len()).max().unwrap_or(0);
            put(&mut data, PS_PATCH_SIZE, &(size as u32).to_le_bytes());
            for p in patches {
                let at = PS_PATCHES + usize::from(p.mrt) * PATCH_BYTES;
                put(&mut data, at, &p.offset.to_le_bytes());
                let full = at + 4 + PATCH_FULL * PATCH_SLOT_DWORDS * 4;
                for (i, word) in p.code.iter().enumerate() {
                    put(&mut data, full + 4 * i, &word.to_le_bytes());
                }
            }
        }
        StageIo::Ms(_) | StageIo::None => {}
    }
    data
}

fn write_outputs(data: &mut [u8], outputs: &[VsOutput]) {
    for (i, o) in outputs.iter().enumerate() {
        let flags = u8::from(o.fp16) | u8::from(o.pack2) << 1;
        put(data, VS_OUTPUTS + 6 * i, &[usage_raw(o.usage), o.row, o.param, o.row, 0, flags]);
    }
    put(data, VS_OUTPUT_COUNT, &(outputs.len() as u32).to_le_bytes());
}

/// An `FSSM` blob holding `programs` (one per stage), as
/// [`super::native_programs`] reads it: per program a record, its stage
/// data, its code, then its root layout and used-resource tables.
pub fn fssm(programs: &[NativeProgram]) -> Vec<u8> {
    let slot = |stage: HwStage| SLOTS.iter().find(|s| s.0 == stage).map(|s| s.1);
    let mut payload = GRAPHICS_SLOT_COUNT.to_le_bytes().to_vec();
    payload.resize(SLOTS[SLOTS.len() - 1].1 + 8, 0);
    for (_, at) in SLOTS {
        put(&mut payload, at, &ABSENT.to_le_bytes());
    }
    for p in programs {
        let Some(at) = slot(p.stage) else { continue };
        let record = payload.len();
        put(&mut payload, at, &(record as u64).to_le_bytes());
        payload.resize(record + RECORD_SIZE as usize, 0);
        payload.extend_from_slice(&stage_data(&p.io));
        let code = payload.len() as u64;
        payload.extend_from_slice(&p.code);
        let layout = payload.len() as u64;
        for r in &p.layout {
            for v in [root_raw(r.root), r.table_offset_dwords, r.sgprs, r.base_register] {
                payload.extend_from_slice(&v.to_le_bytes());
            }
            payload.extend_from_slice(&[r.user_data, r.slot, 0, 0]);
        }
        let used = payload.len() as u64;
        for u in &p.used {
            payload.extend_from_slice(&[resource_raw(u.kind), root_raw(u.root) as u8, 0, 0]);
            for v in [u.table_offset_dwords, u.lower_bound, u.stride] {
                payload.extend_from_slice(&v.to_le_bytes());
            }
            payload.extend_from_slice(&[u.dwords, u.user_data, u.slot, 0]);
        }
        let flags = if p.wave64 { 0 } else { FLAG_WAVE32 };
        let lds = match &p.io {
            StageIo::Ms(m) => m.lds_bytes,
            _ => 0,
        };
        let r = &mut payload[record..];
        put(r, 0, &code.to_le_bytes());
        put(r, 8, &(p.code.len() as u64).to_le_bytes());
        put(r, RECORD_FLAGS, &flags.to_le_bytes());
        put(r, RECORD_LAYOUT, &layout.to_le_bytes());
        put(r, RECORD_USED, &used.to_le_bytes());
        put(r, RECORD_COUNTS, &(p.layout.len() as u32).to_le_bytes());
        put(r, RECORD_COUNTS + 4, &(p.used.len() as u32).to_le_bytes());
        put(r, RECORD_LDS, &lds.to_le_bytes());
        put(r, RECORD_REGS, &(u32::from(p.vgprs) | SGPRS_USED << 20).to_le_bytes());
    }
    let mut out = b"FSSM".to_vec();
    out.extend_from_slice(&FSSM_VERSION.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// `unpacked` (an `FSSM` blob) packed as the console's `XBCH` blob.
pub fn xbch(unpacked: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; zlib_rs::compress_bound(unpacked.len())];
    let (stream, rc) = zlib_rs::compress_slice(&mut buf, unpacked, zlib_rs::DeflateConfig::new(6));
    assert_eq!(rc, zlib_rs::ReturnCode::Ok, "zlib refused a buffer of its own bound");
    let mut out = b"XBCH".to_vec();
    out.extend_from_slice(&XBCH_VERSION.to_le_bytes());
    out.extend_from_slice(&(stream.len() as u64).to_le_bytes());
    out.extend_from_slice(&(unpacked.len() as u64).to_le_bytes());
    out.extend_from_slice(stream);
    out
}

/// `DXIL` part data: an empty program header (no bitcode: the native code
/// is what gets translated), the `XDAT` index, then the blobs `(kind,
/// bytes)`.
pub fn dxil_part(blobs: &[(u32, &[u8])]) -> Vec<u8> {
    let mut out = vec![0u8; XDAT_OFFSET];
    out.extend_from_slice(b"XDAT");
    out.extend_from_slice(&(blobs.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x24u32.to_le_bytes());
    let mut offset = out.len() + 12 * blobs.len();
    for (kind, blob) in blobs {
        for v in [*kind, offset as u32, blob.len() as u32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        offset += blob.len();
    }
    for (_, blob) in blobs {
        out.extend_from_slice(blob);
    }
    out
}

/// A whole shader container holding `programs`, with `parts` (signatures,
/// say) before its `DXIL` part.
pub fn container(programs: &[NativeProgram], parts: &[(crate::FourCC, &[u8])]) -> Vec<u8> {
    let native = xbch(&fssm(programs));
    let dxil = dxil_part(&[(NATIVE_BLOB, &native)]);
    let mut all: Vec<(crate::FourCC, &[u8])> = parts.to_vec();
    all.push((crate::FourCC::DXIL, &dxil));
    crate::dxbc::build(&all)
}

/// GFX10 encodings of the few instructions [`pattern_store`] needs, from
/// the public instruction set reference (the same forms the translator's
/// own tests write).
pub mod gfx10 {
    pub const S_ENDPGM: u32 = 0xBF81_0000;

    /// SOP1 `s_mov_b32 sdst, src0` (`src0`: an SGPR, or 128.. inline).
    pub const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
        0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
    }

    /// SMEM `s_load_dwordx4 s[dst:dst+3], s[base:base+1], 0`.
    pub const fn s_load_dwordx4(dst: u8, base: u8) -> [u32; 2] {
        [0x3D << 26 | 0x02 << 18 | (dst as u32) << 6 | (base as u32 >> 1), 125 << 25]
    }

    /// VOP2 `op vdst, src0, vsrc1` (`src0`: 0..105 an SGPR, 128.. inline,
    /// 256.. a VGPR).
    pub const fn vop2(op: u8, vdst: u8, src0: u16, vsrc1: u8) -> u32 {
        (op as u32 & 0x3F) << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
    }

    pub const V_XOR_B32: u8 = 0x1D;
    pub const V_LSHLREV_B32: u8 = 0x1A;
    pub const V_ADD_NC_U32: u8 = 0x25;

    /// MUBUF `buffer_store_dword vdata, vaddr, s[srsrc:srsrc+3], 0 offen`.
    pub const fn buffer_store_dword_offen(vdata: u8, vaddr: u8, srsrc: u8) -> [u32; 2] {
        [0x38 << 26 | 0x1C << 18 | 1 << 12, 0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32]
    }
}

/// Threads of [`pattern_store`]'s one group.
pub const PATTERN_THREADS: u32 = 64;

/// What [`pattern_store`] stores for thread `i` with root constants `c`.
pub fn pattern_value(i: u32, c: [u32; 2]) -> u32 {
    i.wrapping_add(c[1]) ^ c[0]
}

/// A compute program of one group of [`PATTERN_THREADS`] (wave32): root
/// constants in root slot 0 (four dwords, s0-s3) and a descriptor table in
/// root slot 1 (s4) whose first descriptor is a raw UAV; thread `i` stores
/// [`pattern_value`]`(i, [s0, s1])` at byte `4 i` of that UAV.
pub fn pattern_store() -> NativeProgram {
    use gfx10::*;
    let mut words = vec![s_mov_b32(5, 0x80)]; // the table's high half: 0
    words.extend(s_load_dwordx4(8, 4));
    words.push(vop2(V_LSHLREV_B32, 1, 0x82, 0)); // v1 = v0 << 2
    words.push(vop2(V_ADD_NC_U32, 2, 1, 0)); // v2 = s1 + v0
    words.push(vop2(V_XOR_B32, 2, 0, 2)); // v2 = s0 ^ v2
    words.extend(buffer_store_dword_offen(2, 1, 8));
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![
            RootParam { root: RootType::Constants, table_offset_dwords: 0, sgprs: 4, base_register: 0, user_data: 0, slot: 0 },
            RootParam { root: RootType::Table, table_offset_dwords: 0, sgprs: 1, base_register: 0, user_data: 4, slot: 1 },
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
        io: StageIo::Cs(ComputeIo { rsrc2: 5 << 1, threads: [PATTERN_THREADS, 1, 1] }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every program comes back as it was written, stage data included.
    #[test]
    fn programs_read_back_as_written() {
        let cs = pattern_store();
        let vs = NativeProgram {
            stage: HwStage::Vs,
            wave64: true,
            vgprs: 12,
            layout: vec![],
            used: vec![],
            io: StageIo::Vs {
                inputs: vec![VsInput { usage: Usage::Generic, row: 0, mask: 0b0111, vgpr: 4 }],
                outputs: vec![VsOutput { usage: Usage::Generic, row: 1, param: 0, fp16: false, pack2: false }],
            },
            code: gfx10::S_ENDPGM.to_le_bytes().to_vec(),
        };
        let ps = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs: vec![PsInput { usage: Usage::Generic, row: 1, mask: 3, attr: 0, flat: true, fp16: false, pack2: false, row2: 0 }],
                input_addr: 2,
                db_shader_control: 0,
                patches: vec![ExportPatch { mrt: 0, offset: 0, code: vec![0x1f, 0x18] }],
            },
            code: [0xBF80_0000u32, 0xBF80_0000, gfx10::S_ENDPGM].iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let programs = [vs, ps, cs];
        let data = dxil_part(&[(0x0a, b"xbop"), (NATIVE_BLOB, &xbch(&fssm(&programs)))]);
        assert_eq!(native_programs(&data).unwrap(), programs);
    }

    /// The self-test's program goes through the whole translation from its
    /// container, as a title's would.
    #[test]
    fn the_pattern_program_translates_from_its_container() {
        let container = container(&[pattern_store()], &[]);
        let shader = crate::emit::translate(&container).expect("translates");
        assert_eq!(shader.stage, HwStage::Cs);
        assert!(!shader.dxil.is_empty());
        assert_eq!(pattern_value(3, [0x10, 5]), 8 ^ 0x10);
    }
}
