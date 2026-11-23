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

/// XNU's NCARGS (ARG_MAX): the space execve and posix_spawn give the argument and environment
/// strings.
const NCARGS: usize = 1 << 20;

/// The guest's path, read with the fault-safe copy: None if a byte before its NUL cannot be read,
/// or it has no NUL within `max` bytes.
fn cstr(p: u64, max: usize) -> Option<String> {
    if p == 0 {
        return Some(String::new());
    }
    crate::syscalls::guest_cstr(p, max, crate::guestmem::read)
}

/// The guest's NULL-terminated string vector, read with the fault-safe copy and charged as XNU's
/// exec_extract_strings charges argv: each string its bytes as the guest wrote them, its NUL and
/// an 8-byte pointer, then the final NULL pointer, out of `space` bytes, and no string read past
/// the space left. EFAULT when a pointer in the vector cannot be read, even with no space left (it
/// is read first, as in XNU), or when a byte of a string cannot be read before the space runs out;
/// E2BIG when the space runs out first while a string is copied, or when a string's pointer or the
/// final NULL pointer does not fit. Unlike XNU, the environment is not charged with argv: the host
/// kernel charges it, with its NULL pointer, and scrub_shim reads it with a space of its own.
fn read_argv(p: u64, mut space: usize) -> Result<Vec<String>, i32> {
    let mut v = Vec::new();
    if p == 0 {
        return Ok(v);
    }
    let mut i = 0;
    loop {
        let e = crate::guestmem::read_u64(p + i * 8).ok_or(libc::EFAULT)?;
        if e == 0 {
            break;
        }
        let mut fault = false;
        let s = crate::syscalls::guest_cbytes(e, space, |a, b| {
            let ok = crate::guestmem::read(a, b);
            fault |= !ok;
            ok
        });
        let s = s.ok_or(if fault { libc::EFAULT } else { libc::E2BIG })?;
        space = space.checked_sub(s.len() + 1 + 8).ok_or(libc::E2BIG)?;
        v.push(String::from_utf8_lossy(&s).into_owned());
        i += 1;
    }
    space.checked_sub(8).ok_or(libc::E2BIG)?; // the final NULL pointer
    Ok(v)
}

/// Host processes (native children and the maclator re-launcher) must not inherit the arm64
/// GPU shim in DYLD_INSERT_LIBRARIES; maclator injects it into guests itself (`--gpu`).
/// Returns the scrubbed environment (keep alive until the call), and points `args` at it.
fn scrub_shim(n: u64, args: &mut [u64; 8]) -> Option<(Vec<CString>, Vec<u64>)> {
    let eidx = if n == SYS_EXECVE { 2 } else { 4 };
    let shim = crate::gpu::shim_path()?;
    // An environment that cannot be read, or does not fit, goes to the host kernel as the guest
    // passed it, which answers EFAULT or E2BIG for it.
    let env = read_argv(args[eidx], NCARGS).ok()?;
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
    // A path that cannot be read goes to the host kernel as the guest passed it, and the host picks
    // the error as it does natively (XNU's exec_save_path): EFAULT if one of its first MAXPATHLEN
    // bytes cannot be read, ENAMETOOLONG if they hold no NUL.
    let Some(original_path) = cstr(args[pidx], crate::syscalls::GUEST_NAME_MAX) else {
        return hostsys::unix(n, args);
    };
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
    // The host kernel is handed the re-launch's own argv, never the guest's, so the guest's argv
    // is read here and fails as XNU's copy of it does.
    let argv = match read_argv(args[aidx], NCARGS) {
        Ok(v) => v,
        Err(e) => return SysResult { rax: e as u64, rdx: 0, carry: true },
    };
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
