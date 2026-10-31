//! execve / posix_spawn. Native (x86-64 or universal) programs run directly on
//! the host; arm64-only programs are re-launched through Maclator, carrying over
//! the sysroot and options so children (e.g. Chromium/Electron helper processes)
//! run in the same environment.

use crate::hostsys::{self, SysResult};
use std::ffi::CString;
use crate::slices::arm64_only;
use std::sync::Mutex;

const SYS_EXECVE: u64 = 59;

/// Options to forward to re-launched children (`--sysroot X`, `--dyld D`, ...).
static FORWARD: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn configure(flags: Vec<String>) {
    *FORWARD.lock().unwrap() = flags;
}

unsafe fn cstr(p: u64) -> String {
    if p == 0 {
        return String::new();
    }
    std::ffi::CStr::from_ptr(p as *const libc::c_char).to_string_lossy().into_owned()
}

unsafe fn read_argv(p: u64) -> Vec<String> {
    let mut v = Vec::new();
    if p == 0 {
        return v;
    }
    let mut i = 0;
    loop {
        let e = *((p + i * 8) as *const u64);
        if e == 0 || i > 4096 {
            break;
        }
        v.push(cstr(e));
        i += 1;
    }
    v
}

/// Host processes (native children and the maclator re-launcher) must not inherit the arm64
/// GPU shim in DYLD_INSERT_LIBRARIES; maclator injects it into guests itself (`--gpu`).
/// Returns the scrubbed environment (keep alive until the call), and points `args` at it.
fn scrub_shim(n: u64, args: &mut [u64; 8]) -> Option<(Vec<CString>, Vec<u64>)> {
    let eidx = if n == SYS_EXECVE { 2 } else { 4 };
    let shim = crate::gpu::shim_path()?;
    let env = unsafe { read_argv(args[eidx]) };
    if !env.iter().any(|e| e.starts_with("DYLD_INSERT_LIBRARIES=") && e.contains(&shim)) {
        return None;
    }
    let cleaned: Vec<CString> = env
        .into_iter()
        .filter_map(|e| {
            if let Some(v) = e.strip_prefix("DYLD_INSERT_LIBRARIES=") {
                let rest: Vec<&str> = v.split(':').filter(|p| *p != shim && !p.is_empty()).collect();
                if rest.is_empty() {
                    return None;
                }
                return CString::new(format!("DYLD_INSERT_LIBRARIES={}", rest.join(":"))).ok();
            }
            CString::new(e).ok()
        })
        .collect();
    let mut ptrs: Vec<u64> = cleaned.iter().map(|c| c.as_ptr() as u64).collect();
    ptrs.push(0);
    args[eidx] = ptrs.as_ptr() as u64;
    Some((cleaned, ptrs))
}

pub fn exec_like(n: u64, args: &[u64; 8]) -> SysResult {
    let (pidx, aidx) = if n == SYS_EXECVE { (0, 1) } else { (1, 3) };
    let original_path = unsafe { cstr(args[pidx]) };
    let path = crate::paths::redirect(&original_path).unwrap_or(original_path);
    if path.is_empty() {
        return hostsys::unix(n, args);
    }
    let mut args = *args;
    let physical_path = CString::new(path.as_str()).ok();
    if let Some(ref c) = physical_path {
        args[pidx] = c.as_ptr() as u64;
    }
    let _env = scrub_shim(n, &mut args);
    let args = &args;
    if !arm64_only(&path) {
        return hostsys::unix(n, args);
    }
    let Ok(me) = std::env::current_exe() else { return hostsys::unix(n, args) };
    let argv = unsafe { read_argv(args[aidx]) };
    let mut new_argv: Vec<String> = vec![me.to_string_lossy().into_owned()];
    new_argv.extend(FORWARD.lock().unwrap().iter().cloned());
    new_argv.push(path);
    new_argv.extend(argv.into_iter().skip(1));
    let cstrs: Vec<CString> = new_argv.iter().filter_map(|s| CString::new(s.as_str()).ok()).collect();
    let mut ptrs: Vec<u64> = cstrs.iter().map(|c| c.as_ptr() as u64).collect();
    ptrs.push(0);
    let mut a = *args;
    a[pidx] = cstrs[0].as_ptr() as u64;
    a[aidx] = ptrs.as_ptr() as u64;
    let r = hostsys::unix(n, &a);
    drop(cstrs);
    r
}
