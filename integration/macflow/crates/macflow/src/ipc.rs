//! Bounded, authenticated native XPC transport. All callbacks contain panics.
//!
//! C XPC is used because dynamic Rust protocols cannot express NSXPC's extended
//! block argument metadata. No Objective-C object is made Send or Sync.
use block2::{DynBlock, RcBlock};
use macflow_protocol::{MAX_PAYLOAD, Request, Response, SCHEMA};
use std::{
    ffi::{CString, c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug)]
pub struct Peer {
    pub pid: i32,
    pub uid: u32,
}
type Obj = *mut c_void;
type Queue = *mut c_void;
type Handler = Arc<dyn Fn(Request, Peer) -> Response + Send + Sync>;

// CFRunLoop.h version-0 source context. CFIndex/CFHashCode are pointer-sized
// on this native target; Boolean is unsigned char, not Rust bool.
#[repr(C)]
struct RunLoopSourceContext {
    version: isize,
    info: *mut c_void,
    retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<unsafe extern "C" fn(*const c_void)>,
    copy_description: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    equal: Option<unsafe extern "C" fn(*const c_void, *const c_void) -> u8>,
    hash: Option<unsafe extern "C" fn(*const c_void) -> usize>,
    schedule: Option<unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void)>,
    cancel: Option<unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void)>,
    perform: Option<unsafe extern "C" fn(*mut c_void)>,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    fn CFRunLoopGetMain() -> *const c_void;
    fn CFRunLoopSourceCreate(
        allocator: *const c_void,
        order: isize,
        context: *mut RunLoopSourceContext,
    ) -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *const c_void, source: *const c_void, mode: *const c_void);
    fn CFRunLoopRemoveSource(run_loop: *const c_void, source: *const c_void, mode: *const c_void);
    fn CFRunLoopSourceInvalidate(source: *const c_void);
    fn CFRunLoopRunInMode(
        mode: *const c_void,
        seconds: f64,
        return_after_source_handled: u8,
    ) -> i32;
    fn CFRelease(object: *const c_void);
}

unsafe extern "C" fn keep_alive_perform(_: *mut c_void) {
    // This source is intentionally never signalled. Keep the FFI boundary
    // guarded even if CoreFoundation invokes it during future changes.
    let _ = catch_unwind(AssertUnwindSafe(|| {}));
}

/// An inert source prevents a service's main run loop from immediately
/// returning "finished" when it otherwise has no installed sources. It does
/// not wake the thread; CF sleeps until real Cocoa events or the pool timeout.
struct RunLoopKeepAlive {
    run_loop: *const c_void,
    source: NonNull<c_void>,
}
impl RunLoopKeepAlive {
    fn new(run_loop: *const c_void) -> Result<Self, String> {
        if run_loop.is_null() {
            return Err("service main run loop unavailable".into());
        }
        let mut context = RunLoopSourceContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copy_description: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(keep_alive_perform),
        };
        let source =
            NonNull::new(unsafe { CFRunLoopSourceCreate(std::ptr::null(), 0, &mut context) })
                .ok_or_else(|| "service keepalive source allocation failed".to_string())?;
        unsafe { CFRunLoopAddSource(run_loop, source.as_ptr(), kCFRunLoopDefaultMode) };
        Ok(Self { run_loop, source })
    }
}
impl Drop for RunLoopKeepAlive {
    fn drop(&mut self) {
        unsafe {
            CFRunLoopRemoveSource(self.run_loop, self.source.as_ptr(), kCFRunLoopDefaultMode);
            CFRunLoopSourceInvalidate(self.source.as_ptr());
            CFRelease(self.source.as_ptr());
        }
    }
}

#[link(name = "System")]
unsafe extern "C" {
    fn xpc_connection_create_mach_service(name: *const c_char, queue: Queue, flags: u64) -> Obj;
    fn xpc_connection_set_event_handler(connection: Obj, handler: &DynBlock<dyn Fn(Obj)>);
    fn xpc_connection_set_peer_code_signing_requirement(
        connection: Obj,
        requirement: *const c_char,
    ) -> i32;
    fn xpc_connection_resume(connection: Obj);
    fn xpc_connection_cancel(connection: Obj);
    fn xpc_connection_get_euid(connection: Obj) -> u32;
    fn xpc_connection_get_pid(connection: Obj) -> i32;
    fn xpc_connection_send_message(connection: Obj, message: Obj);
    fn xpc_connection_send_message_with_reply(
        connection: Obj,
        message: Obj,
        queue: Queue,
        reply: &DynBlock<dyn Fn(Obj)>,
    );
    fn xpc_dictionary_create(keys: *const *const c_char, values: *const Obj, count: usize) -> Obj;
    fn xpc_dictionary_create_reply(message: Obj) -> Obj;
    fn xpc_dictionary_set_data(
        dictionary: Obj,
        key: *const c_char,
        value: *const c_void,
        length: usize,
    );
    fn xpc_dictionary_get_data(
        dictionary: Obj,
        key: *const c_char,
        length: *mut usize,
    ) -> *const c_void;
    fn xpc_get_type(object: Obj) -> *const c_void;
    fn xpc_retain(object: Obj) -> Obj;
    fn xpc_release(object: Obj);
    static _xpc_type_connection: c_char;
    static _xpc_type_dictionary: c_char;
    static _xpc_type_error: c_char;
    static _xpc_error_connection_invalid: c_char;
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> Queue;
    fn dispatch_release(object: Obj);
}

struct Owned(NonNull<c_void>);
impl Owned {
    unsafe fn new(pointer: Obj) -> Result<Self, String> {
        NonNull::new(pointer)
            .map(Self)
            .ok_or_else(|| "XPC allocation failed".into())
    }
    fn pointer(&self) -> Obj {
        self.0.as_ptr()
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { xpc_release(self.pointer()) }
    }
}
struct DispatchQueue(NonNull<c_void>);
impl DispatchQueue {
    fn new(label: &str) -> Result<Self, String> {
        let label = CString::new(label).map_err(|_| "invalid queue label")?;
        NonNull::new(unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null()) })
            .map(Self)
            .ok_or_else(|| "dispatch queue allocation failed".into())
    }
    fn pointer(&self) -> Queue {
        self.0.as_ptr()
    }
}
impl Drop for DispatchQueue {
    fn drop(&mut self) {
        unsafe { dispatch_release(self.pointer()) }
    }
}
struct Connection(Owned);
impl Connection {
    fn create(service: &str, queue: &DispatchQueue, flags: u64) -> Result<Self, String> {
        let name = CString::new(service).map_err(|_| "invalid service name")?;
        unsafe {
            Owned::new(xpc_connection_create_mach_service(
                name.as_ptr(),
                queue.pointer(),
                flags,
            ))
        }
        .map(Self)
    }
    fn pointer(&self) -> Obj {
        self.0.pointer()
    }
    fn requirement(&self, requirement: &str) -> Result<(), String> {
        let text = CString::new(requirement).map_err(|_| "invalid signing requirement")?;
        let code = unsafe {
            xpc_connection_set_peer_code_signing_requirement(self.pointer(), text.as_ptr())
        };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("XPC signing requirement rejected ({code})"))
        }
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        unsafe { xpc_connection_cancel(self.pointer()) }
    }
}

fn decode_request(bytes: &[u8]) -> Result<Request, String> {
    if bytes.is_empty() || bytes.len() > MAX_PAYLOAD {
        return Err("request payload exceeds bounds".into());
    }
    let request: Request =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid request: {e}"))?;
    if request.schema != SCHEMA {
        return Err("unsupported request schema".into());
    }
    Ok(request)
}
fn encode_response(response: &Response) -> Vec<u8> {
    match serde_json::to_vec(response) {
        Ok(bytes) if bytes.len() <= MAX_PAYLOAD => bytes,
        _ => serde_json::to_vec(&Response::failure(
            response.id,
            "response exceeds XPC payload bound; reduce status detail",
        ))
        .unwrap_or_default(),
    }
}
unsafe fn payload(object: Obj) -> Result<Vec<u8>, String> {
    if object.is_null()
        || unsafe { xpc_get_type(object) } != std::ptr::addr_of!(_xpc_type_dictionary).cast()
    {
        return Err("XPC peer disconnected, unavailable, or failed authentication".into());
    }
    let mut length = 0usize;
    let pointer = unsafe { xpc_dictionary_get_data(object, c"payload".as_ptr(), &mut length) };
    if pointer.is_null() || length == 0 || length > MAX_PAYLOAD {
        return Err("XPC payload missing or exceeds bounds".into());
    }
    // XPC owns these bytes and they remain valid throughout the callback.
    Ok(unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) }.to_vec())
}
unsafe fn reply_to(connection: Obj, event: Obj, response: &Response) {
    if let Ok(reply) = unsafe { Owned::new(xpc_dictionary_create_reply(event)) } {
        let bytes = encode_response(response);
        unsafe {
            xpc_dictionary_set_data(
                reply.pointer(),
                c"payload".as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
            );
            xpc_connection_send_message(connection, reply.pointer());
        }
    }
}

/// Blocks while the Mach service is live. launchd owns service discovery.
/// Signing requirements are installed on the listener before activation.
pub fn serve(
    service_name: &str,
    handler: Handler,
    expected_requirements: Vec<String>,
) -> Result<(), String> {
    // NSWorkspace caches foreground state and updates it through main-thread
    // Cocoa notifications. Parking this thread leaves that cache stale even
    // though the XPC dispatch queue remains responsive.
    let _main_thread = objc2::MainThreadMarker::new()
        .ok_or_else(|| "XPC service must be started on the main thread".to_string())?;
    let _keep_alive = RunLoopKeepAlive::new(unsafe { CFRunLoopGetMain() })?;
    if service_name == macflow_protocol::HELPER_SERVICE && expected_requirements.is_empty() {
        return Err("privileged helper requires a caller signing requirement".into());
    }
    let queue = DispatchQueue::new(&format!("{service_name}.xpc"))?;
    let listener = Connection::create(service_name, &queue, 1)?; // XPC_CONNECTION_MACH_SERVICE_LISTENER
    if !expected_requirements.is_empty() {
        let requirement = expected_requirements
            .iter()
            .map(|r| format!("({r})"))
            .collect::<Vec<_>>()
            .join(" or ");
        listener.requirement(&requirement)?;
    }
    let listener_name = service_name.to_owned();
    let listener_handler = RcBlock::new(move |event: Obj| {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if event
                == std::ptr::addr_of!(_xpc_error_connection_invalid)
                    .cast_mut()
                    .cast()
            {
                eprintln!(
                    "MacFlow XPC listener {listener_name} invalidated; exiting for service recovery"
                );
                std::process::exit(70);
            }
            if event.is_null()
                || unsafe { xpc_get_type(event) } != std::ptr::addr_of!(_xpc_type_connection).cast()
            {
                return;
            }
            let peer = Peer {
                pid: unsafe { xpc_connection_get_pid(event) },
                uid: unsafe { xpc_connection_get_euid(event) },
            };
            if peer.pid <= 0
                || (unsafe { libc::geteuid() } != 0 && peer.uid != unsafe { libc::geteuid() })
            {
                unsafe { xpc_connection_cancel(event) };
                return;
            }
            // XPC connections are thread safe C objects. A numeric address is
            // captured only while this explicit retain remains owned below.
            unsafe { xpc_retain(event) };
            let address = event as usize;
            let released = Arc::new(AtomicBool::new(false));
            let release_flag = released.clone();
            let handler = handler.clone();
            let peer_handler = RcBlock::new(move |message: Obj| {
                let connection = address as Obj;
                if message.is_null() {
                    return;
                }
                if unsafe { xpc_get_type(message) } == std::ptr::addr_of!(_xpc_type_error).cast() {
                    if !release_flag.swap(true, Ordering::AcqRel) {
                        unsafe {
                            xpc_connection_cancel(connection);
                            xpc_release(connection);
                        }
                    }
                    return;
                }
                let result = catch_unwind(AssertUnwindSafe(|| {
                    match unsafe { payload(message) }.and_then(|bytes| decode_request(&bytes)) {
                        Ok(request) => {
                            let id = request.id;
                            match catch_unwind(AssertUnwindSafe(|| handler(request, peer))) {
                                Ok(response) if response.id == id && response.schema == SCHEMA => {
                                    response
                                }
                                Ok(_) => {
                                    Response::failure(id, "handler response identity mismatch")
                                }
                                Err(_) => Response::failure(id, "service callback panic contained"),
                            }
                        }
                        Err(error) => Response::failure(0, error),
                    }
                }))
                .unwrap_or_else(|_| Response::failure(0, "transport callback panic contained"));
                // One synchronous reply per inbound request, never retried.
                let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
                    reply_to(connection, message, &result)
                }));
            });
            unsafe {
                xpc_connection_set_event_handler(event, &peer_handler);
                xpc_connection_resume(event);
            }
        }));
    });
    unsafe {
        xpc_connection_set_event_handler(listener.pointer(), &listener_handler);
        xpc_connection_resume(listener.pointer());
    }
    loop {
        let result = objc2::rc::autoreleasepool(|_| unsafe {
            // Bounded iterations drain autoreleased Cocoa event objects. The
            // inert source guarantees an idle helper sleeps instead of spin.
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 1.0, 0)
        });
        match result {
            3 | 4 => {} // timed out / source handled
            _ => {
                return Err(format!(
                    "service main run loop exited unexpectedly ({result})"
                ));
            }
        }
    }
}

/// Asynchronous XPC with a bounded caller wait; writes are never retried.
pub fn call(
    service_name: &str,
    privileged: bool,
    request: &Request,
    expected_requirement: Option<&str>,
    timeout: Duration,
) -> Result<Response, String> {
    let bytes = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    decode_request(&bytes)?;
    if privileged && expected_requirement.is_none() {
        return Err("privileged connection requires a server signing requirement".into());
    }
    let queue = DispatchQueue::new("org.macflow.client.reply")?;
    let connection = Connection::create(service_name, &queue, if privileged { 2 } else { 0 })?;
    if let Some(requirement) = expected_requirement {
        connection.requirement(requirement)?;
    }
    let (tx, rx) = mpsc::sync_channel::<Result<Vec<u8>, String>>(1);
    let complete = Arc::new(AtomicBool::new(false));
    let error_tx = tx.clone();
    let error_complete = complete.clone();
    let event_handler = RcBlock::new(move |event: Obj| {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if !event.is_null()
                && unsafe { xpc_get_type(event) } == std::ptr::addr_of!(_xpc_type_error).cast()
                && !error_complete.swap(true, Ordering::AcqRel)
            {
                let _ = error_tx.try_send(Err(
                    "XPC service unavailable, disconnected, or failed authentication".into(),
                ));
            }
        }));
    });
    unsafe {
        xpc_connection_set_event_handler(connection.pointer(), &event_handler);
        xpc_connection_resume(connection.pointer());
    }
    let message =
        unsafe { Owned::new(xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0)) }?;
    unsafe {
        xpc_dictionary_set_data(
            message.pointer(),
            c"payload".as_ptr(),
            bytes.as_ptr().cast(),
            bytes.len(),
        );
    }
    let reply_handler = RcBlock::new(move |reply: Obj| {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if !complete.swap(true, Ordering::AcqRel) {
                let _ = tx.try_send(unsafe { payload(reply) });
            }
        }));
    });
    unsafe {
        xpc_connection_send_message_with_reply(
            connection.pointer(),
            message.pointer(),
            queue.pointer(),
            &reply_handler,
        );
    }
    let bytes = rx.recv_timeout(timeout).map_err(|_| {
        "XPC timeout; request outcome may be unknown; refresh status before retrying".to_string()
    })??;
    let response: Response =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid response: {e}"))?;
    if response.schema != SCHEMA || response.id != request.id {
        return Err("XPC response identity/schema mismatch".into());
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keep_alive_source_sleeps_without_other_sources() {
        unsafe extern "C" {
            fn CFRunLoopGetCurrent() -> *const c_void;
        }
        // Test on a dedicated thread so no unrelated run-loop source or main
        // thread GUI test can hide an empty-loop busy-spin regression.
        std::thread::spawn(|| {
            let _source = RunLoopKeepAlive::new(unsafe { CFRunLoopGetCurrent() }).unwrap();
            let start = std::time::Instant::now();
            let result = unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.03, 0) };
            assert_eq!(result, 3);
            assert!(start.elapsed() >= Duration::from_millis(20));
        })
        .join()
        .unwrap();
    }
    #[test]
    fn run_loop_source_context_matches_native_pointer_layout() {
        assert_eq!(
            std::mem::size_of::<RunLoopSourceContext>(),
            10 * std::mem::size_of::<usize>()
        );
        assert_eq!(
            std::mem::offset_of!(RunLoopSourceContext, perform),
            9 * std::mem::size_of::<usize>()
        );
    }
    #[test]
    fn rejects_bad_schema_and_large_payload() {
        assert!(decode_request(&vec![b' '; MAX_PAYLOAD + 1]).is_err());
        assert!(decode_request(br#"{"schema":2,"id":1,"action":{"command":"status"}}"#).is_err());
    }
    #[test]
    fn response_overflow_is_bounded_and_preserves_id() {
        let response = Response::success(42, serde_json::json!({"text":"x".repeat(MAX_PAYLOAD)}));
        let bytes = encode_response(&response);
        assert!(bytes.len() < MAX_PAYLOAD);
        let decoded: Response = serde_json::from_slice(&bytes).unwrap();
        assert!(!decoded.ok);
        assert_eq!(decoded.id, 42);
    }
    #[test]
    fn unavailable_service_returns_prompt_error() {
        let result = call(
            "org.macflow.test.nonexistent",
            false,
            &Request {
                schema: SCHEMA,
                id: 7,
                action: macflow_protocol::Command::Status,
            },
            None,
            Duration::from_millis(300),
        );
        assert!(result.is_err());
    }
}
