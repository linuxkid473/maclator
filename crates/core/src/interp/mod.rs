//! Reference AArch64 interpreter.
//!
//! This is the semantic ground truth for Maclator: the JIT falls back to it
//! for anything it does not translate natively, and the differential tester
//! checks it against real hardware.

mod dp;
mod fp16;
mod ldst;
pub mod simd;
mod sys;

use crate::cpu::Cpu;
use crate::mem;

/// Reasons execution must leave the interpreter/JIT and go to the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// SVC #imm. `cpu.pc` already points past the instruction.
    Svc(u16),
    /// BRK #imm (pc points at the BRK).
    Brk(u16),
    /// HLT / UDF / permanently undefined.
    Udf(u32),
    /// Instruction we do not implement yet (pc points at it).
    Unimplemented(u32),
    /// Instruction cache invalidation hit (IC IVAU) for the given address;
    /// execution continues normally after the runtime drops translations.
    IcInvalidate(u64),
}

pub type R = Result<(), Exit>;

/// Fetch and execute the instruction at `cpu.pc`.
#[inline]
pub fn step(cpu: &mut Cpu) -> R {
    let insn = unsafe { mem::r32(cpu.pc) };
    exec(cpu, insn)
}

/// Execute `insn` located at `cpu.pc`. On normal completion pc is advanced
/// (or set to the branch target).
pub fn exec(cpu: &mut Cpu, insn: u32) -> R {
    let op0 = (insn >> 25) & 0xf;
    match op0 {
        0b1000 | 0b1001 => dp::dp_imm(cpu, insn)?,
        0b1010 | 0b1011 => {
            // branch / exception / system: these manage pc themselves
            return sys::branch_sys(cpu, insn);
        }
        0b0100 | 0b0110 | 0b1100 | 0b1110 => ldst::ldst(cpu, insn)?,
        0b0101 | 0b1101 => dp::dp_reg(cpu, insn)?,
        0b0111 | 0b1111 => simd::simd_fp(cpu, insn)?,
        _ => return Err(unimpl(insn)),
    }
    cpu.pc = cpu.pc.wrapping_add(4);
    Ok(())
}

#[inline(never)]
#[cold]
pub(crate) fn unimpl(insn: u32) -> Exit {
    if insn == 0 || (insn >> 16) == 0 {
        Exit::Udf(insn)
    } else {
        Exit::Unimplemented(insn)
    }
}

// ---------- shared bit helpers ----------

#[inline(always)]
pub(crate) fn bits(insn: u32, hi: u32, lo: u32) -> u32 {
    (insn >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}
#[inline(always)]
pub(crate) fn bit(insn: u32, b: u32) -> u32 {
    (insn >> b) & 1
}
#[inline(always)]
pub(crate) fn sext(v: u64, nbits: u32) -> u64 {
    let s = 64 - nbits;
    (((v << s) as i64) >> s) as u64
}
#[inline(always)]
pub(crate) fn ones(n: u32) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// AddWithCarry; returns (result, nzcv) for the given datasize (32/64).
#[inline(always)]
pub(crate) fn add_with_carry(x: u64, y: u64, carry: u64, sf: bool) -> (u64, u32) {
    if sf {
        let (r1, c1) = x.overflowing_add(y);
        let (r, c2) = r1.overflowing_add(carry);
        let c = c1 | c2;
        let v = ((x ^ r) & (y ^ r)) >> 63 != 0;
        let n = r >> 63;
        let z = (r == 0) as u64;
        (r, ((n as u32) << 3) | ((z as u32) << 2) | ((c as u32) << 1) | v as u32)
    } else {
        let x = x as u32 as u64;
        let y = y as u32 as u64;
        let full = x + y + carry;
        let r = full as u32 as u64;
        let c = (full >> 32) & 1;
        let v = ((x ^ r) & (y ^ r)) >> 31 & 1;
        let n = r >> 31;
        let z = (r == 0) as u64;
        (r, ((n as u32) << 3) | ((z as u32) << 2) | ((c as u32) << 1) | v as u32)
    }
}

/// DecodeBitMasks from the ARM ARM. Returns (wmask, tmask).
pub fn decode_bit_masks(n: u32, imms: u32, immr: u32, immediate: bool, datasize: u32) -> Option<(u64, u64)> {
    let combined = (n << 6) | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 {
        return None;
    }
    let esize = 1u32 << len;
    if esize > datasize {
        return None;
    }
    let levels = (1u32 << len) - 1;
    if immediate && (imms & levels) == levels {
        return None;
    }
    let s = imms & levels;
    let r = immr & levels;
    let d = s.wrapping_sub(r) & levels;
    let welem = ones(s + 1);
    let telem = ones(d + 1);
    let emask = ones(esize);
    let wrot = if r == 0 { welem } else { ((welem >> r) | (welem << (esize - r))) & emask };
    let mut wmask = 0u64;
    let mut tmask = 0u64;
    let mut i = 0;
    while i < datasize {
        wmask |= wrot << i;
        tmask |= telem << i;
        i += esize;
    }
    let dmask = ones(datasize);
    Some((wmask & dmask, tmask & dmask))
}
