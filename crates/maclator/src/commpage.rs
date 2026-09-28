//! The arm64 commpage (osfmk/arm/cpu_capabilities.h).
//!
//! arm64 userland reads CPU capabilities, page size, CPU counts and time
//! parameters from two fixed pages. We build our own copy describing the
//! emulated CPU. When the architectural addresses can't be mapped (Rosetta
//! reserves that range) guest accesses are redirected to the copy.

use maclator_core::mem::{COMMPAGE_REDIRECT, COMMPAGE_RO, COMMPAGE_RW, COMMPAGE_SPAN_END, COMMPAGE_SPAN_START};
use std::sync::atomic::Ordering;

// Capability bits we can honour (see cpu_capabilities.h).
const K_CACHE64: u64 = 0x20;
const K_FAST_TLS: u64 = 0x80;
const K_HAS_ADVSIMD: u64 = 0x100;
const K_HAS_ADVSIMD_HPFPCVT: u64 = 0x200;
const K_HAS_VFP: u64 = 0x400;
const K_HAS_UC_NORMAL_MEMORY: u64 = 0x800;
const K_HAS_EVENT: u64 = 0x1000;
const K_HAS_FMA: u64 = 0x2000;
const K_HAS_FEAT_LSE: u64 = 0x0200_0000;
const K_HAS_CRC32: u64 = 0x0400_0000;
const K_HAS_FLAGM: u64 = 0x0000_0100_0000_0000;
const K_HAS_FLAGM2: u64 = 0x0000_0200_0000_0000;
const K_HAS_DOTPROD: u64 = 0x0000_0400_0000_0000;
const K_HAS_SB: u64 = 0x0000_2000_0000_0000;
const K_HAS_FRINTTS: u64 = 0x0000_4000_0000_0000;
const K_HAS_LRCPC: u64 = 0x0001_0000_0000_0000;
const K_HAS_LRCPC2: u64 = 0x0002_0000_0000_0000;
const K_HAS_JSCVT: u64 = 0x0004_0000_0000_0000;
const K_HAS_PAUTH: u64 = 0x0008_0000_0000_0000;
const K_HAS_DPB: u64 = 0x0010_0000_0000_0000;
const K_HAS_DPB2: u64 = 0x0020_0000_0000_0000;
const K_HAS_LSE2: u64 = 0x0040_0000_0000_0000;
const K_HAS_CSV2: u64 = 0x0080_0000_0000_0000;
const K_HAS_CSV3: u64 = 0x0100_0000_0000_0000;

/// CPUFAMILY_ARM_FIRESTORM_ICESTORM (Apple M1).
pub const CPUFAMILY: u32 = 0x1b58_8bb3;

pub struct CommPage {
    /// Host base of our 32K copy covering [COMMPAGE_RO, COMMPAGE_RW + 16K).
    pub base: u64,
    pub in_place: bool,
}

fn host_ncpu() -> u8 {
    let mut n: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    let name = b"hw.logicalcpu\0";
    unsafe {
        libc::sysctlbyname(name.as_ptr() as *const _, &mut n as *mut i32 as *mut _, &mut len, std::ptr::null_mut(), 0);
    }
    n.clamp(1, 64) as u8
}

fn host_memsize() -> u64 {
    let mut n: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let name = b"hw.memsize\0";
    unsafe {
        libc::sysctlbyname(name.as_ptr() as *const _, &mut n as *mut u64 as *mut _, &mut len, std::ptr::null_mut(), 0);
    }
    n
}

impl CommPage {
    pub fn install() -> CommPage {
        let span = (COMMPAGE_SPAN_END - COMMPAGE_SPAN_START) as usize;
        unsafe {
            // Try the architectural address first (as a hint, never clobbering).
            let p = libc::mmap(COMMPAGE_SPAN_START as *mut _, span, libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0);
            let (base, in_place) = if p as u64 == COMMPAGE_SPAN_START && std::env::var_os("MACLATOR_NO_COMMPAGE").is_none() {
                (COMMPAGE_SPAN_START, true)
            } else {
                if p != libc::MAP_FAILED {
                    libc::munmap(p, span);
                }
                let q = libc::mmap(std::ptr::null_mut(), span, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0);
                assert!(q != libc::MAP_FAILED, "cannot allocate commpage copy");
                (q as u64, false)
            };
            let cp = CommPage { base, in_place };
            cp.fill();
            if !in_place {
                COMMPAGE_REDIRECT.store(base, Ordering::SeqCst);
            }
            cp
        }
    }

    fn rw(&self, off: u64) -> *mut u8 {
        (self.base + (COMMPAGE_RW - COMMPAGE_SPAN_START) + off) as *mut u8
    }
    fn ro(&self, off: u64) -> *mut u8 {
        (self.base + (COMMPAGE_RO - COMMPAGE_SPAN_START) + off) as *mut u8
    }

    unsafe fn fill(&self) {
        let ncpu = host_ncpu();
        let caps: u64 = K_CACHE64 | K_FAST_TLS | K_HAS_ADVSIMD | K_HAS_ADVSIMD_HPFPCVT | K_HAS_VFP | K_HAS_UC_NORMAL_MEMORY
            | K_HAS_EVENT | K_HAS_FMA | K_HAS_FEAT_LSE | K_HAS_CRC32 | K_HAS_FLAGM | K_HAS_FLAGM2 | K_HAS_DOTPROD
            | K_HAS_SB | K_HAS_FRINTTS | K_HAS_LRCPC | K_HAS_LRCPC2 | K_HAS_JSCVT | K_HAS_PAUTH | K_HAS_DPB | K_HAS_DPB2
            | K_HAS_LSE2 | K_HAS_CSV2 | K_HAS_CSV3 | ((ncpu as u64) << 16) | if ncpu == 1 { 0x8000 } else { 0 };
        std::ptr::copy_nonoverlapping(b"commpage 64-bit\0".as_ptr(), self.rw(0), 16);
        (self.rw(0x10) as *mut u64).write_unaligned(caps);
        (self.rw(0x1e) as *mut u16).write_unaligned(3);
        (self.rw(0x20) as *mut u32).write_unaligned(caps as u32);
        *self.rw(0x22) = ncpu;
        // Page shifts: the host uses 4K pages, report that to the guest.
        *self.rw(0x24) = 12;
        *self.rw(0x25) = 12;
        *self.ro(0x24) = 12;
        *self.ro(0x25) = 12;
        (self.rw(0x26) as *mut u16).write_unaligned(64);
        *self.rw(0x2f) = 1; // clusters
        *self.rw(0x34) = ncpu;
        *self.rw(0x35) = ncpu;
        *self.rw(0x36) = ncpu;
        *self.rw(0x37) = 12;
        *self.ro(0x37) = 12;
        (self.rw(0x38) as *mut u64).write_unaligned(host_memsize());
        (self.rw(0x80) as *mut u32).write_unaligned(CPUFAMILY);
        // No user-readable timebase: mach_absolute_time() traps and we answer
        // with the host clock.
        *self.rw(0x90) = 0;
        *self.rw(0x91) = 0;
        *self.rw(0xc8) = 0; // approx time unsupported
    }
}
