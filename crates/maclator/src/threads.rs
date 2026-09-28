//! Guest threads: emulation of the libpthread kernel interface
//! (bsdthread_register / bsdthread_create / bsdthread_terminate) and a
//! minimal workqueue so libdispatch can obtain worker threads.
//!
//! Every guest thread runs on its own host thread with its own `Cpu`.

use maclator_core::cpu::Cpu;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

static THREAD_START: AtomicU64 = AtomicU64::new(0);
static WQTHREAD_START: AtomicU64 = AtomicU64::new(0);
static PTHSIZE: AtomicU64 = AtomicU64::new(0);
static TSD_OFFSET: AtomicU32 = AtomicU32::new(0);
static MACH_THREAD_SELF_OFFSET: AtomicU32 = AtomicU32::new(0);
static DISPATCH_QUEUE_OFFSET: AtomicU64 = AtomicU64::new(0);
static REGISTERED: AtomicU32 = AtomicU32::new(0);
pub static LIVE_THREADS: AtomicUsize = AtomicUsize::new(1);

const PTHREAD_START_CUSTOM: u64 = 0x0100_0000;
const PTHREAD_START_TSD_BASE_SET: u64 = 0x1000_0000;
const PTHREAD_START_SUSPENDED: u64 = 0x0200_0000;

const PTHREAD_FEATURE_SUPPORTED: u64 = 0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x4000_0000 | 0x40 | 0x80 | 0x100;

/// bsdthread_register(threadstart, wqthread, pthsize, init_data, init_data_size, dispatchqueue_offset)
pub fn bsdthread_register(a: &[u64; 8]) -> Result<u64, u64> {
    if REGISTERED.swap(1, Ordering::SeqCst) != 0 {
        return Err(libc::EINVAL as u64);
    }
    THREAD_START.store(a[0], Ordering::SeqCst);
    WQTHREAD_START.store(a[1], Ordering::SeqCst);
    let pthsize = a[2] as u32 as u64;
    PTHSIZE.store(pthsize, Ordering::SeqCst);
    let data = a[3];
    let size = a[4];
    if data != 0 {
        unsafe {
            let version = *(data as *const u64);
            if version != size {
                return Err(libc::EINVAL as u64);
            }
            let p = data as *mut u8;
            let rd32 = |o: usize| std::ptr::read_unaligned(p.add(o) as *const u32);
            let dq = std::ptr::read_unaligned(p.add(8) as *const u64);
            let tsd = rd32(24);
            let mts = if size >= 36 { rd32(32) } else { 0 };
            let max_tsd = pthsize.saturating_sub(tsd as u64 + 8);
            TSD_OFFSET.store(if (tsd as u64) <= pthsize.saturating_sub(8) { tsd } else { 0 }, Ordering::SeqCst);
            MACH_THREAD_SELF_OFFSET.store(if (mts as u64) <= max_tsd { mts } else { 0 }, Ordering::SeqCst);
            DISPATCH_QUEUE_OFFSET.store(if dq <= max_tsd { dq } else { 0 }, Ordering::SeqCst);
            // copy-out fields
            let n = size.min(56) as usize;
            let mut out = [0u8; 56];
            std::ptr::copy_nonoverlapping(p, out.as_mut_ptr(), n);
            out[0..8].copy_from_slice(&56u64.min(size).to_le_bytes());
            out[16..24].copy_from_slice(&0u64.to_le_bytes()); // main_qos: unspecified
            if n >= 44 {
                out[36..44].copy_from_slice(&0x7000_0000_0000u64.to_le_bytes()); // stack_addr_hint
            }
            if n >= 48 {
                out[44..48].copy_from_slice(&0u32.to_le_bytes()); // mutex_default_policy
            }
            std::ptr::copy_nonoverlapping(out.as_ptr(), p, n);
        }
    } else {
        DISPATCH_QUEUE_OFFSET.store(a[5], Ordering::SeqCst);
    }
    Ok(PTHREAD_FEATURE_SUPPORTED)
}

struct SendCpu(Box<Cpu>);
unsafe impl Send for SendCpu {}

/// Spawn a host thread running guest code from `cpu`.
fn spawn_guest(mut cpu: Box<Cpu>, setup: impl FnOnce(&mut Cpu) + Send + 'static) {
    let sc = SendCpu(cpu_take(&mut cpu));
    LIVE_THREADS.fetch_add(1, Ordering::SeqCst);
    std::thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(move || {
            let mut c = sc;
            setup(&mut c.0);
            crate::engine::run_thread(&mut c.0);
            LIVE_THREADS.fetch_sub(1, Ordering::SeqCst);
        })
        .expect("failed to spawn host thread");
}

fn cpu_take(c: &mut Box<Cpu>) -> Box<Cpu> {
    std::mem::replace(c, Cpu::new())
}

/// bsdthread_create(func, func_arg, stack, pthread, flags)
pub fn bsdthread_create(a: &[u64; 8]) -> Result<u64, u64> {
    let (func, arg, stack, pthread, flags) = (a[0], a[1], a[2], a[3], a[4]);
    if REGISTERED.load(Ordering::SeqCst) == 0 || flags & PTHREAD_START_CUSTOM == 0 {
        return Err(libc::EINVAL as u64);
    }
    let start = THREAD_START.load(Ordering::SeqCst);
    let tsd_off = TSD_OFFSET.load(Ordering::SeqCst) as u64;
    let mts_off = MACH_THREAD_SELF_OFFSET.load(Ordering::SeqCst) as u64;
    let mut cpu = Cpu::new();
    cpu.pc = start;
    cpu.x[0] = pthread;
    cpu.x[2] = func;
    cpu.x[3] = arg;
    cpu.x[4] = stack;
    let mut fl = flags & !PTHREAD_START_SUSPENDED;
    if tsd_off != 0 {
        cpu.tpidrro_el0 = pthread + tsd_off;
        fl |= PTHREAD_START_TSD_BASE_SET;
    }
    cpu.x[5] = fl;
    cpu.x[31] = stack;
    spawn_guest(cpu, move |c| {
        let port = unsafe { libc::mach_thread_self() } as u64;
        c.x[1] = port;
        if tsd_off != 0 && mts_off != 0 {
            unsafe { *((pthread + tsd_off + mts_off) as *mut u64) = port };
        }
    });
    Ok(pthread)
}

/// bsdthread_terminate(stackaddr, freesize, port, sem)
pub fn bsdthread_terminate(_cpu: &mut Cpu, a: &[u64; 8]) {
    let (stackaddr, freesize, _port, sem) = (a[0], a[1], a[2], a[3]);
    unsafe {
        if freesize != 0 {
            libc::munmap(stackaddr as *mut _, freesize as usize);
        }
        if sem != 0 {
            // semaphore_signal_internal_trap
            crate::hostsys::mach(33, &[sem, 0, 0, 0, 0, 0, 0, 0]);
        }
    }
    crate::engine::exit_current_thread();
}

// ---------------- workqueue ----------------

const WQOPS_THREAD_RETURN: u64 = 0x004;
const WQOPS_QUEUE_NEWSPISUPP: u64 = 0x010;
const WQOPS_QUEUE_REQTHREADS: u64 = 0x020;
const WQOPS_QUEUE_REQTHREADS2: u64 = 0x030;
const WQOPS_THREAD_KEVENT_RETURN: u64 = 0x040;
const WQOPS_SET_EVENT_MANAGER_PRIORITY: u64 = 0x080;
const WQOPS_THREAD_WORKLOOP_RETURN: u64 = 0x100;
const WQOPS_SHOULD_NARROW: u64 = 0x200;
const WQOPS_SETUP_DISPATCH: u64 = 0x400;

const WQ_FLAG_THREAD_NEWSPI: u64 = 0x0080_0000;
const WQ_FLAG_THREAD_REUSE: u64 = 0x0002_0000;
const WQ_FLAG_THREAD_TSD_BASE_SET: u64 = 0x0020_0000;

static WQ_PENDING: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static WQ_IDLE: std::sync::Condvar = std::sync::Condvar::new();

/// workq_kernreturn(options, item, affinity, prio)
pub fn workq_kernreturn(cpu: &mut Cpu, a: &[u64; 8]) -> Result<u64, u64> {
    let op = a[0];
    match op {
        WQOPS_QUEUE_NEWSPISUPP | WQOPS_SET_EVENT_MANAGER_PRIORITY | WQOPS_SETUP_DISPATCH => Ok(0),
        WQOPS_SHOULD_NARROW => Ok(0),
        WQOPS_QUEUE_REQTHREADS | WQOPS_QUEUE_REQTHREADS2 => {
            let count = a[1].max(1);
            let prio = a[3];
            for _ in 0..count {
                start_wq_thread(prio);
            }
            Ok(0)
        }
        WQOPS_THREAD_RETURN | WQOPS_THREAD_KEVENT_RETURN | WQOPS_THREAD_WORKLOOP_RETURN => {
            // The worker is done: park it until more work is requested, then
            // restart it at the wqthread entry (REUSE).
            let _ = cpu;
            crate::engine::exit_current_thread();
            Ok(0)
        }
        _ => {
            if std::env::var_os("MACLATOR_TRACE").is_some() {
                eprintln!("[maclator] unhandled workq op {:#x}", op);
            }
            Ok(0)
        }
    }
}

fn start_wq_thread(prio: u64) {
    let start = WQTHREAD_START.load(Ordering::SeqCst);
    let tsd_off = TSD_OFFSET.load(Ordering::SeqCst) as u64;
    let pthsize = PTHSIZE.load(Ordering::SeqCst).max(0x1000);
    // Allocate stack + pthread struct like the kernel does.
    let stack_size: u64 = 512 * 1024;
    let guard: u64 = 0x4000;
    let total = guard + stack_size + ((pthsize + 0xfff) & !0xfff);
    let base = unsafe {
        let p = libc::mmap(std::ptr::null_mut(), total as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0);
        if p == libc::MAP_FAILED {
            return;
        }
        libc::mprotect(p, guard as usize, libc::PROT_NONE);
        p as u64
    };
    let stack_top = base + guard + stack_size;
    let pthread = stack_top;
    let _ = &WQ_PENDING;
    let _ = &WQ_IDLE;
    let mut cpu = Cpu::new();
    cpu.pc = start;
    cpu.x[0] = pthread;
    cpu.x[2] = stack_top - stack_size; // stackaddr (low)
    cpu.x[3] = 0; // kevent list
    let mut flags = WQ_FLAG_THREAD_NEWSPI | (prio & 0xffff);
    if tsd_off != 0 {
        cpu.tpidrro_el0 = pthread + tsd_off;
        flags |= WQ_FLAG_THREAD_TSD_BASE_SET;
    }
    let _ = WQ_FLAG_THREAD_REUSE;
    cpu.x[4] = 0;
    cpu.x[5] = flags;
    cpu.x[31] = stack_top & !0xf;
    spawn_guest(cpu, move |c| {
        c.x[1] = unsafe { libc::mach_thread_self() } as u64;
    });
}
