//! JIT runtime: code cache, dispatcher, block chaining, indirect-branch
//! cache, invalidation. Blocks can also come from the AOT cache (`aot.rs`).

pub mod emit;
pub mod selftest;

use emit::{StubKind, Unit, EXIT_CONTINUE, EXIT_PENDING, EXIT_SVC, HELPER_COMMPAGE, HELPER_INTERP};
use iced_x86::BlockEncoderOptions;
use maclator_core::cpu::Cpu;
use maclator_core::interp::{self, Exit};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

/// Entries in each thread's indirect-branch translation cache.
pub const IBTC_SIZE: usize = 8192;

const CODE_CACHE_SIZE: u64 = 4 << 30;

std::arch::global_asm!(
    ".globl _maclator_jit_enter",
    ".globl _maclator_jit_exit",
    ".p2align 4",
    "_maclator_jit_enter:",
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "sub rsp, 8",
    "mov r15, rdi",
    "jmp rsi",
    ".p2align 4",
    "_maclator_jit_exit:",
    "add rsp, 8",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
);

extern "C" {
    fn maclator_jit_enter(cpu: *mut Cpu, code: u64);
    fn maclator_jit_exit();
}

pub struct CodeCache {
    base: u64,
    end: u64,
    /// Code grows up from `base`.
    code_ptr: AtomicU64,
    /// Chaining slots grow down from `end`.
    slot_ptr: AtomicU64,
}

static CODE: OnceLock<CodeCache> = OnceLock::new();
/// guest pc -> host code
static BLOCKS: RwLock<Option<HashMap<u64, u64>>> = RwLock::new(None);
/// (slot address, default continuation) for every patchable exit.
static SLOTS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
static TRANSLATE: Mutex<()> = Mutex::new(());
/// 16 KiB guest pages that hold (or may hold) translated code, so that
/// executable-permission changes and icache flushes elsewhere are free.
static TRANSLATED_PAGES: Mutex<Option<std::collections::HashSet<u64>>> = Mutex::new(None);

fn note_pages(pc: u64) {
    let mut g = TRANSLATED_PAGES.lock().unwrap();
    let set = g.get_or_insert_with(std::collections::HashSet::new);
    set.insert(pc >> 14);
    // A block can run past its first page.
    set.insert((pc + emit::MAX_BLOCK_INSNS as u64 * 4) >> 14);
}
static EPOCH: AtomicU64 = AtomicU64::new(0);
pub static JIT_ENABLED: AtomicBool = AtomicBool::new(true);
static STATS_BLOCKS: AtomicU64 = AtomicU64::new(0);
static STATS_INSNS: AtomicU64 = AtomicU64::new(0);
static STATS_FALLBACK: AtomicU64 = AtomicU64::new(0);

static STATS_TRANSLATE_NS: AtomicU64 = AtomicU64::new(0);
static STATS_INTERP_BLOCKS: AtomicU64 = AtomicU64::new(0);
static STATS_JIT_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// Executions of a block in the interpreter before it is JIT-compiled.
fn hot_threshold() -> u32 {
    static T: OnceLock<u32> = OnceLock::new();
    *T.get_or_init(|| std::env::var("MACLATOR_JIT_THRESHOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(24))
}

thread_local! {
    static HOT: RefCell<HashMap<u64, u32>> = RefCell::new(HashMap::new());
    static PENDING: Cell<Option<Exit>> = const { Cell::new(None) };
    static IBTC: RefCell<Option<Box<[[u64; 2]]>>> = const { RefCell::new(None) };
    static MY_EPOCH: Cell<u64> = const { Cell::new(0) };
}

pub fn code_cache() -> &'static CodeCache {
    CODE.get_or_init(|| unsafe {
        let p = libc::mmap(std::ptr::null_mut(), CODE_CACHE_SIZE as usize, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0);
        assert!(p != libc::MAP_FAILED, "cannot allocate the JIT code cache: {}", std::io::Error::last_os_error());
        let base = p as u64;
        CodeCache { base, end: base + CODE_CACHE_SIZE, code_ptr: AtomicU64::new(base), slot_ptr: AtomicU64::new(base + CODE_CACHE_SIZE) }
    })
}

impl CodeCache {
    /// Reserve `len` bytes of code space (16-byte aligned).
    pub fn alloc(&self, len: u64) -> u64 {
        let len = (len + 15) & !15;
        let p = self.code_ptr.fetch_add(len, Ordering::SeqCst);
        if p + len > self.slot_ptr.load(Ordering::SeqCst) {
            panic!("maclator: JIT code cache exhausted");
        }
        p
    }
    pub fn cursor(&self) -> u64 {
        self.code_ptr.load(Ordering::SeqCst)
    }
    pub fn contains(&self, a: u64) -> bool {
        a >= self.base && a < self.end
    }
}

pub fn lookup(pc: u64) -> Option<u64> {
    BLOCKS.read().unwrap().as_ref().and_then(|m| m.get(&pc).copied())
}

/// Register externally produced code (AOT) for `pc`.
pub fn register_block(pc: u64, host: u64) {
    note_pages(pc);
    let mut g = BLOCKS.write().unwrap();
    g.get_or_insert_with(HashMap::new).entry(pc).or_insert(host);
}

// ---------------- helpers called from generated code ----------------

extern "C" fn helper_interp(cpu: *mut Cpu, insn: u32, pc: u64) -> u64 {
    let cpu = unsafe { &mut *cpu };
    cpu.pc = pc;
    match interp::exec(cpu, insn) {
        Ok(()) => 0,
        Err(e) => {
            PENDING.with(|p| p.set(Some(e)));
            cpu.exit_reason = EXIT_PENDING;
            1
        }
    }
}

extern "C" fn helper_commpage(addr: u64) -> u64 {
    maclator_core::mem::host(addr) as u64
}

pub fn commpage_checks() -> bool {
    maclator_core::mem::COMMPAGE_REDIRECT.load(Ordering::Relaxed) != 0
}

/// Read guest instruction words, verifying page readability once per page.
pub fn guest_reader() -> impl Fn(u64) -> Option<u32> {
    let last_ok = Cell::new(u64::MAX);
    move |a: u64| {
        let page = a & !0xfff;
        if last_ok.get() != page {
            let mut b = [0u8; 4];
            if !crate::guestmem::read(a, &mut b) {
                return None;
            }
            last_ok.set(page);
        }
        Some(unsafe { std::ptr::read_unaligned(a as *const u32) })
    }
}

// ---------------- translation ----------------

fn verify_mode() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var_os("MACLATOR_JIT_VERIFY").is_some())
}

fn emit_unit(pc: u64, slot_padding: usize) -> Unit {
    let mut u = Unit::new(StubKind::Patchable, commpage_checks());
    u.slot_padding = slot_padding;
    u.no_internal_jumps = verify_mode();
    let l = u.a.create_label();
    u.labels.insert(pc, l);
    let read = guest_reader();
    emit::emit_block(&mut u, pc, &read);
    u.finish();
    u
}

fn translate(pc: u64) -> u64 {
    let t0 = std::time::Instant::now();
    let r = translate_inner(pc);
    STATS_TRANSLATE_NS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
    r
}

fn translate_inner(pc: u64) -> u64 {
    let _g = TRANSLATE.lock().unwrap();
    if let Some(h) = lookup(pc) {
        return h;
    }
    let cc = code_cache();
    let ip = cc.cursor();
    // Assemble; if the slot table isn't 8-byte aligned, redo with padding so
    // slot patches are single aligned stores.
    let mut u = emit_unit(pc, 0);
    let mut res = u.a.assemble_options(ip, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).expect("assemble");
    if let Some(first) = u.first_slot {
        let at = res.label_ip(&first).expect("slot label");
        if at % 8 != 0 {
            let pad = (8 - (at % 8)) as usize;
            u = emit_unit(pc, pad);
            res = u.a.assemble_options(ip, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).expect("assemble");
        }
    }
    STATS_BLOCKS.fetch_add(1, Ordering::Relaxed);
    STATS_INSNS.fetch_add(u.insns_translated, Ordering::Relaxed);
    STATS_FALLBACK.fetch_add(u.insns_fallback, Ordering::Relaxed);
    let bytes = &res.inner.code_buffer;
    let at = cc.alloc(bytes.len() as u64);
    assert_eq!(at, ip, "code cache raced");
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len()) };
    let mut slots = SLOTS.lock().unwrap();
    for s in &u.finished_stubs {
        let cont = res.label_ip(&s.cont).expect("stub label");
        let slot = res.label_ip(&s.slot).expect("slot label");
        unsafe { std::ptr::write_volatile(slot as *mut u64, cont) };
        slots.push((slot, cont));
    }
    drop(slots);
    crate::aot::note_translated(pc);
    for &lpc in u.labels.keys() {
        note_pages(lpc);
    }
    let mut g = BLOCKS.write().unwrap();
    g.get_or_insert_with(HashMap::new).insert(pc, at);
    at
}

fn lookup_or_translate(pc: u64) -> u64 {
    lookup(pc).unwrap_or_else(|| translate(pc))
}

/// Drop every translation (self-modifying code, new executable mappings).
/// Threads currently inside old code leave it through their exit slots.
pub fn invalidate_range(addr: u64, len: u64) {
    let _g = TRANSLATE.lock().unwrap();
    {
        // Nothing translated on these pages: nothing to drop.
        let mut pg = TRANSLATED_PAGES.lock().unwrap();
        let Some(set) = pg.as_mut() else { return };
        let p0 = addr >> 14;
        let p1 = (addr.saturating_add(len.max(1)) - 1) >> 14;
        let n = p1 - p0 + 1;
        let hit = if n as usize <= set.len() { (p0..=p1).any(|p| set.contains(&p)) } else { set.iter().any(|&p| p >= p0 && p <= p1) };
        if !hit {
            return;
        }
        set.clear();
    }
    let has_any = BLOCKS.read().unwrap().as_ref().map(|m| !m.is_empty()).unwrap_or(false);
    if !has_any {
        return;
    }
    if std::env::var_os("MACLATOR_TRACE").is_some() {
        eprintln!("[maclator] invalidating translations ({:#x}+{:#x})", addr, len);
    }
    for (slot, cont) in SLOTS.lock().unwrap().iter() {
        unsafe { std::ptr::write_volatile(*slot as *mut u64, *cont) };
    }
    *BLOCKS.write().unwrap() = Some(HashMap::new());
    EPOCH.fetch_add(1, Ordering::SeqCst);
}

pub fn flush_profile() {
    crate::aot::save_profile();
    if std::env::var_os("MACLATOR_STATS").is_some() {
        eprintln!(
            "[maclator] jit: {} blocks, {} insns translated ({} via interpreter helper), {:.1} ms translating; {} block entries; {} blocks interpreted (cold)",
            STATS_BLOCKS.load(Ordering::Relaxed),
            STATS_INSNS.load(Ordering::Relaxed),
            STATS_FALLBACK.load(Ordering::Relaxed),
            STATS_TRANSLATE_NS.load(Ordering::Relaxed) as f64 / 1e6,
            STATS_JIT_ENTRIES.load(Ordering::Relaxed),
            STATS_INTERP_BLOCKS.load(Ordering::Relaxed),
        );
    }
}

// ---------------- dispatcher ----------------

fn setup_cpu(cpu: &mut Cpu) {
    cpu.jit_exit = maclator_jit_exit as usize as u64;
    cpu.helpers[HELPER_INTERP] = helper_interp as usize as u64;
    cpu.helpers[HELPER_COMMPAGE] = helper_commpage as usize as u64;
    IBTC.with(|t| {
        let mut t = t.borrow_mut();
        if t.is_none() {
            *t = Some(vec![[1, 0]; IBTC_SIZE].into_boxed_slice());
        }
        cpu.ibtc = t.as_mut().unwrap().as_mut_ptr() as u64;
    });
}

fn ibtc_insert(pc: u64, host: u64) {
    IBTC.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t[((pc >> 2) as usize) & (IBTC_SIZE - 1)] = [pc, host];
        }
    });
}

fn ibtc_clear() {
    IBTC.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            for e in t.iter_mut() {
                *e = [1, 0];
            }
        }
    });
}

fn cpu_clone(c: &Cpu) -> Box<Cpu> {
    let mut n = Cpu::new();
    n.x = c.x;
    n.pc = c.pc;
    n.set_nzcv(c.nzcv());
    n.v = c.v;
    n.fpcr = c.fpcr;
    n.fpsr = c.fpsr;
    n.tpidr_el0 = c.tpidr_el0;
    n.tpidrro_el0 = c.tpidrro_el0;
    n.excl_addr = c.excl_addr;
    n.excl_val = c.excl_val;
    n.excl_val2 = c.excl_val2;
    n.excl_size = c.excl_size;
    n
}

/// Run one block in the interpreter (reference), undo its memory writes,
/// then run the translated block and compare.
fn verify_block(cpu: &mut Cpu, host: u64) {
    use maclator_core::mem;
    mem::LOGGING_ENABLED.store(true, Ordering::Relaxed);
    let start_pc = cpu.pc;
    let mut r = cpu_clone(cpu);
    mem::log_start();
    let mut ref_exit = None;
    let mut executed = Vec::new();
    for _ in 0..emit::MAX_BLOCK_INSNS {
        let insn = unsafe { mem::r32(r.pc) };
        executed.push((r.pc, insn));
        let term = emit::is_terminator(insn);
        if let Err(e) = interp::exec(&mut r, insn) {
            ref_exit = Some(e);
            break;
        }
        if term {
            break;
        }
    }
    let log = mem::log_stop();
    // Final bytes written by the reference, then undo.
    let finals: Vec<(u64, Vec<u8>)> = log.iter().map(|(a, _, n)| {
        let mut b = vec![0u8; *n];
        unsafe { std::ptr::copy_nonoverlapping(mem::host(*a), b.as_mut_ptr(), *n) };
        (*a, b)
    }).collect();
    for (a, old, n) in log.iter().rev() {
        unsafe { std::ptr::copy_nonoverlapping(old.as_ptr(), mem::host(*a), *n) };
    }
    unsafe { maclator_jit_enter(cpu, host) };
    // Blocks with atomics race with other threads between the two runs.
    let racy = executed.iter().any(|&(_, i)| {
        (i & 0x3F00_0000 == 0x0800_0000) || (i & 0x3B20_0C00 == 0x3820_0000 && (i >> 26) & 1 == 0)
    });
    if racy {
        return;
    }
    let mut diffs = Vec::new();
    let jit_pending = cpu.exit_reason == EXIT_PENDING;
    if ref_exit.is_some() != (jit_pending || cpu.exit_reason == EXIT_SVC) {
        diffs.push(format!("exit: ref={:?} jit reason={}", ref_exit, cpu.exit_reason));
    }
    for i in 0..32 {
        if r.x[i] != cpu.x[i] {
            diffs.push(format!("x{i}: ref={:#x} jit={:#x}", r.x[i], cpu.x[i]));
        }
    }
    if r.nzcv() != cpu.nzcv() {
        diffs.push(format!("nzcv: ref={:x} jit={:x}", r.nzcv(), cpu.nzcv()));
    }
    if r.pc != cpu.pc && ref_exit.is_none() {
        diffs.push(format!("pc: ref={:#x} jit={:#x}", r.pc, cpu.pc));
    }
    for i in 0..32 {
        if r.v[i] != cpu.v[i] {
            diffs.push(format!("v{i}: ref={:x?} jit={:x?}", r.v[i], cpu.v[i]));
        }
    }
    for (a, want) in &finals {
        let mut got = vec![0u8; want.len()];
        unsafe { std::ptr::copy_nonoverlapping(mem::host(*a), got.as_mut_ptr(), want.len()) };
        if &got != want {
            diffs.push(format!("mem {:#x}: ref={:x?} jit={:x?}", a, want, got));
        }
    }
    if !diffs.is_empty() {
        eprintln!("\n[verify] MISMATCH in block at {:#x} {}", start_pc, crate::symbols::describe(start_pc));
        for (pc, insn) in &executed {
            eprintln!("    {:#x}: {:08x}  {}", pc, insn, maclator_core::disasm::disasm(*insn));
        }
        for d in diffs.iter().take(12) {
            eprintln!("  {d}");
        }
        std::process::exit(99);
    }
    if let Some(e) = PENDING.with(|p| p.take()) {
        PENDING.with(|p| p.set(Some(e)));
    }
}

/// Interpret one basic block. Returns false when the thread should stop.
fn interp_block(cpu: &mut Cpu) -> bool {
    for _ in 0..emit::MAX_BLOCK_INSNS {
        let insn = unsafe { maclator_core::mem::r32(cpu.pc) };
        let term = emit::is_terminator(insn);
        if let Err(e) = interp::exec(cpu, insn) {
            return crate::engine::handle_exit(cpu, e);
        }
        if term {
            break;
        }
    }
    true
}

pub fn run(cpu: &mut Cpu) {
    if !JIT_ENABLED.load(Ordering::Relaxed) {
        crate::engine::interp_loop(cpu);
        return;
    }
    crate::aot::ensure_loaded();
    setup_cpu(cpu);
    let (mut last_pc, mut repeats) = (u64::MAX, 0u64);
    loop {
        if cpu.pc == last_pc {
            repeats += 1;
            if repeats == 20000 && std::env::var_os("MACLATOR_SPIN_DEBUG").is_some() {
                let found = lookup(cpu.pc);
                let hot = HOT.with(|h| h.borrow().get(&cpu.pc).copied());
                let msg = format!("[spin] pc={:#x} lookup={:?} hot={:?} exit_reason={} exit_data={:#x} epoch={} my_epoch={}\n", cpu.pc, found, hot, cpu.exit_reason, cpu.exit_data, EPOCH.load(Ordering::Relaxed), MY_EPOCH.with(|e| e.get()));
                unsafe { libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len()) };
                if let Some(h) = found {
                    let bytes = unsafe { std::slice::from_raw_parts(h as *const u8, 160) };
                    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                    let m2 = format!("[spin] code {hex}\n");
                    unsafe { libc::write(2, m2.as_ptr() as *const libc::c_void, m2.len()) };
                }
            }
        } else {
            last_pc = cpu.pc;
            repeats = 0;
        }
        let epoch = EPOCH.load(Ordering::Relaxed);
        if MY_EPOCH.with(|e| e.get()) != epoch {
            ibtc_clear();
            MY_EPOCH.with(|e| e.set(epoch));
        }
        let pc = cpu.pc;
        let host = match lookup(pc) {
            Some(h) => h,
            None => {
                // Tiering: interpret cold blocks, compile hot ones.
                let n = HOT.with(|h| {
                    let mut h = h.borrow_mut();
                    let c = h.entry(pc).or_insert(0);
                    *c += 1;
                    *c
                });
                if n < hot_threshold() {
                    STATS_INTERP_BLOCKS.fetch_add(1, Ordering::Relaxed);
                    if !interp_block(cpu) {
                        return;
                    }
                    continue;
                }
                translate(pc)
            }
        };
        STATS_JIT_ENTRIES.fetch_add(1, Ordering::Relaxed);
        if verify_mode() {
            verify_block(cpu, host);
        } else {
            ibtc_insert(pc, host);
            unsafe { maclator_jit_enter(cpu, host) };
        }
        match cpu.exit_reason {
            EXIT_CONTINUE => {
                let slot = cpu.exit_data;
                if slot != 0 && !verify_mode() {
                    // Chain: point the exit slot straight at the target block.
                    let target = lookup_or_translate(cpu.pc);
                    if EPOCH.load(Ordering::Relaxed) == epoch {
                        unsafe { std::ptr::write_volatile(slot as *mut u64, target) };
                    }
                }
            }
            EXIT_SVC => {
                let imm = cpu.exit_data as u16;
                if !crate::engine::handle_exit(cpu, Exit::Svc(imm)) {
                    return;
                }
            }
            EXIT_PENDING => {
                let e = PENDING.with(|p| p.take()).expect("pending exit");
                if !crate::engine::handle_exit(cpu, e) {
                    return;
                }
            }
            r => panic!("bad exit reason {r}"),
        }
        cpu.exit_reason = EXIT_CONTINUE;
        cpu.exit_data = 0;
    }
}
