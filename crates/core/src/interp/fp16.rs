//! Half-precision (FEAT_FP16) Advanced SIMD and scalar-SIMD instructions.
//!
//! Lanes are widened to f64, operated on with the shared softfloat helpers and
//! rounded once back to half precision. f64 holds every half-precision value
//! exactly and has enough extra precision that +, -, *, /, sqrt and fused
//! multiply-add round correctly.

use super::simd::{get, put};
use super::*;
use crate::softfloat::{self as sf, Fp};

#[inline(always)]
fn h(v: u64) -> f64 {
    sf::f16_to_f64(v as u16)
}
#[inline(always)]
fn hh(x: f64) -> u64 {
    sf::f64_to_f16(x) as u64
}
#[inline(always)]
fn mask(c: bool) -> u64 {
    if c {
        0xFFFF
    } else {
        0
    }
}

fn to_int16(x: f64, mode: u32, signed: bool) -> u64 {
    if x.is_nan() {
        return 0;
    }
    let r = x.round_mode(mode);
    if signed {
        r.clamp(-32768.0, 32767.0) as i16 as u16 as u64
    } else {
        r.clamp(0.0, 65535.0) as u16 as u64
    }
}

fn finish(cpu: &mut Cpu, rd: u32, v: u128, scalar: bool, q: bool) {
    if scalar {
        cpu.set_vd(rd, (v & 0xFFFF) as u64);
    } else {
        cpu.set_vq(rd, if q { v } else { v & (u64::MAX as u128) });
    }
}

/// Try to execute `insn` as an FP16 vector/scalar-SIMD instruction.
pub(crate) fn try_exec(cpu: &mut Cpu, insn: u32) -> Option<R> {
    let vector = bit(insn, 31) == 0 && bits(insn, 28, 24) == 0b01110;
    let vindexed = bit(insn, 31) == 0 && bits(insn, 28, 24) == 0b01111;
    let scalar = bits(insn, 31, 30) == 0b01 && bits(insn, 28, 24) == 0b11110;
    let sindexed = bits(insn, 31, 30) == 0b01 && bits(insn, 28, 24) == 0b11111;
    let u = bit(insn, 29) != 0;
    let a = bit(insn, 23) != 0;
    if (vector || scalar) && bits(insn, 22, 17) == 0b111100 && bits(insn, 11, 10) == 0b10 {
        return two_misc(cpu, insn, scalar, u, a);
    }
    if (vector || scalar) && bit(insn, 22) == 1 && bit(insn, 21) == 0 && bits(insn, 15, 14) == 0 && bit(insn, 10) == 1 {
        return three_same(cpu, insn, scalar, u, a);
    }
    if (vindexed || sindexed) && bits(insn, 23, 22) == 0 && bit(insn, 10) == 0 && matches!(bits(insn, 15, 12), 1 | 5 | 9) {
        return by_element(cpu, insn, sindexed, u);
    }
    None
}

fn two_misc(cpu: &mut Cpu, insn: u32, scalar: bool, u: bool, a: bool) -> Option<R> {
    let q = bit(insn, 30) != 0;
    let opc = bits(insn, 16, 12);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let n = cpu.vq(rn);
    let elems = if scalar { 1 } else if q { 8 } else { 4 };
    // Element function: half bits -> half bits.
    let f: Box<dyn Fn(u64) -> u64> = match (u, a, opc) {
        (false, false, 0x18) => Box::new(|x| hh(rint(h(x), 0))),
        (false, false, 0x19) => Box::new(|x| hh(rint(h(x), 2))),
        (false, true, 0x18) => Box::new(|x| hh(rint(h(x), 1))),
        (false, true, 0x19) => Box::new(|x| hh(rint(h(x), 3))),
        (true, false, 0x18) => Box::new(|x| hh(rint(h(x), 4))),
        (true, false, 0x19) | (true, true, 0x19) => {
            let mode = ((cpu.fpcr >> 22) & 3) as u32;
            Box::new(move |x| hh(rint(h(x), mode)))
        }
        (false, false, 0x1a) => Box::new(|x| to_int16(h(x), 0, true)),
        (false, false, 0x1b) => Box::new(|x| to_int16(h(x), 2, true)),
        (false, false, 0x1c) => Box::new(|x| to_int16(h(x), 4, true)),
        (false, true, 0x1a) => Box::new(|x| to_int16(h(x), 1, true)),
        (false, true, 0x1b) => Box::new(|x| to_int16(h(x), 3, true)),
        (true, false, 0x1a) => Box::new(|x| to_int16(h(x), 0, false)),
        (true, false, 0x1b) => Box::new(|x| to_int16(h(x), 2, false)),
        (true, false, 0x1c) => Box::new(|x| to_int16(h(x), 4, false)),
        (true, true, 0x1a) => Box::new(|x| to_int16(h(x), 1, false)),
        (true, true, 0x1b) => Box::new(|x| to_int16(h(x), 3, false)),
        (false, false, 0x1d) => Box::new(|x| hh(x as u16 as i16 as f64)),
        (true, false, 0x1d) => Box::new(|x| hh(x as u16 as f64)),
        (false, true, 0x0c) => Box::new(|x| mask(h(x) > 0.0)),
        (false, true, 0x0d) => Box::new(|x| mask(h(x) == 0.0)),
        (false, true, 0x0e) => Box::new(|x| mask(h(x) < 0.0)),
        (true, false, 0x0c) => Box::new(|x| mask(h(x) >= 0.0)),
        (true, false, 0x0d) => Box::new(|x| mask(h(x) <= 0.0)),
        (false, true, 0x0f) => Box::new(|x| x & 0x7FFF),
        (true, true, 0x0f) => Box::new(|x| (x & 0xFFFF) ^ 0x8000),
        (false, true, 0x1d) => Box::new(|x| hh(sf::frecpe(h(x)))),
        (true, true, 0x1d) => Box::new(|x| hh(sf::frsqrte(h(x)))),
        (true, true, 0x1f) => Box::new(|x| hh(sf::fsqrt(h(x)))),
        _ => return None,
    };
    let mut r = 0u128;
    for i in 0..elems {
        put(&mut r, 16, i, f(get(n, 16, i)));
    }
    finish(cpu, rd, r, scalar, q);
    Some(Ok(()))
}

fn rint(x: f64, mode: u32) -> f64 {
    if x.is_nan() {
        return x;
    }
    let r = x.round_mode(mode);
    if r.is_zero() {
        f64::zero(x.sign())
    } else {
        r
    }
}

fn three_same(cpu: &mut Cpu, insn: u32, scalar: bool, u: bool, a: bool) -> Option<R> {
    let q = bit(insn, 30) != 0;
    let opc = bits(insn, 13, 11);
    let rm = bits(insn, 20, 16);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    let n = cpu.vq(rn);
    let m = cpu.vq(rm);
    let d = cpu.vq(rd);
    let elems = if scalar { 1 } else if q { 8 } else { 4 };
    let mut r = 0u128;
    let key = (u, a, opc);
    let pairwise = !scalar && matches!(key, (true, false, 0) | (true, true, 0) | (true, false, 2) | (true, false, 6) | (true, true, 6));
    if scalar && !matches!(key, (false, false, 3) | (false, false, 4) | (false, false, 7) | (false, true, 7) | (true, false, 4) | (true, false, 5) | (true, true, 2) | (true, true, 4) | (true, true, 5)) {
        return None;
    }
    let op2 = |x: f64, y: f64, acc: f64| -> Option<u64> {
        Some(match key {
            (false, false, 0) | (true, false, 0) => hh(sf::fmaxnm(x, y)),
            (false, true, 0) | (true, true, 0) => hh(sf::fminnm(x, y)),
            (false, false, 1) => hh(sf::fmla(acc, x, y)),
            (false, true, 1) => hh(sf::fmla(acc, -x, y)),
            (false, false, 2) | (true, false, 2) => hh(sf::fadd(x, y)),
            (false, true, 2) => hh(sf::fsub(x, y)),
            (false, false, 3) => hh(sf::fmulx(x, y)),
            (false, false, 4) => mask(x == y),
            (false, false, 6) | (true, false, 6) => hh(sf::fmax(x, y)),
            (false, true, 6) | (true, true, 6) => hh(sf::fmin(x, y)),
            (false, false, 7) => hh(sf::frecps(x, y)),
            (false, true, 7) => hh(sf::frsqrts(x, y)),
            (true, false, 3) => hh(sf::fmul(x, y)),
            (true, false, 4) => mask(x >= y),
            (true, false, 5) => mask(x.abs() >= y.abs()),
            (true, false, 7) => hh(sf::fdiv(x, y)),
            (true, true, 2) => hh(sf::fsub(x, y).abs()),
            (true, true, 4) => mask(x > y),
            (true, true, 5) => mask(x.abs() > y.abs()),
            _ => return None,
        })
    };
    if pairwise {
        let mut lanes = [0f64; 16];
        for i in 0..elems {
            lanes[i as usize] = h(get(n, 16, i));
            lanes[(elems + i) as usize] = h(get(m, 16, i));
        }
        for i in 0..elems {
            let v = op2(lanes[(2 * i) as usize], lanes[(2 * i + 1) as usize], 0.0)?;
            put(&mut r, 16, i, v);
        }
    } else {
        for i in 0..elems {
            let v = op2(h(get(n, 16, i)), h(get(m, 16, i)), h(get(d, 16, i)))?;
            put(&mut r, 16, i, v);
        }
    }
    finish(cpu, rd, r, scalar, q);
    Some(Ok(()))
}

fn by_element(cpu: &mut Cpu, insn: u32, scalar: bool, u: bool) -> Option<R> {
    let q = bit(insn, 30) != 0;
    let opc = bits(insn, 15, 12);
    let idx = (bit(insn, 11) << 2) | (bit(insn, 21) << 1) | bit(insn, 20);
    let rm = bits(insn, 19, 16);
    let rn = bits(insn, 9, 5);
    let rd = bits(insn, 4, 0);
    if u && opc != 9 {
        return None;
    }
    let n = cpu.vq(rn);
    let d = cpu.vq(rd);
    let m = h(get(cpu.vq(rm), 16, idx));
    let elems = if scalar { 1 } else if q { 8 } else { 4 };
    let mut r = 0u128;
    for i in 0..elems {
        let x = h(get(n, 16, i));
        let v = match (u, opc) {
            (false, 1) => hh(sf::fmla(h(get(d, 16, i)), x, m)),
            (false, 5) => hh(sf::fmla(h(get(d, 16, i)), -x, m)),
            (false, 9) => hh(sf::fmul(x, m)),
            (true, 9) => hh(sf::fmulx(x, m)),
            _ => return None,
        };
        put(&mut r, 16, i, v);
    }
    finish(cpu, rd, r, scalar, q);
    Some(Ok(()))
}
