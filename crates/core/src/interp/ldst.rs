//! Loads and stores.

use super::dp::extend_reg;
use super::*;
use crate::mem::*;
use std::sync::atomic::{fence, Ordering};

pub(crate) fn ldst(cpu: &mut Cpu, insn: u32) -> R {
    let rt = bits(insn, 4, 0);
    let rn = bits(insn, 9, 5);
    let v = bit(insn, 26) != 0;

    // Advanced SIMD structure loads/stores
    if insn & 0xBF80_0000 == 0x0C00_0000 || insn & 0xBF80_0000 == 0x0C80_0000 {
        return simd::ld_st_multiple(cpu, insn);
    }
    if insn & 0xBF80_0000 == 0x0D00_0000 || insn & 0xBF80_0000 == 0x0D80_0000 {
        return simd::ld_st_single(cpu, insn);
    }

    match bits(insn, 29, 24) {
        0b001000 => return exclusive(cpu, insn),
        0b011001 if !v && bit(insn, 21) == 0 && bits(insn, 11, 10) == 0 => {
            // LDAPUR / STLUR (RCpc, unscaled)
            let size = bits(insn, 31, 30);
            let opc = bits(insn, 23, 22);
            let addr = cpu.xs(rn).wrapping_add(sext(bits(insn, 20, 12) as u64, 9));
            return ld_st_gpr(cpu, rt, addr, size, opc, true);
        }
        0b011000 | 0b011100 => {
            // load literal
            let opc = bits(insn, 31, 30);
            let addr = cpu.pc.wrapping_add(sext(bits(insn, 23, 5) as u64, 19) << 2);
            unsafe {
                if v {
                    match opc {
                        0 => cpu.set_vd(rt, r32(addr) as u64),
                        1 => cpu.set_vd(rt, r64(addr)),
                        2 => cpu.set_vq(rt, r128(addr)),
                        _ => return Err(unimpl(insn)),
                    }
                } else {
                    match opc {
                        0 => cpu.setx(rt, r32(addr) as u64),
                        1 => cpu.setx(rt, r64(addr)),
                        2 => cpu.setx(rt, r32(addr) as i32 as i64 as u64),
                        _ => {} // PRFM literal
                    }
                }
            }
            return Ok(());
        }
        _ => {}
    }

    match bits(insn, 29, 27) {
        0b101 => pair(cpu, insn),
        0b111 => {
            if bit(insn, 24) != 0 {
                // unsigned immediate offset
                let size = bits(insn, 31, 30);
                let opc = bits(insn, 23, 22);
                let scale = if v && opc & 2 != 0 { 4 } else { size };
                let addr = cpu.xs(rn).wrapping_add((bits(insn, 21, 10) as u64) << scale);
                return if v { ld_st_simd(cpu, rt, addr, size, opc) } else { ld_st_gpr(cpu, rt, addr, size, opc, false) };
            }
            let size = bits(insn, 31, 30);
            let opc = bits(insn, 23, 22);
            if bit(insn, 21) == 0 {
                let imm = sext(bits(insn, 20, 12) as u64, 9);
                let base = cpu.xs(rn);
                let mode = bits(insn, 11, 10);
                let addr = match mode {
                    0 | 2 => base.wrapping_add(imm), // unscaled / unprivileged
                    1 => base,                       // post-index
                    _ => base.wrapping_add(imm),     // pre-index
                };
                if v {
                    ld_st_simd(cpu, rt, addr, size, opc)?;
                } else {
                    ld_st_gpr(cpu, rt, addr, size, opc, false)?;
                }
                if mode == 1 || mode == 3 {
                    cpu.setxs(rn, base.wrapping_add(imm));
                }
                return Ok(());
            }
            match bits(insn, 11, 10) {
                0b10 => {
                    // register offset
                    let rm = bits(insn, 20, 16);
                    let option = bits(insn, 15, 13);
                    let s = bit(insn, 12);
                    let scale = if v && opc & 2 != 0 { 4 } else { size };
                    let off = extend_reg(cpu.xr(rm), option, if s != 0 { scale } else { 0 });
                    let addr = cpu.xs(rn).wrapping_add(off);
                    if v {
                        ld_st_simd(cpu, rt, addr, size, opc)
                    } else {
                        ld_st_gpr(cpu, rt, addr, size, opc, false)
                    }
                }
                0b00 if !v => atomic(cpu, insn),
                0b01 | 0b11 if !v && size == 3 => {
                    // LDRAA / LDRAB (pointer auth is identity)
                    let s = bit(insn, 22) as u64;
                    let imm10 = (s << 9) | bits(insn, 20, 12) as u64;
                    let off = sext(imm10, 10) << 3;
                    let base = cpu.xs(rn);
                    let addr = base.wrapping_add(off);
                    let val = unsafe { r64(addr) };
                    if bit(insn, 11) != 0 {
                        cpu.setxs(rn, addr);
                    }
                    cpu.setx(rt, val);
                    Ok(())
                }
                _ => Err(unimpl(insn)),
            }
        }
        _ => Err(unimpl(insn)),
    }
}

/// Integer register load/store for `size` (0..3) and `opc` as in LDR/STR.
#[inline(always)]
fn ld_st_gpr(cpu: &mut Cpu, rt: u32, addr: u64, size: u32, opc: u32, _rcpc: bool) -> R {
    let nbytes = 1u32 << size;
    unsafe {
        match opc {
            0 => write_sized(addr, nbytes, cpu.xr(rt)),
            1 => cpu.setx(rt, read_sized(addr, nbytes)),
            2 => {
                if size == 3 {
                    return Ok(()); // PRFM / PRFUM
                }
                let v = read_sized(addr, nbytes);
                cpu.setx(rt, sext(v, nbytes * 8));
            }
            _ => {
                if size >= 2 {
                    return Err(unimpl(0xdead_0000 | size));
                }
                let v = read_sized(addr, nbytes);
                cpu.setx(rt, sext(v, nbytes * 8) as u32 as u64);
            }
        }
    }
    Ok(())
}

#[inline(always)]
fn ld_st_simd(cpu: &mut Cpu, rt: u32, addr: u64, size: u32, opc: u32) -> R {
    let load = opc & 1 != 0;
    let q = opc & 2 != 0;
    unsafe {
        if q {
            if size != 0 {
                return Err(unimpl(0xdead_1000));
            }
            if load {
                cpu.set_vq(rt, r128(addr));
            } else {
                w128(addr, cpu.vq(rt));
            }
        } else {
            let nbytes = 1u32 << size;
            if load {
                cpu.set_vd(rt, read_sized(addr, nbytes));
            } else {
                write_sized(addr, nbytes, cpu.vd(rt));
            }
        }
    }
    Ok(())
}

fn pair(cpu: &mut Cpu, insn: u32) -> R {
    let opc = bits(insn, 31, 30);
    let v = bit(insn, 26) != 0;
    let idx = bits(insn, 24, 23);
    let l = bit(insn, 22) != 0;
    let rt = bits(insn, 4, 0);
    let rt2 = bits(insn, 14, 10);
    let rn = bits(insn, 9, 5);
    let (scale, signed) = if v {
        if opc == 3 {
            return Err(unimpl(insn));
        }
        (2 + opc, false)
    } else {
        match opc {
            0 => (2, false),
            1 => {
                if !l {
                    return Err(unimpl(insn)); // STGP
                }
                (2, true)
            }
            2 => (3, false),
            _ => return Err(unimpl(insn)),
        }
    };
    let off = sext(bits(insn, 21, 15) as u64, 7) << scale;
    let base = cpu.xs(rn);
    let addr = if idx == 1 { base } else { base.wrapping_add(off) };
    let nb = 1u64 << scale;
    unsafe {
        if v {
            if l {
                let (a, b) = if scale == 4 {
                    (r128(addr), r128(addr + 16))
                } else {
                    (read_sized(addr, nb as u32) as u128, read_sized(addr + nb, nb as u32) as u128)
                };
                cpu.set_vq(rt, a);
                cpu.set_vq(rt2, b);
            } else if scale == 4 {
                let (a, b) = (cpu.vq(rt), cpu.vq(rt2));
                w128(addr, a);
                w128(addr + 16, b);
            } else {
                let (a, b) = (cpu.vd(rt), cpu.vd(rt2));
                write_sized(addr, nb as u32, a);
                write_sized(addr + nb, nb as u32, b);
            }
        } else if l {
            let mut a = read_sized(addr, nb as u32);
            let mut b = read_sized(addr + nb, nb as u32);
            if signed {
                a = sext(a, 32);
                b = sext(b, 32);
            }
            cpu.setx(rt, a);
            cpu.setx(rt2, b);
        } else {
            let (a, b) = (cpu.xr(rt), cpu.xr(rt2));
            write_sized(addr, nb as u32, a);
            write_sized(addr + nb, nb as u32, b);
        }
    }
    if idx == 1 || idx == 3 {
        cpu.setxs(rn, base.wrapping_add(off));
    }
    Ok(())
}

fn exclusive(cpu: &mut Cpu, insn: u32) -> R {
    let size = bits(insn, 31, 30);
    let o2 = bit(insn, 23);
    let l = bit(insn, 22) != 0;
    let o1 = bit(insn, 21);
    let rs = bits(insn, 20, 16);
    let o0 = bit(insn, 15);
    let rt2 = bits(insn, 14, 10);
    let rn = bits(insn, 9, 5);
    let rt = bits(insn, 4, 0);
    let addr = cpu.xs(rn);
    let nb = 1u32 << size;
    unsafe {
        match (o2, o1) {
            (0, 0) => {
                if l {
                    // LDXR / LDAXR
                    let v = read_sized(addr, nb);
                    cpu.excl_addr = addr;
                    cpu.excl_val = v;
                    cpu.excl_size = nb as u64;
                    cpu.setx(rt, v);
                } else {
                    // STXR / STLXR
                    let new = cpu.xr(rt);
                    let ok = cpu.excl_addr == addr
                        && cpu.excl_size == nb as u64
                        && cas_sized(addr, nb, cpu.excl_val, new) == cpu.excl_val;
                    cpu.excl_addr = u64::MAX;
                    cpu.setx(rs, (!ok) as u64);
                }
            }
            (0, 1) => {
                if size < 2 {
                    // CASP / CASPA / CASPL / CASPAL
                    if rs & 1 != 0 || rt & 1 != 0 {
                        return Err(unimpl(insn));
                    }
                    if size == 0 {
                        let exp = (cpu.xr(rs) as u32 as u64) | (cpu.xr(rs + 1) << 32);
                        let new = (cpu.xr(rt) as u32 as u64) | (cpu.xr(rt + 1) << 32);
                        let old = cas_sized(addr, 8, exp, new);
                        cpu.setx(rs, old as u32 as u64);
                        cpu.setx(rs + 1, old >> 32);
                    } else {
                        let exp = (cpu.xr(rs) as u128) | ((cpu.xr(rs + 1) as u128) << 64);
                        let new = (cpu.xr(rt) as u128) | ((cpu.xr(rt + 1) as u128) << 64);
                        let old = cas128(addr, exp, new);
                        cpu.setx(rs, old as u64);
                        cpu.setx(rs + 1, (old >> 64) as u64);
                    }
                    return Ok(());
                }
                let eb = nb; // element bytes (4 or 8)
                if l {
                    // LDXP / LDAXP
                    let (a, b) = if eb == 8 {
                        let v = r128(addr);
                        (v as u64, (v >> 64) as u64)
                    } else {
                        let v = r64(addr);
                        (v as u32 as u64, v >> 32)
                    };
                    cpu.excl_addr = addr;
                    cpu.excl_val = a;
                    cpu.excl_val2 = b;
                    cpu.excl_size = (eb * 2) as u64;
                    cpu.setx(rt, a);
                    cpu.setx(rt2, b);
                } else {
                    // STXP / STLXP
                    let (a, b) = (cpu.xr(rt), cpu.xr(rt2));
                    let ok = if cpu.excl_addr != addr || cpu.excl_size != (eb * 2) as u64 {
                        false
                    } else if eb == 8 {
                        let exp = (cpu.excl_val as u128) | ((cpu.excl_val2 as u128) << 64);
                        let new = (a as u128) | ((b as u128) << 64);
                        cas128(addr, exp, new) == exp
                    } else {
                        let exp = (cpu.excl_val as u32 as u64) | (cpu.excl_val2 << 32);
                        let new = (a as u32 as u64) | (b << 32);
                        cas_sized(addr, 8, exp, new) == exp
                    };
                    cpu.excl_addr = u64::MAX;
                    cpu.setx(rs, (!ok) as u64);
                }
            }
            (1, 0) => {
                // LDAR / LDLAR / STLR / STLLR
                if l {
                    let v = read_sized(addr, nb);
                    fence(Ordering::Acquire);
                    cpu.setx(rt, v);
                } else {
                    // Store-release followed by a possible load-acquire must not
                    // be reordered (RCsc), so use a full fence on x86.
                    write_sized(addr, nb, cpu.xr(rt));
                    if o0 != 0 {
                        fence(Ordering::SeqCst);
                    }
                }
            }
            _ => {
                // CAS / CASA / CASL / CASAL
                let exp = cpu.xr(rs);
                let mask = if nb == 8 { u64::MAX } else { (1u64 << (nb * 8)) - 1 };
                let old = cas_sized(addr, nb, exp & mask, cpu.xr(rt) & mask);
                cpu.setx(rs, old);
            }
        }
    }
    Ok(())
}

fn atomic(cpu: &mut Cpu, insn: u32) -> R {
    let size = bits(insn, 31, 30);
    let rs = bits(insn, 20, 16);
    let o3 = bit(insn, 15);
    let opc = bits(insn, 14, 12);
    let rn = bits(insn, 9, 5);
    let rt = bits(insn, 4, 0);
    let nb = 1u32 << size;
    let addr = cpu.xs(rn);
    let bitsz = nb * 8;
    let s = cpu.xr(rs);
    unsafe {
        if o3 == 1 {
            match opc {
                0 => {
                    // SWP
                    let old = rmw_sized(addr, nb, |_| s);
                    cpu.setx(rt, old);
                }
                4 => {
                    // LDAPR
                    let v = read_sized(addr, nb);
                    cpu.setx(rt, v);
                }
                _ => return Err(unimpl(insn)),
            }
            return Ok(());
        }
        let sx = |v: u64| sext(v, bitsz) as i64;
        let old = rmw_sized(addr, nb, |old| match opc {
            0 => old.wrapping_add(s),
            1 => old & !s,
            2 => old ^ s,
            3 => old | s,
            4 => if sx(old) >= sx(s) { old } else { s },
            5 => if sx(old) <= sx(s) { old } else { s },
            6 => {
                let m = ones(bitsz);
                if old & m >= s & m { old } else { s }
            }
            _ => {
                let m = ones(bitsz);
                if old & m <= s & m { old } else { s }
            }
        });
        cpu.setx(rt, old);
    }
    Ok(())
}
