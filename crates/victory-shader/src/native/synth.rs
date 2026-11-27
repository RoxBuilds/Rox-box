//! Console shader containers made by our own code: the inverse of
//! [`super::native_programs`], for programs we write ourselves from the
//! public GFX10 instruction encodings.
//!
//! Nothing in the repository may come from a title, so whatever exercises
//! the whole translation path (container, `XDAT` index, `XBCH` packing,
//! `FSSM` records, root tables, stage data, the `ISG1`/`OSG1` signatures a
//! vertex and a pixel program link by) has to build its input. This module
//! writes exactly the layout `native.rs` and `emit` document and read, and
//! nothing else: a field the reader ignores is written as zero (of a pixel
//! program's export patch, only the 32-bit variant the layer applies). The
//! parser's own tests and `victory selftest` (`victory_gpu::selftest`) use
//! it; a container from here goes through `emit::translate` like any other.
//!
//! shortcut: mesh stage data and merged stages are not written (their
//! records get empty stage data); `FSPX`, `PSV0` and `PSG1` parts are not
//! made.

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

/// One row of an `ISG1`/`OSG1` part as the translator reads it
/// (`emit::signature`): the fields a console compiler writes for an HLSL
/// signature element. The translator keeps semantics, registers and masks,
/// so the host's stages link by them as the title's did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigRow {
    pub name: &'static str,
    pub index: u32,
    /// `D3D_NAME`: [`sig::ARBITRARY`], [`sig::POSITION`], [`sig::TARGET`]...
    pub system_value: u32,
    /// `D3D_REGISTER_COMPONENT_TYPE`: 1 uint32, 2 sint32, [`sig::FLOAT32`].
    pub component: u32,
    pub register: u32,
    /// Channels the element has.
    pub mask: u8,
    /// Inputs: the channels the program reads; outputs: those it never
    /// writes. The translator reads neither (it sees what the code does).
    pub rw: u8,
}

/// The values [`SigRow`] takes.
pub mod sig {
    pub const ARBITRARY: u32 = 0;
    pub const POSITION: u32 = 1;
    pub const TARGET: u32 = 64;
    pub const FLOAT32: u32 = 3;
}

/// An `ISG1`/`OSG1` part: a count, the offset of the rows (8), 32-byte rows
/// (stream, name offset, semantic index, system value, component type,
/// register, mask, read/write mask, minimum precision), then the names.
pub fn signature(rows: &[SigRow]) -> Vec<u8> {
    let mut out = (rows.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    let mut name_at = 8 + 32 * rows.len();
    for r in rows {
        for v in [0, name_at as u32, r.index, r.system_value, r.component, r.register] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&[r.mask, r.rw, 0, 0]);
        out.extend_from_slice(&0u32.to_le_bytes());
        name_at += r.name.len() + 1;
    }
    for r in rows {
        out.extend_from_slice(r.name.as_bytes());
        out.push(0);
    }
    out
}

/// A container holding one graphics `program` with its input and output
/// signatures, as a console compiler emits a vertex or pixel shader.
pub fn graphics_container(program: NativeProgram, inputs: &[SigRow], outputs: &[SigRow]) -> Vec<u8> {
    let (isg1, osg1) = (signature(inputs), signature(outputs));
    container(&[program], &[(crate::FourCC::ISG1, &isg1), (crate::FourCC::OSG1, &osg1)])
}

/// GFX10 encodings of the instructions the self-test's programs need, from
/// the public instruction set reference (the same forms the translator's
/// own tests write).
pub mod gfx10 {
    pub const S_ENDPGM: u32 = 0xBF81_0000;

    /// SOPP `s_nop wait` (`wait` + 1 cycles of nothing).
    pub const fn s_nop(wait: u16) -> u32 {
        0xBF80_0000 | wait as u32
    }

    /// SOP1 `s_mov_b32 sdst, src0` (`src0`: an SGPR, or 128.. inline).
    pub const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
        0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
    }

    /// A VALU source operand naming VGPR `n` (`src0` 256..511).
    pub const fn vgpr(n: u8) -> u16 {
        256 + n as u16
    }

    /// Inline float constants (`src0` 240..247).
    pub const F_HALF: u16 = 0xF0;
    pub const F_ONE: u16 = 0xF2;

    /// VOP1 `op vdst, src0` (`src0` as for [`vop2`]).
    pub const fn vop1(op: u8, vdst: u8, src0: u16) -> u32 {
        0x7E00_0000 | (vdst as u32) << 17 | (op as u32) << 9 | (src0 as u32 & 0x1FF)
    }

    pub const V_MOV_B32: u8 = 0x01;

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
    /// Two f32 to a dword of two f16, rounded toward zero: how a pixel
    /// program packs its colour for a 16-bit render target.
    pub const V_CVT_PKRTZ_F16_F32: u8 = 0x2F;

    /// MUBUF `buffer_store_dword vdata, vaddr, s[srsrc:srsrc+3], 0 offen`.
    pub const fn buffer_store_dword_offen(vdata: u8, vaddr: u8, srsrc: u8) -> [u32; 2] {
        [0x38 << 26 | 0x1C << 18 | 1 << 12, 0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32]
    }

    /// VINTRP: `op` 0 `v_interp_p1_f32 vdst, vsrc (i), attr<attr>.<chan>`,
    /// 1 `v_interp_p2_f32 vdst, vsrc (j), ...` (vdst holds the first half).
    const fn vintrp(op: u32, vdst: u8, vsrc: u8, attr: u8, chan: u8) -> u32 {
        0x32 << 26 | (vdst as u32) << 18 | op << 16 | (attr as u32 & 0x3F) << 10 | (chan as u32 & 3) << 8 | vsrc as u32
    }

    pub const fn v_interp_p1_f32(vdst: u8, i: u8, attr: u8, chan: u8) -> u32 {
        vintrp(0, vdst, i, attr, chan)
    }

    pub const fn v_interp_p2_f32(vdst: u8, j: u8, attr: u8, chan: u8) -> u32 {
        vintrp(1, vdst, j, attr, chan)
    }

    /// Export targets: colour target `n` is `MRT0 + n`, position `n`
    /// `POS0 + n`, parameter `n` `PARAM0 + n`.
    pub const MRT0: u8 = 0;
    pub const POS0: u8 = 12;
    pub const PARAM0: u8 = 32;

    /// EXP `exp target, vsrc0..vsrc3` of the channels `en` enables; `done`
    /// marks the last export of its kind (for a pixel program, the end),
    /// `vm` that EXEC is the valid mask.
    pub const fn exp(target: u8, en: u8, vsrc: [u8; 4], done: bool, vm: bool) -> [u32; 2] {
        [
            0x3E << 26 | (vm as u32) << 12 | (done as u32) << 11 | ((target as u32) & 0x3F) << 4 | (en as u32 & 0xF),
            u32::from_le_bytes(vsrc),
        ]
    }

    /// EXP with `compr`: four 16-bit channels in two VGPRs (`lo`: x and y,
    /// `hi`: z and w).
    pub const fn exp_compr(target: u8, lo: u8, hi: u8, done: bool, vm: bool) -> [u32; 2] {
        let [w0, _] = exp(target, 0xF, [0; 4], done, vm);
        [w0 | 1 << 10, (hi as u32) << 8 | lo as u32]
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

/// The self-test's vertex program reads its position from vertex attribute
/// `POSITION` (x, y), which the fetch shader loads into these VGPRs.
pub const DRAW_POSITION_VGPR: u8 = 4;

/// The rows the self-test's programs link by: the position in row 0, the
/// colour in row 1 (`exp param0` of the vertex program, `attr0` of the
/// pixel program).
const POSITION_ROW: u8 = 0;
const COLOUR_ROW: u8 = 1;

fn row(name: &'static str, system_value: u32, register: u8, mask: u8, rw: u8) -> SigRow {
    SigRow { name, index: 0, system_value, component: sig::FLOAT32, register: register.into(), mask, rw }
}

/// [`draw_vertex`]'s program.
fn vertex_program() -> NativeProgram {
    use gfx10::*;
    let at = DRAW_POSITION_VGPR;
    let mut words = vec![
        vop1(V_MOV_B32, 1, F_HALF),
        vop1(V_MOV_B32, 2, F_ONE),
        vop1(V_MOV_B32, 6, 0),
        vop1(V_MOV_B32, 7, 1),
        vop1(V_MOV_B32, 8, 2),
        vop1(V_MOV_B32, 9, 3),
    ];
    // The positions first, the last with `done`, then the parameters.
    words.extend(exp(POS0, 0xF, [at, at + 1, 1, 2], true, false));
    words.extend(exp(PARAM0, 0xF, [6, 7, 8, 9], false, false));
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Vs,
        wave64: false,
        vgprs: 12,
        layout: vec![RootParam { root: RootType::Constants, table_offset_dwords: 0, sgprs: 4, base_register: 0, user_data: 0, slot: 0 }],
        used: vec![],
        io: StageIo::Vs {
            inputs: vec![VsInput { usage: Usage::Generic, row: POSITION_ROW, mask: 0b0011, vgpr: at }],
            outputs: vec![VsOutput { usage: Usage::Generic, row: COLOUR_ROW, param: 0, fp16: false, pack2: false }],
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// A vertex program's container (hardware VS, wave32): the position is
/// `(x, y, 0.5, 1)` with x and y from the vertex's `POSITION` attribute
/// (input row 0, loaded into v4 and v5); the colour (`COLOR`, output row 1,
/// `exp param0`) is the four root constants of root slot 0 (s0-s3), the
/// same at every vertex.
pub fn draw_vertex() -> Vec<u8> {
    graphics_container(
        vertex_program(),
        &[row("POSITION", sig::ARBITRARY, POSITION_ROW, 0b0011, 0b0011)],
        &[row("SV_Position", sig::POSITION, POSITION_ROW, 0xF, 0), row("COLOR", sig::ARBITRARY, COLOUR_ROW, 0xF, 0)],
    )
}

/// Where [`draw_pixel`]'s colour export starts in its code, in dwords: the
/// place of its export patch.
const PIXEL_EXPORT_AT: usize = 7;

/// The colour export for a 16-bit float target, as a console compiler
/// leaves it in the code: packed in pairs, compressed (4 dwords).
fn export_fp16() -> Vec<u32> {
    use gfx10::*;
    let mut words = vec![vop2(V_CVT_PKRTZ_F16_F32, 2, vgpr(2), 3), vop2(V_CVT_PKRTZ_F16_F32, 3, vgpr(4), 5)];
    words.extend(exp_compr(MRT0, 2, 3, true, true));
    words
}

/// The same export for a target of 32-bit channels: every channel as it
/// is, padded to the patch's size (the variant the layer applies).
fn export_full() -> Vec<u32> {
    use gfx10::*;
    let mut words = exp(MRT0, 0xF, [2, 3, 4, 5], true, true).to_vec();
    words.extend([s_nop(0), s_nop(0)]);
    words
}

/// [`draw_pixel`]'s program.
fn pixel_program() -> NativeProgram {
    use gfx10::*;
    let mut words = Vec::new();
    for chan in 0..3u8 {
        // i and j arrive in v0 and v1 (`SPI_PS_INPUT_ADDR` bit 1).
        words.push(v_interp_p1_f32(2 + chan, 0, 0, chan));
        words.push(v_interp_p2_f32(2 + chan, 1, 0, chan));
    }
    words.push(vop1(V_MOV_B32, 5, 0));
    assert_eq!(words.len(), PIXEL_EXPORT_AT);
    words.extend(export_fp16());
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 8,
        layout: vec![RootParam { root: RootType::Constants, table_offset_dwords: 0, sgprs: 1, base_register: 1, user_data: 0, slot: 1 }],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![PsInput {
                usage: Usage::Generic,
                row: COLOUR_ROW,
                mask: 0xF,
                attr: 0,
                flat: false,
                fp16: false,
                pack2: false,
                row2: 0,
            }],
            input_addr: 1 << 1,
            db_shader_control: 0,
            patches: vec![ExportPatch { mrt: 0, offset: PIXEL_EXPORT_AT as u32, code: export_full() }],
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

fn pixel_container(program: NativeProgram) -> Vec<u8> {
    graphics_container(
        program,
        &[row("SV_Position", sig::POSITION, POSITION_ROW, 0xF, 0), row("COLOR", sig::ARBITRARY, COLOUR_ROW, 0xF, 0b0111)],
        &[row("SV_Target", sig::TARGET, 0, 0xF, 0)],
    )
}

/// A pixel program's container (wave32): red, green and blue interpolated
/// from the colour its vertex program exports (input row 1, `attr0`, at the
/// perspective-correct pixel centre), alpha its own root constant (root
/// slot 1, s0), exported to render target 0. The export in the code is the
/// one for a 16-bit float target (which the translator refuses: it is
/// compressed); the stage data's export patch for target 0 carries the one
/// for 32-bit channels, which the layer applies, as the console does for
/// the format a pipeline renders to.
pub fn draw_pixel() -> Vec<u8> {
    pixel_container(pixel_program())
}

/// What [`draw_vertex`] and [`draw_pixel`] write to a render target for
/// root constants `vs` (the vertex program's, f32 bits) and `ps` (the
/// pixel program's).
pub fn draw_colour(vs: [u32; 4], ps: u32) -> [f32; 4] {
    [f32::from_bits(vs[0]), f32::from_bits(vs[1]), f32::from_bits(vs[2]), f32::from_bits(ps)]
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

    /// The one instruction `words` (then `s_endpgm`), decoded by the
    /// crate's own decoder: its name, fields and SP3 text.
    fn decoded(words: &[u32]) -> (&'static str, crate::gfx10::Fields, String) {
        let mut code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        code.extend(gfx10::S_ENDPGM.to_le_bytes());
        let insts = crate::gfx10::decode(&code).expect("decodes");
        assert_eq!(insts.len(), 2, "{words:08x?} is one instruction");
        assert_eq!(insts[0].words, words, "{words:08x?} is one instruction of {} dwords", words.len());
        let i = &insts[0];
        (i.name().expect("a known opcode"), i.fields(false), i.text(false))
    }

    /// Each encoder decodes back to what it says it writes.
    #[test]
    fn every_encoder_decodes_back() {
        use crate::gfx10::{Fields, Operand};
        use gfx10::*;
        let v = |reg| Some(Operand::Vector { reg, dwords: 1 });
        let (name, fields, _) = decoded(&[s_nop(0)]);
        assert_eq!((name, fields), ("s_nop", Fields::Sopp { imm: 0 }));
        let (name, fields, text) = decoded(&[vop1(V_MOV_B32, 6, 3)]);
        assert_eq!(name, "v_mov_b32");
        let Fields::Valu(valu) = fields else { panic!("{text}") };
        assert_eq!((valu.dst, &valu.src[..]), (v(6), &[Operand::Scalar { code: 3, dwords: 1 }][..]));
        let (_, fields, _) = decoded(&[vop1(V_MOV_B32, 1, F_HALF)]);
        let Fields::Valu(valu) = fields else { panic!() };
        assert_eq!(valu.src, [Operand::Const(0xF0)]);
        let (name, fields, text) = decoded(&[vop2(V_CVT_PKRTZ_F16_F32, 2, vgpr(4), 5)]);
        assert_eq!(name, "v_cvt_pkrtz_f16_f32");
        let Fields::Valu(valu) = fields else { panic!("{text}") };
        assert_eq!((valu.dst, &valu.src[..]), (v(2), &[v(4).unwrap(), v(5).unwrap()][..]));
        for (word, want) in [(v_interp_p1_f32(3, 0, 2, 1), "v_interp_p1_f32"), (v_interp_p2_f32(4, 1, 0, 3), "v_interp_p2_f32")] {
            let (name, fields, _) = decoded(&[word]);
            assert_eq!(name, want);
            let Fields::Vintrp { dst, src, attr } = fields else { panic!() };
            let (vdst, vsrc, index, chan) = if want.contains("p1") { (3, 0, 2, 1) } else { (4, 1, 0, 3) };
            assert_eq!((Some(dst), Some(src)), (v(vdst), v(vsrc)));
            assert_eq!(attr, Operand::Attr { index, chan, high: false });
        }
        let exports = [
            (exp(POS0, 0xF, [4, 5, 1, 2], true, false), "exp pos0, v4, v5, v1, v2 done"),
            (exp(PARAM0, 0xF, [6, 7, 8, 9], false, false), "exp param0, v6, v7, v8, v9"),
            (exp(PARAM0 + 3, 0b0101, [6, 7, 8, 9], false, false), "exp param3, v6, off, v8, off"),
            (exp(MRT0, 0xF, [2, 3, 4, 5], true, true), "exp mrt0, v2, v3, v4, v5 done vm"),
            (exp_compr(MRT0 + 1, 2, 3, true, true), "exp mrt1, v2, v2, v3, v3 done compr vm"),
        ];
        for (words, want) in exports {
            let (name, fields, text) = decoded(&words);
            assert_eq!((name, &text[..]), ("exp", want), "{words:08x?}");
            assert!(matches!(fields, Fields::Exp { .. }));
        }
        // The compressed form as the public reference's assembler writes it.
        assert_eq!(exp_compr(MRT0, 2, 3, true, true), [0xF800_1C0F, 0x0000_0302]);
    }

    /// A signature part reads back, through the translator's own reader,
    /// as the rows it was written from.
    #[test]
    fn signatures_read_back_as_written() {
        let rows = [
            SigRow { name: "SV_Position", index: 0, system_value: sig::POSITION, component: sig::FLOAT32, register: 0, mask: 0xF, rw: 0 },
            SigRow { name: "COLOR", index: 2, system_value: sig::ARBITRARY, component: sig::FLOAT32, register: 3, mask: 0b0110, rw: 0b0010 },
            SigRow { name: "SV_Target", index: 1, system_value: sig::TARGET, component: sig::FLOAT32, register: 1, mask: 0xF, rw: 0 },
        ];
        let read = crate::emit::signature(&signature(&rows)).expect("reads");
        let got: Vec<_> = read.iter().map(|e| (e.name.as_str(), e.index, e.kind, e.comp, e.row, e.col, e.cols)).collect();
        assert_eq!(
            got,
            [("SV_Position", 0, 3, 9, Some(0), 0, 4), ("COLOR", 2, 0, 9, Some(3), 1, 2), ("SV_Target", 1, 16, 9, Some(1), 0, 4)]
        );
        assert!(crate::emit::signature(&signature(&[])).expect("reads").is_empty());
    }

    /// The self-test's vertex and pixel programs read back with their stage
    /// data and export patch, and translate from their containers; the
    /// host's signatures keep the rows the two link by.
    #[test]
    fn the_draw_programs_translate_from_their_containers() {
        let sig_of = |dxil: &[u8], fourcc| {
            let c = crate::Container::parse(dxil).expect("a container");
            let part = c.part(fourcc).expect("the signature part");
            let rows = crate::emit::signature(part.data).expect("reads");
            rows.iter().map(|e| (e.name.clone(), e.kind, e.row)).collect::<Vec<_>>()
        };
        let row = |name: &str, kind, r| (name.to_owned(), kind, Some(r));
        let vs = crate::emit::translate(&draw_vertex()).expect("the vertex program translates");
        assert_eq!(vs.stage, HwStage::Vs);
        assert_eq!(vs.user_data.iter().map(|u| (u.index, u.slot, u.dword)).collect::<Vec<_>>(), [(0, 0, 0), (1, 0, 1), (2, 0, 2), (3, 0, 3)]);
        assert!(sig_of(&vs.dxil, crate::FourCC::ISG1).contains(&row("POSITION", 0, 0)));
        assert_eq!(sig_of(&vs.dxil, crate::FourCC::OSG1), [row("SV_Position", 3, 0), row("COLOR", 0, 1)]);

        let container = draw_pixel();
        let c = crate::Container::parse(&container).unwrap();
        let programs = native_programs(c.part(crate::FourCC::DXIL).unwrap().data).unwrap();
        assert_eq!(programs, [pixel_program()]);
        let patched = programs[0].patched_code();
        let at = PIXEL_EXPORT_AT * 4;
        let full: Vec<u8> = export_full().iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(&patched[at..at + full.len()], &full[..]);
        let ps = crate::emit::translate(&container).expect("the pixel program translates");
        assert_eq!(ps.stage, HwStage::Ps);
        assert_eq!(ps.user_data.iter().map(|u| (u.index, u.slot, u.dword)).collect::<Vec<_>>(), [(0, 1, 0)]);
        assert!(sig_of(&ps.dxil, crate::FourCC::ISG1).contains(&row("COLOR", 0, 1)));
        assert_eq!(sig_of(&ps.dxil, crate::FourCC::OSG1), [row("SV_Target", 16, 0)]);
        // Without its patch the export left in the code is the compressed
        // one, which the translator refuses: the patch is what is drawn with.
        let mut bare = pixel_program();
        if let StageIo::Ps { patches, .. } = &mut bare.io {
            patches.clear();
        }
        let refused = crate::emit::translate(&pixel_container(bare)).expect_err("a compressed export");
        assert!(refused.to_string().contains("compressed"), "{refused}");
        #[cfg(windows)]
        if let Some(path) = crate::emit::validator::Validator::default_path() {
            let mut v = crate::emit::validator::Validator::load(Some(&path)).expect("dxil.dll loads");
            for shader in [&vs, &ps] {
                v.validate(&mut shader.dxil.clone()).unwrap_or_else(|e| panic!("{:?}: {e}", shader.stage));
            }
        }
        assert_eq!(draw_colour([0.25f32.to_bits(), 0.5f32.to_bits(), 1.0f32.to_bits(), 0], 0.75f32.to_bits()), [0.25, 0.5, 1.0, 0.75]);
    }
}
