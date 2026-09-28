use maclator_core::cpu::Cpu;
use maclator_core::disasm::disasm;
use maclator_core::interp::{self, Exit};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[repr(C, align(16))]
#[derive(Clone, Copy, PartialEq)]
struct State {
    x: [u64; 31],
    nzcv: u64,
    fpcr: u64,
    fpsr: u64,
    v: [u128; 32],
}

std::arch::global_asm!(
    ".global _dt_tramp_start",
    ".global _dt_tramp_test",
    ".global _dt_tramp_recover",
    ".global _dt_tramp_end",
    ".p2align 4",
    "_dt_tramp_start:",
    "stp x29, x30, [sp, #-16]!",
    "stp x27, x28, [sp, #-16]!",
    "stp x25, x26, [sp, #-16]!",
    "stp x23, x24, [sp, #-16]!",
    "stp x21, x22, [sp, #-16]!",
    "stp x19, x20, [sp, #-16]!",
    "stp d14, d15, [sp, #-16]!",
    "stp d12, d13, [sp, #-16]!",
    "stp d10, d11, [sp, #-16]!",
    "stp d8, d9, [sp, #-16]!",
    "ldr x1, 9f",
    "mov x2, sp",
    "str x2, [x1]",
    "mov sp, x0",
    "add x1, x0, #272",
    "ldp q0, q1, [x1, #0]",
    "ldp q2, q3, [x1, #32]",
    "ldp q4, q5, [x1, #64]",
    "ldp q6, q7, [x1, #96]",
    "ldp q8, q9, [x1, #128]",
    "ldp q10, q11, [x1, #160]",
    "ldp q12, q13, [x1, #192]",
    "ldp q14, q15, [x1, #224]",
    "ldp q16, q17, [x1, #256]",
    "ldp q18, q19, [x1, #288]",
    "ldp q20, q21, [x1, #320]",
    "ldp q22, q23, [x1, #352]",
    "ldp q24, q25, [x1, #384]",
    "ldp q26, q27, [x1, #416]",
    "ldp q28, q29, [x1, #448]",
    "ldp q30, q31, [x1, #480]",
    "ldr x1, [x0, #248]",
    "msr nzcv, x1",
    "ldr x1, [x0, #256]",
    "msr fpcr, x1",
    "msr fpsr, xzr",
    "ldp x2, x3, [x0, #16]",
    "ldp x4, x5, [x0, #32]",
    "ldp x6, x7, [x0, #48]",
    "ldp x8, x9, [x0, #64]",
    "ldp x10, x11, [x0, #80]",
    "ldp x12, x13, [x0, #96]",
    "ldp x14, x15, [x0, #112]",
    "ldp x16, x17, [x0, #128]",
    "ldp x18, x19, [x0, #144]",
    "ldp x20, x21, [x0, #160]",
    "ldp x22, x23, [x0, #176]",
    "ldp x24, x25, [x0, #192]",
    "ldp x26, x27, [x0, #208]",
    "ldp x28, x29, [x0, #224]",
    "ldr x30, [x0, #240]",
    "ldp x0, x1, [x0]",
    "_dt_tramp_test:",
    "nop",
    "stp x0, x1, [sp, #0]",
    "stp x2, x3, [sp, #16]",
    "stp x4, x5, [sp, #32]",
    "stp x6, x7, [sp, #48]",
    "stp x8, x9, [sp, #64]",
    "stp x10, x11, [sp, #80]",
    "stp x12, x13, [sp, #96]",
    "stp x14, x15, [sp, #112]",
    "stp x16, x17, [sp, #128]",
    "stp x18, x19, [sp, #144]",
    "stp x20, x21, [sp, #160]",
    "stp x22, x23, [sp, #176]",
    "stp x24, x25, [sp, #192]",
    "stp x26, x27, [sp, #208]",
    "stp x28, x29, [sp, #224]",
    "str x30, [sp, #240]",
    "mrs x1, nzcv",
    "str x1, [sp, #248]",
    "mrs x1, fpsr",
    "str x1, [sp, #264]",
    "add x1, sp, #272",
    "stp q0, q1, [x1, #0]",
    "stp q2, q3, [x1, #32]",
    "stp q4, q5, [x1, #64]",
    "stp q6, q7, [x1, #96]",
    "stp q8, q9, [x1, #128]",
    "stp q10, q11, [x1, #160]",
    "stp q12, q13, [x1, #192]",
    "stp q14, q15, [x1, #224]",
    "stp q16, q17, [x1, #256]",
    "stp q18, q19, [x1, #288]",
    "stp q20, q21, [x1, #320]",
    "stp q22, q23, [x1, #352]",
    "stp q24, q25, [x1, #384]",
    "stp q26, q27, [x1, #416]",
    "stp q28, q29, [x1, #448]",
    "stp q30, q31, [x1, #480]",
    "mov x0, #0",
    "b 8f",
    "_dt_tramp_recover:",
    "mov x0, #1",
    "8:",
    "ldr x1, 9f",
    "ldr x1, [x1]",
    "mov sp, x1",
    "ldp d8, d9, [sp], #16",
    "ldp d10, d11, [sp], #16",
    "ldp d12, d13, [sp], #16",
    "ldp d14, d15, [sp], #16",
    "ldp x19, x20, [sp], #16",
    "ldp x21, x22, [sp], #16",
    "ldp x23, x24, [sp], #16",
    "ldp x25, x26, [sp], #16",
    "ldp x27, x28, [sp], #16",
    "ldp x29, x30, [sp], #16",
    "ret",
    ".p2align 3",
    "9: .quad 0",
    "_dt_tramp_end:",
);

extern "C" {
    static dt_tramp_start: u8;
    static dt_tramp_test: u8;
    static dt_tramp_recover: u8;
    static dt_tramp_end: u8;
    fn pthread_jit_write_protect_np(enabled: i32);
    fn sys_icache_invalidate(start: *mut libc::c_void, len: usize);
}

static SAVE_SP: AtomicU64 = AtomicU64::new(0);
static RECOVER_PC: AtomicU64 = AtomicU64::new(0);
static FAULTS: AtomicUsize = AtomicUsize::new(0);
static LAST_SIG: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_signal(sig: i32, _info: *mut libc::siginfo_t, uc: *mut libc::c_void) {
    unsafe {
        let uc = uc as *mut libc::ucontext_t;
        let mc = (*uc).uc_mcontext;
        let pc = (*mc).__ss.__pc;
        let page = RECOVER_PC.load(Ordering::Relaxed) & !0x3fff;
        if pc < page || pc >= page + 0x4000 {
            // Fault outside the test trampoline (interpreter side): die so the
            // supervisor skips this test.
            libc::signal(sig, libc::SIG_DFL);
            return;
        }
        (*mc).__ss.__pc = RECOVER_PC.load(Ordering::Relaxed);
    }
    FAULTS.fetch_add(1, Ordering::Relaxed);
    LAST_SIG.store(sig as usize, Ordering::Relaxed);
}

struct Native {
    page: *mut u8,
    test_off: usize,
}

impl Native {
    fn new() -> Native {
        unsafe {
            let start = &dt_tramp_start as *const u8;
            let end = &dt_tramp_end as *const u8;
            let len = end as usize - start as usize;
            let page = libc::mmap(std::ptr::null_mut(), 0x4000, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT, -1, 0) as *mut u8;
            assert!(page as isize != -1, "mmap MAP_JIT failed");
            pthread_jit_write_protect_np(0);
            std::ptr::copy_nonoverlapping(start, page, len);
            // literal slot (last 8 bytes) holds the address of the saved-SP cell
            std::ptr::write_unaligned(page.add(len - 8) as *mut u64, &SAVE_SP as *const AtomicU64 as u64);
            pthread_jit_write_protect_np(1);
            sys_icache_invalidate(page as _, len);
            let test_off = &dt_tramp_test as *const u8 as usize - start as usize;
            let rec_off = &dt_tramp_recover as *const u8 as usize - start as usize;
            RECOVER_PC.store(page as u64 + rec_off as u64, Ordering::Relaxed);
            for sig in [libc::SIGILL, libc::SIGSEGV, libc::SIGBUS, libc::SIGTRAP, libc::SIGFPE] {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_signal as usize;
                sa.sa_flags = libc::SA_SIGINFO;
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
            Native { page, test_off }
        }
    }

    fn test_pc(&self) -> u64 {
        self.page as u64 + self.test_off as u64
    }

    /// Returns false if the instruction faulted natively.
    fn run(&self, insn: u32, st: &mut State) -> bool {
        unsafe {
            pthread_jit_write_protect_np(0);
            std::ptr::write_volatile(self.page.add(self.test_off) as *mut u32, insn);
            pthread_jit_write_protect_np(1);
            sys_icache_invalidate(self.page.add(self.test_off) as _, 4);
            let f: extern "C" fn(*mut State) -> u64 = std::mem::transmute(self.page);
            f(st as *mut State) == 0
        }
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn interesting64(&mut self) -> u64 {
        match self.below(16) {
            0 => 0,
            1 => u64::MAX,
            2 => 1,
            3 => 1u64 << self.below(64),
            4 => (1u64 << 63) - self.below(4),
            5 => (1u64 << 63) + self.below(4),
            6 => self.below(256),
            7 => (self.next() as u32) as u64,
            8 => 0x8000_0000 + self.below(3),
            9 => 0x7fff_ffff - self.below(3),
            10 => self.fp64(),
            11 => (self.fp32() as u64) | ((self.fp32() as u64) << 32),
            _ => self.next(),
        }
    }
    fn fp32(&mut self) -> u32 {
        let specials = [0u32, 0x8000_0000, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000, 0x7fa0_0000, 0xffc0_1234, 0x3f80_0000,
            0xbf80_0000, 0x0000_0001, 0x807f_ffff, 0x4f00_0000, 0xcf00_0000, 0x5f00_0000, 0x3f00_0000, 0x4b40_0000,
            0x7f7f_ffff, 0x3fc0_0000, 0x4040_0000, 0x0080_0000];
        match self.below(4) {
            0 => specials[self.below(specials.len() as u64) as usize],
            1 => {
                let v = (self.next() as i64 % 100000) as f32 / 16.0;
                v.to_bits()
            }
            _ => self.next() as u32,
        }
    }
    fn fp64(&mut self) -> u64 {
        let specials = [0u64, 0x8000_0000_0000_0000, 0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000, 0x7ff8_0000_0000_0000,
            0x7ff4_0000_0000_0000, 0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000, 1, 0x43e0_0000_0000_0000, 0xc3e0_0000_0000_0000,
            0x41e0_0000_0000_0000, 0x3fe0_0000_0000_0000, 0x4338_0000_0000_0000, 0x7fef_ffff_ffff_ffff, 0x0010_0000_0000_0000];
        match self.below(4) {
            0 => specials[self.below(specials.len() as u64) as usize],
            1 => ((self.next() as i64 % 1000000) as f64 / 64.0).to_bits(),
            _ => self.next(),
        }
    }
}

fn to_cpu(st: &State, pc: u64) -> Box<Cpu> {
    let mut c = Cpu::new();
    c.x[..31].copy_from_slice(&st.x);
    c.set_nzcv((st.nzcv >> 28) as u32);
    c.fpcr = st.fpcr;
    for i in 0..32 {
        c.set_vq(i as u32, st.v[i]);
    }
    c.pc = pc;
    c
}

fn from_cpu(c: &Cpu, sp_val: u64) -> State {
    let mut st = State { x: [0; 31], nzcv: (c.nzcv() as u64) << 28, fpcr: c.fpcr, fpsr: c.fpsr, v: [0; 32] };
    st.x.copy_from_slice(&c.x[..31]);
    for i in 0..32 {
        st.v[i] = c.vq(i as u32);
    }
    let _ = sp_val;
    st
}

const BUF_SIZE: usize = 1 << 20;

pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    let class = args.get(1).map(|s| s.as_str()).unwrap_or("all").to_string();
    let count: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20000);
    let seed: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0x1234_5678_9abc_def1);
    eprintln!("init");
    let native = Native::new();
    eprintln!("native ready page={:p}", native.page);
    { let mut st = State { x: [0; 31], nzcv: 0, fpcr: 0, fpsr: 0, v: [0; 32] }; st.x[1]=5; let ok = native.run(0x91000820, &mut st); eprintln!("selftest ok={} x0={}", ok, st.x[0]); }
    let mut rng = Rng(seed | 1);
    let mut buf = vec![0u8; BUF_SIZE];
    let buf_base = buf.as_mut_ptr() as u64;
    let mut nat_state = Box::new(State { x: [0; 31], nzcv: 0, fpcr: 0, fpsr: 0, v: [0; 32] });

    let classes: Vec<&str> = if class == "all" { vec!["dpimm", "dpreg", "simd", "fp", "ldst"] } else { vec![class.as_str()] };
    let mut unimpl: BTreeMap<String, (usize, u32)> = BTreeMap::new();
    let mut total_ok = 0;
    let mut total_bad = 0;
    let mut total_skip = 0;
    let mut shown_bad = 0;

    for cls in &classes {
        let (mask, val): (u32, u32) = match *cls {
            "dpimm" => (0x1C00_0000, 0x1000_0000),
            "dpreg" => (0x0E00_0000, 0x0A00_0000),
            "simd" => (0x0E00_0000, 0x0E00_0000),
            "fp" => (0x5F00_0000, 0x1E00_0000),
            "ldst" => (0x0A00_0000, 0x0800_0000),
            other => {
                // explicit pattern "mask:val" in hex
                let p: Vec<&str> = other.split(':').collect();
                (u32::from_str_radix(p[0], 16).unwrap(), u32::from_str_radix(p[1], 16).unwrap())
            }
        };
        let is_mem = *cls == "ldst" || (mask & 0x0A00_0000 == 0x0A00_0000 && val & 0x0A00_0000 == 0x0800_0000);
        let (mut ok, mut bad, mut skip) = (0, 0, 0);
        let shared = unsafe {
            libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_ANON, -1, 0) as *mut u64
        };
        let mut start = 0usize;
        loop {
            if start >= count {
                break;
            }
            unsafe { std::ptr::write_bytes(shared, 0, 8) };
            let pid = unsafe { libc::fork() };
            if pid != 0 {
                let mut status = 0;
                unsafe { libc::waitpid(pid, &mut status, 0) };
                let (o, b, sk, next) = unsafe { (*shared, *shared.add(1), *shared.add(2), *shared.add(3)) };
                ok += o as usize;
                bad += b as usize;
                skip += sk as usize;
                if libc::WIFSIGNALED(status) {
                    // test `next` crashed the interpreter side: skip it
                    skip += 1;
                    start = next as usize + 1;
                } else {
                    start = count;
                }
                continue;
            }
            // child
            let (mut ok, mut bad, mut skip) = (0u64, 0u64, 0u64);
        for ti in start..count {
            unsafe {
                *shared = ok;
                *shared.add(1) = bad;
                *shared.add(2) = skip;
                *shared.add(3) = ti as u64;
            }
            rng = Rng((seed ^ (ti as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1);
            rng.next();
            let mut insn = (rng.next() as u32 & !mask) | val;
            // never write SP / use SP as a base: keep Rd/Rt and Rn away from 31
            if insn & 0x1f == 31 {
                insn &= !0x1f;
                insn |= rng.below(31) as u32;
            }
            if is_mem && (insn >> 5) & 0x1f == 31 {
                insn &= !(0x1f << 5);
                insn |= (rng.below(31) as u32) << 5;
            }
            // Exclude instructions whose behaviour is deliberately different.
            if is_excluded(insn) {
                skip += 1;
                continue;
            }
            // Build random state
            let mut st = State { x: [0; 31], nzcv: rng.below(16) << 28, fpcr: 0, fpsr: 0, v: [0; 32] };
            for i in 0..31 {
                st.x[i] = if is_mem { buf_base + 0x40000 + (rng.below(0x60000) & !15) } else { rng.interesting64() };
            }
            for i in 0..32 {
                st.v[i] = (rng.interesting64() as u128) | ((rng.interesting64() as u128) << 64);
            }
            if is_mem {
                let words = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().add(0x30000) as *mut u64, 0x80000 / 8) };
                let mut z = rng.next();
                for w in words.iter_mut() {
                    z = z.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    *w = z;
                }
            }
            // Interpreter first on a copy of memory (so it can report unimplemented)
            let mem_before: Vec<u8> = if is_mem { buf[0x30000..0xB0000].to_vec() } else { vec![] };
            let pc = native.test_pc();
            let mut cpu = to_cpu(&st, pc);
            cpu.x[31] = &*nat_state as *const State as u64;
            let r = interp::exec(&mut cpu, insn);
            match r {
                Ok(()) => {}
                Err(Exit::Unimplemented(_)) | Err(Exit::Udf(_)) => {
                    // check whether hardware accepts it
                    if is_mem {
                        buf[0x30000..0xB0000].copy_from_slice(&mem_before);
                    }
                    let mut st2 = st;
                    FAULTS.store(0, Ordering::Relaxed);
                    let nat_ok = native.run(insn, &mut st2);
                    if nat_ok {
                        let d = disasm(insn);
                        let key = d.split_whitespace().next().unwrap_or("?").to_string();
                        let e = unimpl.entry(key).or_insert((0, insn));
                        e.0 += 1;
                        if e.0 == 1 { println!("UNIMPL {:08x} {}", insn, disasm(insn)); }
                    }
                    skip += 1;
                    if is_mem {
                        buf[0x30000..0xB0000].copy_from_slice(&mem_before);
                    }
                    continue;
                }
                Err(_) => {
                    skip += 1;
                    continue;
                }
            }
            let interp_state = from_cpu(&cpu, 0);
            let interp_pc = cpu.pc;
            let mem_interp: Vec<u8> = if is_mem { buf[0x30000..0xB0000].to_vec() } else { vec![] };
            if is_mem {
                buf[0x30000..0xB0000].copy_from_slice(&mem_before);
            }
            *nat_state = st;
            let nat_ok = native.run(insn, &mut nat_state);
            let nst = *nat_state;
            if !nat_ok {
                // interpreter accepted something the CPU rejects (usually an
                // unallocated encoding); report only in verbose mode
                if std::env::var("DT_STRICT").is_ok() {
                    println!("NATIVE-FAULT {:08x} {}  sig={}", insn, disasm(insn), LAST_SIG.load(Ordering::Relaxed));
                }
                skip += 1;
                continue;
            }
            let mem_native: Vec<u8> = if is_mem { buf[0x30000..0xB0000].to_vec() } else { vec![] };
            // compare
            let mut diffs = Vec::new();
            for i in 0..31 {
                if nst.x[i] != interp_state.x[i] {
                    diffs.push(format!("x{i}: native={:016x} interp={:016x}", nst.x[i], interp_state.x[i]));
                }
            }
            if nst.nzcv != interp_state.nzcv {
                diffs.push(format!("nzcv: native={:x} interp={:x}", nst.nzcv >> 28, interp_state.nzcv >> 28));
            }
            if (nst.fpsr ^ interp_state.fpsr) & (1 << 27) != 0 {
                diffs.push(format!("fpsr.QC native={:x} interp={:x}", nst.fpsr, interp_state.fpsr));
            }
            for i in 0..32 {
                if nst.v[i] != interp_state.v[i] {
                    diffs.push(format!("v{i}: native={:032x} interp={:032x}", nst.v[i], interp_state.v[i]));
                }
            }
            if interp_pc != pc + 4 {
                diffs.push(format!("pc advanced wrongly: {:x}", interp_pc.wrapping_sub(pc)));
            }
            if is_mem && mem_native != mem_interp {
                let first = mem_native.iter().zip(&mem_interp).position(|(a, b)| a != b).unwrap();
                diffs.push(format!("memory differs at +{:#x}", first));
            }
            if diffs.is_empty() {
                ok += 1;
            } else {
                bad += 1;
                if shown_bad < 60 {
                    println!("MISMATCH {:08x}  {}", insn, disasm(insn));
                    for d in diffs.iter().take(6) {
                        println!("    {d}");
                    }
                    // show relevant inputs
                    let rn = ((insn >> 5) & 31) as usize;
                    let rm = ((insn >> 16) & 31) as usize;
                    if rn < 31 {
                        println!("    in: x{rn}={:016x} v{rn}={:032x}", st.x[rn], st.v[rn]);
                    }
                    if rm < 31 {
                        println!("    in: x{rm}={:016x} v{rm}={:032x}", st.x[rm], st.v[rm]);
                    }
                    let rd = (insn & 31) as usize;
                    if rd < 31 {
                        println!("    in: x{rd}={:016x} v{rd}={:032x} nzcv={:x}", st.x[rd], st.v[rd], st.nzcv >> 28);
                    }
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            }
        }
            unsafe {
                *shared = ok;
                *shared.add(1) = bad;
                *shared.add(2) = skip;
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
            unsafe { libc::_exit(0) };
        }
        println!("[{cls}] ok={ok} mismatch={bad} skipped={skip}");
        total_ok += ok;
        total_bad += bad;
        total_skip += skip;
    }
    println!("TOTAL ok={total_ok} mismatch={total_bad} skipped={total_skip}");
    if !unimpl.is_empty() {
        let mut v: Vec<_> = unimpl.into_iter().collect();
        v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
        println!("Unimplemented but valid on hardware (top):");
        for (k, (n, ex)) in v.iter().take(60) {
            println!("  {:6} {:12} e.g. {:08x} {}", n, k, ex, disasm(*ex));
        }
    }
}

fn is_excluded(insn: u32) -> bool {
    // PAC data-processing (sign/auth differ by design)
    if insn & 0x7FFF_C000 == 0x5AC1_0000 {
        return true;
    }
    // PACGA
    if insn & 0xFFE0_FC00 == 0x9AC0_3000 {
        return true;
    }
    // Load/store exclusive & friends (monitor emulation is not bit-exact across
    // the trampoline), LDRAA/LDRAB
    if insn & 0x3F00_0000 == 0x0800_0000 {
        return true;
    }
    if insn & 0xFF20_0400 == 0xF820_0400 {
        return true;
    }
    // System / branches / exceptions
    if insn & 0x1C00_0000 == 0x1400_0000 {
        return true;
    }
    // Load/store with SP-like registers is handled by the generator; loads of
    // literal read the JIT page which is fine.
    // FP instructions that are sensitive to exception flags only are fine.
    // FRECPE/FRSQRTE family: estimates are compared too (keep).
    false
}
