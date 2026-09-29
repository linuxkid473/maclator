//! Guest memory access.
//!
//! Guest virtual addresses are identical to host virtual addresses: the guest
//! image, stacks and heap live directly in the Maclator process. The only
//! exception is the arm64 commpage, which lives at fixed addresses that may
//! not be mappable on the host (Rosetta reserves that range). When it can't be
//! mapped in place, accesses are redirected to a host copy.

use std::sync::atomic::{AtomicU64, Ordering};

/// arm64 `_COMM_PAGE64_RO_ADDRESS`.
pub const COMMPAGE_RO: u64 = 0x0000_000F_FFFF_4000;
/// arm64 `_COMM_PAGE64_BASE_ADDRESS` (read/write data page as seen by the kernel).
pub const COMMPAGE_RW: u64 = 0x0000_000F_FFFF_C000;
/// Span that covers both commpage pages (16K each).
pub const COMMPAGE_SPAN_START: u64 = COMMPAGE_RO;
pub const COMMPAGE_SPAN_END: u64 = COMMPAGE_RW + 0x4000;

/// Host address that replaces `COMMPAGE_SPAN_START`, or 0 when the commpage is
/// mapped at its architectural address.
pub static COMMPAGE_REDIRECT: AtomicU64 = AtomicU64::new(0);

#[inline(always)]
pub fn host(addr: u64) -> *mut u8 {
    // Fast path: one compare on the high bits.
    if (addr >> 16) == 0xF_FFFF {
        let r = COMMPAGE_REDIRECT.load(Ordering::Relaxed);
        if r != 0 && addr >= COMMPAGE_SPAN_START {
            return (r + (addr - COMMPAGE_SPAN_START)) as *mut u8;
        }
    }
    addr as *mut u8
}

// ---- write logging (JIT verification) ----

use std::cell::{Cell, RefCell};

thread_local! {
    static LOGGING: Cell<bool> = const { Cell::new(false) };
    /// (address, previous 16 bytes, length)
    static WLOG: RefCell<Vec<(u64, [u8; 16], usize)>> = const { RefCell::new(Vec::new()) };
}

/// Start recording the prior contents of every guest write on this thread.
pub fn log_start() {
    WLOG.with(|l| l.borrow_mut().clear());
    LOGGING.with(|f| f.set(true));
}

/// Stop recording and return the log (in write order).
pub fn log_stop() -> Vec<(u64, [u8; 16], usize)> {
    LOGGING.with(|f| f.set(false));
    WLOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

/// Global switch so the common path costs a single relaxed load.
pub static LOGGING_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[inline(always)]
fn note_write(addr: u64, len: usize) {
    if LOGGING_ENABLED.load(Ordering::Relaxed) && LOGGING.with(|f| f.get()) {
        let mut old = [0u8; 16];
        unsafe { std::ptr::copy_nonoverlapping(host(addr), old.as_mut_ptr(), len) };
        WLOG.with(|l| l.borrow_mut().push((addr, old, len)));
    }
}

macro_rules! rw {
    ($r:ident, $w:ident, $t:ty) => {
        #[inline(always)]
        pub unsafe fn $r(addr: u64) -> $t {
            std::ptr::read_unaligned(host(addr) as *const $t)
        }
        #[inline(always)]
        pub unsafe fn $w(addr: u64, v: $t) {
            note_write(addr, std::mem::size_of::<$t>());
            std::ptr::write_unaligned(host(addr) as *mut $t, v)
        }
    };
}
rw!(r8, w8, u8);
rw!(r16, w16, u16);
rw!(r32, w32, u32);
rw!(r64, w64, u64);
rw!(r128, w128, u128);

/// Read a little-endian value of `size` bytes (1,2,4,8), zero-extended.
#[inline(always)]
pub unsafe fn read_sized(addr: u64, size: u32) -> u64 {
    match size {
        1 => r8(addr) as u64,
        2 => r16(addr) as u64,
        4 => r32(addr) as u64,
        _ => r64(addr),
    }
}

#[inline(always)]
pub unsafe fn write_sized(addr: u64, size: u32, v: u64) {
    match size {
        1 => w8(addr, v as u8),
        2 => w16(addr, v as u16),
        4 => w32(addr, v as u32),
        _ => w64(addr, v),
    }
}

/// Atomic compare-and-swap of `size` bytes; returns the old value.
#[inline(always)]
pub unsafe fn cas_sized(addr: u64, size: u32, expected: u64, new: u64) -> u64 {
    use std::sync::atomic::*;
    note_write(addr, size as usize);
    let p = host(addr);
    match size {
        1 => match (*(p as *const AtomicU8)).compare_exchange(expected as u8, new as u8, Ordering::SeqCst, Ordering::SeqCst) { Ok(v) | Err(v) => v as u64 },
        2 => match (*(p as *const AtomicU16)).compare_exchange(expected as u16, new as u16, Ordering::SeqCst, Ordering::SeqCst) { Ok(v) | Err(v) => v as u64 },
        4 => match (*(p as *const AtomicU32)).compare_exchange(expected as u32, new as u32, Ordering::SeqCst, Ordering::SeqCst) { Ok(v) | Err(v) => v as u64 },
        _ => match (*(p as *const AtomicU64)).compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst) { Ok(v) | Err(v) => v },
    }
}

/// 128-bit compare-and-swap (for CASP / STXP). Returns old value.
#[inline(always)]
pub unsafe fn cas128(addr: u64, expected: u128, new: u128) -> u128 {
    note_write(addr, 16);
    let p = host(addr) as *mut u128;
    #[cfg(target_arch = "x86_64")]
    {
        let mut lo = expected as u64;
        let mut hi = (expected >> 64) as u64;
        let nlo = new as u64;
        let nhi = (new >> 64) as u64;
        // rbx is reserved by LLVM, so swap it in and out manually.
        std::arch::asm!(
            "xchg {nlo}, rbx",
            "lock cmpxchg16b [{p}]",
            "mov rbx, {nlo}",
            p = in(reg) p,
            nlo = inout(reg) nlo => _,
            in("rcx") nhi,
            inout("rax") lo,
            inout("rdx") hi,
        );
        (lo as u128) | ((hi as u128) << 64)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // Not used for correctness-critical paths on non-x86 test hosts.
        let old = std::ptr::read_volatile(p);
        if old == expected {
            std::ptr::write_volatile(p, new);
        }
        old
    }
}

/// Atomic read-modify-write for LSE ops. `op` receives the old value and
/// returns the new one. Implemented as a CAS loop.
#[inline(always)]
pub unsafe fn rmw_sized(addr: u64, size: u32, mut op: impl FnMut(u64) -> u64) -> u64 {
    let mask = if size == 8 { u64::MAX } else { (1u64 << (size * 8)) - 1 };
    let mut old = read_sized(addr, size);
    loop {
        let new = op(old) & mask;
        let seen = cas_sized(addr, size, old, new);
        if seen == old {
            return old;
        }
        old = seen;
    }
}
