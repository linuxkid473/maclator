//! GPU passthrough: Metal calls made by the arm64 guest are executed on the host GPU.
//!
//! An injected arm64 shim (`libmclmetal.dylib`, DYLD_INSERT_LIBRARIES) turns Metal API use into
//! RPCs and traps into Maclator with `svc #0x80` and x16 = `SYS_MCL_HOSTCALL`
//! (x0 = op, x1 = request pointer, x2 = request length; result x0 = reply pointer, x1 = length).
//! The x86-64 half (`libmclbridge.dylib`, loaded lazily on the first call) owns the real Metal
//! objects and performs the calls. The guest lives in this process's address space, so guest
//! pointers (texture data, `MTLBuffer.contents`, ...) are valid on the host as they are.

use maclator_core::cpu::Cpu;
use std::ffi::CString;
use std::sync::OnceLock;

pub const SYS_MCL_HOSTCALL: u64 = 0x4D43;

type CallFn = unsafe extern "C" fn(op: u64, input: *const u8, len: u64, outlen: *mut u64) -> *mut u8;

const SHIM_NAME: &str = "libmclmetal.dylib";
const BRIDGE_NAME: &str = "libmclbridge.dylib";

/// Directories searched for the GPU libraries: $MACLATOR_GPU_DIR, next to the executable,
/// then ~/.maclator/gpu.
fn search_dirs() -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("MACLATOR_GPU_DIR") {
        v.push(d.into());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            v.push(d.to_path_buf());
            v.push(d.join("gpu"));
        }
    }
    if let Some(h) = std::env::var_os("HOME") {
        v.push(std::path::Path::new(&h).join(".maclator/gpu"));
    }
    v
}

fn find(name: &str) -> Option<std::path::PathBuf> {
    search_dirs().into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// Path of the guest shim to inject, if the GPU libraries are installed.
pub fn shim_path() -> Option<String> {
    find(SHIM_NAME).map(|p| p.to_string_lossy().into_owned())
}

/// Add the shim to the guest's DYLD_INSERT_LIBRARIES.
pub fn inject(envp: &mut Vec<String>) -> bool {
    let Some(shim) = shim_path() else { return false };
    if let Some(e) = envp.iter_mut().find(|e| e.starts_with("DYLD_INSERT_LIBRARIES=")) {
        if !e.contains(&shim) {
            e.push(':');
            e.push_str(&shim);
        }
    } else {
        envp.push(format!("DYLD_INSERT_LIBRARIES={shim}"));
    }
    if let Some(dir) = find(BRIDGE_NAME) {
        if !envp.iter().any(|e| e.starts_with("MACLATOR_GPU_BRIDGE=")) {
            envp.push(format!("MACLATOR_GPU_BRIDGE={}", dir.to_string_lossy()));
        }
    }
    true
}

static BRIDGE: OnceLock<Option<(CallFn, Option<extern "C" fn()>)>> = OnceLock::new();

fn bridge() -> Option<(CallFn, Option<extern "C" fn()>)> {
    *BRIDGE.get_or_init(|| {
        let path = std::env::var("MACLATOR_GPU_BRIDGE").ok().or_else(|| find(BRIDGE_NAME).map(|p| p.to_string_lossy().into_owned()))?;
        let c = CString::new(path.clone()).ok()?;
        unsafe {
            let h = libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if h.is_null() {
                let e = libc::dlerror();
                let msg = if e.is_null() { String::new() } else { std::ffi::CStr::from_ptr(e).to_string_lossy().into_owned() };
                eprintln!("maclator: cannot load GPU bridge {path}: {msg}");
                return None;
            }
            let f = libc::dlsym(h, b"mcl_call\0".as_ptr() as *const libc::c_char);
            if f.is_null() {
                eprintln!("maclator: GPU bridge {path} has no mcl_call");
                return None;
            }
            let w = libc::dlsym(h, b"mcl_warmup\0".as_ptr() as *const libc::c_char);
            let warm = if w.is_null() { None } else { Some(std::mem::transmute::<*mut libc::c_void, extern "C" fn()>(w)) };
            Some((std::mem::transmute::<*mut libc::c_void, CallFn>(f), warm))
        }
    })
}

/// One job for a bridge thread. Raw pointers are guest/host addresses valid in this process.
struct Job {
    op: u64,
    input: u64,
    len: u64,
    reply: std::sync::mpsc::Sender<(u64, u64)>,
}

/// Host frameworks (CoreFoundation, XPC, Metal) must never run on a guest thread: the kernel
/// keeps one special reply port per thread, which the guest's libxpc and the host's libxpc would
/// both cache and recycle, breaking the guest's XPC calls (MACH_RCV_INVALID_NOTIFY). So every
/// guest thread gets its own dedicated bridge thread and hands its calls over.
struct BridgeThread {
    tx: std::sync::mpsc::Sender<Job>,
}

impl BridgeThread {
    fn spawn(f: CallFn) -> BridgeThread {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("maclator-gpu".into())
            .stack_size(8 << 20)
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let mut outlen = 0u64;
                    let p = unsafe { f(job.op, job.input as *const u8, job.len, &mut outlen) };
                    let _ = job.reply.send((p as u64, outlen));
                }
            })
            .expect("spawn GPU bridge thread");
        BridgeThread { tx }
    }
}

thread_local! {
    static BRIDGE_THREAD: std::cell::RefCell<Option<BridgeThread>> = const { std::cell::RefCell::new(None) };
}

fn call_on_bridge_thread(f: CallFn, op: u64, input: u64, len: u64) -> Option<(u64, u64)> {
    BRIDGE_THREAD.with(|b| {
        let mut b = b.borrow_mut();
        let bt = b.get_or_insert_with(|| BridgeThread::spawn(f));
        let (rtx, rrx) = std::sync::mpsc::channel();
        bt.tx.send(Job { op, input, len, reply: rtx }).ok()?;
        rrx.recv().ok()
    })
}

/// Load the host bridge and warm the GPU up before the guest starts (see `mcl_warmup`).
pub fn preload() {
    if let Some((_, Some(warm))) = bridge() {
        // Run on a scratch thread so the main thread's per-thread kernel state stays untouched.
        let _ = std::thread::Builder::new().stack_size(8 << 20).spawn(move || warm()).map(|h| h.join());
    }
}

/// Handle the guest's host-call trap.
pub fn hostcall(cpu: &mut Cpu) {
    let Some((f, _)) = bridge() else {
        cpu.x[0] = libc::ENOSYS as u64;
        cpu.cf = 1;
        return;
    };
    match call_on_bridge_thread(f, cpu.x[0], cpu.x[1], cpu.x[2]) {
        Some((p, n)) => {
            cpu.x[0] = p;
            cpu.x[1] = n;
            cpu.cf = 0;
        }
        None => {
            cpu.x[0] = libc::EIO as u64;
            cpu.cf = 1;
        }
    }
}
