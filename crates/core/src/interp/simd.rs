//! Scalar floating point and Advanced SIMD (NEON).

use super::*;
use crate::mem::*;
use crate::softfloat::{self as sf, Fp};

// ---------------- element helpers ----------------

#[inline(always)]
pub(crate) fn get(v: u128, esz: u32, i: u32) -> u64 {
    ((v >> (i * esz)) as u64) & ones(esz)
}
#[inline(always)]
pub(crate) fn put(v: &mut u128, esz: u32, i: u32, val: u64) {
    let m = (ones(esz) as u128) << (i * esz);
    *v = (*v & !m) | (((val & ones(esz)) as u128) << (i * esz));
}
#[inline(always)]
fn sx(v: u64, esz: u32) -> i64 {
    sext(v, esz) as i64
}
#[inline(always)]
fn set_vec(cpu: &mut Cpu, rd: u32, v: u128, q: bool) {
    cpu.set_vq(rd, if q { v } else { v & (u64::MAX as u128) });
}
#[inline(always)]
fn sat_s(v: i128, esz: u32, fpsr: &mut u64) -> u64 {
    let max = (1i128 << (esz - 1)) - 1;
    let min = -(1i128 << (esz - 1));
    if v > max {
        *fpsr |= 1 << 27;
        max as u64 & ones(esz)
    } else if v < min {
        *fpsr |= 1 << 27;
        min as u64 & ones(esz)
    } else {
        v as u64 & ones(esz)
    }
}
#[inline(always)]
fn sat_u(v: i128, esz: u32, fpsr: &mut u64) -> u64 {
    let max = ones(esz) as i128;
    if v > max {
        *fpsr |= 1 << 27;
        ones(esz)
    } else if v < 0 {
        *fpsr |= 1 << 27;
        0
    } else {
        v as u64
    }
}

/// Integer shift by signed register amount (SSHL/USHL family).
fn shl_reg(a: u64, sh: i64, esz: u32, unsigned: bool, round: bool, sat: bool, fpsr: &mut u64) -> u64 {
    let av: i128 = if unsigned { a as i128 } else { sx(a, esz) as i128 };
    let r: i128 = if sh >= 0 {
        if sh as u32 >= esz {
            if !sat || av == 0 {
                return 0;
            }
            // any non-zero value overflows
            if av > 0 { i128::MAX >> 1 } else { i128::MIN >> 1 }
        } else {
            av << sh
        }
    } else {
        let s = (-sh).min(100) as u32;
        let rnd = if round { 1i128 << (s - 1) } else { 0 };
        (av + rnd) >> s
    };
    if sat {
        if unsigned { sat_u(r, esz, fpsr) } else { sat_s(r, esz, fpsr) }
    } else {
        r as u64 & ones(esz)
    }
}

pub fn vfp_expand_imm(imm8: u32) -> u64 {
    // returns double-precision bit pattern
    let a = (imm8 >> 7) as u64;
    let b = ((imm8 >> 6) & 1) as u64;
    let cd = ((imm8 >> 4) & 3) as u64;
    let efgh = (imm8 & 0xf) as u64;
    let exp = ((b ^ 1) << 10) | (if b != 0 { 0xff << 2 } else { 0 }) | cd;
    (a << 63) | (exp << 52) | (efgh << 48)
}

pub fn adv_simd_expand_imm(op: u32, cmode: u32, imm8: u32) -> u64 {
    let imm8 = imm8 as u64;
    let rep32 = |v: u64| v | (v << 32);
    let rep16 = |v: u64| v | (v << 16) | (v << 32) | (v << 48);
    match cmode >> 1 {
        0 => rep32(imm8),
        1 => rep32(imm8 << 8),
        2 => rep32(imm8 << 16),
        3 => rep32(imm8 << 24),
        4 => rep16(imm8),
        5 => rep16(imm8 << 8),
        6 => {
            if cmode & 1 == 0 {
                rep32((imm8 << 8) | 0xff)
            } else {
                rep32((imm8 << 16) | 0xffff)
            }
        }
        _ => {
            if cmode & 1 == 0 {
                if op == 0 {
                    let b = imm8;
                    b * 0x0101_0101_0101_0101
                } else {
                    let mut r = 0u64;
                    for i in 0..8 {
                        if (imm8 >> i) & 1 != 0 {
                            r |= 0xff << (i * 8);
                        }
                    }
                    r
                }
            } else if op == 0 {
                // FMOV single (replicated)
                let d = f64::from_bits(vfp_expand_imm(imm8 as u32));
                rep32((d as f32).to_bits() as u64)
            } else {
                vfp_expand_imm(imm8 as u32)
            }
        }
    }
}

// ---------------- FP helpers ----------------

#[inline(always)]
fn fpcr_rmode(cpu: &Cpu) -> u32 {
    ((cpu.fpcr >> 22) & 3) as u32
}

/// Convert FP value to integer with rounding `mode` (0 N,1 P,2 M,3 Z,4 A).
fn fp_to_int<T: Fp>(x: T, mode: u32, signed: bool, bits64: bool, fbits: u32) -> u64 {
    let mut v = x.to_f64();
    if v.is_nan() {
        return 0;
    }
    if fbits != 0 {
        v *= (fbits as f64).exp2();
    }
    let r = v.round_mode(mode);
    match (signed, bits64) {
        (true, true) => r as i64 as u64,
        (true, false) => r as i32 as u32 as u64,
        (false, true) => r as u64,
        (false, false) => r as u32 as u64,
    }
}

fn int_to_fp<T: Fp>(v: u64, signed: bool, bits64: bool, fbits: u32) -> T {
    let r = match (signed, bits64) {
        (true, true) => T::from_i64(v as i64),
        (true, false) => T::from_i64(v as i32 as i64),
        (false, true) => T::from_u64(v),
        (false, false) => T::from_u64(v as u32 as u64),
    };
    if fbits != 0 {
        r * T::from_f64((-(fbits as f64)).exp2())
    } else {
        r
    }
}

#[inline(always)]
fn frint<T: Fp>(x: T, mode: u32) -> T {
    if x.is_nan() {
        return if x.is_snan() { x.quiet() } else { x };
    }
    let r = x.round_mode(mode);
    // preserve sign of zero results
    if r.is_zero() {
        T::zero(x.sign())
    } else {
        r
    }
}

/// FRINT32*/FRINT64*: round, and produce the most negative integer when out of range.
fn frint_n<T: Fp>(x: T, mode: u32, n: u32) -> T {
    if x.is_nan() || x.is_inf() {
        return T::from_f64(-(2f64.powi(n as i32 - 1)));
    }
    let r = frint(x, mode);
    let lim = 2f64.powi(n as i32 - 1);
    let rv = r.to_f64();
    if rv >= lim || rv < -lim {
        T::from_f64(-lim)
    } else {
        r
    }
}

// ---------------- dispatch ----------------

pub(crate) fn simd_fp(cpu: &mut Cpu, insn: u32) -> R {
    if let Some(r) = super::fp16::try_exec(cpu, insn) {
        return r;
    }
    // Scalar FP (incl. conversions, which use bit 31 as sf)
    if insn & 0x7F00_0000 == 0x1E00_0000 {
        return fp_scalar(cpu, insn);
    }
    if insn & 0xFF00_0000 == 0x1F00_0000 {
        return fp_3src(cpu, insn);
    }
    // Crypto
    if insn & 0xFF3E_0C00 == 0x4E28_0800 {
        return crypto_aes(cpu, insn);
    }
    if insn & 0xFF20_8C00 == 0x5E00_0000 {
        return crypto_sha3(cpu, insn);
    }
    if insn & 0xFF3E_0C00 == 0x5E28_0800 {
        return crypto_sha2(cpu, insn);
    }
    if insn >> 24 == 0xCE {
        return crypto_ce(cpu, insn);
    }
    let top = bits(insn, 31, 30);
    let g = bits(insn, 28, 24);
    if top & 2 == 0 && g == 0b01110 {
        // vector 01110
        if bit(insn, 21) == 1 {
            if bit(insn, 10) == 1 {
                return three_same(cpu, insn, false);
            }
            if bits(insn, 11, 10) == 0 {
                return three_diff(cpu, insn, false);
            }
            match bits(insn, 20, 17) {
                0b0000 => return two_misc(cpu, insn, false),
                0b1000 => return across(cpu, insn),
                _ => return Err(unimpl(insn)),
            }
        }
        if bits(insn, 23, 21) == 0 && bit(insn, 15) == 0 && bit(insn, 10) == 1 {
            return copy(cpu, insn, false);
        }
        if bit(insn, 15) == 1 && bit(insn, 10) == 1 {
            return three_same_extra(cpu, insn);
        }
        if bit(insn, 29) == 0 && bit(insn, 15) == 0 {
            match bits(insn, 11, 10) {
                0b10 => return permute(cpu, insn),
                0b00 => return tbl(cpu, insn),
                _ => {}
            }
        }
        if bit(insn, 29) == 1 && bits(insn, 23, 22) == 0 && bit(insn, 15) == 0 && bit(insn, 10) == 0 {
            return ext(cpu, insn);
        }
        return Err(unimpl(insn));
    }
    if top & 2 == 0 && g == 0b01111 {
        if bit(insn, 10) == 1 {
            if bits(insn, 22, 19) == 0 {
                return mod_imm(cpu, insn);
            }
            if bit(insn, 23) == 0 {
                return shift_imm(cpu, insn, false);
            }
            return Err(unimpl(insn));
        }
        return indexed(cpu, insn, false);
    }
    if top == 0b01 && g == 0b11110 {
        if bit(insn, 21) == 1 {
            if bit(insn, 10) == 1 {
                return three_same(cpu, insn, true);
            }
            if bits(insn, 11, 10) == 0 {
                return three_diff(cpu, insn, true);
            }
            match bits(insn, 20, 17) {
                0b0000 => return two_misc(cpu, insn, true),
                0b1000 => return scalar_pairwise(cpu, insn),
                _ => return Err(unimpl(insn)),
            }
        }
        if bits(insn, 23, 21) == 0 && bit(insn, 15) == 0 && bit(insn, 10) == 1 {
            return copy(cpu, insn, true);
        }
        return Err(unimpl(insn));
    }
    if top == 0b01 && g == 0b11111 {
        if bit(insn, 10) == 1 {
            return shift_imm(cpu, insn, true);
        }
        return indexed(cpu, insn, true);
    }
    Err(unimpl(insn))
}

// ---------------- scalar floating point ----------------

fn fp_scalar(cpu: &mut Cpu, insn: u32) -> R {
    let ptype = bits(insn, 23, 22);
    let rd = bits(insn, 4, 0);
    let rn = bits(insn, 9, 5);
    let rm = bits(insn, 20, 16);
    if bit(insn, 21) == 0 {
        // conversion between FP and fixed-point
        let sfb = bit(insn, 31) != 0;
        let rmode = bits(insn, 20, 19);
        let opcode = bits(insn, 18, 16);
        let scale = bits(insn, 15, 10);
        let fbits = 64 - scale;
        if !sfb && scale < 32 {
            return Err(unimpl(insn));
        }
        match (rmode, opcode) {
            (0, 2) | (0, 3) => {
                let signed = opcode == 2;
                let v = cpu.xr(rn);
                match ptype {
                    0 => cpu.set_vd(rd, int_to_fp::<f32>(v, signed, sfb, fbits).to_bits() as u64),
                    1 => cpu.set_vd(rd, int_to_fp::<f64>(v, signed, sfb, fbits).to_bits()),
                    3 => cpu.set_vd(rd, sf::f64_to_f16(int_to_fp::<f64>(v, signed, sfb, fbits)) as u64),
                    _ => return Err(unimpl(insn)),
                }
            }
            (3, 0) | (3, 1) => {
                let signed = opcode == 0;
                let r = match ptype {
                    0 => fp_to_int(f32::from_bits(cpu.vd(rn) as u32), 3, signed, sfb, fbits),
                    1 => fp_to_int(f64::from_bits(cpu.vd(rn)), 3, signed, sfb, fbits),
                    3 => fp_to_int(sf::f16_to_f64(cpu.vd(rn) as u16), 3, signed, sfb, fbits),
                    _ => return Err(unimpl(insn)),
                };
                cpu.setx(rd, r);
            }
            _ => return Err(unimpl(insn)),
        }
        return Ok(());
    }
    if bits(insn, 15, 10) == 0 {
        // conversion between FP and integer
        let sfb = bit(insn, 31) != 0;
        let rmode = bits(insn, 20, 19);
        let opcode = bits(insn, 18, 16);
        match opcode {
            0 | 1 | 4 | 5 => {
                let signed = opcode & 1 == 0;
                let mode = if opcode >= 4 { 4 } else { rmode };
                if opcode >= 4 && rmode != 0 {
                    return Err(unimpl(insn));
                }
                let r = match ptype {
                    0 => fp_to_int(f32::from_bits(cpu.vd(rn) as u32), mode, signed, sfb, 0),
                    1 => fp_to_int(f64::from_bits(cpu.vd(rn)), mode, signed, sfb, 0),
                    3 => fp_to_int(sf::f16_to_f64(cpu.vd(rn) as u16), mode, signed, sfb, 0),
                    _ => return Err(unimpl(insn)),
                };
                cpu.setx(rd, r);
            }
            2 | 3 => {
                if rmode != 0 {
                    return Err(unimpl(insn));
                }
                let signed = opcode == 2;
                let v = cpu.xr(rn);
                match ptype {
                    0 => cpu.set_vd(rd, int_to_fp::<f32>(v, signed, sfb, 0).to_bits() as u64),
                    1 => cpu.set_vd(rd, int_to_fp::<f64>(v, signed, sfb, 0).to_bits()),
                    3 => {
                        // round once from the integer to half
                        let d = int_to_fp::<f64>(v, signed, sfb, 0);
                        cpu.set_vd(rd, sf::f64_to_f16(d) as u64);
                    }
                    _ => return Err(unimpl(insn)),
                }
            }
            6 => {
                if rmode == 3 && ptype == 1 && !sfb {
                    // FJCVTZS
                    let d = f64::from_bits(cpu.vd(rn));
                    let (r, exact) = fjcvtzs(d);
                    cpu.setx(rd, r as u32 as u64);
                    cpu.set_nzcv(if exact { 0b0100 } else { 0 });
                    return Ok(());
                }
                // FMOV to general
                let v = match (sfb, ptype, rmode) {
                    (false, 0, 0) => cpu.vd(rn) as u32 as u64,
                    (true, 1, 0) => cpu.vd(rn),
                    (true, 2, 1) => cpu.v[rn as usize][1],
                    (_, 3, 0) => cpu.vd(rn) as u16 as u64,
                    _ => return Err(unimpl(insn)),
                };
                cpu.setx(rd, v);
            }
            7 => {
                // FMOV from general
                let x = cpu.xr(rn);
                match (sfb, ptype, rmode) {
                    (false, 0, 0) => cpu.set_vd(rd, x as u32 as u64),
                    (true, 1, 0) => cpu.set_vd(rd, x),
                    (true, 2, 1) => cpu.v[rd as usize][1] = x,
                    (_, 3, 0) => cpu.set_vd(rd, x as u16 as u64),
                    _ => return Err(unimpl(insn)),
                }
            }
            _ => return Err(unimpl(insn)),
        }
        return Ok(());
    }
    if bits(insn, 14, 10) == 0b10000 {
        // 1-source
        let opcode = bits(insn, 20, 15);
        return fp_1src(cpu, insn, ptype, opcode, rd, rn);
    }
    if bits(insn, 13, 10) == 0b1000 {
        // compare
        let opc2 = bits(insn, 4, 0);
        let with_zero = opc2 & 0b01000 != 0;
        let f = match ptype {
            0 => {
                let a = f32::from_bits(cpu.vd(rn) as u32);
                let b = if with_zero { 0.0 } else { f32::from_bits(cpu.vd(rm) as u32) };
                sf::fcmp(a, b)
            }
            1 => {
                let a = f64::from_bits(cpu.vd(rn));
                let b = if with_zero { 0.0 } else { f64::from_bits(cpu.vd(rm)) };
                sf::fcmp(a, b)
            }
            3 => {
                let a = sf::f16_to_f64(cpu.vd(rn) as u16);
                let b = if with_zero { 0.0 } else { sf::f16_to_f64(cpu.vd(rm) as u16) };
                sf::fcmp(a, b)
            }
            _ => return Err(unimpl(insn)),
        };
        cpu.set_nzcv(f);
        return Ok(());
    }
    if bits(insn, 12, 10) == 0b100 {
        // FMOV (immediate)
        let imm8 = bits(insn, 20, 13);
        let d = vfp_expand_imm(imm8);
        match ptype {
            0 => cpu.set_vd(rd, (f64::from_bits(d) as f32).to_bits() as u64),
            1 => cpu.set_vd(rd, d),
            3 => cpu.set_vd(rd, sf::f64_to_f16(f64::from_bits(d)) as u64),
            _ => return Err(unimpl(insn)),
        }
        return Ok(());
    }
    match bits(insn, 11, 10) {
        0b01 => {
            // FCCMP / FCCMPE
            let cond = bits(insn, 15, 12);
            let nzcv = bits(insn, 3, 0);
            if cpu.cond(cond) {
                let f = match ptype {
                    0 => sf::fcmp(f32::from_bits(cpu.vd(rn) as u32), f32::from_bits(cpu.vd(rm) as u32)),
                    1 => sf::fcmp(f64::from_bits(cpu.vd(rn)), f64::from_bits(cpu.vd(rm))),
                    3 => sf::fcmp(sf::f16_to_f64(cpu.vd(rn) as u16), sf::f16_to_f64(cpu.vd(rm) as u16)),
                    _ => return Err(unimpl(insn)),
                };
                cpu.set_nzcv(f);
            } else {
                cpu.set_nzcv(nzcv);
            }
            Ok(())
        }
        0b10 => {
            // 2-source
            let opcode = bits(insn, 15, 12);
            match ptype {
                0 => {
                    let a = f32::from_bits(cpu.vd(rn) as u32);
                    let b = f32::from_bits(cpu.vd(rm) as u32);
                    let r = fp_2src(a, b, opcode).ok_or_else(|| unimpl(insn))?;
                    cpu.set_vd(rd, r.to_bits() as u64);
                }
                1 => {
                    let a = f64::from_bits(cpu.vd(rn));
                    let b = f64::from_bits(cpu.vd(rm));
                    let r = fp_2src(a, b, opcode).ok_or_else(|| unimpl(insn))?;
                    cpu.set_vd(rd, r.to_bits());
                }
                3 => {
                    let a = sf::f16_to_f32(cpu.vd(rn) as u16);
                    let b = sf::f16_to_f32(cpu.vd(rm) as u16);
                    let r = fp_2src(a, b, opcode).ok_or_else(|| unimpl(insn))?;
                    cpu.set_vd(rd, sf::f32_to_f16(r) as u64);
                }
                _ => return Err(unimpl(insn)),
            }
            Ok(())
        }
        _ => {
            // FCSEL
            let cond = bits(insn, 15, 12);
            let v = if cpu.cond(cond) { cpu.vd(rn) } else { cpu.vd(rm) };
            let v = match ptype {
                0 => v as u32 as u64,
                1 => v,
                3 => v as u16 as u64,
                _ => return Err(unimpl(insn)),
            };
            cpu.set_vd(rd, v);
            Ok(())
        }
    }
}

fn fjcvtzs(d: f64) -> (i32, bool) {
    if d.is_nan() || d.is_infinite() {
        return (0, false);
    }
    let t = d.trunc();
    let exact = t == d;
    // modulo 2^32
    let m = t.rem_euclid(4294967296.0);
    let r = m as u64 as u32 as i32;
    let in_range = t >= -2147483648.0 && t <= 2147483647.0;
    (r, exact && in_range && !(d == 0.0 && d.is_sign_negative()))
}

#[inline(always)]
fn fp_2src<T: Fp>(a: T, b: T, opcode: u32) -> Option<T> {
    Some(match opcode {
        0 => sf::fmul(a, b),
        1 => sf::fdiv(a, b),
        2 => sf::fadd(a, b),
        3 => sf::fsub(a, b),
        4 => sf::fmax(a, b),
        5 => sf::fmin(a, b),
        6 => sf::fmaxnm(a, b),
        7 => sf::fminnm(a, b),
        8 => -sf::fmul(a, b), // FNMUL negates NaNs too
        _ => return None,
    })
}

fn fp_1src(cpu: &mut Cpu, insn: u32, ptype: u32, opcode: u32, rd: u32, rn: u32) -> R {
    let rmode = fpcr_rmode(cpu);
    // FCVT between precisions
    if opcode & 0b111100 == 0b000100 {
        let to = opcode & 3;
        let src = cpu.vd(rn);
        // go through f64 (exact for f32/f16 sources)
        let d = match ptype {
            0 => f32::from_bits(src as u32) as f64,
            1 => f64::from_bits(src),
            3 => sf::f16_to_f64(src as u16),
            _ => return Err(unimpl(insn)),
        };
        let quiet_nan_from = |d: f64| -> f64 { if d.is_nan() { f64::from_bits(d.to_bits() | (1 << 51)) } else { d } };
        let d = quiet_nan_from(d);
        let r = match to {
            0 => {
                let f = if ptype == 1 { d as f32 } else { d as f32 };
                f.to_bits() as u64
            }
            1 => d.to_bits(),
            3 => {
                if ptype == 0 {
                    sf::f32_to_f16(f32::from_bits(src as u32)) as u64
                } else {
                    sf::f64_to_f16(d) as u64
                }
            }
            _ => return Err(unimpl(insn)),
        };
        cpu.set_vd(rd, r);
        return Ok(());
    }
    macro_rules! go {
        ($t:ty, $load:expr, $store:expr) => {{
            let a: $t = $load;
            let r: $t = match opcode {
                0 => a,
                1 => a.abs(),
                2 => -a,
                3 => sf::fsqrt(a),
                8 => frint(a, 0),
                9 => frint(a, 1),
                10 => frint(a, 2),
                11 => frint(a, 3),
                12 => frint(a, 4),
                14 | 15 => frint(a, rmode),
                16 => frint_n(a, 3, 32),
                17 => frint_n(a, rmode, 32),
                18 => frint_n(a, 3, 64),
                19 => frint_n(a, rmode, 64),
                _ => return Err(unimpl(insn)),
            };
            $store(r)
        }};
    }
    match ptype {
        0 => go!(f32, f32::from_bits(cpu.vd(rn) as u32), |r: f32| cpu.set_vd(rd, r.to_bits() as u64)),
        1 => go!(f64, f64::from_bits(cpu.vd(rn)), |r: f64| cpu.set_vd(rd, r.to_bits())),
        3 => {
            // half: FMOV/FABS/FNEG operate on bits; others via f32
            let h = cpu.vd(rn) as u16;
            let r = match opcode {
                0 => h,
                1 => h & 0x7fff,
                2 => h ^ 0x8000,
                _ => {
                    let a = sf::f16_to_f32(h);
                    let r = match opcode {
                        3 => sf::fsqrt(a),
                        8 => frint(a, 0),
                        9 => frint(a, 1),
                        10 => frint(a, 2),
                        11 => frint(a, 3),
                        12 => frint(a, 4),
                        14 | 15 => frint(a, rmode),
                        _ => return Err(unimpl(insn)),
                    };
                    sf::f32_to_f16(r)
                }
            };
            cpu.set_vd(rd, r as u64);
        }
        _ => return Err(unimpl(insn)),
    }
    Ok(())
}

fn fp_3src(cpu: &mut Cpu, insn: u32) -> R {
    let ptype = bits(insn, 23, 22);
    let o1 = bit(insn, 21);
    let o0 = bit(insn, 15);
    let rd = bits(insn, 4, 0);
    let rn = bits(insn, 9, 5);
    let rm = bits(insn, 20, 16);
    let ra = bits(insn, 14, 10);
    macro_rules! go {
        ($t:ty, $get:expr, $put:expr) => {{
            let n: $t = $get(rn);
            let m: $t = $get(rm);
            let a: $t = $get(ra);
            // FMADD: a + n*m; FMSUB: a - n*m; FNMADD: -a - n*m; FNMSUB: -a + n*m
            let (a2, n2) = match (o1, o0) {
                (0, 0) => (a, n),
                (0, _) => (a, -n),
                (_, 0) => (-a, -n),
                _ => (-a, n),
            };
            // Negation is applied before NaN processing (flips NaN signs too).
            $put(sf::fmla(a2, n2, m))
        }};
    }
    match ptype {
        0 => go!(f32, |r| f32::from_bits(cpu.vd(r) as u32), |r: f32| cpu.set_vd(rd, r.to_bits() as u64)),
        1 => go!(f64, |r| f64::from_bits(cpu.vd(r)), |r: f64| cpu.set_vd(rd, r.to_bits())),
        3 => {
            let g = |r: u32| sf::f16_to_f64(cpu.vd(r) as u16);
            let (n, m, a) = (g(rn), g(rm), g(ra));
            let (a2, n2) = match (o1, o0) {
                (0, 0) => (a, n),
                (0, _) => (a, -n),
                (_, 0) => (-a, -n),
                _ => (-a, n),
            };
            // f64 fma of half inputs is exact enough (single rounding to half after)
            let r = sf::fmla(a2, n2, m);
            cpu.set_vd(rd, sf::f64_to_f16(r) as u64);
        }
        _ => return Err(unimpl(insn)),
    }
    Ok(())
}

// ---------------- vector FP element ops ----------------

/// Apply a per-element FP binary op over a vector for f32 (esz 32) or f64 (esz 64).
fn fp_vec2(a: u128, b: u128, esz: u32, elems: u32, f32op: impl Fn(f32, f32) -> u64, f64op: impl Fn(f64, f64) -> u64) -> u128 {
    let mut r = 0u128;
    for i in 0..elems {
        let x = get(a, esz, i);
        let y = get(b, esz, i);
        let v = if esz == 32 {
            f32op(f32::from_bits(x as u32), f32::from_bits(y as u32))
        } else {
            f64op(f64::from_bits(x), f64::from_bits(y))
        };
        put(&mut r, esz, i, v);
    }
    r
}

fn fp_vec1(a: u128, esz: u32, elems: u32, f32op: impl Fn(f32) -> u64, f64op: impl Fn(f64) -> u64) -> u128 {
    let mut r = 0u128;
    for i in 0..elems {
        let x = get(a, esz, i);
        let v = if esz == 32 { f32op(f32::from_bits(x as u32)) } else { f64op(f64::from_bits(x)) };
        put(&mut r, esz, i, v);
    }
    r
}

#[inline(always)]
fn fbits<T: Fp>(v: T) -> u64 {
    v.to_bits64()
}
#[inline(always)]
fn mask_if(c: bool, esz: u32) -> u64 {
    if c { ones(esz) } else { 0 }
}

// ---------------- three same ----------------

fn three_same(cpu: &mut Cpu, insn: u32, scalar: bool) -> R {
    let q = bit(insn, 30) != 0 || scalar;
    let u = bit(insn, 29) != 0;
    let size = bits(insn, 23, 22);
    let rm = bits(insn, 20, 16);
    let opcode = bits(insn, 15, 11);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let a = cpu.vq(rn);
    let b = cpu.vq(rm);
    let d = cpu.vq(rd);

    if opcode >= 0b11000 {
        // floating point
        let esz = if size & 1 != 0 { 64 } else { 32 };
        let fa = size >> 1;
        let elems = if scalar { 1 } else if bit(insn, 30) != 0 { 128 / esz } else { 64 / esz };
        if esz == 64 && bit(insn, 30) == 0 && !scalar {
            return Err(unimpl(insn));
        }
        let key = ((u as u32) << 6) | (fa << 5) | opcode;
        let r: u128 = match key {
            // U=0 a=0
            0b0_0_11000 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::fmaxnm(x, y)), |x, y| fbits(sf::fmaxnm(x, y))),
            0b0_0_11001 => {
                let mut r = 0u128;
                for i in 0..elems {
                    let v = if esz == 32 {
                        fbits(sf::fmla(f32::from_bits(get(d, 32, i) as u32), f32::from_bits(get(a, 32, i) as u32), f32::from_bits(get(b, 32, i) as u32)))
                    } else {
                        fbits(sf::fmla(f64::from_bits(get(d, 64, i)), f64::from_bits(get(a, 64, i)), f64::from_bits(get(b, 64, i))))
                    };
                    put(&mut r, esz, i, v);
                }
                r
            }
            0b0_0_11010 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::fadd(x, y)), |x, y| fbits(sf::fadd(x, y))),
            0b0_0_11011 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::fmulx(x, y)), |x, y| fbits(sf::fmulx(x, y))),
            0b0_0_11100 => fp_vec2(a, b, esz, elems, |x, y| mask_if(x == y, 32), |x, y| mask_if(x == y, 64)),
            0b0_0_11110 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::fmax(x, y)), |x, y| fbits(sf::fmax(x, y))),
            0b0_0_11111 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::frecps(x, y)), |x, y| fbits(sf::frecps(x, y))),
            // U=0 a=1
            0b0_1_11000 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::fminnm(x, y)), |x, y| fbits(sf::fminnm(x, y))),
            0b0_1_11001 => {
                let mut r = 0u128;
                for i in 0..elems {
                    let v = if esz == 32 {
                        let n = f32::from_bits(get(a, 32, i) as u32);
                        let nn = -n;
                        fbits(sf::fmla(f32::from_bits(get(d, 32, i) as u32), nn, f32::from_bits(get(b, 32, i) as u32)))
                    } else {
                        let n = f64::from_bits(get(a, 64, i));
                        let nn = -n;
                        fbits(sf::fmla(f64::from_bits(get(d, 64, i)), nn, f64::from_bits(get(b, 64, i))))
                    };
                    put(&mut r, esz, i, v);
                }
                r
            }
            0b0_1_11010 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::fsub(x, y)), |x, y| fbits(sf::fsub(x, y))),
            0b0_1_11110 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::fmin(x, y)), |x, y| fbits(sf::fmin(x, y))),
            0b0_1_11111 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::frsqrts(x, y)), |x, y| fbits(sf::frsqrts(x, y))),
            // U=1 a=0
            0b1_0_11000 if !scalar => fp_pairwise(a, b, esz, elems, |x, y| fbits(sf::fmaxnm(x, y)), |x, y| fbits(sf::fmaxnm(x, y))),
            0b1_0_11010 if !scalar => fp_pairwise(a, b, esz, elems, |x, y| fbits(sf::fadd(x, y)), |x, y| fbits(sf::fadd(x, y))),
            0b1_0_11011 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::fmul(x, y)), |x, y| fbits(sf::fmul(x, y))),
            0b1_0_11100 => fp_vec2(a, b, esz, elems, |x, y| mask_if(x >= y, 32), |x, y| mask_if(x >= y, 64)),
            0b1_0_11101 => fp_vec2(a, b, esz, elems, |x, y| mask_if(x.abs() >= y.abs(), 32), |x, y| mask_if(x.abs() >= y.abs(), 64)),
            0b1_0_11110 if !scalar => fp_pairwise(a, b, esz, elems, |x, y| fbits(sf::fmax(x, y)), |x, y| fbits(sf::fmax(x, y))),
            0b1_0_11111 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::fdiv(x, y)), |x, y| fbits(sf::fdiv(x, y))),
            // U=1 a=1
            0b1_1_11000 if !scalar => fp_pairwise(a, b, esz, elems, |x, y| fbits(sf::fminnm(x, y)), |x, y| fbits(sf::fminnm(x, y))),
            0b1_1_11010 => fp_vec2(a, b, esz, elems, |x, y| fbits(sf::fsub(x, y).abs()), |x, y| fbits(sf::fsub(x, y).abs())),
            0b1_1_11100 => fp_vec2(a, b, esz, elems, |x, y| mask_if(x > y, 32), |x, y| mask_if(x > y, 64)),
            0b1_1_11101 => fp_vec2(a, b, esz, elems, |x, y| mask_if(x.abs() > y.abs(), 32), |x, y| mask_if(x.abs() > y.abs(), 64)),
            0b1_1_11110 if !scalar => fp_pairwise(a, b, esz, elems, |x, y| fbits(sf::fmin(x, y)), |x, y| fbits(sf::fmin(x, y))),
            _ => return Err(unimpl(insn)),
        };
        if scalar {
            cpu.set_vq(rd, r & (ones(esz) as u128));
        } else {
            set_vec(cpu, rd, r, bit(insn, 30) != 0);
        }
        return Ok(());
    }

    if opcode == 0b00011 {
        // logical
        let r = match (u, size) {
            (false, 0) => a & b,
            (false, 1) => a & !b,
            (false, 2) => a | b,
            (false, _) => a | !b,
            (true, 0) => a ^ b,
            (true, 1) => (d & a) | (!d & b), // BSL
            (true, 2) => (d & !b) | (a & b), // BIT
            (true, _) => (d & b) | (a & !b), // BIF
        };
        if scalar {
            return Err(unimpl(insn));
        }
        set_vec(cpu, rd, r, q);
        return Ok(());
    }

    let esz = 8u32 << size;
    let elems = if scalar { 1 } else if q { 128 / esz } else { 64 / esz };
    if scalar && size != 3 && !matches!(opcode, 0b00001 | 0b00101 | 0b01001 | 0b01011 | 0b10110) {
        return Err(unimpl(insn));
    }
    let mut fpsr = cpu.fpsr;
    let mut r = 0u128;
    let pairwise = matches!(opcode, 0b10100 | 0b10101 | 0b10111);
    if pairwise {
        if scalar {
            return Err(unimpl(insn));
        }
        let half = elems / 2;
        for i in 0..elems {
            let (src, j) = if i < half { (a, i * 2) } else { (b, (i - half) * 2) };
            let x = get(src, esz, j);
            let y = get(src, esz, j + 1);
            let v = match (opcode, u) {
                (0b10100, false) => if sx(x, esz) >= sx(y, esz) { x } else { y },
                (0b10100, true) => x.max(y),
                (0b10101, false) => if sx(x, esz) <= sx(y, esz) { x } else { y },
                (0b10101, true) => x.min(y),
                (0b10111, false) => x.wrapping_add(y),
                _ => return Err(unimpl(insn)),
            };
            put(&mut r, esz, i, v);
        }
        set_vec(cpu, rd, r, q);
        return Ok(());
    }
    for i in 0..elems {
        let x = get(a, esz, i);
        let y = get(b, esz, i);
        let dd = get(d, esz, i);
        let (xs, ys) = (sx(x, esz) as i128, sx(y, esz) as i128);
        let (xu, yu) = (x as i128, y as i128);
        let (xv, yv) = if u { (xu, yu) } else { (xs, ys) };
        let v: u64 = match opcode {
            0b00000 => ((xv + yv) >> 1) as u64,
            0b00001 => if u { sat_u(xv + yv, esz, &mut fpsr) } else { sat_s(xv + yv, esz, &mut fpsr) },
            0b00010 => ((xv + yv + 1) >> 1) as u64,
            0b00100 => ((xv - yv) >> 1) as u64,
            0b00101 => if u { sat_u(xv - yv, esz, &mut fpsr) } else { sat_s(xv - yv, esz, &mut fpsr) },
            0b00110 => mask_if(xv > yv, esz),
            0b00111 => mask_if(xv >= yv, esz),
            0b01000 | 0b01001 | 0b01010 | 0b01011 => {
                let sh = sext(y & 0xff, 8) as i64;
                let round = opcode & 2 != 0;
                let sat = opcode & 1 != 0;
                shl_reg(x, sh, esz, u, round, sat, &mut fpsr)
            }
            0b01100 => if xv >= yv { x } else { y },
            0b01101 => if xv <= yv { x } else { y },
            0b01110 => (xv - yv).unsigned_abs() as u64,
            0b01111 => dd.wrapping_add((xv - yv).unsigned_abs() as u64),
            0b10000 => if u { x.wrapping_sub(y) } else { x.wrapping_add(y) },
            0b10001 => if u { mask_if(x == y, esz) } else { mask_if(x & y != 0, esz) },
            0b10010 => {
                let p = x.wrapping_mul(y);
                if u { dd.wrapping_sub(p) } else { dd.wrapping_add(p) }
            }
            0b10011 => {
                if u {
                    // PMUL (bytes)
                    if size != 0 {
                        return Err(unimpl(insn));
                    }
                    let mut p = 0u64;
                    for k in 0..8 {
                        if (y >> k) & 1 != 0 {
                            p ^= x << k;
                        }
                    }
                    p
                } else {
                    x.wrapping_mul(y)
                }
            }
            0b10110 => {
                // SQDMULH / SQRDMULH
                let p = 2 * xs * ys + if u { 1i128 << (esz - 1) } else { 0 };
                sat_s(p >> esz, esz, &mut fpsr)
            }
            _ => return Err(unimpl(insn)),
        };
        put(&mut r, esz, i, v);
    }
    cpu.fpsr = fpsr;
    if scalar {
        cpu.set_vq(rd, r & (ones(esz) as u128));
    } else {
        set_vec(cpu, rd, r, q);
    }
    Ok(())
}

fn fp_pairwise(a: u128, b: u128, esz: u32, elems: u32, f32op: impl Fn(f32, f32) -> u64, f64op: impl Fn(f64, f64) -> u64) -> u128 {
    let half = elems / 2;
    let mut r = 0u128;
    for i in 0..elems {
        let (src, j) = if i < half { (a, i * 2) } else { (b, (i - half) * 2) };
        let x = get(src, esz, j);
        let y = get(src, esz, j + 1);
        let v = if esz == 32 {
            f32op(f32::from_bits(x as u32), f32::from_bits(y as u32))
        } else {
            f64op(f64::from_bits(x), f64::from_bits(y))
        };
        put(&mut r, esz, i, v);
    }
    r
}

// ---------------- three same extra (dot product etc.) ----------------

fn three_same_extra(cpu: &mut Cpu, insn: u32) -> R {
    let q = bit(insn, 30) != 0;
    let u = bit(insn, 29) != 0;
    let size = bits(insn, 23, 22);
    let rm = bits(insn, 20, 16);
    let opcode = bits(insn, 14, 11);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let a = cpu.vq(rn);
    let b = cpu.vq(rm);
    let d = cpu.vq(rd);
    let elems = if q { 4 } else { 2 };
    match (opcode, size) {
        (0b0010, 2) => {
            // SDOT / UDOT
            let mut r = 0u128;
            for i in 0..elems {
                let mut acc = get(d, 32, i) as u32;
                for k in 0..4 {
                    let x = get(a, 8, i * 4 + k);
                    let y = get(b, 8, i * 4 + k);
                    let p = if u { (x * y) as u32 } else { ((x as u8 as i8 as i32) * (y as u8 as i8 as i32)) as u32 };
                    acc = acc.wrapping_add(p);
                }
                put(&mut r, 32, i, acc as u64);
            }
            set_vec(cpu, rd, r, q);
            Ok(())
        }
        _ => Err(unimpl(insn)),
    }
}

// ---------------- three different ----------------

fn three_diff(cpu: &mut Cpu, insn: u32, scalar: bool) -> R {
    let q = bit(insn, 30) != 0;
    let u = bit(insn, 29) != 0;
    let size = bits(insn, 23, 22);
    let rm = bits(insn, 20, 16);
    let opcode = bits(insn, 15, 12);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let esz = 8u32 << size; // narrow element size
    if size == 3 && opcode != 0b1110 {
        return Err(unimpl(insn));
    }
    let a = cpu.vq(rn);
    let b = cpu.vq(rm);
    let d = cpu.vq(rd);
    let wsz = esz * 2;
    let half_off = if q { 64 / esz } else { 0 };
    let mut fpsr = cpu.fpsr;
    if scalar {
        // SQDMULL/SQDMLAL/SQDMLSL (scalar)
        let x = sx(get(a, esz, 0), esz) as i128;
        let y = sx(get(b, esz, 0), esz) as i128;
        let p = sat_s(2 * x * y, wsz, &mut fpsr);
        let acc = sx(get(d, wsz, 0), wsz) as i128;
        let v = match opcode {
            0b1101 => p,
            0b1001 => sat_s(acc + sx(p, wsz) as i128, wsz, &mut fpsr),
            0b1011 => sat_s(acc - sx(p, wsz) as i128, wsz, &mut fpsr),
            _ => return Err(unimpl(insn)),
        };
        cpu.fpsr = fpsr;
        cpu.set_vq(rd, v as u128 & ones(wsz) as u128);
        return Ok(());
    }
    let ext = |v: u64, sz: u32| -> i128 { if u { v as i128 } else { sx(v, sz) as i128 } };
    let mut r = 0u128;
    match opcode {
        0b0100 | 0b0110 => {
            // ADDHN / RADDHN / SUBHN / RSUBHN: narrow into half
            let n = 64 / esz;
            let mut out = if q { d } else { 0 };
            for i in 0..n {
                let x = get(a, wsz, i);
                let y = get(b, wsz, i);
                let s = if opcode == 0b0100 { x.wrapping_add(y) } else { x.wrapping_sub(y) };
                let s = if u { s.wrapping_add(1u64 << (esz - 1)) } else { s };
                put(&mut out, esz, i + half_off, (s & ones(wsz)) >> esz);
            }
            set_vec(cpu, rd, out, q);
            return Ok(());
        }
        0b1110 => {
            // PMULL / PMULL2
            if size == 0 {
                for i in 0..8 {
                    let x = get(a, 8, i + half_off);
                    let y = get(b, 8, i + half_off);
                    let mut p = 0u64;
                    for k in 0..8 {
                        if (y >> k) & 1 != 0 {
                            p ^= x << k;
                        }
                    }
                    put(&mut r, 16, i, p);
                }
            } else if size == 3 {
                let x = get(a, 64, if q { 1 } else { 0 }) as u128;
                let y = get(b, 64, if q { 1 } else { 0 }) as u128;
                let mut p = 0u128;
                for k in 0..64 {
                    if (y >> k) & 1 != 0 {
                        p ^= x << k;
                    }
                }
                r = p;
            } else {
                return Err(unimpl(insn));
            }
            cpu.set_vq(rd, r);
            return Ok(());
        }
        _ => {}
    }
    let n = 64 / esz;
    for i in 0..n {
        let wide_a = matches!(opcode, 0b0001 | 0b0011);
        let x = if wide_a { ext(get(a, wsz, i), wsz) } else { ext(get(a, esz, i + half_off), esz) };
        let y = ext(get(b, esz, i + half_off), esz);
        let acc = get(d, wsz, i);
        let v: u64 = match opcode {
            0b0000 | 0b0001 => (x + y) as u64,
            0b0010 | 0b0011 => (x - y) as u64,
            0b0101 => acc.wrapping_add((x - y).unsigned_abs() as u64),
            0b0111 => (x - y).unsigned_abs() as u64,
            0b1000 => acc.wrapping_add((x * y) as u64),
            0b1010 => acc.wrapping_sub((x * y) as u64),
            0b1100 => (x * y) as u64,
            0b1001 | 0b1011 | 0b1101 if !u => {
                let p = sat_s(2 * x * y, wsz, &mut fpsr);
                let accs = sx(acc, wsz) as i128;
                match opcode {
                    0b1101 => p,
                    0b1001 => sat_s(accs + sx(p, wsz) as i128, wsz, &mut fpsr),
                    _ => sat_s(accs - sx(p, wsz) as i128, wsz, &mut fpsr),
                }
            }
            _ => return Err(unimpl(insn)),
        };
        put(&mut r, wsz, i, v);
    }
    cpu.fpsr = fpsr;
    cpu.set_vq(rd, r);
    Ok(())
}

// ---------------- two-register misc ----------------

fn two_misc(cpu: &mut Cpu, insn: u32, scalar: bool) -> R {
    let q = bit(insn, 30) != 0;
    let u = bit(insn, 29) != 0;
    let size = bits(insn, 23, 22);
    let opcode = bits(insn, 16, 12);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let a = cpu.vq(rn);
    let d = cpu.vq(rd);
    let mut fpsr = cpu.fpsr;

    // Floating point group: opcodes 0b01100..0b01111 with size<1>=1, and >= 0b10110 (except narrowing ints)
    let is_fp = (opcode >= 0b11000) || (opcode >= 0b01100 && opcode <= 0b01111 && size >= 2) || opcode == 0b10110 || opcode == 0b10111;
    if is_fp {
        return two_misc_fp(cpu, insn, scalar, q, u, size, opcode, rn, rd);
    }

    let esz = 8u32 << size;
    let elems = if scalar { 1 } else if q { 128 / esz } else { 64 / esz };
    let mut r = 0u128;
    match (u, opcode) {
        (_, 0b00000) | (false, 0b00001) => {
            // REV64 / REV32 / REV16
            if scalar {
                return Err(unimpl(insn));
            }
            let container = match (u, opcode) {
                (false, 0) => 64,
                (true, 0) => 32,
                _ => 16,
            };
            if esz >= container {
                return Err(unimpl(insn));
            }
            let per = container / esz;
            for i in 0..elems {
                let base = (i / per) * per;
                let j = base + (per - 1 - (i - base));
                put(&mut r, esz, i, get(a, esz, j));
            }
        }
        (_, 0b00010) | (_, 0b00110) => {
            // SADDLP/UADDLP, SADALP/UADALP
            if scalar || size == 3 {
                return Err(unimpl(insn));
            }
            let wsz = esz * 2;
            for i in 0..elems / 2 {
                let x = get(a, esz, 2 * i);
                let y = get(a, esz, 2 * i + 1);
                let s = if u { x.wrapping_add(y) } else { (sx(x, esz) + sx(y, esz)) as u64 };
                let v = if opcode == 0b00110 { get(d, wsz, i).wrapping_add(s) } else { s };
                put(&mut r, wsz, i, v);
            }
        }
        (_, 0b00100) => {
            // CLS / CLZ
            if scalar {
                return Err(unimpl(insn));
            }
            for i in 0..elems {
                let x = get(a, esz, i);
                let v = if u {
                    (x.leading_zeros() - (64 - esz)) as u64
                } else {
                    let s = sx(x, esz) as u64;
                    let y = s ^ ((s as i64 >> 1) as u64);
                    (y.leading_zeros() as u64).saturating_sub(1 + (64 - esz as u64))
                };
                put(&mut r, esz, i, v);
            }
        }
        (false, 0b00101) => {
            // CNT
            if size != 0 || scalar {
                return Err(unimpl(insn));
            }
            for i in 0..elems {
                put(&mut r, 8, i, get(a, 8, i).count_ones() as u64);
            }
        }
        (true, 0b00101) => {
            if scalar {
                return Err(unimpl(insn));
            }
            match size {
                0 => r = !a,
                1 => {
                    for i in 0..elems.max(8) {
                        if i >= (if q { 16 } else { 8 }) {
                            break;
                        }
                        put(&mut r, 8, i, (get(a, 8, i) as u8).reverse_bits() as u64);
                    }
                }
                _ => return Err(unimpl(insn)),
            }
        }
        (_, 0b00011) | (_, 0b00111) | (_, 0b01000) | (_, 0b01001) | (false, 0b01010) | (_, 0b01011) => {
            for i in 0..elems {
                let x = get(a, esz, i);
                let xs = sx(x, esz) as i128;
                let v = match (u, opcode) {
                    (false, 0b00011) => sat_s(sx(get(d, esz, i), esz) as i128 + x as i128, esz, &mut fpsr), // SUQADD
                    (true, 0b00011) => sat_u(get(d, esz, i) as i128 + xs, esz, &mut fpsr),                   // USQADD
                    (false, 0b00111) => sat_s(xs.abs(), esz, &mut fpsr),
                    (true, 0b00111) => sat_s(-xs, esz, &mut fpsr),
                    (false, 0b01000) => mask_if(xs > 0, esz),
                    (true, 0b01000) => mask_if(xs >= 0, esz),
                    (false, 0b01001) => mask_if(xs == 0, esz),
                    (true, 0b01001) => mask_if(xs <= 0, esz),
                    (false, 0b01010) => mask_if(xs < 0, esz),
                    (false, 0b01011) => xs.unsigned_abs() as u64,
                    (true, 0b01011) => (x as i64).wrapping_neg() as u64,
                    _ => return Err(unimpl(insn)),
                };
                put(&mut r, esz, i, v);
            }
        }
        (_, 0b10010) | (_, 0b10100) => {
            // XTN / SQXTUN / SQXTN / UQXTN
            if size == 3 {
                return Err(unimpl(insn));
            }
            let wsz = esz * 2;
            let n = if scalar { 1 } else { 64 / esz };
            let off = if q && !scalar { 64 / esz } else { 0 };
            let mut out = if q && !scalar { d } else { 0 };
            for i in 0..n {
                let x = get(a, wsz, i);
                let v = match (u, opcode) {
                    (false, 0b10010) => x & ones(esz),
                    (true, 0b10010) => sat_u(sx(x, wsz) as i128, esz, &mut fpsr),
                    (false, 0b10100) => sat_s(sx(x, wsz) as i128, esz, &mut fpsr),
                    _ => sat_u(x as i128, esz, &mut fpsr),
                };
                put(&mut out, esz, i + off, v);
            }
            cpu.fpsr = fpsr;
            if scalar {
                cpu.set_vq(rd, out & ones(esz) as u128);
            } else {
                set_vec(cpu, rd, out, q);
            }
            return Ok(());
        }
        (true, 0b10011) => {
            // SHLL / SHLL2
            let off = if q { 64 / esz } else { 0 };
            for i in 0..64 / esz {
                put(&mut r, esz * 2, i, get(a, esz, i + off) << esz);
            }
            cpu.set_vq(rd, r);
            return Ok(());
        }
        _ => return Err(unimpl(insn)),
    }
    cpu.fpsr = fpsr;
    if scalar {
        cpu.set_vq(rd, r & ones(esz) as u128);
    } else {
        set_vec(cpu, rd, r, q);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn two_misc_fp(cpu: &mut Cpu, insn: u32, scalar: bool, q: bool, u: bool, size: u32, opcode: u32, rn: u32, rd: u32) -> R {
    let a = cpu.vq(rn);
    let d = cpu.vq(rd);
    let fa = size >> 1;
    let sz = size & 1;
    // FCVTN/FCVTL/FCVTXN (element size changes)
    if opcode == 0b10110 || opcode == 0b10111 {
        if scalar && !(u && opcode == 0b10110) {
            return Err(unimpl(insn));
        }
        let mut r = 0u128;
        if opcode == 0b10111 {
            if u {
                return Err(unimpl(insn));
            }
            // FCVTL / FCVTL2 (half->single or single->double)
            if sz == 0 {
                let off = if q { 4 } else { 0 };
                for i in 0..4 {
                    let h = get(a, 16, i + off) as u16;
                    put(&mut r, 32, i, sf::f16_to_f32(h).to_bits() as u64);
                }
            } else {
                let off = if q { 2 } else { 0 };
                for i in 0..2 {
                    let f = f32::from_bits(get(a, 32, i + off) as u32);
                    let dd = if f.is_nan() { f64::from_bits((f as f64).to_bits() | (1 << 51)) } else { f as f64 };
                    put(&mut r, 64, i, dd.to_bits());
                }
            }
            cpu.set_vq(rd, r);
            return Ok(());
        }
        // FCVTN (U=0) / FCVTXN (U=1)
        let mut out = if q && !scalar { d } else { 0 };
        if sz == 0 {
            if u {
                return Err(unimpl(insn));
            }
            let off = if q { 4 } else { 0 };
            for i in 0..4 {
                let f = f32::from_bits(get(a, 32, i) as u32);
                put(&mut out, 16, i + off, sf::f32_to_f16(f) as u64);
            }
        } else {
            let n = if scalar { 1 } else { 2 };
            let off = if q && !scalar { 2 } else { 0 };
            for i in 0..n {
                let dv = f64::from_bits(get(a, 64, i));
                let f = if u {
                    // round to odd
                    let f = dv as f32;
                    if dv.is_finite() && !f.is_finite() {
                        // round-to-odd never overflows to infinity
                        f32::from_bits(if dv < 0.0 { 0xff7f_ffff } else { 0x7f7f_ffff })
                    } else if (f as f64) != dv && !dv.is_nan() && f.is_finite() {
                        let fb = f.to_bits();
                        // choose the truncated value and force the lsb to 1
                        let t = if (f as f64).abs() > dv.abs() { fb - 1 } else { fb };
                        f32::from_bits(t | 1)
                    } else {
                        f
                    }
                } else {
                    dv as f32
                };
                put(&mut out, 32, i + off, f.to_bits() as u64);
            }
        }
        if scalar {
            cpu.set_vq(rd, out & 0xffff_ffff);
        } else {
            set_vec(cpu, rd, out, q);
        }
        return Ok(());
    }
    let esz = if sz != 0 { 64 } else { 32 };
    if esz == 64 && !q && !scalar {
        return Err(unimpl(insn));
    }
    let elems = if scalar { 1 } else if q { 128 / esz } else { 64 / esz };
    let rmode = fpcr_rmode(cpu);
    let key = ((u as u32) << 6) | (fa << 5) | opcode;
    let r = match key {
        0b0_0_11000 => fp_vec1(a, esz, elems, |x| fbits(frint(x, 0)), |x| fbits(frint(x, 0))),
        0b0_0_11001 => fp_vec1(a, esz, elems, |x| fbits(frint(x, 2)), |x| fbits(frint(x, 2))),
        0b0_1_11000 => fp_vec1(a, esz, elems, |x| fbits(frint(x, 1)), |x| fbits(frint(x, 1))),
        0b0_1_11001 => fp_vec1(a, esz, elems, |x| fbits(frint(x, 3)), |x| fbits(frint(x, 3))),
        0b1_0_11000 => fp_vec1(a, esz, elems, |x| fbits(frint(x, 4)), |x| fbits(frint(x, 4))),
        0b1_0_11001 | 0b1_1_11001 => fp_vec1(a, esz, elems, |x| fbits(frint(x, rmode)), |x| fbits(frint(x, rmode))),
        0b0_0_11110 => fp_vec1(a, esz, elems, |x| fbits(frint_n(x, 3, 32)), |x| fbits(frint_n(x, 3, 32))),
        0b0_0_11111 => fp_vec1(a, esz, elems, |x| fbits(frint_n(x, 3, 64)), |x| fbits(frint_n(x, 3, 64))),
        0b1_0_11110 => fp_vec1(a, esz, elems, |x| fbits(frint_n(x, rmode, 32)), |x| fbits(frint_n(x, rmode, 32))),
        0b1_0_11111 => fp_vec1(a, esz, elems, |x| fbits(frint_n(x, rmode, 64)), |x| fbits(frint_n(x, rmode, 64))),
        // conversions to integer
        0b0_0_11010 | 0b0_0_11011 | 0b0_0_11100 | 0b0_1_11010 | 0b0_1_11011 | 0b1_0_11010 | 0b1_0_11011 | 0b1_0_11100 | 0b1_1_11010 | 0b1_1_11011 => {
            let mode = match (fa, opcode) {
                (0, 0b11010) => 0,
                (0, 0b11011) => 2,
                (0, _) => 4,
                (_, 0b11010) => 1,
                _ => 3,
            };
            let signed = !u;
            fp_vec1(a, esz, elems, |x| fp_to_int(x, mode, signed, false, 0), |x| fp_to_int(x, mode, signed, true, 0))
        }
        0b0_0_11101 | 0b1_0_11101 => {
            let signed = !u;
            fp_vec1(a, esz, elems, |x| int_to_fp::<f32>(x.to_bits() as u64, signed, false, 0).to_bits() as u64, |x| int_to_fp::<f64>(x.to_bits(), signed, true, 0).to_bits())
        }
        0b0_1_01100 => fp_vec1(a, esz, elems, |x| mask_if(x > 0.0, 32), |x| mask_if(x > 0.0, 64)),
        0b0_1_01101 => fp_vec1(a, esz, elems, |x| mask_if(x == 0.0, 32), |x| mask_if(x == 0.0, 64)),
        0b0_1_01110 => fp_vec1(a, esz, elems, |x| mask_if(x < 0.0, 32), |x| mask_if(x < 0.0, 64)),
        0b1_1_01100 => fp_vec1(a, esz, elems, |x| mask_if(x >= 0.0, 32), |x| mask_if(x >= 0.0, 64)),
        0b1_1_01101 => fp_vec1(a, esz, elems, |x| mask_if(x <= 0.0, 32), |x| mask_if(x <= 0.0, 64)),
        0b0_1_01111 => fp_vec1(a, esz, elems, |x| (x.to_bits() & 0x7fff_ffff) as u64, |x| x.to_bits() & 0x7fff_ffff_ffff_ffff),
        0b1_1_01111 => fp_vec1(a, esz, elems, |x| (x.to_bits() ^ 0x8000_0000) as u64, |x| x.to_bits() ^ 0x8000_0000_0000_0000),
        0b1_1_11111 => fp_vec1(a, esz, elems, |x| fbits(sf::fsqrt(x)), |x| fbits(sf::fsqrt(x))),
        0b0_1_11101 => fp_vec1(a, esz, elems, |x| fbits(sf::frecpe(x)), |x| fbits(sf::frecpe(x))),
        0b1_1_11101 => fp_vec1(a, esz, elems, |x| fbits(sf::frsqrte(x)), |x| fbits(sf::frsqrte(x))),
        _ => return Err(unimpl(insn)),
    };
    if scalar {
        cpu.set_vq(rd, r & ones(esz) as u128);
    } else {
        set_vec(cpu, rd, r, q);
    }
    Ok(())
}

// ---------------- across lanes ----------------

fn across(cpu: &mut Cpu, insn: u32) -> R {
    let q = bit(insn, 30) != 0;
    let u = bit(insn, 29) != 0;
    let size = bits(insn, 23, 22);
    let opcode = bits(insn, 16, 12);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let a = cpu.vq(rn);
    if opcode == 0b01100 || opcode == 0b01111 {
        // FMAXNMV / FMINNMV / FMAXV / FMINV (single only here)
        if !u || size & 1 != 0 || !q {
            return Err(unimpl(insn));
        }
        let min = size & 2 != 0;
        let f = |i| f32::from_bits(get(a, 32, i) as u32);
        let op = |x: f32, y: f32| match (opcode, min) {
            (0b01100, false) => sf::fmaxnm(x, y),
            (0b01100, true) => sf::fminnm(x, y),
            (_, false) => sf::fmax(x, y),
            _ => sf::fmin(x, y),
        };
        let r = op(op(f(0), f(1)), op(f(2), f(3)));
        cpu.set_vq(rd, r.to_bits() as u128);
        return Ok(());
    }
    let esz = 8u32 << size;
    let elems = if q { 128 / esz } else { 64 / esz };
    if size == 3 || (size == 2 && !q) {
        return Err(unimpl(insn));
    }
    let r: u64 = match opcode {
        0b00011 => {
            // SADDLV / UADDLV
            let mut s: i128 = 0;
            for i in 0..elems {
                let x = get(a, esz, i);
                s += if u { x as i128 } else { sx(x, esz) as i128 };
            }
            (s as u64) & ones(esz * 2)
        }
        0b01010 | 0b11010 => {
            let mut best = get(a, esz, 0);
            for i in 1..elems {
                let x = get(a, esz, i);
                let better = match (u, opcode) {
                    (true, 0b01010) => x > best,
                    (true, _) => x < best,
                    (false, 0b01010) => sx(x, esz) > sx(best, esz),
                    (false, _) => sx(x, esz) < sx(best, esz),
                };
                if better {
                    best = x;
                }
            }
            best
        }
        0b11011 if !u => {
            let mut s = 0u64;
            for i in 0..elems {
                s = s.wrapping_add(get(a, esz, i));
            }
            s & ones(esz)
        }
        _ => return Err(unimpl(insn)),
    };
    cpu.set_vq(rd, r as u128);
    Ok(())
}

fn scalar_pairwise(cpu: &mut Cpu, insn: u32) -> R {
    let u = bit(insn, 29) != 0;
    let size = bits(insn, 23, 22);
    let opcode = bits(insn, 16, 12);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let a = cpu.vq(rn);
    if !u {
        if opcode == 0b11011 && size == 3 {
            cpu.set_vq(rd, (get(a, 64, 0).wrapping_add(get(a, 64, 1))) as u128);
            return Ok(());
        }
        return Err(unimpl(insn));
    }
    let sz = size & 1;
    let min = size & 2 != 0;
    macro_rules! go {
        ($t:ty, $esz:expr) => {{
            let x = <$t>::from_bits64(get(a, $esz, 0));
            let y = <$t>::from_bits64(get(a, $esz, 1));
            let r: $t = match (opcode, min) {
                (0b01100, false) => sf::fmaxnm(x, y),
                (0b01100, true) => sf::fminnm(x, y),
                (0b01101, false) => sf::fadd(x, y),
                (0b01111, false) => sf::fmax(x, y),
                (0b01111, true) => sf::fmin(x, y),
                _ => return Err(unimpl(insn)),
            };
            cpu.set_vq(rd, r.to_bits64() as u128);
        }};
    }
    if sz == 0 {
        go!(f32, 32)
    } else {
        go!(f64, 64)
    }
    Ok(())
}

// ---------------- copy (DUP/INS/UMOV/SMOV) ----------------

fn copy(cpu: &mut Cpu, insn: u32, scalar: bool) -> R {
    let q = bit(insn, 30) != 0;
    let op = bit(insn, 29);
    let imm5 = bits(insn, 20, 16);
    let imm4 = bits(insn, 14, 11);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    if imm5 & 0xf == 0 {
        return Err(unimpl(insn));
    }
    let size = imm5.trailing_zeros();
    let esz = 8u32 << size;
    let idx = imm5 >> (size + 1);
    if scalar {
        // DUP (element), scalar form
        if op != 0 || imm4 != 0 {
            return Err(unimpl(insn));
        }
        let v = get(cpu.vq(rn), esz, idx);
        cpu.set_vq(rd, v as u128);
        return Ok(());
    }
    if op == 1 {
        // INS (element)
        let src_idx = imm4 >> size;
        let v = get(cpu.vq(rn), esz, src_idx);
        let mut d = cpu.vq(rd);
        put(&mut d, esz, idx, v);
        cpu.set_vq(rd, d);
        return Ok(());
    }
    match imm4 {
        0b0000 => {
            let v = get(cpu.vq(rn), esz, idx);
            let mut r = 0u128;
            for i in 0..(128 / esz) {
                put(&mut r, esz, i, v);
            }
            set_vec(cpu, rd, r, q);
        }
        0b0001 => {
            let v = cpu.xr(rn);
            let mut r = 0u128;
            for i in 0..(128 / esz) {
                put(&mut r, esz, i, v);
            }
            set_vec(cpu, rd, r, q);
        }
        0b0011 => {
            let v = cpu.xr(rn);
            let mut d = cpu.vq(rd);
            put(&mut d, esz, idx, v);
            cpu.set_vq(rd, d);
        }
        0b0101 => {
            let v = sext(get(cpu.vq(rn), esz, idx), esz);
            cpu.setx(rd, if q { v } else { v as u32 as u64 });
        }
        0b0111 => {
            let v = get(cpu.vq(rn), esz, idx);
            cpu.setx(rd, v);
        }
        _ => return Err(unimpl(insn)),
    }
    Ok(())
}

// ---------------- permute / ext / tbl ----------------

fn permute(cpu: &mut Cpu, insn: u32) -> R {
    let q = bit(insn, 30) != 0;
    let size = bits(insn, 23, 22);
    let rm = bits(insn, 20, 16);
    let opcode = bits(insn, 14, 12);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let esz = 8u32 << size;
    if size == 3 && !q {
        return Err(unimpl(insn));
    }
    let elems = if q { 128 / esz } else { 64 / esz };
    let a = cpu.vq(rn);
    let b = cpu.vq(rm);
    let half = elems / 2;
    let mut r = 0u128;
    let part = (opcode >> 2) & 1;
    match opcode & 3 {
        1 => {
            // UZP1/UZP2
            for i in 0..elems {
                let j = 2 * i + part;
                let v = if j < elems { get(a, esz, j) } else { get(b, esz, j - elems) };
                put(&mut r, esz, i, v);
            }
        }
        2 => {
            // TRN1/TRN2
            for p in 0..half {
                put(&mut r, esz, 2 * p, get(a, esz, 2 * p + part));
                put(&mut r, esz, 2 * p + 1, get(b, esz, 2 * p + part));
            }
        }
        3 => {
            // ZIP1/ZIP2
            let base = part * half;
            for p in 0..half {
                put(&mut r, esz, 2 * p, get(a, esz, base + p));
                put(&mut r, esz, 2 * p + 1, get(b, esz, base + p));
            }
        }
        _ => return Err(unimpl(insn)),
    }
    set_vec(cpu, rd, r, q);
    Ok(())
}

fn ext(cpu: &mut Cpu, insn: u32) -> R {
    let q = bit(insn, 30) != 0;
    let rm = bits(insn, 20, 16);
    let imm4 = bits(insn, 14, 11);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let a = cpu.vq(rn);
    let b = cpu.vq(rm);
    let r = if q {
        if imm4 == 0 { a } else { (a >> (imm4 * 8)) | (b << (128 - imm4 * 8)) }
    } else {
        if imm4 >= 8 {
            return Err(unimpl(insn));
        }
        let lo = a & u64::MAX as u128;
        let hi = b & u64::MAX as u128;
        let cat = lo | (hi << 64);
        (cat >> (imm4 * 8)) & u64::MAX as u128
    };
    set_vec(cpu, rd, r, q);
    Ok(())
}

fn tbl(cpu: &mut Cpu, insn: u32) -> R {
    let q = bit(insn, 30) != 0;
    let rm = bits(insn, 20, 16);
    let len = bits(insn, 14, 13) + 1;
    let tbx = bit(insn, 12) != 0;
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let mut table = [0u8; 64];
    for k in 0..len {
        let v = cpu.vq((rn + k) % 32);
        table[(k * 16) as usize..(k * 16 + 16) as usize].copy_from_slice(&v.to_le_bytes());
    }
    let idx = cpu.vq(rm);
    let d = cpu.vq(rd);
    let n = if q { 16 } else { 8 };
    let mut r = 0u128;
    for i in 0..n {
        let ix = get(idx, 8, i) as u32;
        let v = if ix < len * 16 { table[ix as usize] as u64 } else if tbx { get(d, 8, i) } else { 0 };
        put(&mut r, 8, i, v);
    }
    set_vec(cpu, rd, r, q);
    Ok(())
}

// ---------------- modified immediate ----------------

fn mod_imm(cpu: &mut Cpu, insn: u32) -> R {
    let q = bit(insn, 30) != 0;
    let op = bit(insn, 29);
    let cmode = bits(insn, 15, 12);
    let o2 = bit(insn, 11);
    let rd = bits(insn, 4, 0);
    let imm8 = (bits(insn, 18, 16) << 5) | bits(insn, 9, 5);
    if o2 != 0 {
        if cmode == 0b1111 && op == 0 {
            // FMOV (vector, half)
            let h = sf::f64_to_f16(f64::from_bits(vfp_expand_imm(imm8))) as u64;
            let v = h * 0x0001_0001_0001_0001;
            let r = (v as u128) | ((v as u128) << 64);
            set_vec(cpu, rd, r, q);
            return Ok(());
        }
        return Err(unimpl(insn));
    }
    if cmode == 0b1111 && op == 1 && !q {
        return Err(unimpl(insn));
    }
    let imm = adv_simd_expand_imm(op, cmode, imm8);
    let imm128 = (imm as u128) | ((imm as u128) << 64);
    let d = cpu.vq(rd);
    let r = match (cmode, op) {
        (c, 0) if c & 0b1001 == 0b0001 || c & 0b1101 == 0b1001 => d | imm128, // ORR
        (c, 1) if c & 0b1001 == 0b0001 || c & 0b1101 == 0b1001 => d & !imm128, // BIC
        (c, 1) if c < 0b1110 => !imm128, // MVNI
        _ => imm128, // MOVI / FMOV
    };
    set_vec(cpu, rd, r, q);
    Ok(())
}

// ---------------- shift by immediate ----------------

fn shift_imm(cpu: &mut Cpu, insn: u32, scalar: bool) -> R {
    let q = bit(insn, 30) != 0;
    let u = bit(insn, 29) != 0;
    let immh = bits(insn, 22, 19);
    let immb = bits(insn, 18, 16);
    let opcode = bits(insn, 15, 11);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let hsb = 31 - immh.leading_zeros();
    let esz = 8u32 << hsb;
    let immhb = (immh << 3) | immb;
    let shr = 2 * esz - immhb;
    let shl = immhb - esz;
    let a = cpu.vq(rn);
    let d = cpu.vq(rd);
    let mut fpsr = cpu.fpsr;

    if matches!(opcode, 0b10000 | 0b10001 | 0b10010 | 0b10011) {
        // narrowing shifts: element size = esz (dest), source 2*esz
        if esz == 64 {
            return Err(unimpl(insn));
        }
        let wsz = esz * 2;
        let shr = 2 * esz - immhb; // shift for narrowing = (2*esize) - immh:immb with esize=dest
        let n = if scalar { 1 } else { 64 / esz };
        let off = if q && !scalar { 64 / esz } else { 0 };
        let mut out = if q && !scalar { d } else { 0 };
        for i in 0..n {
            let x = get(a, wsz, i);
            let round = opcode & 1 != 0;
            let rnd = if round { 1i128 << (shr - 1) } else { 0 };
            let v = match (u, opcode) {
                (false, 0b10000) | (false, 0b10001) => {
                    if scalar {
                        return Err(unimpl(insn));
                    }
                    (((x as i128 + rnd) >> shr) as u64) & ones(esz)
                }
                (true, 0b10000) | (true, 0b10001) => sat_u((sx(x, wsz) as i128 + rnd) >> shr, esz, &mut fpsr), // SQSHRUN
                (false, _) => sat_s((sx(x, wsz) as i128 + rnd) >> shr, esz, &mut fpsr),                      // SQSHRN
                (true, _) => sat_u((x as i128 + rnd) >> shr, esz, &mut fpsr),                                // UQSHRN
            };
            put(&mut out, esz, i + off, v);
        }
        cpu.fpsr = fpsr;
        if scalar {
            cpu.set_vq(rd, out & ones(esz) as u128);
        } else {
            set_vec(cpu, rd, out, q);
        }
        return Ok(());
    }
    if opcode == 0b10100 {
        // SSHLL / USHLL (and SXTL/UXTL)
        if esz == 64 || scalar {
            return Err(unimpl(insn));
        }
        let off = if q { 64 / esz } else { 0 };
        let mut r = 0u128;
        for i in 0..64 / esz {
            let x = get(a, esz, i + off);
            let v = if u { x << shl } else { (sx(x, esz) << shl) as u64 };
            put(&mut r, esz * 2, i, v);
        }
        cpu.set_vq(rd, r);
        return Ok(());
    }
    if opcode == 0b11100 || opcode == 0b11111 {
        // fixed-point conversions
        if esz < 32 {
            return Err(unimpl(insn));
        }
        let fb = shr;
        let elems = if scalar { 1 } else if q { 128 / esz } else { 64 / esz };
        let r = if opcode == 0b11100 {
            fp_vec1(a, esz, elems, |x| int_to_fp::<f32>(x.to_bits() as u64, !u, false, fb).to_bits() as u64, |x| int_to_fp::<f64>(x.to_bits(), !u, true, fb).to_bits())
        } else {
            fp_vec1(a, esz, elems, |x| fp_to_int(x, 3, !u, false, fb), |x| fp_to_int(x, 3, !u, true, fb))
        };
        if scalar {
            cpu.set_vq(rd, r & ones(esz) as u128);
        } else {
            set_vec(cpu, rd, r, q);
        }
        return Ok(());
    }
    if esz == 64 && !q && !scalar {
        return Err(unimpl(insn));
    }
    if scalar && esz != 64 && !matches!(opcode, 0b01110 | 0b01100) {
        return Err(unimpl(insn));
    }
    let elems = if scalar { 1 } else if q { 128 / esz } else { 64 / esz };
    let mut r = 0u128;
    for i in 0..elems {
        let x = get(a, esz, i);
        let dd = get(d, esz, i);
        let xv: i128 = if u { x as i128 } else { sx(x, esz) as i128 };
        let v = match opcode {
            0b00000 | 0b00010 | 0b00100 | 0b00110 => {
                let round = opcode & 0b100 != 0;
                let rnd = if round { 1i128 << (shr - 1) } else { 0 };
                let s = ((xv + rnd) >> shr) as u64;
                if opcode & 0b010 != 0 { dd.wrapping_add(s) } else { s }
            }
            0b01000 if u => {
                // SRI
                if shr == esz {
                    dd
                } else {
                    let m = ones(esz) >> shr;
                    (dd & !m) | (x >> shr)
                }
            }
            0b01010 => {
                if u {
                    // SLI
                    let m = (ones(esz) << shl) & ones(esz);
                    (dd & !m) | ((x << shl) & m)
                } else {
                    x << shl
                }
            }
            0b01100 if u => sat_u((sx(x, esz) as i128) << shl, esz, &mut fpsr), // SQSHLU
            0b01110 => if u { sat_u((x as i128) << shl, esz, &mut fpsr) } else { sat_s(xv << shl, esz, &mut fpsr) },
            _ => return Err(unimpl(insn)),
        };
        put(&mut r, esz, i, v);
    }
    cpu.fpsr = fpsr;
    if scalar {
        cpu.set_vq(rd, r & ones(esz) as u128);
    } else {
        set_vec(cpu, rd, r, q);
    }
    Ok(())
}

// ---------------- by element ----------------

fn indexed(cpu: &mut Cpu, insn: u32, scalar: bool) -> R {
    let q = bit(insn, 30) != 0;
    let u = bit(insn, 29) != 0;
    let size = bits(insn, 23, 22);
    let l = bit(insn, 21);
    let m = bit(insn, 20);
    let rm4 = bits(insn, 19, 16);
    let opcode = bits(insn, 15, 12);
    let h = bit(insn, 11);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let a = cpu.vq(rn);
    let d = cpu.vq(rd);
    let is_fp = matches!(opcode, 0b0001 | 0b0101 | 0b1001) && size >= 2;
    if is_fp {
        let sz = size & 1;
        let (esz, idx, rm) = if sz == 0 { (32, (h << 1) | l, (m << 4) | rm4) } else {
            if l != 0 {
                return Err(unimpl(insn));
            }
            (64, h, (m << 4) | rm4)
        };
        let b = get(cpu.vq(rm), esz, idx);
        let elems = if scalar { 1 } else if q { 128 / esz } else { 64 / esz };
        let mut r = 0u128;
        for i in 0..elems {
            let x = get(a, esz, i);
            let acc = get(d, esz, i);
            let v = if esz == 32 {
                let (xf, bf, af) = (f32::from_bits(x as u32), f32::from_bits(b as u32), f32::from_bits(acc as u32));
                fbits(match (opcode, u) {
                    (0b0001, false) => sf::fmla(af, xf, bf),
                    (0b0101, false) => sf::fmla(af, -xf, bf),
                    (0b1001, false) => sf::fmul(xf, bf),
                    (0b1001, true) => sf::fmulx(xf, bf),
                    _ => return Err(unimpl(insn)),
                })
            } else {
                let (xf, bf, af) = (f64::from_bits(x), f64::from_bits(b), f64::from_bits(acc));
                fbits(match (opcode, u) {
                    (0b0001, false) => sf::fmla(af, xf, bf),
                    (0b0101, false) => sf::fmla(af, -xf, bf),
                    (0b1001, false) => sf::fmul(xf, bf),
                    (0b1001, true) => sf::fmulx(xf, bf),
                    _ => return Err(unimpl(insn)),
                })
            };
            put(&mut r, esz, i, v);
        }
        if scalar {
            cpu.set_vq(rd, r & ones(esz) as u128);
        } else {
            set_vec(cpu, rd, r, q);
        }
        return Ok(());
    }
    // integer
    let (esz, idx, rm) = match size {
        1 => (16u32, (h << 2) | (l << 1) | m, rm4),
        2 => (32u32, (h << 1) | l, (m << 4) | rm4),
        _ => return Err(unimpl(insn)),
    };
    let b = get(cpu.vq(rm), esz, idx);
    let bs = sx(b, esz) as i128;
    let mut fpsr = cpu.fpsr;
    let mut r = 0u128;
    match (opcode, u) {
        (0b1000, false) | (0b0000, true) | (0b0100, true) | (0b1100, false) | (0b1101, false) | (0b1110, _) => {
            let elems = if scalar { 1 } else if q { 128 / esz } else { 64 / esz };
            if opcode == 0b1110 {
                // SDOT/UDOT by element (size must be 2, bytes)
                if size != 2 || scalar {
                    return Err(unimpl(insn));
                }
                let bw = get(cpu.vq(rm), 32, idx);
                for i in 0..elems {
                    let mut acc = get(d, 32, i) as u32;
                    for k in 0..4 {
                        let x = get(a, 8, i * 4 + k);
                        let y = (bw >> (k * 8)) & 0xff;
                        let p = if u { (x * y) as u32 } else { ((x as u8 as i8 as i32) * (y as u8 as i8 as i32)) as u32 };
                        acc = acc.wrapping_add(p);
                    }
                    put(&mut r, 32, i, acc as u64);
                }
                set_vec(cpu, rd, r, q);
                return Ok(());
            }
            for i in 0..elems {
                let x = get(a, esz, i);
                let acc = get(d, esz, i);
                let v = match opcode {
                    0b1000 => x.wrapping_mul(b),
                    0b0000 => acc.wrapping_add(x.wrapping_mul(b)),
                    0b0100 => acc.wrapping_sub(x.wrapping_mul(b)),
                    0b1100 => sat_s((2 * sx(x, esz) as i128 * bs) >> esz, esz, &mut fpsr),
                    _ => sat_s((2 * sx(x, esz) as i128 * bs + (1i128 << (esz - 1))) >> esz, esz, &mut fpsr),
                };
                put(&mut r, esz, i, v);
            }
            cpu.fpsr = fpsr;
            if scalar {
                cpu.set_vq(rd, r & ones(esz) as u128);
            } else {
                set_vec(cpu, rd, r, q);
            }
        }
        (0b0010, _) | (0b0110, _) | (0b1010, _) | (0b0011, false) | (0b0111, false) | (0b1011, false) => {
            // long forms: SMLAL/UMLAL, SMLSL/UMLSL, SMULL/UMULL, SQDMLAL, SQDMLSL, SQDMULL
            let wsz = esz * 2;
            let off = if q && !scalar { 64 / esz } else { 0 };
            let n = if scalar { 1 } else { 64 / esz };
            for i in 0..n {
                let x = get(a, esz, i + off);
                let acc = get(d, wsz, i);
                let (xv, bv) = if u { (x as i128, b as i128) } else { (sx(x, esz) as i128, bs) };
                let p = xv * bv;
                let v = match opcode {
                    0b0010 => acc.wrapping_add(p as u64),
                    0b0110 => acc.wrapping_sub(p as u64),
                    0b1010 => p as u64,
                    _ => {
                        let dp = sat_s(2 * p, wsz, &mut fpsr);
                        let accs = sx(acc, wsz) as i128;
                        match opcode {
                            0b1011 => dp,
                            0b0011 => sat_s(accs + sx(dp, wsz) as i128, wsz, &mut fpsr),
                            _ => sat_s(accs - sx(dp, wsz) as i128, wsz, &mut fpsr),
                        }
                    }
                };
                put(&mut r, wsz, i, v);
            }
            cpu.fpsr = fpsr;
            if scalar {
                cpu.set_vq(rd, r & ones(wsz) as u128);
            } else {
                cpu.set_vq(rd, r);
            }
        }
        _ => return Err(unimpl(insn)),
    }
    Ok(())
}

// ---------------- structure loads/stores ----------------

pub(crate) fn ld_st_multiple(cpu: &mut Cpu, insn: u32) -> R {
    let q = bit(insn, 30) != 0;
    let post = bit(insn, 23) != 0;
    let load = bit(insn, 22) != 0;
    let rm = bits(insn, 20, 16);
    let opcode = bits(insn, 15, 12);
    let size = bits(insn, 11, 10);
    let rn = bits(insn, 9, 5);
    let rt = bits(insn, 4, 0);
    let (rpt, selem) = match opcode {
        0b0000 => (1, 4),
        0b0010 => (4, 1),
        0b0100 => (1, 3),
        0b0110 => (3, 1),
        0b0111 => (1, 1),
        0b1000 => (1, 2),
        0b1010 => (2, 1),
        _ => return Err(unimpl(insn)),
    };
    if size == 3 && !q && selem != 1 {
        return Err(unimpl(insn));
    }
    let esz = 8u32 << size;
    let ebytes = (esz / 8) as u64;
    let elems = if q { 128 / esz } else { 64 / esz };
    let base = cpu.xs(rn);
    let mut addr = base;
    unsafe {
        if selem == 1 {
            // Whole-register fast path
            for r in 0..rpt {
                let t = (rt + r) % 32;
                if load {
                    let v = if q { r128(addr) } else { r64(addr) as u128 };
                    cpu.set_vq(t, v);
                } else if q {
                    w128(addr, cpu.vq(t));
                } else {
                    w64(addr, cpu.vd(t));
                }
                addr += if q { 16 } else { 8 };
            }
        } else {
            let mut regs: Vec<u128> = (0..selem).map(|s| if load { 0 } else { cpu.vq((rt + s) % 32) }).collect();
            for e in 0..elems {
                for s in 0..selem as usize {
                    if load {
                        let v = read_sized(addr, ebytes as u32);
                        put(&mut regs[s], esz, e, v);
                    } else {
                        write_sized(addr, ebytes as u32, get(regs[s], esz, e));
                    }
                    addr += ebytes;
                }
            }
            if load {
                for s in 0..selem {
                    set_vec(cpu, (rt + s) % 32, regs[s as usize], q);
                }
            }
        }
    }
    if post {
        let inc = if rm == 31 { addr - base } else { cpu.xr(rm) };
        cpu.setxs(rn, base.wrapping_add(inc));
    }
    Ok(())
}

pub(crate) fn ld_st_single(cpu: &mut Cpu, insn: u32) -> R {
    let q = bit(insn, 30);
    let post = bit(insn, 23) != 0;
    let load = bit(insn, 22) != 0;
    let r = bit(insn, 21);
    let rm = bits(insn, 20, 16);
    let opcode = bits(insn, 15, 13);
    let s = bit(insn, 12);
    let size = bits(insn, 11, 10);
    let rn = bits(insn, 9, 5);
    let rt = bits(insn, 4, 0);
    let selem = ((opcode & 1) << 1 | r) + 1;
    let mut scale = opcode >> 1;
    let base = cpu.xs(rn);
    let mut addr = base;
    let replicate = scale == 3;
    let index;
    match scale {
        0 => index = (q << 3) | (s << 2) | size,
        1 => {
            if size & 1 != 0 {
                return Err(unimpl(insn));
            }
            index = (q << 2) | (s << 1) | (size >> 1);
        }
        2 => {
            if size & 2 != 0 {
                return Err(unimpl(insn));
            }
            if size & 1 == 0 {
                index = (q << 1) | s;
            } else {
                if s != 0 {
                    return Err(unimpl(insn));
                }
                index = q;
                scale = 3;
            }
        }
        _ => {
            if !load || s != 0 {
                return Err(unimpl(insn));
            }
            scale = size;
            index = 0;
        }
    }
    let esz = 8u32 << scale;
    let eb = esz / 8;
    unsafe {
        for k in 0..selem {
            let t = (rt + k) % 32;
            if replicate {
                let v = read_sized(addr, eb);
                let mut out = 0u128;
                for i in 0..(128 / esz) {
                    put(&mut out, esz, i, v);
                }
                set_vec(cpu, t, out, q != 0);
            } else if load {
                let v = read_sized(addr, eb);
                let mut cur = cpu.vq(t);
                put(&mut cur, esz, index, v);
                cpu.set_vq(t, cur);
            } else {
                write_sized(addr, eb, get(cpu.vq(t), esz, index));
            }
            addr += eb as u64;
        }
    }
    if post {
        let inc = if rm == 31 { addr - base } else { cpu.xr(rm) };
        cpu.setxs(rn, base.wrapping_add(inc));
    }
    Ok(())
}

// ---------------- crypto ----------------

const SBOX: [u8; 256] = {
    let mut sbox = [0u8; 256];
    // Generate the AES S-box at compile time.
    let mut p: u8 = 1;
    let mut qv: u8 = 1;
    loop {
        // multiply p by 3
        p = p ^ (p << 1) ^ (if p & 0x80 != 0 { 0x1B } else { 0 });
        // divide q by 3
        qv ^= qv << 1;
        qv ^= qv << 2;
        qv ^= qv << 4;
        if qv & 0x80 != 0 {
            qv ^= 0x09;
        }
        let x = qv ^ qv.rotate_left(1) ^ qv.rotate_left(2) ^ qv.rotate_left(3) ^ qv.rotate_left(4);
        sbox[p as usize] = x ^ 0x63;
        if p == 1 {
            break;
        }
    }
    sbox[0] = 0x63;
    sbox
};

const INV_SBOX: [u8; 256] = {
    let mut inv = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        inv[SBOX[i] as usize] = i as u8;
        i += 1;
    }
    inv
};

fn xtime(b: u8) -> u8 {
    (b << 1) ^ if b & 0x80 != 0 { 0x1b } else { 0 }
}
fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    p
}

fn crypto_aes(cpu: &mut Cpu, insn: u32) -> R {
    let opcode = bits(insn, 16, 12);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let mut st = cpu.vq(rd).to_le_bytes();
    let key = cpu.vq(rn).to_le_bytes();
    match opcode {
        0b00100 | 0b00101 => {
            // AESE / AESD: AddRoundKey, (Inv)ShiftRows, (Inv)SubBytes
            for i in 0..16 {
                st[i] ^= key[i];
            }
            let mut out = [0u8; 16];
            let dec = opcode == 0b00101;
            for c in 0..4 {
                for r in 0..4 {
                    let src_c = if dec { (c + 4 - r) % 4 } else { (c + r) % 4 };
                    let b = st[src_c * 4 + r];
                    out[c * 4 + r] = if dec { INV_SBOX[b as usize] } else { SBOX[b as usize] };
                }
            }
            st = out;
        }
        0b00110 | 0b00111 => {
            // AESMC / AESIMC (source is Vn, not Vd)
            st = key;
            let inv = opcode == 0b00111;
            let mut out = [0u8; 16];
            for c in 0..4 {
                let col = [st[c * 4], st[c * 4 + 1], st[c * 4 + 2], st[c * 4 + 3]];
                for r in 0..4 {
                    out[c * 4 + r] = if inv {
                        gmul(col[r], 14) ^ gmul(col[(r + 1) % 4], 11) ^ gmul(col[(r + 2) % 4], 13) ^ gmul(col[(r + 3) % 4], 9)
                    } else {
                        gmul(col[r], 2) ^ gmul(col[(r + 1) % 4], 3) ^ col[(r + 2) % 4] ^ col[(r + 3) % 4]
                    };
                }
            }
            st = out;
        }
        _ => return Err(unimpl(insn)),
    }
    cpu.set_vq(rd, u128::from_le_bytes(st));
    Ok(())
}

fn sha_ch(x: u32, y: u32, z: u32) -> u32 {
    (x & y) | (!x & z)
}
fn sha_maj(x: u32, y: u32, z: u32) -> u32 {
    (x & y) | (x & z) | (y & z)
}
fn sha_par(x: u32, y: u32, z: u32) -> u32 {
    x ^ y ^ z
}

fn crypto_sha3(cpu: &mut Cpu, insn: u32) -> R {
    let rm = bits(insn, 20, 16);
    let opcode = bits(insn, 14, 12);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let d = cpu.vq(rd);
    let n = cpu.vq(rn);
    let m = cpu.vq(rm);
    let w = |v: u128, i: u32| get(v, 32, i) as u32;
    let r: u128 = match opcode {
        0..=2 => {
            // SHA1C / SHA1P / SHA1M
            let mut x = [w(d, 0), w(d, 1), w(d, 2), w(d, 3)];
            let mut y = w(n, 0);
            for i in 0..4 {
                let t = match opcode {
                    0 => sha_ch(x[1], x[2], x[3]),
                    1 => sha_par(x[1], x[2], x[3]),
                    _ => sha_maj(x[1], x[2], x[3]),
                };
                y = y.wrapping_add(x[0].rotate_left(5)).wrapping_add(t).wrapping_add(w(m, i));
                x[1] = x[1].rotate_left(30);
                let ny = x[3];
                x[3] = x[2];
                x[2] = x[1];
                x[1] = x[0];
                x[0] = y;
                y = ny;
            }
            (x[0] as u128) | ((x[1] as u128) << 32) | ((x[2] as u128) << 64) | ((x[3] as u128) << 96)
        }
        3 => {
            // SHA1SU0
            let r = ((n & u64::MAX as u128) << 64) | (d >> 64);
            r ^ d ^ m
        }
        4 | 5 => {
            // SHA256H (X=Vd, Y=Vn) / SHA256H2 (X=Vn, Y=Vd)
            let (x, y) = if opcode == 4 { (d, n) } else { (n, d) };
            let (x, y) = sha256_hash(x, y, m);
            if opcode == 4 { x } else { y }
        }
        6 => {
            // SHA256SU1
            let t0 = |e: u32| if e < 3 { w(n, e + 1) } else { w(m, 0) };
            let sig1 = |x: u32| x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10);
            let r0 = sig1(w(m, 2)).wrapping_add(w(d, 0)).wrapping_add(t0(0));
            let r1 = sig1(w(m, 3)).wrapping_add(w(d, 1)).wrapping_add(t0(1));
            let r2 = sig1(r0).wrapping_add(w(d, 2)).wrapping_add(t0(2));
            let r3 = sig1(r1).wrapping_add(w(d, 3)).wrapping_add(t0(3));
            (r0 as u128) | ((r1 as u128) << 32) | ((r2 as u128) << 64) | ((r3 as u128) << 96)
        }
        _ => return Err(unimpl(insn)),
    };
    cpu.set_vq(rd, r);
    Ok(())
}

fn sha256_hash(mut x: u128, mut y: u128, w: u128) -> (u128, u128) {
    let el = |v: u128, i: u32| (v >> (i * 32)) as u32;
    for e in 0..4 {
        let chs = sha_ch(el(y, 0), el(y, 1), el(y, 2));
        let maj = sha_maj(el(x, 0), el(x, 1), el(x, 2));
        let s1 = el(y, 0).rotate_right(6) ^ el(y, 0).rotate_right(11) ^ el(y, 0).rotate_right(25);
        let t = el(y, 3).wrapping_add(s1).wrapping_add(chs).wrapping_add(el(w, e));
        let x3 = t.wrapping_add(el(x, 3));
        let s0 = el(x, 0).rotate_right(2) ^ el(x, 0).rotate_right(13) ^ el(x, 0).rotate_right(22);
        let y3 = t.wrapping_add(s0).wrapping_add(maj);
        x = (x & !(0xffff_ffffu128 << 96)) | ((x3 as u128) << 96);
        y = (y & !(0xffff_ffffu128 << 96)) | ((y3 as u128) << 96);
        // ROL(Y:X, 32)
        let nx = (x << 32) | (y >> 96);
        let ny = (y << 32) | (x >> 96);
        x = nx;
        y = ny;
    }
    (x, y)
}

/// SHA-512 and SHA-3 instructions (0xCE.. encodings).
fn crypto_ce(cpu: &mut Cpu, insn: u32) -> R {
    let rm = bits(insn, 20, 16);
    let ra = bits(insn, 14, 10);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let n = cpu.vq(rn);
    let m = cpu.vq(rm);
    let d = cpu.vq(rd);
    let lo = |v: u128| v as u64;
    let hi = |v: u128| (v >> 64) as u64;
    let mk = |l: u64, h: u64| (l as u128) | ((h as u128) << 64);
    let r = match bits(insn, 23, 21) {
        0b000 if bit(insn, 15) == 0 => n ^ m ^ cpu.vq(ra), // EOR3
        0b001 if bit(insn, 15) == 0 => n ^ (m & !cpu.vq(ra)), // BCAX
        0b100 => {
            // XAR
            let imm = bits(insn, 15, 10);
            let t = n ^ m;
            mk(lo(t).rotate_right(imm), hi(t).rotate_right(imm))
        }
        0b011 => match bits(insn, 15, 10) {
            0b100000 => {
                // SHA512H: X=Vn, Y=Vm, W=Vd
                let (x, y, w) = (n, m, d);
                let sig1 = |v: u64| v.rotate_right(14) ^ v.rotate_right(18) ^ v.rotate_right(41);
                let vh = ((hi(y) & lo(x)) ^ (!hi(y) & hi(x))).wrapping_add(sig1(hi(y))).wrapping_add(hi(w));
                let tmp = vh.wrapping_add(lo(y));
                let vl = ((tmp & hi(y)) ^ (!tmp & lo(x))).wrapping_add(sig1(tmp)).wrapping_add(lo(w));
                mk(vl, vh)
            }
            0b100001 => {
                // SHA512H2: X=Vn, Y=Vm, W=Vd
                let (x, y, w) = (n, m, d);
                let sig0 = |v: u64| v.rotate_right(28) ^ v.rotate_right(34) ^ v.rotate_right(39);
                let vh = ((lo(x) & hi(y)) ^ (lo(x) & lo(y)) ^ (hi(y) & lo(y))).wrapping_add(sig0(lo(y))).wrapping_add(hi(w));
                let vl = ((vh & lo(y)) ^ (vh & hi(y)) ^ (hi(y) & lo(y))).wrapping_add(sig0(vh)).wrapping_add(lo(w));
                mk(vl, vh)
            }
            0b100010 => {
                // SHA512SU1: W=Vd, X=Vn, Y=Vm
                let sig1 = |v: u64| v.rotate_right(19) ^ v.rotate_right(61) ^ (v >> 6);
                mk(lo(d).wrapping_add(sig1(lo(n))).wrapping_add(lo(m)), hi(d).wrapping_add(sig1(hi(n))).wrapping_add(hi(m)))
            }
            0b100011 => mk(lo(n) ^ lo(m).rotate_left(1), hi(n) ^ hi(m).rotate_left(1)), // RAX1
            _ => return Err(unimpl(insn)),
        },
        0b110 if rm == 0 && bits(insn, 15, 10) == 0b100000 => {
            // SHA512SU0: W=Vd, X=Vn
            let sig0 = |v: u64| v.rotate_right(1) ^ v.rotate_right(8) ^ (v >> 7);
            mk(lo(d).wrapping_add(sig0(hi(d))), hi(d).wrapping_add(sig0(lo(n))))
        }
        _ => return Err(unimpl(insn)),
    };
    cpu.set_vq(rd, r);
    Ok(())
}

fn crypto_sha2(cpu: &mut Cpu, insn: u32) -> R {
    let opcode = bits(insn, 16, 12);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let n = cpu.vq(rn);
    let d = cpu.vq(rd);
    let r = match opcode {
        0 => {
            // SHA1H
            ((get(n, 32, 0) as u32).rotate_left(30)) as u128
        }
        2 => {
            // SHA256SU0
            let t = |e: u32| if e < 3 { get(d, 32, e + 1) as u32 } else { get(n, 32, 0) as u32 };
            let sig0 = |x: u32| x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
            let mut r = 0u128;
            for e in 0..4 {
                put(&mut r, 32, e, sig0(t(e)).wrapping_add(get(d, 32, e) as u32) as u64);
            }
            r
        }
        1 => {
            // SHA1SU1
            let t = d ^ (n >> 32);
            let w = |i| (get(t, 32, i) as u32).rotate_left(1);
            let w3 = (get(t, 32, 3) as u32 ^ (get(t, 32, 0) as u32).rotate_left(1)).rotate_left(1);
            (w(0) as u128) | ((w(1) as u128) << 32) | ((w(2) as u128) << 64) | ((w3 as u128) << 96)
        }
        _ => return Err(unimpl(insn)),
    };
    cpu.set_vq(rd, r);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aes_mixcolumns_vector() {
        let mut cpu = Cpu::new();
        // column db 13 53 45 -> 8e 4d a1 bc (FIPS-197 / wikipedia test column)
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&[0xdb, 0x13, 0x53, 0x45]);
        cpu.set_vq(1, u128::from_le_bytes(b));
        // aesmc v0.16b, v1.16b
        crypto_aes(&mut cpu, 0x4e28_6820).unwrap();
        let out = cpu.vq(0).to_le_bytes();
        assert_eq!(&out[..4], &[0x8e, 0x4d, 0xa1, 0xbc]);
    }
    #[test]
    fn sbox() {
        assert_eq!(SBOX[0x00], 0x63);
        assert_eq!(SBOX[0x01], 0x7c);
        assert_eq!(SBOX[0x53], 0xed);
        assert_eq!(INV_SBOX[0xed], 0x53);
    }
}
