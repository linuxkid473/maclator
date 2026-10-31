//! Deferred asynchronous Unix signal delivery at ARM instruction boundaries.
//! The host callback publishes atomics only. Hardware faults retain the host
//! disposition until exact host-PC recovery is available.
use maclator_core::cpu::Cpu;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Default)]
pub struct GuestAction { pub handler: u64, pub tramp: u64, pub mask: u32, pub flags: u32 }
static ACTIONS: Mutex<[GuestAction; 32]> = Mutex::new([GuestAction { handler: 0, tramp: 0, mask: 0, flags: 0 }; 32]);
const SIG_IGN: u64 = 1;
const UC_TRAD: i32 = 1;
const UC_FLAVOR: i32 = 30;
const UC_SET_ALT_STACK: i32 = 0x4000_0000;
const UC_RESET_ALT_STACK: i32 = 0x8000_0000u32 as i32;
const SIGINFO_SIZE: usize = 104;
const UCONTEXT_SIZE: usize = 56;
const MCONTEXT_SIZE: usize = 816;
const FRAME_SIZE: usize = SIGINFO_SIZE + UCONTEXT_SIZE + MCONTEXT_SIZE;
const MAX_NESTING: usize = 32;

struct InfoSlot { state: AtomicU32, words: [AtomicU64; SIGINFO_SIZE / 8] }
impl InfoSlot {
    const fn new() -> Self { Self { state: AtomicU32::new(0), words: [const { AtomicU64::new(0) }; SIGINFO_SIZE / 8] } }
    unsafe fn publish(&self, sig: i32, info: *mut libc::siginfo_t) -> bool {
        // Standard Darwin signals coalesce. Never spin in a signal callback,
        // and never overwrite a partially published/read siginfo record.
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() { return false; }
        for (i, word) in self.words.iter().enumerate() {
            let value = if info.is_null() { if i == 0 { sig as u64 } else { 0 } }
                else { std::ptr::read_unaligned((info as *const u8).add(i * 8) as *const u64) };
            word.store(value, Ordering::Relaxed);
        }
        self.state.store(2, Ordering::Release);
        true
    }
    fn read(&self) -> [u8; SIGINFO_SIZE] {
        let mut bytes = [0u8; SIGINFO_SIZE];
        for (i, word) in self.words.iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        bytes
    }
    fn consume(&self) -> [u8; SIGINFO_SIZE] {
        let bytes = self.read();
        self.state.store(0, Ordering::Release);
        bytes
    }
}
struct Pending { guest: AtomicBool, bits: AtomicU32, force: AtomicU32, info: [InfoSlot; 32] }
impl Pending {
    const fn new() -> Self {
        Self { guest: AtomicBool::new(false), bits: AtomicU32::new(0), force: AtomicU32::new(0), info: [const { InfoSlot::new() }; 32] }
    }
    unsafe fn publish(&self, sig: i32, info: *mut libc::siginfo_t) {
        if self.info[sig as usize].publish(sig, info) {
            self.bits.fetch_or(sigbit(sig), Ordering::Release);
        }
    }
    fn take(&self, mask: u32) -> Option<(i32, [u8; SIGINFO_SIZE])> {
        let bits = self.bits.load(Ordering::Acquire) & !(mask & !self.force.load(Ordering::Acquire));
        if bits == 0 { return None; }
        let bit = 1u32 << bits.trailing_zeros();
        if self.bits.fetch_and(!bit, Ordering::AcqRel) & bit == 0 { return None; }
        self.force.fetch_and(!bit, Ordering::Relaxed);
        let sig = bits.trailing_zeros() as i32 + 1;
        Some((sig, self.info[sig as usize].consume()))
    }
}
#[derive(Clone, Copy)]
struct Frame { ucontext: u64, token: u64 }
struct ThreadState { stack: u64, size: u64, enabled: bool, onstack: bool, frames: Vec<Frame> }
impl ThreadState {
    const fn new() -> Self { Self { stack: 0, size: 0, enabled: false, onstack: false, frames: Vec::new() } }
}
// The callback touches only constant-initialized LOCAL, never STATE.
thread_local! {
    static LOCAL: Pending = const { Pending::new() };
    static STATE: RefCell<ThreadState> = const { RefCell::new(ThreadState::new()) };
}
static PROCESS_PENDING: Pending = Pending::new();
pub fn enter_thread() {
    LOCAL.with(|p| p.guest.store(true, Ordering::Release));
    STATE.with(|_| {});
}
pub fn leave_thread() {
    LOCAL.with(|p| {
        p.guest.store(false, Ordering::Release);
        let bits = p.bits.swap(0, Ordering::AcqRel);
        for sig in 1..32 {
            if bits & sigbit(sig) != 0 {
                let info = p.info[sig as usize].consume();
                unsafe { PROCESS_PENDING.publish(sig, info.as_ptr() as *mut libc::siginfo_t) };
            }
        }
    });
}
fn sigbit(sig: i32) -> u32 { 1u32 << (sig - 1) }
fn hardware_signal(sig: i32) -> bool {
    matches!(sig, libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE | libc::SIGTRAP)
}
extern "C" fn receive(sig: i32, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    if !(1..32).contains(&sig) { return; }
    LOCAL.with(|p| unsafe {
        if p.guest.load(Ordering::Relaxed) { p.publish(sig, info); }
        else { PROCESS_PENDING.publish(sig, info); }
    });
}
fn install_host(sig: i32, action: GuestAction) -> Result<(), i32> {
    // Preserve existing hardware-fault diagnostics and default behavior.
    if hardware_signal(sig) && (action.handler > SIG_IGN || std::env::var_os("MACLATOR_DUMP_ON_FAULT").is_some()) { return Ok(()); }
    unsafe {
        let mut host: libc::sigaction = std::mem::zeroed();
        if action.handler <= SIG_IGN {
            host.sa_sigaction = action.handler as usize;
            libc::sigemptyset(&mut host.sa_mask);
        } else {
            host.sa_sigaction = receive as *const () as usize;
            // A restarted host wait prevents reaching a guest boundary.
            host.sa_flags = libc::SA_SIGINFO | (action.flags as i32 & (libc::SA_NOCLDSTOP | libc::SA_NOCLDWAIT));
            libc::sigfillset(&mut host.sa_mask);
        }
        if libc::sigaction(sig, &host, std::ptr::null_mut()) == 0 { Ok(()) } else { Err(*libc::__error()) }
    }
}
fn rd64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }
fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn wr64(b: &mut [u8], o: usize, v: u64) { b[o..o + 8].copy_from_slice(&v.to_le_bytes()); }
fn wr32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }

/// sigsuspend restores the original mask before returning EINTR on the host.
/// Its delivered signal must still run, even if that original mask blocks it.
pub fn interrupted_suspend(temporary_mask: u32) {
    LOCAL.with(|p| p.force.fetch_or(p.bits.load(Ordering::Acquire) & !temporary_mask, Ordering::Release));
}

/// Kernel ABI: sigaction(signum, struct __sigaction *nsa, struct sigaction *osa).
pub fn sigaction(sig: i32, nsa: u64, osa: u64) -> Result<(), i32> {
    if !(1..32).contains(&sig) || (nsa != 0 && matches!(sig, libc::SIGKILL | libc::SIGSTOP)) { return Err(libc::EINVAL); }
    let new = if nsa == 0 { None } else {
        let mut b = [0u8; 24];
        if !crate::guestmem::read(nsa, &mut b) { return Err(libc::EFAULT); }
        Some(GuestAction { handler: rd64(&b, 0), tramp: rd64(&b, 8), mask: rd32(&b, 16), flags: rd32(&b, 20) })
    };
    let mut acts = ACTIONS.lock().unwrap();
    let old = acts[sig as usize];
    if osa != 0 {
        let mut b = [0u8; 16];
        wr64(&mut b, 0, old.handler); wr32(&mut b, 8, old.mask); wr32(&mut b, 12, old.flags);
        if !crate::guestmem::write(osa, &b) { return Err(libc::EFAULT); }
    }
    if let Some(new) = new { install_host(sig, new)?; acts[sig as usize] = new; }
    Ok(())
}
/// ARM and x86 Darwin stack_t have the same 24-byte pointer/size/flags layout.
pub fn sigaltstack(new: u64, old: u64) -> Result<(), i32> {
    let new = if new == 0 { None } else {
        let mut b = [0u8; 24];
        if !crate::guestmem::read(new, &mut b) { return Err(libc::EFAULT); }
        let (stack, size, flags) = (rd64(&b, 0), rd64(&b, 8), rd32(&b, 16) as i32);
        if flags & !libc::SS_DISABLE != 0 { return Err(libc::EINVAL); }
        if flags & libc::SS_DISABLE == 0 && (size < libc::MINSIGSTKSZ as u64 || stack.checked_add(size).is_none()) { return Err(libc::ENOMEM); }
        Some((stack, size, flags & libc::SS_DISABLE == 0))
    };
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if new.is_some() && s.onstack { return Err(libc::EPERM); }
        if old != 0 {
            let mut b = [0u8; 24]; wr64(&mut b, 0, s.stack); wr64(&mut b, 8, s.size);
            wr32(&mut b, 16, if !s.enabled { libc::SS_DISABLE as u32 } else if s.onstack { libc::SS_ONSTACK as u32 } else { 0 });
            if !crate::guestmem::write(old, &b) { return Err(libc::EFAULT); }
        }
        if let Some((stack, size, enabled)) = new { s.stack = stack; s.size = size; s.enabled = enabled; }
        Ok(())
    })
}
/// Deliver one pending signal, with all guest state already written back.
pub fn poll(cpu: &mut Cpu) -> Result<(), i32> {
    if LOCAL.with(|p| p.bits.load(Ordering::Acquire)) | PROCESS_PENDING.bits.load(Ordering::Acquire) == 0 { return Ok(()); }
    let mut oldmask: libc::sigset_t = 0;
    let err = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut oldmask) };
    if err != 0 { return Err(err); }
    let signal = LOCAL.with(|p| p.take(oldmask)).or_else(|| PROCESS_PENDING.take(oldmask));
    let Some((sig, info)) = signal else { return Ok(()); };
    let action = {
        let mut actions = ACTIONS.lock().unwrap(); let action = actions[sig as usize];
        if action.handler > SIG_IGN && action.flags & libc::SA_RESETHAND as u32 != 0 {
            let default = GuestAction::default(); install_host(sig, default)?; actions[sig as usize] = default;
        }
        action
    };
    if action.handler == SIG_IGN { return Ok(()); }
    if action.handler == 0 { unsafe { libc::raise(sig) }; return Ok(()); }
    if action.tramp == 0 || action.tramp & 3 != 0 { return Err(libc::EINVAL); }
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.frames.len() >= MAX_NESTING { return Err(libc::EOVERFLOW); }
        let use_alt = s.enabled && !s.onstack && action.flags & libc::SA_ONSTACK as u32 != 0;
        let top = if use_alt { s.stack.checked_add(s.size).ok_or(libc::EFAULT)? } else { cpu.sp() };
        let sp = top.checked_sub((FRAME_SIZE + 128) as u64).ok_or(libc::EFAULT)? & !15;
        if use_alt && sp < s.stack { return Err(libc::EFAULT); }
        let ucaddr = sp + SIGINFO_SIZE as u64; let mcaddr = ucaddr + UCONTEXT_SIZE as u64;
        let mut b = [0u8; FRAME_SIZE]; b[..SIGINFO_SIZE].copy_from_slice(&info);
        let uc = &mut b[SIGINFO_SIZE..SIGINFO_SIZE + UCONTEXT_SIZE];
        wr32(uc, 0, s.onstack as u32); wr32(uc, 4, oldmask); wr64(uc, 8, sp);
        wr64(uc, 16, if use_alt { s.size } else { 0 }); wr32(uc, 24, if s.onstack { libc::SS_ONSTACK as u32 } else { 0 });
        wr64(uc, 40, MCONTEXT_SIZE as u64); wr64(uc, 48, mcaddr);
        let mc = &mut b[SIGINFO_SIZE + UCONTEXT_SIZE..];
        for i in 0..32 { wr64(mc, 16 + i * 8, cpu.x[i]); }
        wr64(mc, 272, cpu.pc); wr32(mc, 280, cpu.nzcv() << 28); wr32(mc, 284, 1); // NO_PTRAUTH
        for i in 0..32 { wr64(mc, 288 + i * 16, cpu.v[i][0]); wr64(mc, 296 + i * 16, cpu.v[i][1]); }
        wr32(mc, 800, cpu.fpsr as u32); wr32(mc, 804, cpu.fpcr as u32);
        if !crate::guestmem::write(sp, &b) { return Err(libc::EFAULT); }
        let mask = action.mask | if action.flags & libc::SA_NODEFER as u32 == 0 { sigbit(sig) } else { 0 };
        let err = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) };
        if err != 0 { return Err(err); }
        let token = unsafe { (libc::arc4random() as u64) << 32 | libc::arc4random() as u64 };
        s.frames.push(Frame { ucontext: ucaddr, token }); if use_alt { s.onstack = true; }
        cpu.x[0] = action.handler; cpu.x[1] = if action.flags & libc::SA_SIGINFO as u32 != 0 { UC_FLAVOR as u64 } else { UC_TRAD as u64 };
        cpu.x[2] = sig as u64; cpu.x[3] = sp; cpu.x[4] = ucaddr; cpu.x[5] = token;
        cpu.x[31] = sp; cpu.pc = action.tramp; cpu.set_nzcv(0); cpu.excl_size = 0;
        Ok(())
    })
}
/// Restart only the known interruptible read/write/receive/wait family.
pub fn should_restart(syscall: u64) -> bool {
    if !matches!(syscall, 3 | 4 | 7 | 27 | 29 | 30 | 120 | 121 | 396 | 397) { return false; }
    let bits = LOCAL.with(|p| p.bits.load(Ordering::Acquire)) | PROCESS_PENDING.bits.load(Ordering::Acquire);
    if bits == 0 { return false; }
    let actions = ACTIONS.lock().unwrap();
    (1..32).any(|sig| bits & sigbit(sig) != 0 && actions[sig as usize].flags & libc::SA_RESTART as u32 != 0)
}
/// Never pass an ARM context to host sigreturn.
pub fn sigreturn(cpu: &mut Cpu, uctx: u64, infostyle: i32, token: u64) -> Result<(), i32> {
    if infostyle == UC_SET_ALT_STACK || infostyle == UC_RESET_ALT_STACK {
        STATE.with(|s| s.borrow_mut().onstack = infostyle == UC_SET_ALT_STACK);
        cpu.x[0] = 0; cpu.x[1] = 0; cpu.cf = 0; return Ok(());
    }
    if !matches!(infostyle, UC_TRAD | UC_FLAVOR) { return Err(libc::EINVAL); }
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(frame) = s.frames.last() else { return Err(libc::EINVAL); };
        if frame.ucontext != uctx || frame.token != token { return Err(libc::EINVAL); }
        let mut uc = [0u8; UCONTEXT_SIZE];
        if !crate::guestmem::read(uctx, &mut uc) { return Err(libc::EFAULT); }
        if rd64(&uc, 40) != MCONTEXT_SIZE as u64 { return Err(libc::EINVAL); }
        let mut mc = [0u8; MCONTEXT_SIZE];
        if !crate::guestmem::read(rd64(&uc, 48), &mut mc) { return Err(libc::EFAULT); }
        let pc = rd64(&mc, 272); let sp = rd64(&mc, 264);
        if pc & 3 != 0 || sp & 15 != 0 { return Err(libc::EINVAL); }
        for i in 0..32 { cpu.x[i] = rd64(&mc, 16 + i * 8); }
        cpu.pc = pc; cpu.set_nzcv(rd32(&mc, 280) >> 28);
        for i in 0..32 { cpu.v[i] = [rd64(&mc, 288 + i * 16), rd64(&mc, 296 + i * 16)]; }
        cpu.fpsr = rd32(&mc, 800) as u64; cpu.fpcr = rd32(&mc, 804) as u64; cpu.excl_size = 0;
        s.onstack = rd32(&uc, 0) & 1 != 0; s.frames.pop();
        let mask = rd32(&uc, 4);
        let err = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) };
        if err != 0 { return Err(err); }
        Ok(())
    })
}
