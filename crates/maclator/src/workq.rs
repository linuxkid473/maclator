//! Workqueue emulation: libdispatch worker threads, kernel event delivery and
//! workloops.
//!
//! XNU owns three things libdispatch relies on:
//!  * the per-process *workq kqueue* (`KEVENT_FLAG_WORKQ`): knotes fire by
//!    spawning a workqueue thread that receives the events on its stack;
//!  * *workloops* (`kevent_id` + `KEVENT_FLAG_WORKLOOP`): dynamic kqueues, one
//!    per serial queue, with EVFILT_WORKLOOP knotes that request a servicer
//!    thread or implement dispatch_sync waits;
//!  * workqueue thread creation (`workq_kernreturn`).
//!
//! We back the workq kqueue and each workloop's regular knotes with ordinary
//! host kqueues, implement EVFILT_WORKLOOP in user space, and run guest
//! worker threads laid out exactly as the kernel lays them out.

use crate::hostsys;
use maclator_core::cpu::Cpu;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Condvar, Mutex, Once};

// ---- constants (workqueue_syscalls.h, event_private.h, priority_private.h) ----

const WQOPS_THREAD_RETURN: u64 = 0x004;
const WQOPS_QUEUE_NEWSPISUPP: u64 = 0x010;
const WQOPS_QUEUE_REQTHREADS: u64 = 0x020;
const WQOPS_QUEUE_REQTHREADS2: u64 = 0x030;
const WQOPS_THREAD_KEVENT_RETURN: u64 = 0x040;
const WQOPS_SET_EVENT_MANAGER_PRIORITY: u64 = 0x080;
const WQOPS_THREAD_WORKLOOP_RETURN: u64 = 0x100;
const WQOPS_SHOULD_NARROW: u64 = 0x200;
const WQOPS_SETUP_DISPATCH: u64 = 0x400;

const WQ_FLAG_THREAD_PRIO_QOS: u64 = 0x0000_4000;
const WQ_FLAG_THREAD_OVERCOMMIT: u64 = 0x0001_0000;
const WQ_FLAG_THREAD_REUSE: u64 = 0x0002_0000;
const WQ_FLAG_THREAD_NEWSPI: u64 = 0x0004_0000;
const WQ_FLAG_THREAD_KEVENT: u64 = 0x0008_0000;
const WQ_FLAG_THREAD_EVENT_MANAGER: u64 = 0x0010_0000;
const WQ_FLAG_THREAD_TSD_BASE_SET: u64 = 0x0020_0000;
const WQ_FLAG_THREAD_WORKLOOP: u64 = 0x0040_0000;
const WQ_FLAG_THREAD_COOPERATIVE: u64 = 0x0100_0000;

const PP_OVERCOMMIT: u64 = 0x8000_0000;
const PP_COOPERATIVE: u64 = 0x0800_0000;
const PP_EVENT_MANAGER: u64 = 0x0200_0000;

const KEVENT_FLAG_IMMEDIATE: u64 = 0x001;
const KEVENT_FLAG_ERROR_EVENTS: u64 = 0x002;
const KEVENT_FLAG_STACK_DATA: u64 = 0x008;
const KEVENT_FLAG_WORKQ: u64 = 0x020;
const KEVENT_FLAG_PARKING: u64 = 0x800;

const EVFILT_READ: i16 = -1;
const EVFILT_WORKLOOP: i16 = -17;
const EVFILT_USER_: i16 = -10;
const EV_ADD: u16 = 0x1;
const EV_DELETE: u16 = 0x2;
const EV_ENABLE: u16 = 0x4;
const EV_DISPATCH: u16 = 0x80;
const EV_ERROR: u16 = 0x4000;

const NOTE_WL_THREAD_REQUEST: u32 = 0x1;
const NOTE_WL_SYNC_WAIT: u32 = 0x4;
const NOTE_WL_SYNC_WAKE: u32 = 0x8;
const NOTE_WL_SYNC_IPC: u32 = 0x8000_0000;
const NOTE_WL_END_OWNERSHIP: u32 = 0x20;
const NOTE_WL_DISCOVER_OWNER: u32 = 0x80;
const NOTE_WL_IGNORE_ESTALE: u32 = 0x100;
const NOTE_WL_COMMANDS_MASK: u32 = 0x8000_000f;

const SYS_KEVENT_QOS: u64 = 374;
const SYS_KEVENT_ID: u64 = 375;
const SYS_KQUEUE: u64 = 362;

const KEV_SIZE: u64 = 72;
const WQ_KEVENT_LIST_LEN: u64 = 16;
const WQ_KEVENT_DATA_SIZE: u64 = 32 * 1024;
const STACK_SIZE: u64 = 512 * 1024;
const GUARD: u64 = 0x4000;
const PTHREAD_T_OFFSET: u64 = 12 * 1024;

// ---- raw kevent_qos_s helpers ----

#[derive(Clone, Copy, Default, Debug)]
struct Kev {
    ident: u64,
    filter: i16,
    flags: u16,
    qos: i32,
    udata: u64,
    fflags: u32,
    xflags: u32,
    data: i64,
    ext: [u64; 4],
}

impl Kev {
    unsafe fn read(p: u64) -> Kev {
        let b = p as *const u8;
        let r64 = |o: usize| std::ptr::read_unaligned(b.add(o) as *const u64);
        Kev {
            ident: r64(0),
            filter: std::ptr::read_unaligned(b.add(8) as *const i16),
            flags: std::ptr::read_unaligned(b.add(10) as *const u16),
            qos: std::ptr::read_unaligned(b.add(12) as *const i32),
            udata: r64(16),
            fflags: std::ptr::read_unaligned(b.add(24) as *const u32),
            xflags: std::ptr::read_unaligned(b.add(28) as *const u32),
            data: r64(32) as i64,
            ext: [r64(40), r64(48), r64(56), r64(64)],
        }
    }
    unsafe fn write(&self, p: u64) {
        let b = p as *mut u8;
        let w64 = |o: usize, v: u64| std::ptr::write_unaligned(b.add(o) as *mut u64, v);
        w64(0, self.ident);
        std::ptr::write_unaligned(b.add(8) as *mut i16, self.filter);
        std::ptr::write_unaligned(b.add(10) as *mut u16, self.flags);
        std::ptr::write_unaligned(b.add(12) as *mut i32, self.qos);
        w64(16, self.udata);
        std::ptr::write_unaligned(b.add(24) as *mut u32, self.fflags);
        std::ptr::write_unaligned(b.add(28) as *mut u32, self.xflags);
        w64(32, self.data as u64);
        for i in 0..4 {
            w64(40 + i * 8, self.ext[i]);
        }
    }
}

fn host_kqueue() -> i32 {
    let r = hostsys::unix(SYS_KQUEUE, &[0; 8]);
    let fd = r.rax as i32;
    // Move it out of the guest's usual descriptor range.
    unsafe {
        let high = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 900);
        if high >= 0 {
            libc::close(fd);
            return high;
        }
    }
    fd
}

fn kevent_qos(kq: i32, changes: u64, nchanges: u64, out: u64, nout: u64, data: u64, avail: u64, flags: u64) -> hostsys::SysResult {
    hostsys::unix(SYS_KEVENT_QOS, &[kq as u64, changes, nchanges, out, nout, data, avail, flags])
}

// ---- workers ----

struct Job {
    flags: u64,
    kevent_list: u64,
    nevents: u64,
    stack_top: u64,
    wl: Option<u64>,
}

struct Worker {
    stack_low: u64,
    pthread: u64,
    tx: Option<Sender<Job>>,
}

impl Worker {
    fn new() -> Option<Worker> {
        let (_, _, _, pthsize) = crate::threads::wq_registration();
        let pthsize = pthsize.max(0x1000);
        let total = GUARD + STACK_SIZE + PTHREAD_T_OFFSET + ((pthsize + 0x3fff) & !0x3fff);
        let base = unsafe {
            let p = libc::mmap(std::ptr::null_mut(), total as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0);
            if p == libc::MAP_FAILED {
                return None;
            }
            libc::mprotect(p, GUARD as usize, libc::PROT_NONE);
            p as u64
        };
        let stack_low = base + GUARD;
        Some(Worker { stack_low, pthread: stack_low + STACK_SIZE + PTHREAD_T_OFFSET, tx: None })
    }
    fn kevent_list(&self) -> u64 {
        self.pthread - WQ_KEVENT_LIST_LEN * KEV_SIZE
    }
    fn data_buf(&self) -> u64 {
        self.kevent_list() - WQ_KEVENT_DATA_SIZE
    }
}

static IDLE: Mutex<Vec<Worker>> = Mutex::new(Vec::new());

thread_local! {
    static PARK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static BOUND_WL: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

fn take_worker() -> Option<Worker> {
    if let Some(w) = IDLE.lock().unwrap().pop() {
        return Some(w);
    }
    Worker::new()
}

fn start_job(mut w: Worker, job: Job) {
    match &w.tx {
        Some(tx) => {
            let _ = tx.send(Job { flags: job.flags | WQ_FLAG_THREAD_REUSE, ..job });
        }
        None => {
            let (tx, rx) = channel::<Job>();
            w.tx = Some(tx);
            spawn_worker_thread(w, job, rx);
        }
    }
}

fn spawn_worker_thread(w: Worker, job: Job, rx: Receiver<Job>) {
    let (start, tsd_off, mts_off, _) = crate::threads::wq_registration();
    let (stack_low, pthread) = (w.stack_low, w.pthread);
    let tx = w.tx.clone().unwrap();
    crate::threads::LIVE_THREADS.fetch_add(1, Ordering::SeqCst);
    std::thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(move || {
            let port = unsafe { libc::mach_thread_self() } as u64;
            if tsd_off != 0 && mts_off != 0 {
                unsafe { *((pthread + tsd_off + mts_off) as *mut u64) = port };
            }
            let mut cpu = Cpu::new();
            let mut job = job;
            loop {
                *cpu = Cpu::default();
                cpu.pc = start;
                cpu.x[0] = pthread;
                cpu.x[1] = port;
                cpu.x[2] = stack_low;
                cpu.x[3] = job.kevent_list;
                cpu.x[4] = job.flags;
                cpu.x[5] = job.nevents;
                cpu.tpidrro_el0 = pthread + tsd_off;
                cpu.x[31] = job.stack_top & !0xf;
                PARK.with(|p| p.set(false));
                IS_MANAGER.with(|m| m.set(job.flags & WQ_FLAG_THREAD_EVENT_MANAGER != 0));
                BOUND_WL.with(|b| b.set(job.wl));
                if let Some(id) = job.wl {
                    if let Some(st) = WL.lock().unwrap().as_mut() {
                        if let Some(w) = st.loops.get_mut(&id) {
                            w.servicer = owner_key(port);
                        }
                    }
                }
                crate::engine::run_thread(&mut cpu);
                if !PARK.with(|p| p.get()) {
                    break;
                }
                // The workloop (if any) was released by the return path.
                IDLE.lock().unwrap().push(Worker { stack_low, pthread, tx: Some(tx.clone()) });
                match rx.recv() {
                    Ok(j) => job = j,
                    Err(_) => break,
                }
            }
            crate::threads::LIVE_THREADS.fetch_sub(1, Ordering::SeqCst);
        })
        .expect("failed to spawn workqueue thread");
}

fn flags_for(pp: u64) -> u64 {
    let mut f = WQ_FLAG_THREAD_NEWSPI | WQ_FLAG_THREAD_TSD_BASE_SET;
    if pp & PP_EVENT_MANAGER != 0 {
        return f | WQ_FLAG_THREAD_KEVENT | WQ_FLAG_THREAD_EVENT_MANAGER;
    }
    let qos_bits = (pp >> 8) & 0x3f;
    let qos = if qos_bits == 0 { 4 } else { 64 - qos_bits.leading_zeros() as u64 };
    f |= WQ_FLAG_THREAD_PRIO_QOS | qos.min(6);
    if pp & PP_OVERCOMMIT != 0 {
        f |= WQ_FLAG_THREAD_OVERCOMMIT;
    }
    if pp & PP_COOPERATIVE != 0 {
        f |= WQ_FLAG_THREAD_COOPERATIVE;
    }
    f
}

// ---- single event-manager thread ----

/// libdispatch requires at most one event-manager thread at a time; the
/// kernel keeps the manager's events pending while one is bound. Events that
/// fire while the manager runs are held here and handed over when it parks.
#[derive(Default)]
struct ManagerState {
    active: bool,
    pending: Vec<Kev>,
}

static MANAGER: Mutex<Option<ManagerState>> = Mutex::new(None);

thread_local! {
    static IS_MANAGER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Claim the manager slot for a new manager thread. Returns false when one is
/// already running (the event, if any, is queued for later).
fn claim_manager(ev: Option<Kev>) -> bool {
    let mut g = MANAGER.lock().unwrap();
    let st = g.get_or_insert_with(ManagerState::default);
    if st.active {
        if let Some(ev) = ev {
            // EV_CLEAR user events coalesce; other events are kept in order.
            if !st.pending.iter().any(|k| k.filter == ev.filter && k.ident == ev.ident && ev.filter == EVFILT_USER_) {
                st.pending.push(ev);
            }
        }
        return false;
    }
    st.active = true;
    true
}

/// The manager thread is parking: release the slot and start a new manager
/// for any events that fired meanwhile.
fn release_manager() {
    let pending = {
        let mut g = MANAGER.lock().unwrap();
        let st = g.get_or_insert_with(ManagerState::default);
        st.active = false;
        std::mem::take(&mut st.pending)
    };
    if pending.is_empty() {
        return;
    }
    let Some(w) = take_worker() else { return };
    if !claim_manager(None) {
        IDLE.lock().unwrap().push(w);
        return;
    }
    let list = w.kevent_list();
    let n = pending.len().min(WQ_KEVENT_LIST_LEN as usize);
    for (i, k) in pending.iter().take(n).enumerate() {
        unsafe { k.write(list + i as u64 * KEV_SIZE) };
    }
    let flags = flags_for(PP_EVENT_MANAGER) | WQ_FLAG_THREAD_KEVENT;
    let stack_top = w.data_buf() + WQ_KEVENT_DATA_SIZE;
    start_job(w, Job { flags, kevent_list: list, nevents: n as u64, stack_top, wl: None });
}

// ---- the anonymous workq kqueue ----

/// QoS/priority each knote on the workq kqueue was registered with, keyed by
/// (filter, ident). The host kernel does not hand the guest's pthread priority
/// (e.g. the event-manager flag) back on fired events, but the thread we start
/// for the event depends on it.
static ANON_QOS: Mutex<Option<HashMap<(i16, u64), i32>>> = Mutex::new(None);

fn record_anon_qos(changes: u64, nchanges: u64) {
    let mut g = ANON_QOS.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    for i in 0..nchanges {
        let ke = unsafe { Kev::read(changes + i * KEV_SIZE) };
        if ke.flags & EV_DELETE != 0 {
            m.remove(&(ke.filter, ke.ident));
        } else if ke.flags & EV_ADD != 0 {
            m.insert((ke.filter, ke.ident), ke.qos);
        }
    }
}

fn restore_anon_qos(k: &mut Kev) {
    if k.qos != 0 {
        return;
    }
    if let Some(q) = ANON_QOS.lock().unwrap().as_ref().and_then(|m| m.get(&(k.filter, k.ident)).copied()) {
        k.qos = q;
    }
}

static ANON_KQ: AtomicI32 = AtomicI32::new(-1);
static ANON_MONITOR: Once = Once::new();

fn anon_kq() -> i32 {
    ANON_MONITOR.call_once(|| {
        ANON_KQ.store(host_kqueue(), Ordering::SeqCst);
        std::thread::Builder::new().name("maclator-workq".into()).spawn(anon_monitor).unwrap();
    });
    ANON_KQ.load(Ordering::SeqCst)
}

/// Deliver each event fired on the workq kqueue to a worker thread.
fn anon_monitor() {
    let kq = ANON_KQ.load(Ordering::SeqCst);
    loop {
        let Some(w) = take_worker() else {
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        };
        let list = w.kevent_list();
        let mut avail: u64 = WQ_KEVENT_DATA_SIZE;
        let r = kevent_qos(kq, 0, 0, list, 1, w.data_buf(), &mut avail as *mut u64 as u64, KEVENT_FLAG_STACK_DATA);
        if r.carry || r.rax == 0 {
            IDLE.lock().unwrap().push(w);
            continue;
        }
        let mut ev = unsafe { Kev::read(list) };
        restore_anon_qos(&mut ev);
        unsafe { ev.write(list) };
        let qos = ev.qos as u32 as u64;
        let flags = flags_for(qos) | WQ_FLAG_THREAD_KEVENT;
        if crate::syscalls::TRACE.load(Ordering::Relaxed) {
            let k = unsafe { Kev::read(list) };
            eprintln!("[workq] anon event ident={:#x} filter={} flags={:#x} qos={:#x} fflags={:#x} data={:#x} udata={:#x} -> thread flags={:#x} n={}", k.ident, k.filter, k.flags, k.qos, k.fflags, k.data, k.udata, flags, r.rax);
        }
        let stack_top = w.data_buf() + avail;
        if flags & WQ_FLAG_THREAD_EVENT_MANAGER != 0 && !claim_manager(Some(ev)) {
            IDLE.lock().unwrap().push(w);
            continue;
        }
        start_job(w, Job { flags, kevent_list: list, nevents: r.rax, stack_top, wl: None });
    }
}

// ---- workloops ----

#[derive(Default)]
struct Workloop {
    /// Host kqueue holding this workloop's regular knotes (-1 until needed).
    kq: i32,
    /// The thread-request knote (EVFILT_WORKLOOP), if any.
    thread_request: Option<Kev>,
    /// The thread request is active (fired and not yet delivered). Each
    /// delivery hands libdispatch a +1 reference, so it must be delivered
    /// once per activation.
    tr_active: bool,
    /// A servicer thread is currently bound.
    bound: bool,
    /// Regular knotes have fired and need a servicer.
    needs_service: bool,
    /// Thread (mach port name) that owns the workloop, e.g. a dispatch_sync
    /// drainer. No async servicer is started while there is an owner.
    owner: u64,
    /// Port of the bound servicer thread.
    servicer: u64,
}

/// A NOTE_WL_SYNC_WAIT/WAKE knote (ident = waiter thread id).
#[derive(Default)]
struct SyncKnote {
    /// NOTE_WL_SYNC_WAKE has been applied (sticky until the knote is deleted).
    woken: bool,
}

static WLOG: Mutex<std::collections::VecDeque<String>> = Mutex::new(std::collections::VecDeque::new());

pub fn wlog_enabled() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("MACLATOR_WL_LOG").is_some())
}

pub fn wlog(msg: String) {
    let mut l = WLOG.lock().unwrap();
    if l.len() >= 600 {
        l.pop_front();
    }
    l.push_back(msg);
}

pub fn dump_log() {
    if let Ok(l) = WLOG.try_lock() {
        for m in l.iter() {
            eprintln!("  wl| {m}");
        }
    }
}

fn owner_tracking() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("MACLATOR_WL_NO_OWNER").is_none())
}

/// Thread identity as stored in dispatch locks: the port name with its low
/// two bits masked off (the kernel's ipc_entry_name_mask undoes this).
fn current_port() -> u64 {
    owner_key(unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) as u64 })
}

fn owner_key(port: u64) -> u64 {
    port & !3
}

struct WlState {
    loops: HashMap<u64, Workloop>,
    sync: HashMap<(u64, u64), SyncKnote>,
    /// workloop kq fd -> workloop id
    by_fd: HashMap<i32, u64>,
}

static WL: Mutex<Option<WlState>> = Mutex::new(None);
static WL_CV: Condvar = Condvar::new();
static WL_MON_KQ: AtomicI32 = AtomicI32::new(-1);
static WL_MONITOR: Once = Once::new();

fn with_wl<R>(f: impl FnOnce(&mut WlState) -> R) -> R {
    let mut g = WL.lock().unwrap();
    let st = g.get_or_insert_with(|| WlState { loops: HashMap::new(), sync: HashMap::new(), by_fd: HashMap::new() });
    f(st)
}

fn wl_mon_kq() -> i32 {
    WL_MONITOR.call_once(|| {
        WL_MON_KQ.store(host_kqueue(), Ordering::SeqCst);
        std::thread::Builder::new().name("maclator-workloops".into()).spawn(wl_monitor).unwrap();
    });
    WL_MON_KQ.load(Ordering::SeqCst)
}

/// Watch every workloop's host kqueue; when one has pending events and no
/// servicer, bind a worker to it.
fn wl_monitor() {
    let mon = WL_MON_KQ.load(Ordering::SeqCst);
    let mut buf = [0u8; 72 * 8];
    loop {
        let r = kevent_qos(mon, 0, 0, buf.as_mut_ptr() as u64, 8, 0, 0, 0);
        if r.carry {
            continue;
        }
        for i in 0..r.rax {
            let ev = unsafe { Kev::read(buf.as_ptr() as u64 + i * KEV_SIZE) };
            let wl_id = ev.udata;
            let mut g = WL.lock().unwrap();
            if let Some(st) = g.as_mut() {
                if let Some(wl) = st.loops.get_mut(&wl_id) {
                    wl.needs_service = true;
                }
                schedule(st, wl_id);
            }
        }
    }
}

/// Arm the monitor to watch `fd` (one-shot until re-enabled).
fn watch_wl_kq(fd: i32, wl_id: u64, first: bool) {
    let mon = wl_mon_kq();
    let ev = Kev { ident: fd as u64, filter: EVFILT_READ, flags: if first { EV_ADD | EV_DISPATCH } else { EV_ENABLE | EV_DISPATCH }, udata: wl_id, ..Default::default() };
    let mut b = [0u8; 72];
    unsafe { ev.write(b.as_mut_ptr() as u64) };
    kevent_qos(mon, b.as_ptr() as u64, 1, 0, 0, 0, 0, KEVENT_FLAG_IMMEDIATE);
}

/// If the workloop needs a servicer and has none, bind a worker to it.
fn schedule(st: &mut WlState, wl_id: u64) {
    let Some(wl) = st.loops.get_mut(&wl_id) else { return };
    let tr_pending = wl.thread_request.is_some() && wl.tr_active;
    let owned = wl.owner != 0 && owner_tracking();
    if wl.bound || owned || (!tr_pending && !wl.needs_service) {
        return;
    }
    let Some(w) = take_worker() else { return };
    let list = w.kevent_list();
    let mut n: u64 = 0;
    let mut qos: u64 = 0;
    if let (Some(tr), true) = (wl.thread_request, wl.tr_active) {
        unsafe { tr.write(list) };
        qos = tr.qos as u32 as u64;
        n = 1;
        wl.tr_active = false; // delivery deactivates the knote
    }
    // kqueue id lives just below the event list; data goes below that.
    unsafe { *((list - 8) as *mut u64) = wl_id };
    let data_buf = w.data_buf();
    let mut avail: u64 = WQ_KEVENT_DATA_SIZE - 8;
    if wl.kq >= 0 {
        let r = kevent_qos(wl.kq, 0, 0, list + n * KEV_SIZE, WQ_KEVENT_LIST_LEN - n, data_buf, &mut avail as *mut u64 as u64, KEVENT_FLAG_STACK_DATA | KEVENT_FLAG_IMMEDIATE);
        if !r.carry {
            if n == 0 && r.rax > 0 {
                qos = unsafe { Kev::read(list) }.qos as u32 as u64;
            }
            n += r.rax;
        }
    }
    wl.needs_service = false;
    if n == 0 {
        IDLE.lock().unwrap().push(w);
        if wl.kq >= 0 {
            watch_wl_kq(wl.kq, wl_id, false);
        }
        return;
    }
    wl.bound = true;
    if wlog_enabled() { wlog(format!("schedule wl{:x}: deliver n={} (tr={})", wl_id & 0xffffff, n, wl.thread_request.is_some())); }
    let flags = (flags_for(qos) & !(WQ_FLAG_THREAD_EVENT_MANAGER)) | WQ_FLAG_THREAD_KEVENT | WQ_FLAG_THREAD_WORKLOOP;
    start_job(w, Job { flags, kevent_list: list, nevents: n, stack_top: data_buf + avail, wl: Some(wl_id) });
}

/// Apply a single change to a workloop. Returns an errno (0 on success).
fn wl_apply(st: &mut WlState, wl_id: u64, ke: &Kev) -> i64 {
    if ke.filter != EVFILT_WORKLOOP {
        // Regular knote: lives on the workloop's host kqueue.
        let first = {
            let wl = st.loops.entry(wl_id).or_insert_with(|| Workloop { kq: -1, ..Default::default() });
            if wl.kq < 0 {
                wl.kq = host_kqueue();
                true
            } else {
                false
            }
        };
        let fd = st.loops[&wl_id].kq;
        if first {
            st.by_fd.insert(fd, wl_id);
            watch_wl_kq(fd, wl_id, true);
        }
        let mut b = [0u8; 72];
        let mut out = [0u8; 72];
        unsafe { ke.write(b.as_mut_ptr() as u64) };
        let r = kevent_qos(fd, b.as_ptr() as u64, 1, out.as_mut_ptr() as u64, 1, 0, 0, KEVENT_FLAG_IMMEDIATE | KEVENT_FLAG_ERROR_EVENTS);
        if r.carry {
            return r.rax as i64;
        }
        if r.rax > 0 {
            let e = unsafe { Kev::read(out.as_ptr() as u64) };
            if e.flags & EV_ERROR != 0 {
                return e.data;
            }
        }
        return 0;
    }
    let ff = ke.fflags;
    let cmd = ff & NOTE_WL_COMMANDS_MASK;
    if cmd == NOTE_WL_SYNC_IPC {
        return libc::ENOENT as i64;
    }
    let me = current_port();
    let wl = st.loops.entry(wl_id).or_insert_with(|| Workloop { kq: -1, ..Default::default() });
    // State check and owner discovery (filt_wlupdate).
    // EV_EXTIDX_WL_ADDR = 1, _MASK = 2, _VALUE = 3 (0 is the lane)
    let (addr, mask, value) = (ke.ext[1], ke.ext[2], ke.ext[3]);
    let mut err: i64 = 0;
    let mut new_owner = wl.owner;
    if addr != 0 {
        match crate::guestmem::read_u64(addr) {
            None => return libc::EFAULT as i64,
            Some(cur) => {
                if cur & mask != value & mask {
                    err = libc::ESTALE as i64;
                } else if ff & NOTE_WL_DISCOVER_OWNER != 0 {
                    let name = (cur as u32 & !3) as u64;
                    if name != 0 {
                        new_owner = name;
                    }
                }
            }
        }
    }
    if ff & NOTE_WL_END_OWNERSHIP != 0 && new_owner == me {
        new_owner = 0;
    }
    if wl.bound && new_owner == wl.servicer {
        new_owner = 0;
    }
    let owner_released = wl.owner != 0 && new_owner == 0;
    if wlog_enabled() { wlog(format!("t{:x} wl{:x} apply id={:x} fl={:x} ff={:x} qos={:x} err={} owner {:x}->{:x} bound={} tr={}/{}", me, wl_id & 0xffffff, ke.ident & 0xffffff, ke.flags, ke.fflags, ke.qos, err, wl.owner, new_owner, wl.bound, wl.thread_request.is_some(), wl.tr_active)); }
    wl.owner = new_owner;
    if err == libc::ESTALE as i64 && ff & NOTE_WL_IGNORE_ESTALE != 0 {
        // The update is dropped silently.
        if owner_released {
            schedule(st, wl_id);
        }
        return 0;
    }
    let mut result: i64 = err;
    if err == 0 {
        if cmd == NOTE_WL_THREAD_REQUEST {
            if ke.flags & EV_DELETE != 0 {
                wl.thread_request = None;
                wl.tr_active = false;
            } else if ke.flags & EV_ADD != 0 {
                // Attaching or touching the thread request fires it.
                let mut tr = *ke;
                tr.flags = EV_ADD | EV_ENABLE;
                wl.thread_request = Some(tr);
                wl.tr_active = true;
            }
        } else if cmd == NOTE_WL_SYNC_WAKE || cmd == NOTE_WL_SYNC_WAIT || cmd == 0 {
            let key = (wl_id, ke.ident);
            if ke.flags & EV_DELETE != 0 {
                // Dropping a waiter knote wakes the waiter.
                if st.sync.remove(&key).is_none() {
                    result = libc::ENOENT as i64;
                }
                WL_CV.notify_all();
            } else if cmd == NOTE_WL_SYNC_WAKE {
                st.sync.entry(key).or_default().woken = true;
                WL_CV.notify_all();
            } else if cmd == NOTE_WL_SYNC_WAIT {
                st.sync.entry(key).or_default();
            } else {
                result = libc::EINVAL as i64;
            }
        } else {
            result = libc::EINVAL as i64;
        }
    }
    schedule(st, wl_id);
    let _ = owner_released;
    result
}

/// kevent_id() on a workloop.
fn kevent_id(args: &[u64; 8]) -> hostsys::SysResult {
    let (wl_id, changes, nchanges, out, nout, data, avail, flags) = (args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7]);
    let mut nerr: u64 = 0;
    let mut wait_on: Option<u64> = None;
    {
        let mut g = WL.lock().unwrap();
        let st = g.get_or_insert_with(|| WlState { loops: HashMap::new(), sync: HashMap::new(), by_fd: HashMap::new() });
        for i in 0..nchanges {
            let ke = unsafe { Kev::read(changes + i * KEV_SIZE) };
            let err = wl_apply(st, wl_id, &ke);
            if crate::syscalls::TRACE.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!("[kev]   wl={:#x} chg ident={:#x} filter={} flags={:#x} fflags={:#x} data={:#x} qos={:#x} -> err={}", wl_id, ke.ident, ke.filter, ke.flags, ke.fflags, ke.data, ke.qos, err);
            }
            if err == 0 && ke.filter == EVFILT_WORKLOOP && ke.fflags & NOTE_WL_COMMANDS_MASK == NOTE_WL_SYNC_WAIT && ke.flags & EV_DELETE == 0 {
                wait_on = Some(ke.ident);
            }
            if err != 0 && flags & KEVENT_FLAG_ERROR_EVENTS != 0 && nerr < nout {
                let mut e = ke;
                e.flags |= EV_ERROR;
                e.data = err;
                unsafe { e.write(out + nerr * KEV_SIZE) };
                nerr += 1;
            }
        }
    }
    if let Some(tid) = wait_on {
        if wlog_enabled() { wlog(format!("t{:x} wl{:x} SYNC_WAIT id={:x} begin", current_port(), wl_id & 0xffffff, tid & 0xffffff)); }
        // dispatch_sync waiter: block until SYNC_WAKE for our tid.
        let g = WL.lock().unwrap();
        return finish_wait(g, (wl_id, tid), nerr);
    }
    if nerr > 0 || flags & KEVENT_FLAG_ERROR_EVENTS != 0 || nout == 0 {
        return hostsys::SysResult { rax: nerr, rdx: 0, carry: false };
    }
    // Plain poll of the workloop's regular knotes.
    let kq = with_wl(|st| st.loops.get(&wl_id).map(|w| w.kq).unwrap_or(-1));
    if kq < 0 {
        return hostsys::SysResult { rax: 0, rdx: 0, carry: false };
    }
    kevent_qos(kq, 0, 0, out, nout, data, avail, (flags & (KEVENT_FLAG_STACK_DATA | KEVENT_FLAG_IMMEDIATE)) | KEVENT_FLAG_IMMEDIATE)
}

fn finish_wait(mut g: std::sync::MutexGuard<'_, Option<WlState>>, key: (u64, u64), nerr: u64) -> hostsys::SysResult {
    loop {
        let st = g.as_mut().unwrap();
        match st.sync.get(&key) {
            Some(k) if k.woken => {
                return hostsys::SysResult { rax: nerr, rdx: 0, carry: false };
            }
            // Deleted by the waker: also a wake-up.
            None => return hostsys::SysResult { rax: nerr, rdx: 0, carry: false },
            _ => {}
        }
        g = WL_CV.wait(g).unwrap();
    }
}

// ---- syscall entry points ----

/// Guest kevent_qos / kevent_id aimed at the workq kqueue or a workloop.
/// Returns None when the call should go to the host unchanged.
pub fn kevent_redirect(n: u64, args: &[u64; 8]) -> Option<hostsys::SysResult> {
    match n {
        SYS_KEVENT_QOS => {
            let flags = args[7];
            if flags & KEVENT_FLAG_WORKQ == 0 {
                return None;
            }
            record_anon_qos(args[1], args[2]);
            let mut a = *args;
            a[0] = anon_kq() as u64;
            a[7] = flags & !(KEVENT_FLAG_WORKQ | KEVENT_FLAG_PARKING);
            if a[4] == 0 {
                a[7] |= KEVENT_FLAG_IMMEDIATE;
            }
            Some(hostsys::unix(SYS_KEVENT_QOS, &a))
        }
        SYS_KEVENT_ID => Some(kevent_id(args)),
        _ => None,
    }
}

/// workq_kernreturn(options, item, affinity, prio)
pub fn kernreturn(_cpu: &mut Cpu, a: &[u64; 8]) -> Result<u64, u64> {
    match a[0] {
        WQOPS_QUEUE_NEWSPISUPP | WQOPS_SET_EVENT_MANAGER_PRIORITY | WQOPS_SETUP_DISPATCH | WQOPS_SHOULD_NARROW => Ok(0),
        WQOPS_QUEUE_REQTHREADS | WQOPS_QUEUE_REQTHREADS2 => {
            let count = a[1].max(1);
            let pp = a[3];
            anon_kq();
            for _ in 0..count {
                let Some(w) = take_worker() else { return Err(libc::EAGAIN as u64) };
                let mut flags = flags_for(pp);
                if flags & WQ_FLAG_THREAD_EVENT_MANAGER != 0 && !claim_manager(None) {
                    IDLE.lock().unwrap().push(w);
                    continue;
                }
                if flags & WQ_FLAG_THREAD_EVENT_MANAGER == 0 {
                    flags &= !WQ_FLAG_THREAD_KEVENT;
                }
                let stack_top = w.pthread;
                start_job(w, Job { flags, kevent_list: 0, nevents: 0, stack_top, wl: None });
            }
            Ok(0)
        }
        WQOPS_THREAD_KEVENT_RETURN | WQOPS_THREAD_RETURN => {
            if a[0] == WQOPS_THREAD_KEVENT_RETURN && a[1] != 0 && (a[2] as i64) > 0 {
                let kq = anon_kq();
                record_anon_qos(a[1], a[2]);
                kevent_qos(kq, a[1], a[2], 0, 0, 0, 0, KEVENT_FLAG_IMMEDIATE);
            }
            park();
            Ok(0)
        }
        WQOPS_THREAD_WORKLOOP_RETURN => {
            let wl = BOUND_WL.with(|b| b.get());
            if let Some(wl_id) = wl {
                let mut g = WL.lock().unwrap();
                let st = g.get_or_insert_with(|| WlState { loops: HashMap::new(), sync: HashMap::new(), by_fd: HashMap::new() });
                if a[1] != 0 && (a[2] as i64) > 0 {
                    for i in 0..a[2] {
                        let ke = unsafe { Kev::read(a[1] + i * KEV_SIZE) };
                        wl_apply(st, wl_id, &ke);
                    }
                }
                let me = current_port();
                if wlog_enabled() { wlog(format!("t{:x} wl{:x} WORKLOOP_RETURN nchanges={}", me, wl_id & 0xffffff, a[2])); }
                let kq = if let Some(w) = st.loops.get_mut(&wl_id) {
                    w.bound = false;
                    w.servicer = 0;
                    if w.owner == me {
                        w.owner = 0;
                    }
                    w.kq
                } else {
                    -1
                };
                if kq >= 0 {
                    watch_wl_kq(kq, wl_id, false);
                }
                // Parking happens after this returns; the next servicer is
                // picked by schedule() (possibly this same thread later).
                drop(g);
                park();
                let mut g = WL.lock().unwrap();
                if let Some(st) = g.as_mut() {
                    schedule(st, wl_id);
                }
            } else {
                park();
            }
            Ok(0)
        }
        op => {
            if std::env::var_os("MACLATOR_TRACE").is_some() {
                eprintln!("[maclator] unhandled workq op {:#x}", op);
            }
            Ok(0)
        }
    }
}

fn park() {
    if IS_MANAGER.with(|m| m.replace(false)) {
        release_manager();
    }
    PARK.with(|p| p.set(true));
    BOUND_WL.with(|b| b.set(None));
    crate::engine::exit_current_thread();
}
