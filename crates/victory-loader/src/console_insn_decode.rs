//! Clean-room x86-64 decoder for AMD/console-specific user-mode instructions
//! that may raise `STATUS_ILLEGAL_INSTRUCTION` on host processors.
//!
//! Implemented directly from the public AMD64 Architecture Programmer's Manual
//! (Volume 3: General-Purpose and System Instructions; Volume 4: 128-Bit and
//! 256-Bit Media Instructions):
//! - `CLZERO` (`0F 01 FC`), including a common 12-byte cache-line zeroing loop
//! - `RDPRU` (`0F 01 FD`)
//! - `MONITORX` (`0F 01 FA`) and `MWAITX` (`0F 01 FB`)
//! - `UD2` (`0F 0B`)
//! - `CLWB m` (`66 0F AE /6`)
//! - `RDPID r` (`F3 0F C7 /7`)
//! - `EXTRQ` (`66 0F 78 /0 ib ib`, `66 0F 79 /r`) and `INSERTQ` (`F2 0F 78 /r ib ib`, `F2 0F 79 /r`)
//! - `MOVNTSS m32, xmm` (`F3 0F 2B /r`) and `MOVNTSD m64, xmm` (`F2 0F 2B /r`)

/// Maximum architectural instruction length on x86-64.
const MAX_INSN_LEN: usize = 15;

/// Console-specific or privileged/ISA-extension instructions handled by the
/// exception-emulation path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleOnly {
    /// Cache line write back (`66 0F AE /6`).
    Clwb,
    /// Zero cache line (`0F 01 FC`).
    Clzero,
    /// Standard 12-byte zeroing loop starting with `clzero`:
    /// `clzero; add rax, 0x40; cmp rax, <end_reg>; jb -12`.
    ClzeroLoop { end_reg: u8 },
    /// Read processor register (`0F 01 FD`).
    Rdpru,
    /// Set up monitor address (`0F 01 FA`).
    Monitorx,
    /// Monitor wait with optional timeout (`0F 01 FB`).
    Mwaitx,
    /// Read processor ID into register `0..=15` (`F3 0F C7 /7`).
    Rdpid(u8),
    /// SSE4a bit-field extract (`66 0F 78 /0 ib ib` or `66 0F 79 /r`).
    Extrq { dst: u8, src: Option<u8>, len: u8, idx: u8 },
    /// SSE4a bit-field insert (`F2 0F 78 /r ib ib` or `F2 0F 79 /r`).
    Insertq { dst: u8, src: u8, reg: bool, len: u8, idx: u8 },
    /// SSE4a scalar non-temporal store of the low 4 (`F3 0F 2B /r`) or, `wide`, the low 8
    /// bytes (`F2 0F 2B /r`) of XMM register `src` to memory.
    Movnt { src: u8, wide: bool, mem: Mem },
    /// Undefined instruction trap (`0F 0B`), left fatal.
    Ud2,
}

/// A ModRM memory operand in 64-bit mode: `base + (index << scale) + disp`, from the end of
/// the instruction where `rip`, and cut to 32 bits where `a32` (a `67h` prefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mem {
    /// Register `0..=15`.
    pub base: Option<u8>,
    /// Register `0..=15`, shifted left by `scale` (`0..=3`).
    pub index: Option<u8>,
    pub scale: u8,
    pub disp: i32,
    pub rip: bool,
    pub a32: bool,
}

impl Mem {
    /// The address named with the general registers `regs` (in encoding order), for an
    /// instruction that ends at `next`.
    pub fn address(&self, regs: &[u64; 16], next: u64) -> u64 {
        let base = if self.rip { next } else { self.base.map_or(0, |r| regs[r as usize & 15]) };
        let index = self.index.map_or(0, |r| regs[r as usize & 15] << self.scale);
        let at = base.wrapping_add(index).wrapping_add(self.disp as i64 as u64);
        if self.a32 { at & 0xFFFF_FFFF } else { at }
    }
}

/// The memory operand of `modrm` (`mod != 0b11`) with the bytes after it, and how many of
/// those are its SIB byte and displacement.
fn modrm_mem_operand(after_modrm: &[u8], modrm: u8, rex_x: u8, rex_b: u8, a32: bool) -> Option<(Mem, usize)> {
    let extra = modrm_mem_operand_len(after_modrm, modrm)?;
    let (mod_bits, rm) = (modrm >> 6, modrm & 7);
    let mut mem = Mem { base: Some(rm | rex_b), index: None, scale: 0, disp: 0, rip: false, a32 };
    let mut at = 0usize;
    if rm == 4 {
        let sib = after_modrm[0];
        at = 1;
        mem.base = Some((sib & 7) | rex_b);
        // (index 4 without REX.X is none)
        let index = ((sib >> 3) & 7) | rex_x;
        if index != 4 {
            mem.index = Some(index);
            mem.scale = sib >> 6;
        }
        if mod_bits == 0 && (sib & 7) == 5 {
            mem.base = None;
        }
    } else if mod_bits == 0 && rm == 5 {
        mem.base = None;
        mem.rip = true;
    }
    mem.disp = match extra - at {
        1 => i32::from(after_modrm[at] as i8),
        4 => i32::from_le_bytes(after_modrm[at..at + 4].try_into().ok()?),
        _ => 0,
    };
    Some((mem, extra))
}

/// Evaluates `EXTRQ` on the low 64 bits of an XMM register: extracts `len` bits
/// (64 when `len & 63 == 0`) starting at bit `idx & 63`, zero-extended.
pub fn extrq(v: u64, len: u8, idx: u8) -> u64 {
    let field_len = (len & 0x3f) as u32;
    let bit_idx = (idx & 0x3f) as u32;
    let width = if field_len == 0 { 64 } else { field_len };
    let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
    (v >> bit_idx) & mask
}

/// Evaluates `INSERTQ` on the low 64 bits of XMM registers: inserts the low
/// `len` bits of `src` (64 when `len & 63 == 0`) at bit `idx & 63` of `dst`.
pub fn insertq(dst: u64, src: u64, len: u8, idx: u8) -> u64 {
    let field_len = (len & 0x3f) as u32;
    let bit_idx = (idx & 0x3f) as u32;
    let width = if field_len == 0 { 64 } else { field_len };
    let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
    (dst & !(mask << bit_idx)) | ((src & mask) << bit_idx)
}

fn is_legacy_prefix(byte: u8) -> bool {
    matches!(
        byte,
        0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65 | 0x66 | 0x67 | 0xF0 | 0xF2 | 0xF3
    )
}

/// Computes the byte length of ModRM + optional SIB + displacement for a
/// memory operand (`mod != 0b11`) in 64-bit mode (`addr_32` for `67h` is
/// handled uniformly for ModRM 32-bit/64-bit SIB+disp encoding).
fn modrm_mem_operand_len(after_modrm: &[u8], modrm: u8) -> Option<usize> {
    let mod_bits = modrm >> 6;
    let rm = modrm & 7;
    if mod_bits == 3 {
        return None;
    }
    let mut extra = 0usize;
    let base_is_5 = if rm == 4 {
        let sib = *after_modrm.first()?;
        extra += 1;
        (sib & 7) == 5
    } else {
        false
    };
    let disp = match mod_bits {
        0 => usize::from(rm == 5 || base_is_5) * 4,
        1 => 1,
        2 => 4,
        _ => unreachable!(),
    };
    if after_modrm.len() < extra + disp {
        return None;
    }
    Some(extra + disp)
}

fn match_clzero_loop(code: &[u8]) -> Option<u8> {
    if code.len() < 12 {
        return None;
    }
    // clzero; add rax, 0x40; cmp rax, r64; jb -12
    if code[..7] != [0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40] {
        return None;
    }
    let rex_cmp = code[7];
    let opcode_cmp = code[8];
    let modrm_cmp = code[9];
    if !matches!(rex_cmp, 0x48 | 0x4C) || opcode_cmp != 0x39 {
        return None;
    }
    // `cmp rax, reg`: ModRM.mod == 3, ModRM.rm == 0 (rax), ModRM.reg != rax
    if (modrm_cmp & 0xC7) != 0xC0 || (rex_cmp == 0x48 && modrm_cmp == 0xC0) {
        return None;
    }
    if code[10..12] != [0x72, 0xF4] {
        return None;
    }
    let reg = ((modrm_cmp >> 3) & 7) | if (rex_cmp & 0x04) != 0 { 8 } else { 0 };
    Some(reg)
}

/// Decodes one instruction from `code`, returning `(kind, byte_len)` if it is
/// one of the handled console/extension instructions and fits in `code`.
pub fn decode(code: &[u8]) -> Option<(ConsoleOnly, usize)> {
    let mut pos = 0usize;
    let mut op_66 = false;
    let mut rep: Option<u8> = None;
    let mut a32 = false;
    // An FS or GS override: the operand is from a base the decoder does not have.
    let mut fs_gs = false;

    while pos < code.len().min(MAX_INSN_LEN) && is_legacy_prefix(code[pos]) {
        match code[pos] {
            0x66 => op_66 = true,
            0x67 => a32 = true,
            0x64 | 0x65 => fs_gs = true,
            0xF2 | 0xF3 => rep = Some(code[pos]),
            _ => {}
        }
        pos += 1;
    }

    let mut rex_r = 0u8;
    let mut rex_x = 0u8;
    let mut rex_b = 0u8;
    if let Some(&b) = code.get(pos)
        && (0x40..=0x4F).contains(&b)
    {
        if (b & 0x04) != 0 {
            rex_r = 8;
        }
        if (b & 0x02) != 0 {
            rex_x = 8;
        }
        if (b & 0x01) != 0 {
            rex_b = 8;
        }
        pos += 1;
    }

    if *code.get(pos)? != 0x0F {
        return None;
    }
    let op2 = *code.get(pos + 1)?;
    let finish = |kind: ConsoleOnly, len: usize| (len <= MAX_INSN_LEN && code.len() >= len).then_some((kind, len));

    match op2 {
        0x0B => finish(ConsoleOnly::Ud2, pos + 2),
        // Without F2 or F3 only: with one these bytes are other instructions (`F3 0F 01 FA`
        // is MCOMMIT, `F3 0F 01 FD` RMPQUERY), which are not emulated.
        0x01 if rep.is_none() => {
            let op3 = *code.get(pos + 2)?;
            match op3 {
                0xFA => finish(ConsoleOnly::Monitorx, pos + 3),
                0xFB => finish(ConsoleOnly::Mwaitx, pos + 3),
                0xFD => finish(ConsoleOnly::Rdpru, pos + 3),
                0xFC => {
                    if pos == 0
                        && let Some(end_reg) = match_clzero_loop(code)
                    {
                        return finish(ConsoleOnly::ClzeroLoop { end_reg }, 12);
                    }
                    finish(ConsoleOnly::Clzero, pos + 3)
                }
                _ => None,
            }
        }
        // MOVNTSS and MOVNTSD: a store of the register's low bytes (the non-temporal hint
        // is only about caching). To memory only: with a register operand they are undefined.
        0x2B if rep.is_some() && !fs_gs => {
            let modrm = *code.get(pos + 2)?;
            let (mem, extra) = modrm_mem_operand(code.get(pos + 3..)?, modrm, rex_x, rex_b, a32)?;
            let src = ((modrm >> 3) & 7) | rex_r;
            finish(ConsoleOnly::Movnt { src, wide: rep == Some(0xF2), mem }, pos + 3 + extra)
        }
        0xAE if op_66 && rep.is_none() => {
            let modrm = *code.get(pos + 2)?;
            if ((modrm >> 3) & 7) != 6 {
                return None;
            }
            let op_extra = modrm_mem_operand_len(&code[pos + 3..], modrm)?;
            finish(ConsoleOnly::Clwb, pos + 3 + op_extra)
        }
        0xC7 if rep == Some(0xF3) && !op_66 => {
            let modrm = *code.get(pos + 2)?;
            if (modrm >> 6) != 3 || ((modrm >> 3) & 7) != 7 {
                return None;
            }
            finish(ConsoleOnly::Rdpid((modrm & 7) | rex_b), pos + 3)
        }
        0x78 | 0x79 => {
            let modrm = *code.get(pos + 2)?;
            if (modrm >> 6) != 3 {
                return None;
            }
            let reg = ((modrm >> 3) & 7) | rex_r;
            let rm = (modrm & 7) | rex_b;
            match (op2, rep, op_66) {
                (0x78, Some(0xF2), _) => {
                    let len = *code.get(pos + 3)?;
                    let idx = *code.get(pos + 4)?;
                    finish(ConsoleOnly::Insertq { dst: reg, src: rm, reg: false, len, idx }, pos + 5)
                }
                (0x79, Some(0xF2), _) => finish(ConsoleOnly::Insertq { dst: reg, src: rm, reg: true, len: 0, idx: 0 }, pos + 3),
                (0x78, None, true) if ((modrm >> 3) & 7) == 0 => {
                    let len = *code.get(pos + 3)?;
                    let idx = *code.get(pos + 4)?;
                    finish(ConsoleOnly::Extrq { dst: rm, src: None, len, idx }, pos + 5)
                }
                (0x79, None, true) => finish(ConsoleOnly::Extrq { dst: reg, src: Some(rm), len: 0, idx: 0 }, pos + 3),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_clzero_and_zeroing_loop() {
        assert_eq!(decode(&[0x0F, 0x01, 0xFC]), Some((ConsoleOnly::Clzero, 3)));
        let loop_rdx = [0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40, 0x48, 0x39, 0xD0, 0x72, 0xF4];
        assert_eq!(decode(&loop_rdx), Some((ConsoleOnly::ClzeroLoop { end_reg: 2 }, 12)));
        let loop_r12 = [0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40, 0x4C, 0x39, 0xE0, 0x72, 0xF4];
        assert_eq!(decode(&loop_r12), Some((ConsoleOnly::ClzeroLoop { end_reg: 12 }, 12)));
        // Address-size override stays plain Clzero so 32-bit EAX masking applies.
        let a32_loop = [0x67, 0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40, 0x48, 0x39, 0xD0, 0x72, 0xF4];
        assert_eq!(decode(&a32_loop), Some((ConsoleOnly::Clzero, 4)));
    }

    #[test]
    fn decodes_rdpru_monitorx_mwaitx_ud2_clwb_and_rdpid() {
        assert_eq!(decode(&[0x0F, 0x01, 0xFD]), Some((ConsoleOnly::Rdpru, 3)));
        assert_eq!(decode(&[0x0F, 0x01, 0xFA]), Some((ConsoleOnly::Monitorx, 3)));
        assert_eq!(decode(&[0x0F, 0x01, 0xFB]), Some((ConsoleOnly::Mwaitx, 3)));
        assert_eq!(decode(&[0x0F, 0x0B]), Some((ConsoleOnly::Ud2, 2)));

        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x30]), Some((ConsoleOnly::Clwb, 4)));
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x71, 0x40]), Some((ConsoleOnly::Clwb, 5)));
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x74, 0x24, 0x08]), Some((ConsoleOnly::Clwb, 6)));
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x35, 1, 2, 3, 4]), Some((ConsoleOnly::Clwb, 8)));
        assert_eq!(decode(&[0x66, 0x41, 0x0F, 0xAE, 0x30]), Some((ConsoleOnly::Clwb, 5)));

        assert_eq!(decode(&[0xF3, 0x0F, 0xC7, 0xF8]), Some((ConsoleOnly::Rdpid(0), 4)));
        assert_eq!(decode(&[0xF3, 0x41, 0x0F, 0xC7, 0xF9]), Some((ConsoleOnly::Rdpid(9), 5)));
    }

    #[test]
    fn decodes_sse4a_and_prefix_precedence() {
        assert_eq!(
            decode(&[0xF2, 0x0F, 0x78, 0xC1, 0x08, 0x10]),
            Some((ConsoleOnly::Insertq { dst: 0, src: 1, reg: false, len: 8, idx: 16 }, 6))
        );
        assert_eq!(
            decode(&[0xF2, 0x45, 0x0F, 0x79, 0xC1]),
            Some((ConsoleOnly::Insertq { dst: 8, src: 9, reg: true, len: 0, idx: 0 }, 5))
        );
        assert_eq!(
            decode(&[0x66, 0x0F, 0x78, 0xC2, 0x04, 0x08]),
            Some((ConsoleOnly::Extrq { dst: 2, src: None, len: 4, idx: 8 }, 6))
        );
        assert_eq!(
            decode(&[0x66, 0x0F, 0x79, 0xCA]),
            Some((ConsoleOnly::Extrq { dst: 1, src: Some(2), len: 0, idx: 0 }, 4))
        );

        // Last F2/F3 prefix wins regardless of 66 prefix ordering.
        let expected = Some((ConsoleOnly::Insertq { dst: 0, src: 1, reg: false, len: 16, idx: 16 }, 7));
        assert_eq!(decode(&[0x66, 0xF2, 0x0F, 0x78, 0xC1, 0x10, 0x10]), expected);
        assert_eq!(decode(&[0xF2, 0x66, 0x0F, 0x78, 0xC1, 0x10, 0x10]), expected);
        assert_eq!(decode(&[0xF3, 0xF2, 0x0F, 0x78, 0xC1, 0x10, 0x10]), expected);
        assert_eq!(decode(&[0xF2, 0xF3, 0x0F, 0x78, 0xC1, 0x10, 0x10]), None);
    }

    #[test]
    fn a_rep_prefix_makes_the_0f01_group_other_instructions() {
        // MCOMMIT, RMPQUERY and the like: not ours, the fault stays a fault.
        for op3 in [0xFA, 0xFB, 0xFC, 0xFD] {
            for prefix in [0xF3, 0xF2] {
                assert_eq!(decode(&[prefix, 0x0F, 0x01, op3]), None, "{prefix:#x} 0F 01 {op3:#x}");
                assert_eq!(decode(&[0x66, prefix, 0x0F, 0x01, op3]), None);
                assert_eq!(decode(&[prefix, 0x41, 0x0F, 0x01, op3]), None);
            }
        }
        // (other prefixes leave them what they are)
        assert_eq!(decode(&[0x67, 0x0F, 0x01, 0xFA]), Some((ConsoleOnly::Monitorx, 4)));
        assert_eq!(decode(&[0x2E, 0x0F, 0x01, 0xFB]), Some((ConsoleOnly::Mwaitx, 4)));
        assert_eq!(decode(&[0x0F, 0x01, 0xF9]), None);
        assert_eq!(decode(&[0x0F, 0x01, 0xFE]), None);
    }

    #[test]
    fn decodes_movntss_and_movntsd_to_memory_only() {
        let mem = |base, index, scale, disp| Mem { base, index, scale, disp, rip: false, a32: false };
        let movnt = |src, wide, mem| ConsoleOnly::Movnt { src, wide, mem };
        // movntss [rax], xmm1; movntsd [rcx + 8], xmm2; movntsd [rdx + 0x100], xmm0
        assert_eq!(decode(&[0xF3, 0x0F, 0x2B, 0x08]), Some((movnt(1, false, mem(Some(0), None, 0, 0)), 4)));
        assert_eq!(decode(&[0xF2, 0x0F, 0x2B, 0x51, 0x08]), Some((movnt(2, true, mem(Some(1), None, 0, 8)), 5)));
        assert_eq!(
            decode(&[0xF2, 0x0F, 0x2B, 0x82, 0x00, 0x01, 0x00, 0x00]),
            Some((movnt(0, true, mem(Some(2), None, 0, 0x100)), 8))
        );
        // movntss [r9 + r10*4 - 4], xmm11 (REX.RXB)
        assert_eq!(
            decode(&[0xF3, 0x47, 0x0F, 0x2B, 0x5C, 0x91, 0xFC]),
            Some((movnt(11, false, mem(Some(9), Some(10), 2, -4)), 7))
        );
        // [rsp]: a SIB byte without an index; [r12*2 + 0x10] without a base; [rbp + 0]
        assert_eq!(decode(&[0xF3, 0x0F, 0x2B, 0x04, 0x24]), Some((movnt(0, false, mem(Some(4), None, 0, 0)), 5)));
        assert_eq!(
            decode(&[0xF3, 0x42, 0x0F, 0x2B, 0x04, 0x65, 0x10, 0, 0, 0]),
            Some((movnt(0, false, mem(None, Some(12), 1, 0x10)), 10))
        );
        assert_eq!(decode(&[0xF3, 0x0F, 0x2B, 0x45, 0x00]), Some((movnt(0, false, mem(Some(5), None, 0, 0)), 5)));
        // rip-relative, and with an address-size prefix
        let rip = Mem { base: None, index: None, scale: 0, disp: -8, rip: true, a32: false };
        assert_eq!(decode(&[0xF2, 0x0F, 0x2B, 0x05, 0xF8, 0xFF, 0xFF, 0xFF]), Some((movnt(0, true, rip), 8)));
        let short = Mem { a32: true, ..mem(Some(3), None, 0, 0) };
        assert_eq!(decode(&[0x67, 0xF3, 0x0F, 0x2B, 0x03]), Some((movnt(0, false, short), 5)));
        // the last of F2 and F3 says which
        assert!(matches!(decode(&[0xF3, 0xF2, 0x0F, 0x2B, 0x08]), Some((ConsoleOnly::Movnt { wide: true, .. }, 5))));

        // Not ours: a register operand (undefined), no F2/F3 (MOVNTPS, MOVNTPD, which every
        // CPU has), and an FS or GS base.
        assert_eq!(decode(&[0xF3, 0x0F, 0x2B, 0xC8]), None);
        assert_eq!(decode(&[0xF2, 0x0F, 0x2B, 0xC8]), None);
        assert_eq!(decode(&[0x0F, 0x2B, 0x08]), None);
        assert_eq!(decode(&[0x66, 0x0F, 0x2B, 0x08]), None);
        assert_eq!(decode(&[0x65, 0xF3, 0x0F, 0x2B, 0x08]), None);
        assert_eq!(decode(&[0xF3, 0x64, 0x0F, 0x2B, 0x08]), None);
    }

    #[test]
    fn a_memory_operand_names_its_address() {
        let mut regs = [0u64; 16];
        regs[9] = 0x1_0000_1000;
        regs[10] = 0x20;
        let Some((ConsoleOnly::Movnt { mem, .. }, 7)) = decode(&[0xF3, 0x47, 0x0F, 0x2B, 0x5C, 0x91, 0xFC]) else {
            panic!("movntss [r9 + r10*4 - 4], xmm11")
        };
        assert_eq!(mem.address(&regs, 0), 0x1_0000_1000 + 0x80 - 4);
        assert_eq!(Mem { a32: true, ..mem }.address(&regs, 0), 0x1000 + 0x80 - 4);
        let rip = Mem { base: None, index: None, scale: 0, disp: -8, rip: true, a32: false };
        assert_eq!(rip.address(&regs, 0x7000), 0x6FF8);
    }

    /// Every instruction cut short anywhere is nothing, and so is one longer than 15 bytes.
    #[test]
    fn a_truncated_instruction_decodes_as_nothing() {
        let whole: [&[u8]; 16] = [
            &[0x0F, 0x0B],
            &[0x0F, 0x01, 0xFA],
            &[0x0F, 0x01, 0xFB],
            &[0x0F, 0x01, 0xFC],
            &[0x0F, 0x01, 0xFD],
            &[0x66, 0x0F, 0xAE, 0x74, 0x24, 0x08],
            &[0x66, 0x0F, 0xAE, 0x35, 1, 2, 3, 4],
            &[0xF3, 0x41, 0x0F, 0xC7, 0xF9],
            &[0xF2, 0x0F, 0x78, 0xC1, 0x08, 0x10],
            &[0xF2, 0x45, 0x0F, 0x79, 0xC1],
            &[0x66, 0x0F, 0x78, 0xC2, 0x04, 0x08],
            &[0x66, 0x0F, 0x79, 0xCA],
            &[0xF3, 0x0F, 0x2B, 0x08],
            &[0xF3, 0x47, 0x0F, 0x2B, 0x5C, 0x91, 0xFC],
            &[0xF2, 0x0F, 0x2B, 0x84, 0x88, 1, 2, 3, 4],
            &[0xF2, 0x0F, 0x2B, 0x05, 0xF8, 0xFF, 0xFF, 0xFF],
        ];
        for code in whole {
            assert_eq!(decode(code).map(|d| d.1), Some(code.len()), "{code:x?}");
            for cut in 0..code.len() {
                assert_eq!(decode(&code[..cut]), None, "{code:x?} cut to {cut}");
            }
            // (what follows an instruction is not part of it)
            let mut more = code.to_vec();
            more.extend([0xCC; 4]);
            assert_eq!(decode(&more).map(|d| d.1), Some(code.len()), "{code:x?}");
        }
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[0x66]), None);
        assert_eq!(decode(&[0xF3, 0x48]), None);
        // 15 bytes at most: seven prefixes and an 8-byte store are 15, eight are too many
        let store = [0xF2, 0x0F, 0x2B, 0x84, 0x88, 1, 2, 3, 4];
        let prefixed = |n: usize| [vec![0x2E; n], store.to_vec()].concat();
        assert_eq!(decode(&prefixed(6)).map(|d| d.1), Some(15));
        assert_eq!(decode(&prefixed(7)), None);
        // (and a run of prefixes alone ends nowhere)
        assert_eq!(decode(&[0x66; 16]), None);
    }

    #[test]
    fn other_instructions_decode_as_nothing() {
        // nop, ret, a plain store, syscall, cpuid, rdtscp, xgetbv, lfence, clflushopt, rdrand
        let others: [&[u8]; 12] = [
            &[0x90],
            &[0xC3],
            &[0x48, 0x89, 0x08],
            &[0x0F, 0x05],
            &[0x0F, 0xA2],
            &[0x0F, 0x01, 0xF9],
            &[0x0F, 0x01, 0xD0],
            &[0x0F, 0xAE, 0xE8],
            &[0x66, 0x0F, 0xAE, 0x38],
            &[0x0F, 0xC7, 0xF0],
            // rdpid needs F3 and a register; clwb a memory operand; extrq /0
            &[0xF3, 0x0F, 0xC7, 0x38],
            &[0x66, 0x0F, 0x78, 0xCA, 0x04, 0x08],
        ];
        for code in others {
            assert_eq!(decode(code), None, "{code:x?}");
        }
        assert_eq!(decode(&[0x0F, 0xC7, 0xF8]), None, "rdpid without F3");
        assert_eq!(decode(&[0x66, 0xF3, 0x0F, 0xC7, 0xF8]), None, "rdpid with 66");
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0xF0]), None, "clwb with a register");
        assert_eq!(decode(&[0x0F, 0x78, 0xC1, 0x08, 0x10]), None, "0F 78 without 66 or F2");
        assert_eq!(decode(&[0x66, 0x0F, 0x79, 0x08]), None, "extrq with a memory operand");
    }

    #[test]
    fn extrq_and_insertq_bitfield_helpers() {
        assert_eq!(extrq(0x1122_3344_5566_7788, 8, 16), 0x66);
        assert_eq!(extrq(0x1122_3344_5566_7788, 0, 0), 0x1122_3344_5566_7788);
        assert_eq!(insertq(0x1111_2222_3333_4444, 0xABCD, 16, 16), 0x1111_2222_ABCD_4444);
        assert_eq!(insertq(0, 0xDEAD_BEEF_CAFE_F00D, 0, 0), 0xDEAD_BEEF_CAFE_F00D);
    }
}
