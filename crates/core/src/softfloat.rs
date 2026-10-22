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

// The usual helpers above retain the historical default-mode behavior used by
// the JIT. Non-default FPCR needs a real rounding step, rather than a host fma
// followed by adjustment: cancellation can discard every bit of the product.
// This fixed-width integer representation covers the entire binary64 product
// range (2^-2148 through just below 2^2048), with room for an addition carry.
const FMA_SCALE: i32 = -2148;
const FMA_LIMBS: usize = 66;
const IOC: u64 = 1 << 0;
const OFC: u64 = 1 << 2;
const UFC: u64 = 1 << 3;
const IXC: u64 = 1 << 4;
const IDC: u64 = 1 << 7;

#[derive(Clone, Copy)]
struct Format { frac: u32, bias: i32, exp_mask: u64, sign_mask: u64 }

impl Format {
    fn of<T: Fp>() -> Self {
        if T::BITS == 32 {
            Self { frac: 23, bias: 127, exp_mask: 255, sign_mask: 1 << 31 }
        } else {
            Self { frac: 52, bias: 1023, exp_mask: 2047, sign_mask: 1 << 63 }
        }
    }
    fn fraction(self, bits: u64) -> u64 { bits & ((1 << self.frac) - 1) }
    fn exponent(self, bits: u64) -> u64 { (bits >> self.frac) & self.exp_mask }
    fn nan(self, bits: u64) -> bool { self.exponent(bits) == self.exp_mask && self.fraction(bits) != 0 }
    fn snan(self, bits: u64) -> bool { self.nan(bits) && bits & (1 << (self.frac - 1)) == 0 }
    fn inf(self, bits: u64) -> bool { self.exponent(bits) == self.exp_mask && self.fraction(bits) == 0 }
    fn zero(self, bits: u64) -> bool { bits & !self.sign_mask == 0 }
    fn sign(self, bits: u64) -> bool { bits & self.sign_mask != 0 }
    fn signed(self, bits: u64, sign: bool) -> u64 { bits | if sign { self.sign_mask } else { 0 } }
    fn infinity(self, sign: bool) -> u64 { self.signed(self.exp_mask << self.frac, sign) }
    fn default_nan(self) -> u64 { (self.exp_mask << self.frac) | (1 << (self.frac - 1)) }
    fn unpack(self, bits: u64) -> (u64, i32) {
        let e = self.exponent(bits);
        let mantissa = self.fraction(bits) | if e == 0 { 0 } else { 1 << self.frac };
        (mantissa, (e.max(1) as i32) - self.bias - self.frac as i32)
    }
    fn flush_input(self, bits: u64, fpcr: u64, fpsr: &mut u64) -> u64 {
        if fpcr & (1 << 24) != 0 && self.exponent(bits) == 0 && self.fraction(bits) != 0 {
            *fpsr |= IDC;
            bits & self.sign_mask
        } else { bits }
    }
}

struct FmaMagnitude([u64; FMA_LIMBS]);

impl FmaMagnitude {
    fn shifted(value: u128, shift: usize) -> Self {
        let mut out = Self([0; FMA_LIMBS]);
        let limb = shift / 64;
        let bit = shift % 64;
        let lo = value as u64;
        let hi = (value >> 64) as u64;
        out.0[limb] = lo << bit;
        if bit == 0 {
            out.0[limb + 1] = hi;
        } else {
            out.0[limb + 1] = (lo >> (64 - bit)) | (hi << bit);
            out.0[limb + 2] = hi >> (64 - bit);
        }
        out
    }
    fn cmp(&self, rhs: &Self) -> std::cmp::Ordering {
        for i in (0..FMA_LIMBS).rev() {
            if self.0[i] != rhs.0[i] { return self.0[i].cmp(&rhs.0[i]); }
        }
        std::cmp::Ordering::Equal
    }
    fn add(&mut self, rhs: &Self) {
        let mut carry = 0u128;
        for i in 0..FMA_LIMBS {
            let v = self.0[i] as u128 + rhs.0[i] as u128 + carry;
            self.0[i] = v as u64;
            carry = v >> 64;
        }
        debug_assert_eq!(carry, 0);
    }
    // The caller ensures self >= rhs.
    fn sub(&mut self, rhs: &Self) {
        let mut borrow = false;
        for i in 0..FMA_LIMBS {
            let (v, b1) = self.0[i].overflowing_sub(rhs.0[i]);
            let (v, b2) = v.overflowing_sub(borrow as u64);
            self.0[i] = v;
            borrow = b1 || b2;
        }
        debug_assert!(!borrow);
    }
    fn highest(&self) -> Option<usize> {
        for i in (0..FMA_LIMBS).rev() {
            if self.0[i] != 0 { return Some(i * 64 + 63 - self.0[i].leading_zeros() as usize); }
        }
        None
    }
    fn bit(&self, bit: usize) -> bool { self.0[bit / 64] & (1 << (bit % 64)) != 0 }
    fn any_below(&self, bit: usize) -> bool {
        self.0[..bit / 64].iter().any(|&v| v != 0)
            || (bit % 64 != 0 && self.0[bit / 64] & ((1 << (bit % 64)) - 1) != 0)
    }
    fn shr_u64(&self, bit: usize) -> u64 {
        let i = bit / 64;
        let shift = bit % 64;
        (self.0[i] >> shift)
            | if shift != 0 && i + 1 < FMA_LIMBS { self.0[i + 1] << (64 - shift) } else { 0 }
    }
}

fn round_magnitude(mag: &FmaMagnitude, sign: bool, fmt: Format, fpcr: u64, fpsr: &mut u64) -> u64 {
    let highest = mag.highest().expect("nonzero exact result");
    let mut exponent = highest as i32 + FMA_SCALE;
    let minimum = 1 - fmt.bias;
    if exponent < minimum && fpcr & (1 << 24) != 0 {
        // Arm flushes before rounding and raises UFC alone, even for an exact
        // subnormal. x86 FTZ is not a substitute for this rule.
        *fpsr |= UFC;
        return fmt.signed(0, sign);
    }
    let shift = (exponent.max(minimum) - fmt.frac as i32 - FMA_SCALE) as usize;
    let mut mantissa = mag.shr_u64(shift);
    let inexact = mag.any_below(shift);
    if exponent < minimum && inexact { *fpsr |= UFC; }
    let rmode = (fpcr >> 22) & 3;
    let up = match rmode {
        0 => shift != 0 && mag.bit(shift - 1)
            && (mag.any_below(shift - 1) || mantissa & 1 != 0),
        1 => inexact && !sign,
        2 => inexact && sign,
        _ => false,
    };
    exponent = exponent.max(minimum);
    if up {
        mantissa += 1;
        if mantissa == 1 << (fmt.frac + 1) {
            mantissa >>= 1;
            exponent += 1;
        }
    }
    if exponent > fmt.bias {
        *fpsr |= OFC | IXC;
        let infinity = rmode == 0 || (rmode == 1 && !sign) || (rmode == 2 && sign);
        return if infinity { fmt.infinity(sign) }
            else { fmt.signed((fmt.exp_mask << fmt.frac) - 1, sign) };
    }
    if inexact { *fpsr |= IXC; }
    let encoded_exp = if mantissa >= 1 << fmt.frac { (exponent + fmt.bias) as u64 } else { 0 };
    fmt.signed((encoded_exp << fmt.frac) | (mantissa & ((1 << fmt.frac) - 1)), sign)
}

/// Scalar binary32/binary64 FMA under non-default FPCR. Uses integer arithmetic
/// so the host's rounding mode, FTZ/DAZ state and exception flags are untouched.
/// Default-mode FPSR is deliberately unchanged until the JIT fast paths collect
/// exceptions too. FP exception traps and FEAT_AFP alternative handling remain
/// outside this helper; the existing interpreter does not implement them.
pub fn fmla_controlled<T: Fp>(a: T, n: T, m: T, fpcr: u64, fpsr: &mut u64) -> T {
    if fpcr == 0 { return fmla(a, n, m); }
    let fmt = Format::of::<T>();
    let a = fmt.flush_input(a.to_bits64(), fpcr, fpsr);
    let n = fmt.flush_input(n.to_bits64(), fpcr, fpsr);
    let m = fmt.flush_input(m.to_bits64(), fpcr, fpsr);
    let invalid_product = (fmt.inf(n) && fmt.zero(m)) || (fmt.zero(n) && fmt.inf(m));
    if fmt.snan(a) || fmt.snan(n) || fmt.snan(m) { *fpsr |= IOC; }
    if fmt.nan(a) && !fmt.snan(a) && invalid_product {
        *fpsr |= IOC;
        return T::from_bits64(fmt.default_nan());
    }
    if let Some(nan) = [a, n, m].into_iter().find(|&v| fmt.snan(v))
        .or_else(|| [a, n, m].into_iter().find(|&v| fmt.nan(v))) {
        return T::from_bits64(if fpcr & (1 << 25) != 0 { fmt.default_nan() }
            else { nan | (1 << (fmt.frac - 1)) });
    }
    let product_sign = fmt.sign(n) != fmt.sign(m);
    if invalid_product || ((fmt.inf(n) || fmt.inf(m)) && fmt.inf(a) && fmt.sign(a) != product_sign) {
        *fpsr |= IOC;
        return T::from_bits64(fmt.default_nan());
    }
    if fmt.inf(a) { return T::from_bits64(a); }
    if fmt.inf(n) || fmt.inf(m) { return T::from_bits64(fmt.infinity(product_sign)); }
    let (ns, ne) = fmt.unpack(n);
    let (ms, me) = fmt.unpack(m);
    let (asig, ae) = fmt.unpack(a);
    let mut product = FmaMagnitude::shifted(ns as u128 * ms as u128, (ne + me - FMA_SCALE) as usize);
    let mut addend = FmaMagnitude::shifted(asig as u128, (ae - FMA_SCALE) as usize);
    let sign = if product_sign == fmt.sign(a) {
        product.add(&addend);
        product_sign
    } else {
        match product.cmp(&addend) {
            std::cmp::Ordering::Greater => { product.sub(&addend); product_sign }
            std::cmp::Ordering::Less => { addend.sub(&product); product = addend; fmt.sign(a) }
            std::cmp::Ordering::Equal => {
                product = FmaMagnitude([0; FMA_LIMBS]);
                ((fpcr >> 22) & 3) == 2
            }
        }
    };
    if product.highest().is_none() { return T::from_bits64(fmt.signed(0, sign)); }
    T::from_bits64(round_magnitude(&product, sign, fmt, fpcr, fpsr))
}

/// Non-default FPCR scalar multiply/add/subtract/min/max. Unsupported opcodes
/// return None so callers can retain their existing paths (notably division).
pub fn binary_controlled<T: Fp>(a: T, b: T, opcode: u32, fpcr: u64, fpsr: &mut u64) -> Option<T> {
    if !matches!(opcode, 0 | 2..=8) { return None; }
    // Preserve the existing JIT/reference contract for default-mode execution.
    if fpcr == 0 {
        return Some(match opcode {
            0 => fmul(a, b), 2 => fadd(a, b), 3 => fsub(a, b),
            4 => fmax(a, b), 5 => fmin(a, b), 6 => fmaxnm(a, b),
            7 => fminnm(a, b), _ => -fmul(a, b),
        });
    }
    let fmt = Format::of::<T>();
    let a = fmt.flush_input(a.to_bits64(), fpcr, fpsr);
    let b = fmt.flush_input(b.to_bits64(), fpcr, fpsr);
    let nan = if fmt.snan(a) { Some(a) } else if fmt.snan(b) { Some(b) }
        else if fmt.nan(a) { Some(a) } else if fmt.nan(b) { Some(b) } else { None };
    if let Some(nan) = nan {
        if fmt.snan(a) || fmt.snan(b) { *fpsr |= IOC; }
        else if matches!(opcode, 6 | 7) && (fmt.nan(a) != fmt.nan(b)) {
            return Some(T::from_bits64(if fmt.nan(a) { b } else { a }));
        }
        let mut result = if fpcr & (1 << 25) != 0 { fmt.default_nan() }
            else { nan | (1 << (fmt.frac - 1)) };
        if opcode == 8 { result ^= fmt.sign_mask; }
        return Some(T::from_bits64(result));
    }
    let a_value = T::from_bits64(a);
    let b_value = T::from_bits64(b);
    let result = match opcode {
        0 | 8 => {
            let sign = fmt.sign(a) != fmt.sign(b);
            let zero = T::from_bits64(fmt.signed(0, sign));
            let result = fmla_controlled(zero, a_value, b_value, fpcr, fpsr).to_bits64();
            T::from_bits64(if opcode == 8 { result ^ fmt.sign_mask } else { result })
        }
        2 => fmla_controlled(b_value, a_value, T::from_f64(1.), fpcr, fpsr),
        3 => fmla_controlled(T::from_bits64(b ^ fmt.sign_mask), a_value, T::from_f64(1.), fpcr, fpsr),
        _ => {
            let minimum = matches!(opcode, 5 | 7);
            let result = if fmt.zero(a) && fmt.zero(b) {
                fmt.signed(0, if minimum { fmt.sign(a) || fmt.sign(b) } else { fmt.sign(a) && fmt.sign(b) })
            } else {
                // Monotonic integer keys avoid host FP comparisons (including
                // their exception flags) for finite values and infinities.
                let key = |v| if fmt.sign(v) { !v & (fmt.sign_mask | (fmt.sign_mask - 1)) }
                    else { v | fmt.sign_mask };
                if (key(a) < key(b)) == minimum { a } else { b }
            };
            T::from_bits64(result)
        }
    };
    Some(result)
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

#[cfg(test)]
mod controlled_tests {
    use super::*;
    const DN: u64 = 1 << 25;
    const FZ: u64 = 1 << 24;

    #[test]
    fn binary_modes_nan_and_minmax() {
        let mut fpsr = 0;
        let half_ulp = f32::from_bits(103 << 23);
        assert_eq!(binary_controlled(1f32, half_ulp, 2, 1 << 22, &mut fpsr).unwrap().to_bits(), 0x3f80_0001);
        assert_eq!(fpsr, IXC);
        fpsr = 0;
        assert_eq!(binary_controlled(1f64, 1., 3, 2 << 22, &mut fpsr).unwrap().to_bits(), 1 << 63);
        assert_eq!(fpsr, 0);
        assert_eq!(binary_controlled(-2f32, 1., 4, DN, &mut fpsr), Some(1.));
        assert_eq!(binary_controlled(-2f32, 1., 5, DN, &mut fpsr), Some(-2.));
        assert_eq!(binary_controlled(-2f32, -1., 4, DN, &mut fpsr), Some(-1.));
        assert_eq!(binary_controlled(0f32, -0., 5, DN, &mut fpsr).unwrap().to_bits(), 1 << 31);
        assert_eq!(binary_controlled(0f32, -0., 4, DN, &mut fpsr).unwrap().to_bits(), 0);
        let qnan = f32::from_bits(0xffc0_1234);
        let snan = f32::from_bits(0x7f80_5678);
        assert_eq!(binary_controlled(qnan, -1., 6, DN, &mut fpsr), Some(-1.));
        assert_eq!(binary_controlled(qnan, 1., 3, 1 << 22, &mut fpsr).unwrap().to_bits(), qnan.to_bits());
        assert_eq!(binary_controlled(1f32, qnan, 3, 1 << 22, &mut fpsr).unwrap().to_bits(), qnan.to_bits());
        assert_eq!(binary_controlled(snan, -1., 6, DN, &mut fpsr).unwrap().to_bits(), DEFAULT_NAN32);
        assert_eq!(fpsr, IOC);
        fpsr = 0;
        assert_eq!(binary_controlled(0f64, f64::INFINITY, 0, DN, &mut fpsr).unwrap().to_bits(), DEFAULT_NAN64);
        assert_eq!(fpsr, IOC);
        assert_eq!(binary_controlled(0f64, f64::INFINITY, 8, DN, &mut fpsr).unwrap().to_bits(), DEFAULT_NAN64 ^ (1 << 63));
    }

    #[test]
    fn fma_directed_rounding_and_single_rounding() {
        for (mode, positive, negative) in [
            (0, 0x3f80_0000, 0xbf80_0000),
            (1, 0x3f80_0001, 0xbf80_0000),
            (2, 0x3f80_0000, 0xbf80_0001),
            (3, 0x3f80_0000, 0xbf80_0000),
        ] {
            let fpcr = DN | (mode << 22);
            let mut fpsr = 1 << 27;
            let half_ulp = f32::from_bits(103 << 23); // 2^-24
            assert_eq!(fmla_controlled(1f32, half_ulp, 1., fpcr, &mut fpsr).to_bits(), positive);
            assert_eq!(fmla_controlled(-1f32, -half_ulp, 1., fpcr, &mut fpsr).to_bits(), negative);
            assert_eq!(fpsr, (1 << 27) | IXC);
            let half_ulp = f64::from_bits(970u64 << 52); // 2^-53
            let expected = 0x3ff0_0000_0000_0000 + u64::from(mode == 1);
            assert_eq!(fmla_controlled(1f64, half_ulp, 1., fpcr, &mut fpsr).to_bits(), expected);
        }
        let mut fpsr = 0;
        let (n, m) = (f32::from_bits(0x3f80_0001), f32::from_bits(0x3f7f_fffe));
        assert_eq!(fmla_controlled(-1f32, n, m, DN, &mut fpsr).to_bits(), 0xa880_0000); // -2^-46
        let (n, m) = (f64::from_bits(0x3ff0_0000_0000_0001), f64::from_bits(0x3fef_ffff_ffff_fffe));
        assert_eq!(fmla_controlled(-1f64, n, m, DN, &mut fpsr).to_bits(), 0xb970_0000_0000_0000); // -2^-104
        assert_eq!(fpsr, 0);
    }

    #[test]
    fn fma_tininess_before_rounding_and_flush_flags() {
        let minimum = f32::from_bits(0x0080_0000);
        let below_one = f32::from_bits(0x3f7f_ffff);
        let mut fpsr = 0;
        // This exact product is halfway below the smallest normal. Arm detects
        // tininess before the result rounds up to a normal number.
        assert_eq!(fmla_controlled(0f32, minimum, below_one, DN, &mut fpsr).to_bits(), minimum.to_bits());
        assert_eq!(fpsr, UFC | IXC);
        fpsr = 0;
        assert_eq!(fmla_controlled(0f32, minimum, below_one, FZ, &mut fpsr).to_bits(), 0);
        assert_eq!(fpsr, UFC);
        fpsr = 0;
        assert_eq!(fmla_controlled(-0f32, -minimum, 0.5, FZ, &mut fpsr).to_bits(), 0x8000_0000);
        assert_eq!(fpsr, UFC); // exact subnormal flush does not raise IXC
        fpsr = 0;
        assert_eq!(fmla_controlled(0f32, minimum, 0.5, DN, &mut fpsr).to_bits(), 0x0040_0000);
        assert_eq!(fpsr, 0);
        fpsr = 0;
        assert_eq!(fmla_controlled(1f32, f32::from_bits(1), f32::INFINITY, FZ, &mut fpsr).to_bits(), DEFAULT_NAN32);
        assert_eq!(fpsr, IDC | IOC);
        let mut fpsr = 0;
        let minimum = f64::from_bits(0x0010_0000_0000_0000);
        let below_one = f64::from_bits(0x3fef_ffff_ffff_ffff);
        assert_eq!(fmla_controlled(0f64, minimum, below_one, DN, &mut fpsr).to_bits(), minimum.to_bits());
        assert_eq!(fpsr, UFC | IXC);
    }

    #[test]
    fn fma_nan_priority_overflow_and_zero_signs() {
        let mut fpsr = 0;
        let qnan = f32::from_bits(0xffc0_1234);
        let snan = f32::from_bits(0x7f80_5678);
        assert_eq!(fmla_controlled(qnan, snan, 1., 1 << 22, &mut fpsr).to_bits(), 0x7fc0_5678);
        assert_eq!(fpsr, IOC);
        fpsr = 0;
        assert_eq!(fmla_controlled(qnan, 1., 2., DN, &mut fpsr).to_bits(), DEFAULT_NAN32);
        assert_eq!(fpsr, 0);
        assert_eq!(fmla_controlled(qnan, f32::INFINITY, 0., 1 << 22, &mut fpsr).to_bits(), DEFAULT_NAN32);
        assert_eq!(fpsr, IOC);
        for mode in 0..4 {
            fpsr = 0;
            let fpcr = DN | (mode << 22);
            let result = fmla_controlled(0f32, f32::MAX, 2., fpcr, &mut fpsr);
            assert_eq!(result.to_bits(), if mode < 2 { 0x7f80_0000 } else { 0x7f7f_ffff });
            assert_eq!(fpsr, OFC | IXC);
            fpsr = 0;
            assert_eq!(fmla_controlled(-1f64, 1., 1., fpcr, &mut fpsr).to_bits(), if mode == 2 { 1 << 63 } else { 0 });
            assert_eq!(fmla_controlled(-0f64, -0., 1., fpcr, &mut fpsr).to_bits(), 1 << 63);
            assert_eq!(fmla_controlled(0f64, 0., 1., fpcr, &mut fpsr).to_bits(), 0);
            assert_eq!(fpsr, 0);
        }
    }

    #[test]
    fn default_mode_retains_jit_reference_contract() {
        let mut fpsr = 1 << 27;
        assert_eq!(fmla_controlled(0f64, f64::INFINITY, 0., 0, &mut fpsr).to_bits(), DEFAULT_NAN64);
        assert_eq!(fpsr, 1 << 27);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn binary_matches_independent_hardware_rounding_and_exception_flags() {
        fn next(state: &mut u64) -> u64 {
            *state ^= *state << 13; *state ^= *state >> 7; *state ^= *state << 17; *state
        }
        let mut seed = 0x8903_023c_3742_bcadu64;
        macro_rules! check {
            ($t:ty, $mask:expr, $operation:literal, $opcode:expr) => {
                for mode in 0..4 {
                    let control = 0x1f80u32 | ([0, 2, 1, 3][mode as usize] << 13);
                    for _ in 0..256 {
                        let (ab, bb) = (next(&mut seed) & $mask, next(&mut seed) & $mask);
                        let (a, b) = (<$t>::from_bits(ab as _), <$t>::from_bits(bb as _));
                        let mut host = a;
                        let (mut saved, mut raised) = (0u32, 0u32);
                        unsafe {
                            std::arch::asm!("stmxcsr [{saved}]", "ldmxcsr [{control}]", $operation,
                                "stmxcsr [{raised}]", "ldmxcsr [{saved}]",
                                saved = in(reg) &mut saved, control = in(reg) &control, raised = in(reg) &mut raised,
                                result = inout(xmm_reg) host, b = in(xmm_reg) b, options(nostack));
                        }
                        let mut fpsr = 0;
                        let result = binary_controlled(a, b, $opcode, DN | (mode << 22), &mut fpsr).unwrap();
                        assert_eq!(result.to_bits(), host.to_bits(), "{} opcode {} mode {mode}, a={ab:x} b={bb:x}", stringify!($t), $opcode);
                        // Overflow and inexact are IEEE flags shared by both
                        // ISAs. Underflow differs at the pre-rounding boundary
                        // and is checked analytically by the Arm-specific test.
                        let expected_flags = (((raised as u64 >> 3) & 1) << 2)
                            | (((raised as u64 >> 5) & 1) << 4);
                        assert_eq!(fpsr & (OFC | IXC), expected_flags);
                        let mut after = 0u32;
                        unsafe { std::arch::asm!("stmxcsr [{out}]", out = in(reg) &mut after, options(nostack)); }
                        assert_eq!(after, saved);
                    }
                }
            }
        }
        check!(f32, 0xff7f_ffffu64, "addss {result}, {b}", 2);
        check!(f32, 0xff7f_ffffu64, "subss {result}, {b}", 3);
        check!(f32, 0xff7f_ffffu64, "mulss {result}, {b}", 0);
        check!(f64, 0xffef_ffff_ffff_ffffu64, "addsd {result}, {b}", 2);
        check!(f64, 0xffef_ffff_ffff_ffffu64, "subsd {result}, {b}", 3);
        check!(f64, 0xffef_ffff_ffff_ffffu64, "mulsd {result}, {b}", 0);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn fma_matches_hardware_all_rounding_modes_without_touching_mxcsr() {
        if !std::is_x86_feature_detected!("avx") || !std::is_x86_feature_detected!("fma") { return; }
        // Hardware is an independent oracle for finite IEEE rounding. Arm NaN
        // priority, FZ and before-rounding tininess are tested separately above.
        fn next(state: &mut u64) -> u64 {
            *state ^= *state << 13; *state ^= *state >> 7; *state ^= *state << 17; *state
        }
        let mut seed = 0x0ef1_b173_e73c_492bu64;
        for mode in 0..4 {
            let rc = [0, 2, 1, 3][mode as usize];
            let control = 0x1f80u32 | (rc << 13);
            for _ in 0..1024 {
                let bits = [next(&mut seed), next(&mut seed), next(&mut seed)];
                let [a, n, m] = bits.map(|v| f64::from_bits(v & !0x0010_0000_0000_0000));
                let mut host = n;
                let (mut saved, mut raised) = (0u32, 0u32);
                unsafe {
                    std::arch::asm!("stmxcsr [{saved}]", "ldmxcsr [{control}]", "vfmadd213sd {result}, {m}, {a}",
                        "stmxcsr [{raised}]", "ldmxcsr [{saved}]",
                        saved = in(reg) &mut saved, control = in(reg) &control, raised = in(reg) &mut raised,
                        result = inout(xmm_reg) host, m = in(xmm_reg) m, a = in(xmm_reg) a, options(nostack));
                }
                let mut fpsr = 0;
                assert_eq!(fmla_controlled(a, n, m, DN | (mode << 22), &mut fpsr).to_bits(), host.to_bits(), "f64 mode {mode}, {bits:x?}");
                let mut after = 0u32;
                unsafe { std::arch::asm!("stmxcsr [{out}]", out = in(reg) &mut after, options(nostack)); }
                assert_eq!(after, saved);
                let [a, n, m] = bits.map(|v| f32::from_bits(v as u32 & !0x0080_0000));
                let mut host = n;
                unsafe {
                    std::arch::asm!("stmxcsr [{saved}]", "ldmxcsr [{control}]", "vfmadd213ss {result}, {m}, {a}",
                        "stmxcsr [{raised}]", "ldmxcsr [{saved}]",
                        saved = in(reg) &mut saved, control = in(reg) &control, raised = in(reg) &mut raised,
                        result = inout(xmm_reg) host, m = in(xmm_reg) m, a = in(xmm_reg) a, options(nostack));
                }
                fpsr = 0;
                assert_eq!(fmla_controlled(a, n, m, DN | (mode << 22), &mut fpsr).to_bits(), host.to_bits(), "f32 mode {mode}, {bits:x?}");
                unsafe { std::arch::asm!("stmxcsr [{out}]", out = in(reg) &mut after, options(nostack)); }
                assert_eq!(after, saved);
            }
        }
    }
}
