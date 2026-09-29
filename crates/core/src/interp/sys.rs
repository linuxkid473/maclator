//! Branches, exception generation and system instructions.

use super::*;
use std::sync::atomic::{fence, Ordering};

/// CNTFRQ_EL0 we report (Apple Silicon uses 24 MHz).
pub const CNTFRQ: u64 = 24_000_000;

/// Host-provided clock source, in nanoseconds (set by the runtime).
pub static mut HOST_NANOS: fn() -> u64 = default_nanos;

fn default_nanos() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

#[inline]
pub fn counter_ticks() -> u64 {
    let ns = unsafe { HOST_NANOS() };
    // ticks = ns * 24 / 1000 (= ns * 3 / 125) without overflow for ~centuries
    ((ns as u128 * 3) / 125) as u64
}

pub(crate) fn branch_sys(cpu: &mut Cpu, insn: u32) -> R {
    let pc = cpu.pc;
    let next = pc.wrapping_add(4);
    // B / BL
    if insn & 0x7C00_0000 == 0x1400_0000 {
        let off = sext((insn & 0x03ff_ffff) as u64, 26) << 2;
        if insn & 0x8000_0000 != 0 {
            cpu.x[30] = next;
        }
        cpu.pc = pc.wrapping_add(off);
        return Ok(());
    }
    // B.cond / BC.cond
    if insn & 0xFF00_0000 == 0x5400_0000 {
        let off = sext(bits(insn, 23, 5) as u64, 19) << 2;
        cpu.pc = if cpu.cond(bits(insn, 3, 0)) { pc.wrapping_add(off) } else { next };
        return Ok(());
    }
    // CBZ / CBNZ
    if insn & 0x7E00_0000 == 0x3400_0000 {
        let sf = bit(insn, 31) != 0;
        let mut v = cpu.xr(bits(insn, 4, 0));
        if !sf {
            v &= 0xffff_ffff;
        }
        let nz = bit(insn, 24) != 0;
        let off = sext(bits(insn, 23, 5) as u64, 19) << 2;
        cpu.pc = if (v != 0) == nz { pc.wrapping_add(off) } else { next };
        return Ok(());
    }
    // TBZ / TBNZ
    if insn & 0x7E00_0000 == 0x3600_0000 {
        let b = (bit(insn, 31) << 5) | bits(insn, 23, 19);
        let v = (cpu.xr(bits(insn, 4, 0)) >> b) & 1;
        let nz = bit(insn, 24) != 0;
        let off = sext(bits(insn, 18, 5) as u64, 14) << 2;
        cpu.pc = if (v != 0) == nz { pc.wrapping_add(off) } else { next };
        return Ok(());
    }
    // Unconditional branch (register)
    if insn & 0xFE00_0000 == 0xD600_0000 {
        let opc = bits(insn, 24, 21);
        let op2 = bits(insn, 20, 16);
        let op3 = bits(insn, 15, 10);
        let rn = bits(insn, 9, 5);
        if op2 != 0x1f {
            return Err(unimpl(insn));
        }
        let target = match opc {
            0b0000 | 0b0001 => {
                // BR / BLR, BRAAZ/BRABZ, BLRAAZ/BLRABZ
                if op3 != 0 && op3 != 2 && op3 != 3 {
                    return Err(unimpl(insn));
                }
                cpu.xr(rn)
            }
            0b0010 => {
                // RET, RETAA/RETAB
                if op3 == 0 {
                    cpu.xr(rn)
                } else {
                    cpu.x[30]
                }
            }
            0b1000 | 0b1001 => cpu.xr(rn), // BRAA/BRAB, BLRAA/BLRAB
            _ => return Err(unimpl(insn)),
        };
        if opc & 1 != 0 {
            cpu.x[30] = next;
        }
        cpu.pc = target;
        return Ok(());
    }
    // Exception generation
    if insn & 0xFF00_0000 == 0xD400_0000 {
        let opc = bits(insn, 23, 21);
        let ll = bits(insn, 1, 0);
        let imm = bits(insn, 20, 5) as u16;
        return match (opc, ll) {
            (0, 1) => {
                cpu.pc = next;
                Err(Exit::Svc(imm))
            }
            (1, 0) => Err(Exit::Brk(imm)),
            _ => Err(Exit::Udf(insn)),
        };
    }
    // System
    if insn & 0xFFC0_0000 == 0xD500_0000 {
        let r = system(cpu, insn);
        if matches!(r, Ok(()) | Err(Exit::IcInvalidate(_))) {
            cpu.pc = next;
        }
        return r;
    }
    Err(unimpl(insn))
}

/// System register encodings as `op0(1):op1:CRn:CRm:op2` = insn bits 19:5.
const SR_NZCV: u32 = sr(3, 3, 4, 2, 0);
const SR_DAIF: u32 = sr(3, 3, 4, 2, 1);
const SR_FPCR: u32 = sr(3, 3, 4, 4, 0);
const SR_FPSR: u32 = sr(3, 3, 4, 4, 1);
const SR_DIT: u32 = sr(3, 3, 4, 2, 5);
const SR_SSBS: u32 = sr(3, 3, 4, 2, 6);
const SR_TCO: u32 = sr(3, 3, 4, 2, 7);
const SR_TPIDR: u32 = sr(3, 3, 13, 0, 2);
const SR_TPIDRRO: u32 = sr(3, 3, 13, 0, 3);
const SR_CNTFRQ: u32 = sr(3, 3, 14, 0, 0);
const SR_CNTPCT: u32 = sr(3, 3, 14, 0, 1);
const SR_CNTVCT: u32 = sr(3, 3, 14, 0, 2);
const SR_CNTPCTSS: u32 = sr(3, 3, 14, 0, 5);
const SR_CNTVCTSS: u32 = sr(3, 3, 14, 0, 6);
const SR_DCZID: u32 = sr(3, 3, 0, 0, 7);
const SR_CTR: u32 = sr(3, 3, 0, 0, 1);
const SR_MIDR: u32 = sr(3, 0, 0, 0, 0);

const fn sr(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    ((op0 & 1) << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

fn system(cpu: &mut Cpu, insn: u32) -> R {
    let l = bit(insn, 21);
    let op0 = bits(insn, 20, 19);
    let op1 = bits(insn, 18, 16);
    let crn = bits(insn, 15, 12);
    let crm = bits(insn, 11, 8);
    let op2 = bits(insn, 7, 5);
    let rt = bits(insn, 4, 0);
    if l == 0 && op0 == 0 {
        if crn == 2 && rt == 31 {
            // Hints: NOP, YIELD, WFE, WFI, SEV, SEVL, PAC hints, BTI, CSDB...
            if crm == 0 && op2 == 7 {
                // XPACLRI
                cpu.x[30] = dp::xpac(cpu.x[30]);
            }
            return Ok(());
        }
        if crn == 3 {
            match op2 {
                2 => {
                    // CLREX
                    cpu.excl_addr = u64::MAX;
                }
                4 | 5 => {
                    // DSB/DMB: loads-only (ld) or stores-only variants need no
                    // host fence under x86 TSO; full barriers do.
                    let ty = crm & 3;
                    if ty == 3 || ty == 0 {
                        fence(Ordering::SeqCst);
                    }
                }
                _ => {} // ISB, SB, TCOMMIT...
            }
            return Ok(());
        }
        if crn == 4 {
            // MSR (immediate) / flag manipulation
            if op1 == 0 && crm == 0 && rt == 31 {
                match op2 {
                    0 => cpu.cf ^= 1, // CFINV
                    1 => {
                        // XAFLAG
                        let (n, z, c, v) = (cpu.nf, cpu.zf, cpu.cf, cpu.vf);
                        cpu.nf = (c == 0 && z == 0) as u8;
                        cpu.zf = (z != 0 && c != 0) as u8;
                        cpu.cf = (c != 0 || z != 0) as u8;
                        cpu.vf = (c == 0 && z != 0) as u8;
                        let _ = (n, v);
                    }
                    2 => {
                        // AXFLAG
                        let (z, c, v) = (cpu.zf, cpu.cf, cpu.vf);
                        cpu.nf = 0;
                        cpu.zf = (z != 0 || v != 0) as u8;
                        cpu.cf = (c != 0 && v == 0) as u8;
                        cpu.vf = 0;
                    }
                    _ => {}
                }
            }
            // PSTATE fields (DIT, SSBS, PAN, TCO...) are ignored.
            return Ok(());
        }
        return Err(unimpl(insn));
    }
    if op0 == 1 {
        // SYS / SYSL: cache maintenance etc.
        if l == 0 && op1 == 3 && crn == 7 {
            let addr = cpu.xr(rt);
            match (crm, op2) {
                (4, 1) => {
                    // DC ZVA: zero 64-byte block
                    let base = addr & !63;
                    for i in 0..4 {
                        unsafe { mem::w128(base + i * 16, 0) };
                    }
                }
                (5, 1) => {
                    // IC IVAU: let the runtime invalidate translations
                    // (pc is advanced past this instruction).
                    return Err(Exit::IcInvalidate(addr));
                }
                _ => {} // DC CVAU, CIVAC, CVAC, CVAP...
            }
            return Ok(());
        }
        return Ok(());
    }
    // MRS / MSR (register)
    let key = bits(insn, 19, 5);
    if l == 1 {
        let v = match key {
            SR_NZCV => (cpu.nzcv() as u64) << 28,
            SR_FPCR => cpu.fpcr,
            SR_FPSR => cpu.fpsr,
            SR_TPIDR => cpu.tpidr_el0,
            SR_TPIDRRO => cpu.tpidrro_el0,
            SR_CNTFRQ => CNTFRQ,
            SR_CNTPCT | SR_CNTVCT | SR_CNTPCTSS | SR_CNTVCTSS => counter_ticks(),
            SR_DCZID => 4,
            SR_CTR => 0x8444_c004, // DminLine=4, IminLine=4, like Apple cores
            SR_DAIF | SR_DIT | SR_SSBS | SR_TCO => 0,
            SR_MIDR => 0x611f_0000,
            _ => return Err(unimpl(insn)),
        };
        cpu.setx(rt, v);
    } else {
        let v = cpu.xr(rt);
        match key {
            SR_NZCV => cpu.set_nzcv((v >> 28) as u32 & 0xf),
            SR_FPCR => cpu.fpcr = v & 0x07ff_ff00,
            SR_FPSR => cpu.fpsr = v & 0xf800_009f,
            SR_TPIDR => cpu.tpidr_el0 = v,
            SR_DAIF | SR_DIT | SR_SSBS | SR_TCO => {}
            _ => return Err(unimpl(insn)),
        }
    }
    Ok(())
}
