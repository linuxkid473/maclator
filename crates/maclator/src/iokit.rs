//! IOKit shim for Apple Silicon-only services.
//!
//! The guest's IOKit MIG messages normally pass straight through to the host
//! kernel. Services that only exist on Apple Silicon (for example the
//! diagnostic data access driver MobileGestalt reads syscfg from) don't exist
//! on an Intel host, so Maclator answers for them: a lookup of such a service
//! returns a port we own, and messages sent to that port are answered here
//! without ever reaching the kernel.

use std::collections::HashMap;
use std::sync::Mutex;

const KERN_SUCCESS: u32 = 0;
const K_IO_RETURN_UNSUPPORTED: u32 = 0xe000_02c7;

const ID_GET_MATCHING_SERVICE: u32 = 2880;
const ID_ADD_NOTIFICATION: u32 = 2884;
const ID_ITERATOR_NEXT: u32 = 2802;
const K_IO_RETURN_NO_DEVICE: u32 = 0xe000_02c0;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Service,
    /// Notification iterator that yields one fake service, then ends.
    Iterator { delivered: bool },
}

/// Service classes that exist only on Apple Silicon.
const FAKE_CLASSES: &[&str] = &["AppleDiagnosticDataAccessReadOnly"];

static FAKE_PORTS: Mutex<Option<HashMap<u32, Kind>>> = Mutex::new(None);

extern "C" {
    static mach_task_self_: u32;
    fn mach_port_allocate(task: u32, right: u32, name: *mut u32) -> i32;
    fn mach_port_insert_right(task: u32, name: u32, poly: u32, poly_poly: u32) -> i32;
}

fn new_fake_port(kind: Kind) -> Option<u32> {
    unsafe {
        let mut name = 0u32;
        if mach_port_allocate(mach_task_self_, 1 /* RECEIVE */, &mut name) != 0 {
            return None;
        }
        // Also hold a send right under the same name so the guest can "send" to it.
        if mach_port_insert_right(mach_task_self_, name, name, 20 /* MAKE_SEND */) != 0 {
            return None;
        }
        FAKE_PORTS.lock().unwrap().get_or_insert_with(HashMap::new).insert(name, kind);
        Some(name)
    }
}

fn kind_of(port: u32) -> Option<Kind> {
    if port == 0 {
        return None;
    }
    FAKE_PORTS.lock().unwrap().as_ref().and_then(|m| m.get(&port).copied())
}

unsafe fn write_error_reply(msg: *mut u32, req_id: u32, reply_port: u32, kr: u32) {
    // mig_reply_error_t: header, NDR record, RetCode.
    *msg = 0x1200;
    *msg.add(1) = 36;
    *msg.add(2) = 0;
    *msg.add(3) = reply_port;
    *msg.add(4) = 0;
    *msg.add(5) = req_id + 100;
    *msg.add(6) = 0; // NDR record, as the kernel writes it
    *msg.add(7) = 1;
    *msg.add(8) = kr;
}

unsafe fn write_service_reply(msg: *mut u32, req_id: u32, reply_port: u32, port: u32) {
    // Reply { Head; msgh_body; mach_msg_port_descriptor_t service; }
    *msg = 0x8000_1200; // complex
    *msg.add(1) = 40;
    *msg.add(2) = 0;
    *msg.add(3) = reply_port;
    *msg.add(4) = 0;
    *msg.add(5) = req_id + 100;
    *msg.add(6) = 1; // descriptor count
    *msg.add(7) = port;
    *msg.add(8) = 0;
    *msg.add(9) = 0x11 << 16; // disposition MOVE_SEND, type PORT
}

/// Called for `mach_msg2` before it reaches the host. Returns true when the
/// message was answered here.
pub fn intercept(msg_addr: u64, options: u64) -> bool {
    if msg_addr == 0 || options & 1 == 0 || options & 2 == 0 {
        return false; // only combined send+receive RPCs
    }
    unsafe {
        let m = msg_addr as *mut u32;
        let size = (*m.add(1)) as usize;
        let rport = *m.add(2);
        let id = *m.add(5);
        let lport = *m.add(3);
        let kind = kind_of(rport);
        if kind == Some(Kind::Service) || matches!(kind, Some(Kind::Iterator { .. }) if id != ID_ITERATOR_NEXT) {
            write_error_reply(m, id, lport, K_IO_RETURN_UNSUPPORTED);
            return true;
        }
        if let Some(Kind::Iterator { delivered }) = kind {
            if delivered {
                write_error_reply(m, id, lport, K_IO_RETURN_NO_DEVICE);
            } else if let Some(svc) = new_fake_port(Kind::Service) {
                FAKE_PORTS.lock().unwrap().as_mut().unwrap().insert(rport, Kind::Iterator { delivered: true });
                write_service_reply(m, id, lport, svc);
            } else {
                write_error_reply(m, id, lport, K_IO_RETURN_NO_DEVICE);
            }
            return true;
        }
        if (id == ID_GET_MATCHING_SERVICE || id == ID_ADD_NOTIFICATION) && size <= 4096 {
            let body = std::slice::from_raw_parts(msg_addr as *const u8, size);
            let text = String::from_utf8_lossy(body);
            if FAKE_CLASSES.iter().any(|c| text.contains(c)) {
                let k = if id == ID_ADD_NOTIFICATION { Kind::Iterator { delivered: false } } else { Kind::Service };
                if let Some(p) = new_fake_port(k) {
                    write_service_reply(m, id, lport, p);
                    return true;
                }
            }
        }
    }
    // Thread suspend/resume/get_state on guest threads (Chromium's stack-sampling profiler).
    // Host threads run guest code, so suspending one can deadlock the whole process and the
    // host register state is meaningless to the guest: make suspend/resume no-ops and
    // get_state a failure.
    if msg_addr != 0 && options & 3 == 3 {
        let m = msg_addr as *mut u32;
        unsafe {
            let id = *m.add(5);
            let lport = *m.add(3);
            match id {
                3605 | 3606 => {
                    write_error_reply(m, id, lport, KERN_SUCCESS);
                    return true;
                }
                3603 => {
                    write_error_reply(m, id, lport, 5 /* KERN_FAILURE */);
                    return true;
                }
                _ => {}
            }
        }
    }
    false
}
