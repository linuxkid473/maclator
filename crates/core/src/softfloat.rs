//! Floating-point helpers implementing AArch64 semantics on top of host IEEE
//! arithmetic: NaN propagation rules, default NaN, min/max variants,
//! half precision conversion, reciprocal estimates.

pub const DEFAULT_NAN32: u32 = 0x7fc0_0000;
pub const DEFAULT_NAN64: u64 = 0x7ff8_0000_0000_0000;
pub const DEFAULT_NAN16: u16 = 0x7e00;

/// Abstraction over f32/f64 so NEON code can be generic over element size.
pub trait Fp: Copy + PartialOrd + std::ops::Add<Output = Self> + std::ops::Sub<Output = Self>
    + std::ops::Mul<Output = Self> + std::ops::Div<Output = Self> + std::ops::Neg<Output = Self>
{
    const BITS: u32;
    fn from_bits64(b: u64) -> Self;
    fn to_bits64(self) -> u64;
    fn is_nan(self) -> bool;
    fn is_snan(self) -> bool;
    fn quiet(self) -> Self;
    fn default_nan() -> Self;
    fn zero(neg: bool) -> Self;
    fn is_zero(self) -> bool;
    fn is_inf(self) -> bool;
    fn sign(self) -> bool;
    fn sqrt(self) -> Self;
    fn mul_add(self, b: Self, c: Self) -> Self;
    fn abs(self) -> Self;
    fn from_f64(v: f64) -> Self;
    fn to_f64(self) -> f64;
    fn round_mode(self, mode: u32) -> Self;
    fn from_i64(v: i64) -> Self;
    fn from_u64(v: u64) -> Self;
}

macro_rules! impl_fp {
    ($t:ty, $bits:expr, $ubits:ty, $qbit:expr, $dnan:expr) => {
        impl Fp for $t {
            const BITS: u32 = $bits;
            #[inline(always)]
            fn from_bits64(b: u64) -> Self { <$t>::from_bits(b as $ubits) }
            #[inline(always)]
            fn to_bits64(self) -> u64 { self.to_bits() as u64 }
            #[inline(always)]
            fn is_nan(self) -> bool { self.is_nan() }
            #[inline(always)]
            fn is_snan(self) -> bool { self.is_nan() && (self.to_bits() & $qbit) == 0 }
            #[inline(always)]
            fn quiet(self) -> Self { <$t>::from_bits(self.to_bits() | $qbit) }
            #[inline(always)]
            fn default_nan() -> Self { <$t>::from_bits($dnan) }
            #[inline(always)]
            fn zero(neg: bool) -> Self { if neg { -0.0 } else { 0.0 } }
            #[inline(always)]
            fn is_zero(self) -> bool { self == 0.0 }
            #[inline(always)]
            fn is_inf(self) -> bool { self.is_infinite() }
            #[inline(always)]
            fn sign(self) -> bool { self.is_sign_negative() }
            #[inline(always)]
            fn sqrt(self) -> Self { <$t>::sqrt(self) }
            #[inline(always)]
            fn mul_add(self, b: Self, c: Self) -> Self { <$t>::mul_add(self, b, c) }
            #[inline(always)]
            fn abs(self) -> Self { <$t>::abs(self) }
            #[inline(always)]
            fn from_f64(v: f64) -> Self { v as $t }
            #[inline(always)]
            fn to_f64(self) -> f64 { self as f64 }
            #[inline(always)]
            fn round_mode(self, mode: u32) -> Self {
                match mode {
                    0 => self.round_ties_even(),
                    1 => self.ceil(),
                    2 => self.floor(),
                    3 => self.trunc(),
                    _ => self.round(), // ties away
                }
            }
            #[inline(always)]
            fn from_i64(v: i64) -> Self { v as $t }
            #[inline(always)]
            fn from_u64(v: u64) -> Self { v as $t }
        }
    };
}
impl_fp!(f32, 32, u32, 0x0040_0000u32, DEFAULT_NAN32);
impl_fp!(f64, 64, u64, 0x0008_0000_0000_0000u64, DEFAULT_NAN64);

/// FPProcessNaNs for two operands: returns Some(result) if a NaN must be
/// propagated.
#[inline(always)]
pub fn process_nans<T: Fp>(a: T, b: T) -> Option<T> {
    if !(a.is_nan() || b.is_nan()) {
        return None;
    }
    if a.is_snan() {
        Some(a.quiet())
    } else if b.is_snan() {
        Some(b.quiet())
    } else if a.is_nan() {
        Some(a)
    } else {
        Some(b)
    }
}

#[inline(always)]
pub fn process_nans3<T: Fp>(a: T, b: T, c: T) -> Option<T> {
    if !(a.is_nan() || b.is_nan() || c.is_nan()) {
        return None;
    }
    if a.is_snan() {
        Some(a.quiet())
    } else if b.is_snan() {
        Some(b.quiet())
    } else if c.is_snan() {
        Some(c.quiet())
    } else if a.is_nan() {
        Some(a)
    } else if b.is_nan() {
        Some(b)
    } else {
        Some(c)
    }
}

/// Replace a freshly generated NaN (host default NaN) by the ARM default NaN.
#[inline(always)]
pub fn fix<T: Fp>(r: T) -> T {
    if r.is_nan() {
        T::default_nan()
    } else {
        r
    }
}

#[inline(always)]
pub fn fadd<T: Fp>(a: T, b: T) -> T {
    process_nans(a, b).unwrap_or_else(|| fix(a + b))
}
#[inline(always)]
pub fn fsub<T: Fp>(a: T, b: T) -> T {
    process_nans(a, b).unwrap_or_else(|| fix(a - b))
}
#[inline(always)]
pub fn fmul<T: Fp>(a: T, b: T) -> T {
    process_nans(a, b).unwrap_or_else(|| fix(a * b))
}
#[inline(always)]
pub fn fdiv<T: Fp>(a: T, b: T) -> T {
    process_nans(a, b).unwrap_or_else(|| fix(a / b))
}
/// FMULX: like FMUL but 0 * inf = 2 (signed).
#[inline(always)]
pub fn fmulx<T: Fp>(a: T, b: T) -> T {
    if let Some(n) = process_nans(a, b) {
        return n;
    }
    if (a.is_zero() && b.is_inf()) || (a.is_inf() && b.is_zero()) {
        let two = T::from_f64(2.0);
        return if a.sign() != b.sign() { -two } else { two };
    }
    a * b
}
#[inline(always)]
pub fn fsqrt<T: Fp>(a: T) -> T {
    if a.is_nan() {
        return if a.is_snan() { a.quiet() } else { a };
    }
    fix(a.sqrt())
}
/// Fused a + n*m (FMADD semantics: result = a + n*m).
#[inline(always)]
pub fn fmla<T: Fp>(a: T, n: T, m: T) -> T {
    // ARM: FPMulAdd(addend, op1, op2): NaN processing order op1? The ARM
    // pseudocode checks (addend, op1, op2) with a special case for
    // quiet addend with inf*0 producing default NaN.
    if a.is_nan() && !a.is_snan() && ((n.is_inf() && m.is_zero()) || (n.is_zero() && m.is_inf())) {
        return T::default_nan();
    }
    if let Some(r) = process_nans3(a, n, m) {
        return r;
    }
    fix(n.mul_add(m, a))
}

#[inline(always)]
pub fn fmax<T: Fp>(a: T, b: T) -> T {
    if let Some(n) = process_nans(a, b) {
        return n;
    }
    if a.is_zero() && b.is_zero() {
        return T::zero(a.sign() && b.sign());
    }
    if a > b { a } else { b }
}
#[inline(always)]
pub fn fmin<T: Fp>(a: T, b: T) -> T {
    if let Some(n) = process_nans(a, b) {
        return n;
    }
    if a.is_zero() && b.is_zero() {
        return T::zero(a.sign() || b.sign());
    }
    if a < b { a } else { b }
}
#[inline(always)]
pub fn fmaxnm<T: Fp>(a: T, b: T) -> T {
    let (qa, qb) = (a.is_nan() && !a.is_snan(), b.is_nan() && !b.is_snan());
    if qa && !b.is_nan() {
        return b;
    }
    if qb && !a.is_nan() {
        return a;
    }
    fmax(a, b)
}
#[inline(always)]
pub fn fminnm<T: Fp>(a: T, b: T) -> T {
    let (qa, qb) = (a.is_nan() && !a.is_snan(), b.is_nan() && !b.is_snan());
    if qa && !b.is_nan() {
        return b;
    }
    if qb && !a.is_nan() {
        return a;
    }
    fmin(a, b)
}

/// FCMP flags: returns NZCV nibble.
#[inline(always)]
pub fn fcmp<T: Fp>(a: T, b: T) -> u32 {
    if a.is_nan() || b.is_nan() {
        0b0011
    } else if a == b {
        0b0110
    } else if a < b {
        0b1000
    } else {
        0b0010
    }
}

/// FRECPS: 2 - a*b (with inf*0 -> 2). The first operand is negated before
/// NaN processing, as in the ARM pseudocode.
#[inline(always)]
pub fn frecps<T: Fp>(a: T, b: T) -> T {
    let a = -a;
    if let Some(n) = process_nans(a, b) {
        return n;
    }
    if (a.is_inf() && b.is_zero()) || (a.is_zero() && b.is_inf()) {
        return T::from_f64(2.0);
    }
    fix(a.mul_add(b, T::from_f64(2.0)))
}
/// FRSQRTS: (3 - a*b) / 2 (with inf*0 -> 1.5).
#[inline(always)]
pub fn frsqrts<T: Fp>(a: T, b: T) -> T {
    let a = -a;
    if let Some(n) = process_nans(a, b) {
        return n;
    }
    if (a.is_inf() && b.is_zero()) || (a.is_zero() && b.is_inf()) {
        return T::from_f64(1.5);
    }
    fix(a.mul_add(b, T::from_f64(3.0)) / T::from_f64(2.0))
}

/// ARM RecipEstimate on a 9-bit input a in [256, 511] -> [256, 511].
fn recip_estimate(a: u64) -> u64 {
    let a = a * 2 + 1;
    let b = (1u64 << 19) / a;
    (b + 1) / 2
}

/// FRECPE for f32/f64.
pub fn frecpe<T: Fp>(x: T) -> T {
    let bits = T::BITS;
    let (exp_bits, frac_bits) = if bits == 32 { (8u32, 23u32) } else { (11, 52) };
    if x.is_nan() {
        return if x.is_snan() { x.quiet() } else { x };
    }
    if x.is_inf() {
        return T::zero(x.sign());
    }
    if x.is_zero() {
        let inf = T::from_f64(f64::INFINITY);
        return if x.sign() { -inf } else { inf };
    }
    let raw = x.to_bits64();
    let sign = raw >> (bits - 1);
    let bias = (1i64 << (exp_bits - 1)) - 1;
    let mut exp = ((raw >> frac_bits) & ((1 << exp_bits) - 1)) as i64;
    let mut frac = raw & ((1u64 << frac_bits) - 1);
    // Very small inputs overflow to infinity / max.
    let v = x.abs().to_f64();
    let tiny = if bits == 32 { 2f64.powi(-128) } else { 2f64.powi(-1024) };
    if v < tiny {
        let inf = T::from_f64(f64::INFINITY);
        return if x.sign() { -inf } else { inf };
    }
    if exp == 0 {
        // denormal: normalise
        if (frac >> (frac_bits - 1)) & 1 == 0 {
            exp -= 1;
            frac = (frac << 2) & ((1u64 << frac_bits) - 1);
        } else {
            frac = (frac << 1) & ((1u64 << frac_bits) - 1);
        }
    }
    let scaled = (1u64 << 8) | (frac >> (frac_bits - 8));
    let est = recip_estimate(scaled);
    let mut result_exp = 2 * bias - 1 - exp;
    let mut fraction = (est & 0xff) << (frac_bits - 8);
    if result_exp == 0 {
        fraction = (1u64 << (frac_bits - 1)) | (fraction >> 1);
    } else if result_exp == -1 {
        fraction = (1u64 << (frac_bits - 2)) | (fraction >> 2);
        result_exp = 0;
    }
    let out = (sign << (bits - 1)) | ((result_exp as u64 & ((1 << exp_bits) - 1)) << frac_bits) | fraction;
    T::from_bits64(out)
}

/// FRSQRTE for f32/f64.
pub fn frsqrte<T: Fp>(x: T) -> T {
    let bits = T::BITS;
    let (exp_bits, frac_bits) = if bits == 32 { (8u32, 23u32) } else { (11, 52) };
    if x.is_nan() {
        return if x.is_snan() { x.quiet() } else { x };
    }
    if x.is_zero() {
        let inf = T::from_f64(f64::INFINITY);
        return if x.sign() { -inf } else { inf };
    }
    if x.sign() {
        return T::default_nan();
    }
    if x.is_inf() {
        return T::zero(false);
    }
    let raw = x.to_bits64();
    let bias = (1i64 << (exp_bits - 1)) - 1;
    let mut exp = ((raw >> frac_bits) & ((1 << exp_bits) - 1)) as i64;
    let mut frac = raw & ((1u64 << frac_bits) - 1);
    if exp == 0 {
        while (frac >> (frac_bits - 1)) & 1 == 0 {
            frac = (frac << 1) & ((1u64 << frac_bits) - 1);
            exp -= 1;
        }
        frac = (frac << 1) & ((1u64 << frac_bits) - 1);
    }
    let scaled = if exp & 1 == 0 {
        (1u64 << 8) | (frac >> (frac_bits - 8))
    } else {
        (1u64 << 7) | (frac >> (frac_bits - 7))
    };
    let est = rsqrt_est_arm(scaled);
    let result_exp = (3 * bias - 1 - exp) / 2;
    let out = ((result_exp as u64 & ((1 << exp_bits) - 1)) << frac_bits) | ((est & 0xff) << (frac_bits - 8));
    T::from_bits64(out)
}

/// RecipSqrtEstimate from the ARM ARM: input a in [128, 511] (units of
/// 1/512), output in [256, 511].
fn rsqrt_est_arm(a: u64) -> u64 {
    // Convert to units of 1/1024, rounded to the middle of the interval.
    let a = if a < 256 { a * 2 + 1 } else { (((a >> 1) << 1) + 1) * 2 };
    let mut b: u64 = 512;
    while a * (b + 1) * (b + 1) < (1u64 << 28) {
        b += 1;
    }
    (b + 1) / 2
}

// ---------------- half precision ----------------

pub fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if frac == 0 {
            sign
        } else {
            // subnormal
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | ((e as u32) << 23) | ((f & 0x3ff) << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (frac << 13)
    } else {
        sign | ((exp + 127 - 15) << 23) | (frac << 13)
    };
    f32::from_bits(bits)
}

pub fn f16_to_f64(h: u16) -> f64 {
    f16_to_f32(h) as f64
}

/// Round-to-nearest-even conversion from f64 to half precision.
pub fn f64_to_f16(v: f64) -> u16 {
    let b = v.to_bits();
    let sign = ((b >> 63) as u16) << 15;
    let exp = ((b >> 52) & 0x7ff) as i64;
    let frac = b & ((1u64 << 52) - 1);
    if exp == 0x7ff {
        if frac == 0 {
            return sign | 0x7c00;
        }
        // NaN: keep top payload bits, force quiet
        return sign | 0x7e00 | ((frac >> 42) as u16 & 0x1ff);
    }
    let e = exp - 1023 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    let mant = if exp == 0 { frac } else { frac | (1u64 << 52) };
    // amount to shift the 53-bit mantissa right to get the 11-bit half mantissa
    let (shift, e_out) = if e <= 0 { (42 + 1 - e, 0i64) } else { (42, e) };
    if shift >= 64 {
        return sign;
    }
    let shift = shift as u32;
    let mut m = mant >> shift;
    let rem = mant & ((1u64 << shift) - 1);
    let half = 1u64 << (shift - 1);
    if rem > half || (rem == half && (m & 1) == 1) {
        m += 1;
    }
    // m includes the implicit bit for normals; carry may bump exponent
    let out = if e_out == 0 {
        // subnormal (m < 0x400) or rounded up to smallest normal (m == 0x400)
        m as u16
    } else {
        let ex = e_out as u64 + (m >> 11);
        let mm = if m >> 11 != 0 { (m >> 1) & 0x3ff } else { m & 0x3ff };
        if ex >= 0x1f {
            return sign | 0x7c00;
        }
        ((ex << 10) | mm) as u16
    };
    sign | out
}

pub fn f32_to_f16(v: f32) -> u16 {
    // Converting f32 -> f64 is exact, so a single rounding happens.
    f64_to_f16(v as f64)
}
