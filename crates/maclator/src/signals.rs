//! Guest signal handling.
//!
//! Handlers registered by the guest are recorded; delivery of asynchronous
//! signals to guest handlers is not implemented yet (the host default action
//! applies unless the guest ignores the signal).

use maclator_core::cpu::Cpu;
use std::sync::Mutex;

#[derive(Clone, Copy, Default)]
pub struct GuestAction {
    pub handler: u64,
    pub tramp: u64,
    pub mask: u32,
    pub flags: u32,
}

static ACTIONS: Mutex<[GuestAction; 32]> = Mutex::new([GuestAction { handler: 0, tramp: 0, mask: 0, flags: 0 }; 32]);

const SIG_IGN: u64 = 1;

/// sigaction(signum, struct __sigaction *nsa, struct sigaction *osa)
pub fn sigaction(sig: i32, nsa: u64, osa: u64) -> Result<(), i32> {
    if !(1..32).contains(&sig) {
        return Err(libc::EINVAL);
    }
    let mut acts = ACTIONS.lock().unwrap();
    let old = acts[sig as usize];
    if osa != 0 {
        unsafe {
            *(osa as *mut u64) = old.handler;
            *((osa + 8) as *mut u32) = old.mask;
            *((osa + 12) as *mut u32) = old.flags;
        }
    }
    if nsa != 0 {
        if sig == libc::SIGKILL || sig == libc::SIGSTOP {
            return Err(libc::EINVAL);
        }
        let a = unsafe {
            GuestAction {
                handler: *(nsa as *const u64),
                tramp: *((nsa + 8) as *const u64),
                mask: *((nsa + 16) as *const u32),
                flags: *((nsa + 20) as *const u32),
            }
        };
        acts[sig as usize] = a;
        // Mirror "ignore" and "default" dispositions on the host so that
        // e.g. SIGPIPE-ignoring programs keep working.
        unsafe {
            if a.handler == SIG_IGN {
                libc::signal(sig, libc::SIG_IGN);
            } else if a.handler == 0 {
                libc::signal(sig, libc::SIG_DFL);
            }
        }
    }
    Ok(())
}

pub fn sigreturn(_cpu: &mut Cpu, _uctx: u64, _infostyle: i32) {
    eprintln!("maclator: sigreturn without a delivered signal");
    std::process::abort();
}
