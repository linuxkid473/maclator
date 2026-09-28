//! Integer data processing (immediate and register forms).

use super::*;

#[inline(always)]
fn wmask(sf: bool, v: u64) -> u64 {
    if sf {
        v
    } else {
        v as u32 as u64
    }
}

pub(crate) fn dp_imm(cpu: &mut Cpu, insn: u32) -> R {
    let sf = bit(insn, 31) != 0;
    let rd = bits(insn, 4, 0);
    let rn = bits(insn, 9, 5);
    match bits(insn, 25, 23) {
        0b000 | 0b001 => {
            // ADR / ADRP
            let immlo = bits(insn, 30, 29) as u64;
            let immhi = bits(insn, 23, 5) as u64;
            let imm = sext((immhi << 2) | immlo, 21);
            let v = if bit(insn, 31) != 0 {
                (cpu.pc & !0xfff).wrapping_add(imm << 12)
            } else {
                cpu.pc.wrapping_add(imm)
            };
            cpu.setx(rd, v);
        }
        0b010 => {
            // add/sub immediate
            let op = bit(insn, 30);
            let s = bit(insn, 29) != 0;
            let sh = bit(insn, 22);
            let mut imm = bits(insn, 21, 10) as u64;
            if sh != 0 {
                imm <<= 12;
            }
            let a = cpu.xs(rn);
            let (r, f) = if op == 0 {
                add_with_carry(a, imm, 0, sf)
            } else {
                add_with_carry(a, !imm, 1, sf)
            };
            if s {
                cpu.set_nzcv(f);
                cpu.setx(rd, r);
            } else {
                cpu.setxs(rd, r);
            }
        }
        0b011 => return Err(unimpl(insn)), // add/sub with tags (MTE)
        0b100 => {
            // logical immediate
            let opc = bits(insn, 30, 29);
            let n = bit(insn, 22);
            if !sf && n != 0 {
                return Err(unimpl(insn));
            }
            let (imm, _) = decode_bit_masks(n, bits(insn, 15, 10), bits(insn, 21, 16), true, if sf { 64 } else { 32 })
                .ok_or_else(|| unimpl(insn))?;
            let a = cpu.xr(rn);
            let r = wmask(sf, match opc {
                0 | 3 => a & imm,
                1 => a | imm,
                _ => a ^ imm,
            });
            if opc == 3 {
                set_logic_flags(cpu, r, sf);
                cpu.setx(rd, r);
            } else {
                cpu.setxs(rd, r);
            }
        }
        0b101 => {
            // move wide
            let opc = bits(insn, 30, 29);
            let hw = bits(insn, 22, 21);
            if !sf && hw > 1 {
                return Err(unimpl(insn));
            }
            let imm = (bits(insn, 20, 5) as u64) << (hw * 16);
            let r = match opc {
                0 => wmask(sf, !imm),
                2 => imm,
                3 => {
                    let old = cpu.xr(rd);
                    wmask(sf, (old & !(0xffffu64 << (hw * 16))) | imm)
                }
                _ => return Err(unimpl(insn)),
            };
            cpu.setx(rd, r);
        }
        0b110 => {
            // bitfield
            let opc = bits(insn, 30, 29);
            let n = bit(insn, 22);
            let immr = bits(insn, 21, 16);
            let imms = bits(insn, 15, 10);
            let datasize = if sf { 64 } else { 32 };
            if (sf as u32) != n {
                return Err(unimpl(insn));
            }
            let (wm, tm) = decode_bit_masks(n, imms, immr, false, datasize).ok_or_else(|| unimpl(insn))?;
            let src = wmask(sf, cpu.xr(rn));
            let dst = if opc == 1 { cpu.xr(rd) } else { 0 };
            let rot = if immr == 0 {
                src
            } else if sf {
                src.rotate_right(immr)
            } else {
                (src as u32).rotate_right(immr) as u64
            };
            let bot = (dst & !wm) | (rot & wm);
            let top = if opc == 0 {
                // replicate sign bit S
                if (src >> imms) & 1 != 0 {
                    ones(datasize)
                } else {
                    0
                }
            } else {
                dst
            };
            let r = match opc {
                0 | 1 | 2 => wmask(sf, (top & !tm) | (bot & tm)),
                _ => return Err(unimpl(insn)),
            };
            cpu.setx(rd, r);
        }
        _ => {
            // extract (EXTR)
            let rm = bits(insn, 20, 16);
            let imms = bits(insn, 15, 10);
            let r = if sf {
                let hi = cpu.xr(rn);
                let lo = cpu.xr(rm);
                if imms == 0 {
                    lo
                } else {
                    (lo >> imms) | (hi << (64 - imms))
                }
            } else {
                let v = ((cpu.xr(rn) as u32 as u64) << 32) | (cpu.xr(rm) as u32 as u64);
                (v >> imms) as u32 as u64
            };
            cpu.setx(rd, r);
        }
    }
    Ok(())
}

#[inline(always)]
fn set_logic_flags(cpu: &mut Cpu, r: u64, sf: bool) {
    cpu.nf = if sf { (r >> 63) as u8 } else { ((r >> 31) & 1) as u8 };
    cpu.zf = (r == 0) as u8;
    cpu.cf = 0;
    cpu.vf = 0;
}

#[inline(always)]
pub(crate) fn shift_reg(v: u64, ty: u32, amt: u32, sf: bool) -> u64 {
    if sf {
        match ty {
            0 => v << amt,
            1 => v >> amt,
            2 => ((v as i64) >> amt) as u64,
            _ => v.rotate_right(amt),
        }
    } else {
        let v = v as u32;
        (match ty {
            0 => v << amt,
            1 => v >> amt,
            2 => ((v as i32) >> amt) as u32,
            _ => v.rotate_right(amt),
        }) as u64
    }
}

#[inline(always)]
pub(crate) fn extend_reg(v: u64, option: u32, shift: u32) -> u64 {
    let e = match option {
        0 => v as u8 as u64,
        1 => v as u16 as u64,
        2 => v as u32 as u64,
        3 => v,
        4 => v as u8 as i8 as i64 as u64,
        5 => v as u16 as i16 as i64 as u64,
        6 => v as u32 as i32 as i64 as u64,
        _ => v,
    };
    e << shift
}

pub(crate) fn dp_reg(cpu: &mut Cpu, insn: u32) -> R {
    let sf = bit(insn, 31) != 0;
    let rd = bits(insn, 4, 0);
    let rn = bits(insn, 9, 5);
    let rm = bits(insn, 20, 16);
    let op1 = bit(insn, 28);
    let op2 = bits(insn, 24, 21);
    if op1 == 0 {
        if bit(insn, 24) == 0 {
            // logical (shifted register)
            let opc = bits(insn, 30, 29);
            let ty = bits(insn, 23, 22);
            let n = bit(insn, 21);
            let amt = bits(insn, 15, 10);
            if !sf && amt >= 32 {
                return Err(unimpl(insn));
            }
            let mut b = shift_reg(cpu.xr(rm), ty, amt, sf);
            if n != 0 {
                b = !b;
            }
            let a = cpu.xr(rn);
            let r = wmask(sf, match opc {
                0 | 3 => a & b,
                1 => a | b,
                _ => a ^ b,
            });
            if opc == 3 {
                set_logic_flags(cpu, r, sf);
            }
            cpu.setx(rd, r);
        } else {
            let op = bit(insn, 30);
            let s = bit(insn, 29) != 0;
            if bit(insn, 21) == 0 {
                // add/sub shifted register
                let ty = bits(insn, 23, 22);
                let amt = bits(insn, 15, 10);
                if ty == 3 || (!sf && amt >= 32) {
                    return Err(unimpl(insn));
                }
                let b = shift_reg(cpu.xr(rm), ty, amt, sf);
                let a = cpu.xr(rn);
                let (r, f) = if op == 0 { add_with_carry(a, b, 0, sf) } else { add_with_carry(a, !b, 1, sf) };
                if s {
                    cpu.set_nzcv(f);
                }
                cpu.setx(rd, r);
            } else {
                // add/sub extended register
                let option = bits(insn, 15, 13);
                let imm3 = bits(insn, 12, 10);
                if imm3 > 4 || bits(insn, 23, 22) != 0 {
                    return Err(unimpl(insn));
                }
                let b = extend_reg(cpu.xr(rm), option, imm3);
                let a = cpu.xs(rn);
                let (r, f) = if op == 0 { add_with_carry(a, b, 0, sf) } else { add_with_carry(a, !b, 1, sf) };
                if s {
                    cpu.set_nzcv(f);
                    cpu.setx(rd, r);
                } else {
                    cpu.setxs(rd, r);
                }
            }
        }
        return Ok(());
    }
    match op2 {
        0b0000 => {
            let op3 = bits(insn, 15, 10);
            if op3 == 0 {
                // ADC/ADCS/SBC/SBCS
                let op = bit(insn, 30);
                let s = bit(insn, 29) != 0;
                let a = cpu.xr(rn);
                let b = cpu.xr(rm);
                let c = cpu.cf as u64;
                let (r, f) = if op == 0 { add_with_carry(a, b, c, sf) } else { add_with_carry(a, !b, c, sf) };
                if s {
                    cpu.set_nzcv(f);
                }
                cpu.setx(rd, r);
            } else if bits(insn, 14, 10) == 0b00001 && bits(insn, 31, 29) == 0b101 && bit(insn, 4) == 0 {
                // RMIF
                let imm6 = bits(insn, 20, 15);
                let mask = bits(insn, 3, 0);
                let tmp = cpu.xr(rn).rotate_right(imm6);
                let f = cpu.nzcv();
                let nf = (f & !mask) | ((tmp as u32 & 0xf) & mask);
                cpu.set_nzcv(nf);
            } else if bits(insn, 13, 10) == 0b0010 && bits(insn, 31, 29) == 0b001 {
                // SETF8 / SETF16
                let sz16 = bit(insn, 14) != 0;
                let v = cpu.xr(rn) as u32;
                let msb = if sz16 { 15 } else { 7 };
                cpu.nf = ((v >> msb) & 1) as u8;
                cpu.zf = if sz16 { (v as u16 == 0) as u8 } else { (v as u8 == 0) as u8 };
                cpu.vf = (((v >> (msb + 1)) ^ (v >> msb)) & 1) as u8;
            } else {
                return Err(unimpl(insn));
            }
        }
        0b0010 => {
            // conditional compare (register / immediate)
            if bit(insn, 10) != 0 || bit(insn, 4) != 0 || bit(insn, 29) == 0 {
                return Err(unimpl(insn));
            }
            let op = bit(insn, 30);
            let cond = bits(insn, 15, 12);
            let nzcv = bits(insn, 3, 0);
            if cpu.cond(cond) {
                let a = cpu.xr(rn);
                let b = if bit(insn, 11) != 0 { rm as u64 } else { cpu.xr(rm) };
                let (_, f) = if op == 0 { add_with_carry(a, b, 0, sf) } else { add_with_carry(a, !b, 1, sf) };
                cpu.set_nzcv(f);
            } else {
                cpu.set_nzcv(nzcv);
            }
        }
        0b0100 => {
            // conditional select
            if bit(insn, 29) != 0 || bit(insn, 11) != 0 {
                return Err(unimpl(insn));
            }
            let op = bit(insn, 30);
            let o2 = bit(insn, 10);
            let cond = bits(insn, 15, 12);
            let r = if cpu.cond(cond) {
                cpu.xr(rn)
            } else {
                let b = cpu.xr(rm);
                match (op, o2) {
                    (0, 0) => b,
                    (0, _) => b.wrapping_add(1),
                    (_, 0) => !b,
                    _ => b.wrapping_neg(),
                }
            };
            cpu.setx(rd, wmask(sf, r));
        }
        0b0110 => {
            if bit(insn, 30) == 0 {
                dp2(cpu, insn, sf, rd, rn, rm)?;
            } else {
                dp1(cpu, insn, sf, rd, rn)?;
            }
        }
        _ if op2 & 0b1000 != 0 => {
            // data processing 3-source
            let op54 = bits(insn, 30, 29);
            let op31 = bits(insn, 23, 21);
            let o0 = bit(insn, 15);
            let ra = bits(insn, 14, 10);
            if op54 != 0 {
                return Err(unimpl(insn));
            }
            let a = cpu.xr(ra);
            let n = cpu.xr(rn);
            let m = cpu.xr(rm);
            let r = match (op31, o0) {
                (0b000, 0) => wmask(sf, a.wrapping_add(n.wrapping_mul(m))),
                (0b000, 1) => wmask(sf, a.wrapping_sub(n.wrapping_mul(m))),
                (0b001, 0) if sf => a.wrapping_add((n as i32 as i64 * m as i32 as i64) as u64),
                (0b001, 1) if sf => a.wrapping_sub((n as i32 as i64 * m as i32 as i64) as u64),
                (0b010, 0) if sf => (((n as i64 as i128) * (m as i64 as i128)) >> 64) as u64,
                (0b101, 0) if sf => a.wrapping_add((n as u32 as u64) * (m as u32 as u64)),
                (0b101, 1) if sf => a.wrapping_sub((n as u32 as u64) * (m as u32 as u64)),
                (0b110, 0) if sf => (((n as u128) * (m as u128)) >> 64) as u64,
                _ => return Err(unimpl(insn)),
            };
            cpu.setx(rd, r);
        }
        _ => return Err(unimpl(insn)),
    }
    Ok(())
}

fn dp2(cpu: &mut Cpu, insn: u32, sf: bool, rd: u32, rn: u32, rm: u32) -> R {
    if bit(insn, 29) != 0 {
        return Err(unimpl(insn));
    }
    let opcode = bits(insn, 15, 10);
    let a = cpu.xr(rn);
    let b = cpu.xr(rm);
    let r = match opcode {
        0b000010 => {
            // UDIV
            if sf {
                if b == 0 { 0 } else { a / b }
            } else {
                let (a, b) = (a as u32, b as u32);
                if b == 0 { 0 } else { (a / b) as u64 }
            }
        }
        0b000011 => {
            // SDIV
            if sf {
                let (a, b) = (a as i64, b as i64);
                if b == 0 { 0 } else { a.wrapping_div(b) as u64 }
            } else {
                let (a, b) = (a as i32, b as i32);
                if b == 0 { 0 } else { a.wrapping_div(b) as u32 as u64 }
            }
        }
        0b001000..=0b001011 => {
            let amt = if sf { (b & 63) as u32 } else { (b & 31) as u32 };
            shift_reg(a, opcode & 3, amt, sf)
        }
        0b001100 if sf => {
            // PACGA: produce a stable "signature" in the top 32 bits.
            let h = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.rotate_left(29);
            (h ^ (h >> 31)) & 0xffff_ffff_0000_0000
        }
        0b010000..=0b010111 => {
            // CRC32{B,H,W,X} / CRC32C*
            let sz = opcode & 3;
            let castagnoli = opcode & 4 != 0;
            if (sz == 3) != sf {
                return Err(unimpl(insn));
            }
            crc32(a as u32, b, 8 << sz, castagnoli) as u64
        }
        // FEAT_CSSC min/max (register)
        0b011000 => if sf { (a as i64).max(b as i64) as u64 } else { (a as i32).max(b as i32) as u32 as u64 },
        0b011001 => if sf { a.max(b) } else { (a as u32).max(b as u32) as u64 },
        0b011010 => if sf { (a as i64).min(b as i64) as u64 } else { (a as i32).min(b as i32) as u32 as u64 },
        0b011011 => if sf { a.min(b) } else { (a as u32).min(b as u32) as u64 },
        _ => return Err(unimpl(insn)),
    };
    cpu.setx(rd, r);
    Ok(())
}

fn crc32(acc: u32, val: u64, nbits: u32, castagnoli: bool) -> u32 {
    let poly: u32 = if castagnoli { 0x82F6_3B78 } else { 0xEDB8_8320 };
    let mut crc = acc;
    for i in 0..(nbits / 8) {
        crc ^= ((val >> (i * 8)) & 0xff) as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ poly } else { crc >> 1 };
        }
    }
    crc
}

/// XPACI: strip a PAC from an instruction pointer (no top-byte-ignore for
/// instruction addresses: the PAC field is bits 63:56 and 54:47).
#[inline(always)]
pub(crate) fn xpac(v: u64) -> u64 {
    if (v >> 55) & 1 == 0 {
        v & 0x0000_7FFF_FFFF_FFFF
    } else {
        v | 0xFFFF_8000_0000_0000
    }
}

/// XPACD: data pointers use top-byte-ignore on Darwin/arm64e, so the PAC
/// field is bits 54:47 only and the top byte (often used for flags) is kept.
#[inline(always)]
pub(crate) fn xpacd(v: u64) -> u64 {
    const FIELD: u64 = 0x007F_8000_0000_0000;
    if (v >> 55) & 1 == 0 {
        v & !FIELD
    } else {
        v | FIELD
    }
}

fn dp1(cpu: &mut Cpu, insn: u32, sf: bool, rd: u32, rn: u32) -> R {
    let opcode2 = bits(insn, 20, 16);
    let opcode = bits(insn, 15, 10);
    if bit(insn, 29) != 0 {
        return Err(unimpl(insn));
    }
    if opcode2 == 1 {
        // Pointer authentication: sign/auth are identity, xpac strips.
        if !sf {
            return Err(unimpl(insn));
        }
        match opcode {
            0..=15 => {} // PACxx / AUTxx (and zero-modifier forms): identity
            16 => {
                let v = cpu.xr(rd);
                cpu.setx(rd, xpac(v));
            }
            17 => {
                let v = cpu.xr(rd);
                cpu.setx(rd, xpacd(v));
            }
            _ => return Err(unimpl(insn)),
        }
        return Ok(());
    }
    if opcode2 != 0 {
        return Err(unimpl(insn));
    }
    let a = cpu.xr(rn);
    let r = match opcode {
        0b000000 => if sf { a.reverse_bits() } else { (a as u32).reverse_bits() as u64 },
        0b000001 => {
            // REV16
            let v = if sf { a } else { a as u32 as u64 };
            ((v & 0x00ff_00ff_00ff_00ff) << 8) | ((v >> 8) & 0x00ff_00ff_00ff_00ff)
        }
        0b000010 => {
            if sf {
                // REV32
                let lo = (a as u32).swap_bytes() as u64;
                let hi = ((a >> 32) as u32).swap_bytes() as u64;
                lo | (hi << 32)
            } else {
                (a as u32).swap_bytes() as u64
            }
        }
        0b000011 if sf => a.swap_bytes(),
        0b000100 => if sf { a.leading_zeros() as u64 } else { (a as u32).leading_zeros() as u64 },
        0b000101 => {
            // CLS
            if sf {
                let x = a ^ ((a as i64 >> 1) as u64);
                (x.leading_zeros() as u64).saturating_sub(1).min(63)
            } else {
                let a = a as u32;
                let x = a ^ ((a as i32 >> 1) as u32);
                (x.leading_zeros() as u64).saturating_sub(1).min(31)
            }
        }
        // FEAT_CSSC
        0b000110 => if sf { a.trailing_zeros() as u64 } else { (a as u32).trailing_zeros() as u64 },
        0b000111 => if sf { a.count_ones() as u64 } else { (a as u32).count_ones() as u64 },
        0b001000 => if sf { (a as i64).wrapping_abs() as u64 } else { (a as i32).wrapping_abs() as u32 as u64 },
        _ => return Err(unimpl(insn)),
    };
    cpu.setx(rd, r);
    Ok(())
}
