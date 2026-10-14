//! macOS boundaries for MacFlow. Raw handles and foreign buffers never leave this crate.
#![deny(unsafe_op_in_unsafe_fn)]

use macflow_protocol::{
    GpuTelemetry, MemoryPressure, ProcessIdentity, ProcessSample, TaskRole, Telemetry,
};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSProcessInfo, NSString};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::io::Read;
use std::mem::{MaybeUninit, size_of};
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;

const MAX_PROCESSES: usize = 16_384;
const MAX_PATH: usize = 4096;
const MAX_SYSCTL: usize = 64 * 1024;

/// The monotonic epoch is shared by processes and includes system sleep.
pub fn now_ms() -> u64 {
    let timebase = TIMEBASE.get_or_init(|| {
        let mut info = MachTimebase { numer: 1, denom: 1 };
        // SAFETY: valid, initialized storage for the fixed ABI structure.
        if unsafe { mach_timebase_info(&mut info) } != 0 || info.denom == 0 {
            (1, 1)
        } else {
            (info.numer, info.denom)
        }
    });
    // SAFETY: mach_continuous_time has no arguments or ownership contract.
    let ticks = unsafe { mach_continuous_time() };
    ((ticks as u128 * timebase.0 as u128 / timebase.1 as u128) / 1_000_000) as u64
}

static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
static BOOT: OnceLock<String> = OnceLock::new();

pub fn boot_id() -> String {
    BOOT.get_or_init(|| {
        sysctl_bytes("kern.bootsessionuuid")
            .ok()
            .and_then(|v| String::from_utf8(v.into_iter().take_while(|b| *b != 0).collect()).ok())
            .filter(|v| !v.is_empty())
            // No fabricated UUID: the fallback still identifies this boot across processes.
            .or_else(|| {
                sysctl_value::<libc::timeval>("kern.boottime")
                    .ok()
                    .map(|v| format!("boottime:{}:{}", v.tv_sec, v.tv_usec))
            })
            .unwrap_or_else(|| "unknown-boot".to_string())
    })
    .clone()
}

pub fn current_uid() -> u32 {
    // SAFETY: geteuid has no arguments.
    unsafe { libc::geteuid() }
}

fn os_error(operation: &str) -> String {
    format!("{operation}: {}", std::io::Error::last_os_error())
}

fn sysctl_bytes(name: &str) -> Result<Vec<u8>, String> {
    let name = CString::new(name).map_err(|_| "invalid sysctl name".to_string())?;
    let mut len = 0usize;
    // SAFETY: name is NUL-terminated; the null output requests a size only.
    if unsafe { libc::sysctlbyname(name.as_ptr(), ptr::null_mut(), &mut len, ptr::null_mut(), 0) }
        != 0
    {
        return Err(os_error("sysctl size"));
    }
    if len == 0 || len > MAX_SYSCTL {
        return Err("sysctl length out of bounds".into());
    }
    let mut bytes = vec![0u8; len];
    // SAFETY: bytes holds exactly len writable bytes. Kernel updates len.
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut len,
            ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(os_error("sysctl read"));
    }
    if len > bytes.len() {
        return Err("sysctl length changed beyond buffer".into());
    }
    bytes.truncate(len);
    Ok(bytes)
}

fn sysctl_value<T: Copy>(name: &str) -> Result<T, String> {
    let bytes = sysctl_bytes(name)?;
    if bytes.len() != size_of::<T>() {
        return Err(format!("unexpected {name} ABI length"));
    }
    // SAFETY: the primitive / C structure has exact verified length. Buffer need not be aligned.
    Ok(unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<T>()) })
}

fn bsd_info(pid: i32) -> Result<libc::proc_bsdinfo, String> {
    if pid <= 0 {
        return Err("invalid process id".into());
    }
    let mut value = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // SAFETY: libproc receives one ABI-sized writable object; result size is verified below.
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            value.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if bytes != size_of::<libc::proc_bsdinfo>() as i32 {
        return Err(os_error("proc_pidinfo identity"));
    }
    // SAFETY: exact full structure was filled by libproc.
    let value = unsafe { value.assume_init() };
    if value.pbi_pid != pid as u32 {
        return Err("process identity mismatch".into());
    }
    Ok(value)
}

fn executable(pid: i32) -> Result<String, String> {
    let mut buffer = [0u8; MAX_PATH];
    // SAFETY: the array is initialized and its full size is passed to libproc.
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 || length as usize > buffer.len() {
        return Err(os_error("proc_pidpath"));
    }
    let end = buffer
        .iter()
        .position(|v| *v == 0)
        .ok_or_else(|| "unterminated process path".to_string())?;
    String::from_utf8(buffer[..end].to_vec()).map_err(|_| "process path is not UTF-8".into())
}

fn identity_from_info(info: &libc::proc_bsdinfo, path: String) -> ProcessIdentity {
    ProcessIdentity {
        boot_id: boot_id(),
        pid: info.pbi_pid as i32,
        start_secs: info.pbi_start_tvsec,
        start_micros: info.pbi_start_tvusec,
        uid: info.pbi_uid,
        executable: path,
    }
}

pub fn identity(pid: i32) -> Result<ProcessIdentity, String> {
    let before = bsd_info(pid)?;
    let path = executable(pid)?;
    let after = bsd_info(pid)?;
    if before.pbi_start_tvsec != after.pbi_start_tvsec
        || before.pbi_start_tvusec != after.pbi_start_tvusec
        || before.pbi_uid != after.pbi_uid
    {
        return Err("process changed while reading identity".into());
    }
    Ok(identity_from_info(&after, path))
}

pub fn nice(pid: i32) -> Result<i32, String> {
    if pid <= 0 {
        return Err("invalid process id".into());
    }
    // SAFETY: getpriority accepts a live PID; errno distinguishes a valid -1 from error.
    unsafe {
        *libc::__error() = 0;
        let value = libc::getpriority(libc::PRIO_PROCESS, pid as u32);
        if *libc::__error() != 0 {
            Err(os_error("getpriority"))
        } else {
            Ok(value)
        }
    }
}

/// Protected roles cannot be overridden by user rules or helper requests.
pub fn protected_reason(id: &ProcessIdentity) -> Option<String> {
    protected_path_reason(id.uid, &id.executable)
}

fn protected_path_reason(uid: u32, path: &str) -> Option<String> {
    let name = Path::new(path)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if uid == 0 {
        return Some("root process".into());
    }
    if path.starts_with("/System/")
        || path.starts_with("/usr/libexec/")
        || path.starts_with("/usr/sbin/")
        || path.starts_with("/sbin/")
    {
        return Some("system service".into());
    }
    let protected = [
        "windowserver",
        "loginwindow",
        "dock",
        "finder",
        "systemuiserver",
        "coreaudiod",
        "audiomxd",
        "bluetoothd",
        "runningboardd",
        "powerd",
        "cameracaptured",
        "avconferenced",
        "replayd",
        "controlcenter",
        "voiceover",
        "macflow",
    ];
    if protected
        .iter()
        .any(|v| name == *v || name.starts_with("macflow"))
    {
        return Some("desktop, media or management dependency".into());
    }
    let lower = path.to_ascii_lowercase();
    if [
        "/music.app/",
        "/facetime.app/",
        "/quicktime player.app/",
        "/zoom.us.app/",
        "/microsoft teams.app/",
        "/uu remote.app/",
        "/uuremote",
        "/teamviewer",
        "/anydesk",
        "inputmethod",
        "input method",
        "/obs.app/",
    ]
    .iter()
    .any(|v| lower.contains(v))
    {
        return Some("media, input or remote-session application".into());
    }
    None
}

/// Validates the exact start identity immediately before changing a third-party process.
/// PID APIs are not atomic with identity queries; this limitation is recorded in the product.
pub fn set_nice(id: &ProcessIdentity, target: i32) -> Result<(), String> {
    if !(-20..=19).contains(&target) {
        return Err("nice target out of range".into());
    }
    if boot_id() == "unknown-boot" {
        return Err("boot identity unavailable".into());
    }
    if identity(id.pid)? != *id {
        return Err("process identity changed".into());
    }
    if let Some(reason) = protected_reason(id) {
        return Err(format!("protected process: {reason}"));
    }
    if current_uid() != 0 && id.uid != current_uid() {
        return Err("process belongs to another user".into());
    }
    // SAFETY: target range and process identity were validated. No pointer arguments.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, id.pid as u32, target) } != 0 {
        return Err(os_error("setpriority"));
    }
    if identity(id.pid)? != *id {
        return Err("process exited or changed during priority update".into());
    }
    Ok(())
}

pub fn apply_own_qos(qos: u32) -> Result<(), String> {
    if ![0x09, 0x11, 0x15, 0x19, 0x21].contains(&qos) {
        return Err("unsupported QoS class".into());
    }
    // SAFETY: uses only documented classes and relative priority zero; affects caller thread.
    let result = unsafe { pthread_set_qos_class_self_np(qos, 0) };
    if result != 0 {
        Err(format!(
            "pthread QoS: {}",
            std::io::Error::from_raw_os_error(result)
        ))
    } else {
        Ok(())
    }
}

pub fn apply_background_io() -> Result<(), String> {
    // SAFETY: disk/passive/process is a documented policy and affects only the calling process.
    if unsafe { setiopolicy_np(0, 0, 2) } != 0 {
        Err(os_error("setiopolicy_np"))
    } else {
        Ok(())
    }
}

#[derive(Default)]
pub struct Sampler {
    ticks: Option<[u32; 4]>,
    processes: HashMap<ProcessIdentity, (u64, u64)>,
    gpu: GpuTelemetry,
    gpu_at: Option<u64>,
    driver_version: Option<String>,
    vm_counters: Option<[u64; 4]>,
}

impl Sampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample(&mut self) -> Telemetry {
        let at_ms = now_ms();
        let mut out = Telemetry {
            at_ms,
            boot_id: boot_id(),
            ..Telemetry::default()
        };
        match cpu_ticks() {
            Ok(ticks) => {
                out.cpu_percent = self.ticks.and_then(|previous| cpu_percent(previous, ticks));
                self.ticks = Some(ticks);
            }
            Err(error) => {
                out.errors.push(error);
                self.ticks = None;
            }
        }
        out.memory_pressure = match sysctl_value::<i32>("kern.memorystatus_vm_pressure_level") {
            Ok(1) => MemoryPressure::Normal,
            Ok(2) => MemoryPressure::Warning,
            Ok(4) => MemoryPressure::Critical,
            Ok(value) => {
                out.errors
                    .push(format!("unknown memory-pressure level {value}"));
                MemoryPressure::Unknown
            }
            Err(error) => {
                out.errors.push(error);
                MemoryPressure::Unknown
            }
        };
        match vm_statistics() {
            Ok((bytes, counters)) => {
                out.compressed_bytes = Some(bytes);
                if let Some(previous) = self.vm_counters {
                    out.compressed_pages_delta = counters[0].checked_sub(previous[0]);
                    out.decompressed_pages_delta = counters[1].checked_sub(previous[1]);
                    out.swap_in_pages_delta = counters[2].checked_sub(previous[2]);
                    out.swap_out_pages_delta = counters[3].checked_sub(previous[3]);
                }
                self.vm_counters = Some(counters);
            }
            Err(error) => {
                out.errors.push(error);
                self.vm_counters = None;
            }
        }
        match sysctl_value::<libc::xsw_usage>("vm.swapusage") {
            Ok(value) => out.swap_used_bytes = Some(value.xsu_used),
            Err(error) => out.errors.push(error),
        }
        // ACPI power reporting on this machine is not validated. Do not use it as a control signal.
        out.ac_power = None;
        out.thermal = thermal_state();
        let app_metadata = application_metadata();
        out.foreground_pid = app_metadata.0;
        let mut next = HashMap::new();
        match process_ids() {
            Ok(pids) => {
                for pid in pids {
                    let Ok(info) = bsd_info(pid) else {
                        continue;
                    };
                    let Ok(path) = executable(pid) else {
                        continue;
                    };
                    let id = identity_from_info(&info, path);
                    let Ok(usage) = process_usage(pid) else {
                        continue;
                    };
                    let cpu_time = usage.ri_user_time.saturating_add(usage.ri_system_time);
                    let cpu = self
                        .processes
                        .get(&id)
                        .and_then(|(previous_time, previous_ms)| {
                            let elapsed = at_ms.checked_sub(*previous_ms)?;
                            if elapsed == 0 {
                                return None;
                            }
                            Some(
                                cpu_time.saturating_sub(*previous_time) as f64
                                    / (elapsed as f64 * 1_000_000.0)
                                    * 100.0,
                            )
                        })
                        .unwrap_or(0.0);
                    next.insert(id.clone(), (cpu_time, at_ms));
                    let name = array_string(&info.pbi_name)
                        .filter(|v| !v.is_empty())
                        .or_else(|| array_string(&info.pbi_comm))
                        .unwrap_or_else(|| format!("pid {pid}"));
                    let reason = if info.pbi_flags & 1 != 0 {
                        Some("kernel system process".into())
                    } else {
                        protected_reason(&id)
                    };
                    let role = if reason.is_some() {
                        TaskRole::Protected
                    } else if out.foreground_pid == Some(pid) {
                        TaskRole::Foreground
                    } else {
                        TaskRole::Unknown
                    };
                    out.processes.push(ProcessSample {
                        identity: id,
                        parent_pid: info.pbi_ppid as i32,
                        name,
                        bundle_id: app_metadata.1.get(&pid).cloned(),
                        nice: info.pbi_nice,
                        cpu_percent: cpu,
                        memory_bytes: usage.ri_phys_footprint,
                        role,
                        protected_reason: reason,
                    });
                }
            }
            Err(error) => out.errors.push(error),
        }
        self.processes = next;
        if self
            .gpu_at
            .is_none_or(|last| at_ms.saturating_sub(last) >= 30_000)
        {
            self.gpu = gpu_telemetry();
            self.driver_version = nullmoth_version();
            self.gpu.driver_version = self.driver_version.clone();
            self.gpu_at = Some(at_ms);
        }
        out.gpu = self.gpu.clone();
        out
    }
}

fn process_ids() -> Result<Vec<i32>, String> {
    let mut pids = vec![0i32; MAX_PROCESSES];
    // SAFETY: initialized PID array with bounded byte count.
    let length = unsafe {
        libc::proc_listpids(
            1,
            0,
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<i32>()) as i32,
        )
    };
    if length <= 0
        || length as usize > pids.len() * size_of::<i32>()
        || length as usize % size_of::<i32>() != 0
    {
        return Err(os_error("proc_listpids"));
    }
    if length as usize == pids.len() * size_of::<i32>() {
        return Err("process inventory exceeds safe bound".into());
    }
    pids.truncate(length as usize / size_of::<i32>());
    pids.retain(|pid| *pid > 0);
    Ok(pids)
}

fn process_usage(pid: i32) -> Result<libc::rusage_info_v0, String> {
    let mut value = MaybeUninit::<libc::rusage_info_v0>::uninit();
    // SAFETY: RUSAGE_INFO_V0 fills the exact ABI-sized rusage_info_v0 buffer.
    if unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, value.as_mut_ptr().cast()) } != 0 {
        return Err(os_error("proc_pid_rusage"));
    }
    // SAFETY: the call succeeded and filled the selected flavor.
    Ok(unsafe { value.assume_init() })
}

fn array_string(value: &[c_char]) -> Option<String> {
    let bytes: Vec<u8> = value
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as u8)
        .collect();
    String::from_utf8(bytes).ok()
}

struct HostPort(u32);
impl HostPort {
    fn new() -> Self {
        // SAFETY: returns one owned send right, released by Drop.
        Self(unsafe { mach_host_self() })
    }
}
impl Drop for HostPort {
    fn drop(&mut self) {
        // SAFETY: this owns the send right returned by mach_host_self.
        unsafe {
            mach_port_deallocate(mach_task_self_, self.0);
        }
    }
}

fn cpu_ticks() -> Result<[u32; 4], String> {
    let host = HostPort::new();
    let mut value = MaybeUninit::<libc::host_cpu_load_info>::uninit();
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: pointer and count describe the fixed ABI structure, checked after call.
    let result = unsafe {
        libc::host_statistics(
            host.0,
            libc::HOST_CPU_LOAD_INFO,
            value.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if result != 0 || count != libc::HOST_CPU_LOAD_INFO_COUNT {
        return Err(format!("host CPU statistics: {result}"));
    }
    // SAFETY: full count was returned.
    Ok(unsafe { value.assume_init() }.cpu_ticks)
}

fn cpu_percent(before: [u32; 4], after: [u32; 4]) -> Option<f64> {
    let delta: [u64; 4] = std::array::from_fn(|i| after[i].wrapping_sub(before[i]) as u64);
    let total: u64 = delta.iter().sum();
    if total == 0 {
        None
    } else {
        Some((total - delta[2]) as f64 / total as f64 * 100.0)
    }
}

fn vm_statistics() -> Result<(u64, [u64; 4]), String> {
    let host = HostPort::new();
    // libc may describe a newer revision than the running kernel. All fields
    // are integers, so zero is a valid initialized value even for the tail
    // that an older kernel does not return.
    let mut value = unsafe { std::mem::zeroed::<libc::vm_statistics64>() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: writable storage covers the requested count. The prefix consumed
    // below is validated by byte length after host_statistics64.
    let result = unsafe {
        libc::host_statistics64(
            host.0,
            libc::HOST_VM_INFO64,
            (&mut value as *mut libc::vm_statistics64).cast(),
            &mut count,
        )
    };
    if result != 0 || !vm_compressor_prefix_present(count) {
        return Err(format!(
            "host memory statistics: result={result}, count={count}"
        ));
    }
    // SAFETY: sysconf has no pointer arguments.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err("unknown memory page size".into());
    }
    Ok((
        value.compressor_page_count as u64 * page as u64,
        [
            value.compressions,
            value.decompressions,
            value.swapins,
            value.swapouts,
        ],
    ))
}

fn vm_compressor_prefix_present(count: libc::mach_msg_type_number_t) -> bool {
    let returned_bytes = count as usize * std::mem::size_of::<libc::integer_t>();
    let required_bytes = std::mem::offset_of!(libc::vm_statistics64, compressor_page_count)
        + std::mem::size_of::<libc::natural_t>();
    (required_bytes..=std::mem::size_of::<libc::vm_statistics64>()).contains(&returned_bytes)
}

pub fn foreground_pid() -> Option<i32> {
    objc2::rc::autoreleasepool(|_| {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier())
    })
}

fn application_metadata() -> (Option<i32>, HashMap<i32, String>) {
    objc2::rc::autoreleasepool(|_| {
        let workspace = NSWorkspace::sharedWorkspace();
        let foreground = workspace
            .frontmostApplication()
            .map(|app| app.processIdentifier());
        let mut bundles = HashMap::new();
        for app in workspace.runningApplications().iter() {
            if let Some(id) = app.bundleIdentifier() {
                bundles.insert(app.processIdentifier(), id.to_string());
            }
        }
        (foreground, bundles)
    })
}

fn thermal_state() -> Option<String> {
    objc2::rc::autoreleasepool(|_| {
        let value = NSProcessInfo::processInfo().thermalState().0;
        match value {
            0 => Some("nominal"),
            1 => Some("fair"),
            2 => Some("serious"),
            3 => Some("critical"),
            _ => None,
        }
        .map(str::to_string)
    })
}

// CoreFoundation reference wrappers deliberately expose only type-checked reads.
type CfRef = *const c_void;
struct CfOwned(CfRef);
impl CfOwned {
    fn string(value: &str) -> Result<Self, String> {
        let len = isize::try_from(value.len()).map_err(|_| "CF string too long".to_string())?;
        // SAFETY: valid UTF-8 slice and exact byte length; CF copies it.
        let result =
            unsafe { CFStringCreateWithBytes(ptr::null(), value.as_ptr(), len, 0x08000100, false) };
        if result.is_null() {
            Err("CFString allocation failed".into())
        } else {
            Ok(Self(result))
        }
    }
}
impl Drop for CfOwned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: wrapper owns one Create/Copy reference.
            unsafe {
                CFRelease(self.0);
            }
        }
    }
}
struct IoObject(u32);
impl Drop for IoObject {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: one IOServiceGetMatchingService ownership reference.
            unsafe {
                IOObjectRelease(self.0);
            }
        }
    }
}

fn dictionary_value(dictionary: CfRef, key: &str) -> Option<CfRef> {
    if dictionary.is_null() {
        return None;
    }
    // SAFETY: valid borrowed CF object from API, type validated before dictionary access.
    if unsafe { CFGetTypeID(dictionary) != CFDictionaryGetTypeID() } {
        return None;
    }
    let key = CfOwned::string(key).ok()?;
    // SAFETY: dictionary and key are valid objects. Result is borrowed from dictionary.
    let value = unsafe { CFDictionaryGetValue(dictionary, key.0) };
    if value.is_null() { None } else { Some(value) }
}

fn number_u64(value: CfRef) -> Option<u64> {
    if value.is_null() {
        return None;
    }
    // SAFETY: only access numeric CF values after runtime type verification.
    if unsafe { CFGetTypeID(value) != CFNumberGetTypeID() } {
        return None;
    }
    let mut result = 0i64;
    // SAFETY: CFNumber kCFNumberSInt64Type=4 writes one i64.
    if unsafe { CFNumberGetValue(value, 4, (&mut result as *mut i64).cast()) } && result >= 0 {
        Some(result as u64)
    } else {
        None
    }
}

fn gpu_telemetry() -> GpuTelemetry {
    let mut out = GpuTelemetry::default();
    // SAFETY: constant terminated class name. IOKit consumes the matching dictionary.
    let service = unsafe { IOServiceGetMatchingService(0, IOServiceMatching(c"NVAccel".as_ptr())) };
    let service = IoObject(service);
    if service.0 == 0 {
        out.error = Some("NVAccel registry service unavailable".into());
        return out;
    }
    let mut properties = ptr::null();
    // SAFETY: registry object is live; API creates a retained dictionary output.
    let result =
        unsafe { IORegistryEntryCreateCFProperties(service.0, &mut properties, ptr::null(), 0) };
    if result != 0 || properties.is_null() {
        out.error = Some(format!("NVAccel registry properties: {result}"));
        return out;
    }
    let properties = CfOwned(properties);
    if let Some(stats) = dictionary_value(properties.0, "PerformanceStatistics") {
        out.vram_used_bytes = dictionary_value(stats, "inUseVidMemoryBytes").and_then(number_u64);
        out.recovery_count = dictionary_value(stats, "recoveryCount").and_then(number_u64);
    } else {
        out.error = Some("driver statistics unavailable".into());
    }
    // No validated utilization or temperature keys exist for this driver.
    out
}

fn cf_string(value: CfRef) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: type checked before accessing as CFString.
    if unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
        return None;
    }
    let mut buffer = vec![0i8; 4096];
    // SAFETY: buffer holds the advertised capacity. Function ensures NUL on success.
    if !unsafe {
        CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            buffer.len() as isize,
            0x08000100,
        )
    } {
        return None;
    }
    // SAFETY: NUL termination was guaranteed by successful CFStringGetCString.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_str()
        .ok()
        .map(str::to_string)
}

fn nullmoth_version() -> Option<String> {
    if let Ok(file) = std::fs::File::open("/Library/NullMoth/driver-version") {
        let mut bytes = Vec::new();
        if file.take(128).read_to_end(&mut bytes).is_ok() {
            if let Ok(value) = String::from_utf8(bytes) {
                let value = value.trim();
                if !value.is_empty()
                    && value.len() < 128
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                {
                    return Some(value.to_string());
                }
            }
        }
    }
    for path in [
        "/Library/Extensions/NVAccel.kext/Contents/Info.plist",
        "/Library/GPUBundles/NVMTLDriver.bundle/Contents/Info.plist",
    ] {
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file.take(1024 * 1024 + 1).read_to_end(&mut bytes).is_err() {
            continue;
        }
        if bytes.len() > 1024 * 1024 {
            continue;
        }
        // SAFETY: CFData copies the valid byte slice.
        let data =
            CfOwned(unsafe { CFDataCreate(ptr::null(), bytes.as_ptr(), bytes.len() as isize) });
        if data.0.is_null() {
            continue;
        }
        // SAFETY: valid CFData input, no mutable output/error pointers requested.
        let value = CfOwned(unsafe {
            CFPropertyListCreateWithData(ptr::null(), data.0, 0, ptr::null_mut(), ptr::null_mut())
        });
        if let Some(version) = dictionary_value(value.0, "CFBundleShortVersionString")
            .or_else(|| dictionary_value(value.0, "CFBundleVersion"))
            .and_then(cf_string)
        {
            return Some(version);
        }
    }
    None
}

/// Generates an exact code-directory-hash requirement rather than a path-based trust rule.
pub fn code_requirement_for_file(path: &Path) -> Result<String, String> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["--display", "--verbose=4"])
        .arg(path)
        .output()
        .map_err(|e| format!("codesign: {e}"))?;
    if !output.status.success() {
        return Err("file has no usable code signature".into());
    }
    let text =
        String::from_utf8(output.stderr).map_err(|_| "invalid codesign output".to_string())?;
    let hash = text
        .lines()
        .find_map(|line| line.strip_prefix("CDHash="))
        .ok_or_else(|| "code-directory hash missing".to_string())?;
    if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid code-directory hash".into());
    }
    Ok(format!("cdhash H\"{hash}\""))
}

pub fn verify_code_signature(pid: i32, requirements: &[String]) -> Result<(), String> {
    if pid <= 0 || requirements.is_empty() {
        return Err("signature trust list is empty or PID invalid".into());
    }
    let pid_key = CfOwned::string("pid")?;
    // SAFETY: kCFNumberSInt32Type=3 reads one i32. CFNumberCreate copies the value.
    let pid_value = CfOwned(unsafe { CFNumberCreate(ptr::null(), 3, (&pid as *const i32).cast()) });
    if pid_value.0.is_null() {
        return Err("PID number allocation failed".into());
    }
    let keys = [pid_key.0];
    let values = [pid_value.0];
    // SAFETY: arrays have one live object each; standard callbacks retain keys and values.
    let dictionary = CfOwned(unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            ptr::addr_of!(kCFTypeDictionaryKeyCallBacks).cast(),
            ptr::addr_of!(kCFTypeDictionaryValueCallBacks).cast(),
        )
    });
    if dictionary.0.is_null() {
        return Err("signature attributes allocation failed".into());
    }
    let mut code = ptr::null();
    // SAFETY: valid attributes dictionary; successful API returns one owned SecCode reference.
    let result = unsafe { SecCodeCopyGuestWithAttributes(ptr::null(), dictionary.0, 0, &mut code) };
    if result != 0 || code.is_null() {
        return Err(format!("SecCode guest lookup: {result}"));
    }
    let code = CfOwned(code);
    for requirement in requirements {
        let text = CfOwned::string(requirement)?;
        let mut parsed = ptr::null();
        // SAFETY: valid CFString, output receives retained requirement.
        let result = unsafe { SecRequirementCreateWithString(text.0, 0, &mut parsed) };
        if result != 0 || parsed.is_null() {
            continue;
        }
        let parsed = CfOwned(parsed);
        // SAFETY: valid SecCode and SecRequirement objects retained throughout call.
        if unsafe { SecCodeCheckValidity(code.0, 0, parsed.0) } == 0 {
            return Ok(());
        }
    }
    Err("caller code signature is not trusted".into())
}

pub fn register_services() -> Result<String, String> {
    service_management(true)
}
pub fn unregister_services() -> Result<String, String> {
    service_management(false)
}
pub fn unregister_user_service() -> Result<String, String> {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    objc2::rc::autoreleasepool(|_| {
        let agent = unsafe {
            SMAppService::agentServiceWithPlistName(&NSString::from_str("org.macflow.agent.plist"))
        };
        if unsafe { agent.status() } != SMAppServiceStatus::NotRegistered {
            unsafe { agent.unregisterAndReturnError() }
                .map_err(|e| format!("agent service: {e}"))?;
        }
        Ok(format!(
            "agent: {}",
            service_status_name(unsafe { agent.status() })
        ))
    })
}

fn service_management(register: bool) -> Result<String, String> {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: fixed plist basenames refer only to the calling app's bundled service plists.
        let agent = unsafe {
            SMAppService::agentServiceWithPlistName(&NSString::from_str("org.macflow.agent.plist"))
        };
        // SAFETY: same bundled-plist contract; no untrusted path is accepted.
        let helper = unsafe {
            SMAppService::daemonServiceWithPlistName(&NSString::from_str(
                "org.macflow.helper.plist",
            ))
        };
        let mut statuses = Vec::new();
        for (name, service) in [("agent", agent), ("helper", helper)] {
            // SAFETY: status is queried on the valid retained SMAppService.
            let before = unsafe { service.status() };
            if (register
                && (before == SMAppServiceStatus::Enabled
                    || before == SMAppServiceStatus::RequiresApproval))
                || (!register && before == SMAppServiceStatus::NotRegistered)
            {
                statuses.push(format!("{name}: {}", service_status_name(before)));
                continue;
            }
            // SAFETY: service is a valid retained SMAppService; errors are returned, never ignored.
            let result = unsafe {
                if register {
                    service.registerAndReturnError()
                } else {
                    service.unregisterAndReturnError()
                }
            };
            if let Err(error) = result {
                return Err(format!("{name} service: {error}"));
            }
            // SAFETY: registration did not invalidate the retained service object.
            statuses.push(format!(
                "{name}: {}",
                service_status_name(unsafe { service.status() })
            ));
        }
        Ok(statuses.join("; "))
    })
}

fn service_status_name(status: objc2_service_management::SMAppServiceStatus) -> &'static str {
    match status.0 {
        0 => "not registered",
        1 => "enabled",
        2 => "requires approval in System Settings",
        3 => "bundled service not found",
        _ => "unknown service state",
    }
}

#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebase) -> i32;
    fn mach_host_self() -> u32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    static mach_task_self_: u32;
    fn pthread_set_qos_class_self_np(qos: u32, relative_priority: i32) -> i32;
    fn setiopolicy_np(kind: i32, scope: i32, policy: i32) -> i32;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CfRef;
    fn IOServiceGetMatchingService(port: u32, matching: CfRef) -> u32;
    fn IORegistryEntryCreateCFProperties(
        entry: u32,
        properties: *mut CfRef,
        allocator: CfRef,
        options: u32,
    ) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: CfRef);
    fn CFGetTypeID(value: CfRef) -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringCreateWithBytes(
        allocator: CfRef,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: bool,
    ) -> CfRef;
    fn CFStringGetCString(value: CfRef, output: *mut c_char, length: isize, encoding: u32) -> bool;
    fn CFDictionaryGetValue(dictionary: CfRef, key: CfRef) -> CfRef;
    fn CFDictionaryCreate(
        allocator: CfRef,
        keys: *const CfRef,
        values: *const CfRef,
        count: isize,
        key_callbacks: CfRef,
        value_callbacks: CfRef,
    ) -> CfRef;
    fn CFNumberGetValue(value: CfRef, number_type: isize, output: *mut c_void) -> bool;
    fn CFNumberCreate(allocator: CfRef, number_type: isize, input: *const c_void) -> CfRef;
    fn CFDataCreate(allocator: CfRef, bytes: *const u8, length: isize) -> CfRef;
    fn CFPropertyListCreateWithData(
        allocator: CfRef,
        data: CfRef,
        options: usize,
        format: *mut isize,
        error: *mut CfRef,
    ) -> CfRef;
    static kCFTypeDictionaryKeyCallBacks: [usize; 6];
    static kCFTypeDictionaryValueCallBacks: [usize; 5];
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecCodeCopyGuestWithAttributes(
        host: CfRef,
        attributes: CfRef,
        flags: u32,
        code: *mut CfRef,
    ) -> i32;
    fn SecRequirementCreateWithString(value: CfRef, flags: u32, output: *mut CfRef) -> i32;
    fn SecCodeCheckValidity(code: CfRef, flags: u32, requirement: CfRef) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_revision_length_is_checked_before_reading_compressor() {
        // macOS 26.7.1 returns the SDK rev1 (38 integer words), while
        // newer libc also declares fields added after that revision.
        assert!(vm_compressor_prefix_present(38));
        assert!(vm_compressor_prefix_present(libc::HOST_VM_INFO64_COUNT));
        assert!(!vm_compressor_prefix_present(24));
        assert!(!vm_compressor_prefix_present(
            libc::HOST_VM_INFO64_COUNT + 1
        ));
        assert!(vm_statistics().is_ok());
    }

    #[test]
    fn identity_is_live_and_rejects_invalid_pid() {
        let me = identity(std::process::id() as i32).unwrap();
        assert_eq!(me.uid, current_uid());
        assert_eq!(me.boot_id, boot_id());
        assert!(!me.executable.is_empty());
        assert!(identity(0).is_err());
        assert!(identity(-1).is_err());
        assert_eq!(identity(me.pid).unwrap(), me);
    }

    #[test]
    fn monotonic_clock_and_cpu_delta() {
        assert!(now_ms() <= now_ms());
        assert_eq!(cpu_percent([0, 0, 0, 0], [20, 20, 60, 0]), Some(40.0));
        assert_eq!(cpu_percent([1; 4], [1; 4]), None);
        assert_eq!(cpu_percent([u32::MAX, 0, 0, 0], [0, 0, 1, 0]), Some(50.0));
    }

    #[test]
    fn priority_checks_do_not_modify_processes() {
        let mut me = identity(std::process::id() as i32).unwrap();
        assert!(nice(me.pid).is_ok());
        assert!(nice(0).is_err());
        assert!(set_nice(&me, 20).is_err());
        me.start_micros = me.start_micros.wrapping_add(1);
        assert!(set_nice(&me, 0).is_err());
    }

    #[test]
    fn critical_dependencies_are_protected() {
        assert!(protected_path_reason(0, "/tmp/calculator").is_some());
        assert!(
            protected_path_reason(
                501,
                "/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder"
            )
            .is_some()
        );
        assert!(
            protected_path_reason(501, "/Applications/zoom.us.app/Contents/MacOS/zoom.us")
                .is_some()
        );
        assert!(
            protected_path_reason(
                501,
                "/Applications/MacFlow.app/Contents/MacOS/macflow-agent"
            )
            .is_some()
        );
        assert!(protected_path_reason(501, "/tmp/background-compiler").is_none());
    }

    #[test]
    fn sampler_uses_real_metrics_and_unknown_gpu_capabilities() {
        let mut sampler = Sampler::new();
        let first = sampler.sample();
        assert!(first.cpu_percent.is_none());
        assert!(
            first
                .processes
                .iter()
                .any(|p| p.identity.pid == std::process::id() as i32)
        );
        assert!(first.gpu.utilization_percent.is_none());
        assert!(first.gpu.temperature_c.is_none());
        assert!(first.ac_power.is_none());
        assert!(first.swap_used_bytes.is_some());
        let second = sampler.sample();
        if let Some(cpu) = second.cpu_percent {
            assert!((0.0..=100.0).contains(&cpu));
        }
    }

    #[test]
    fn cf_dictionary_access_is_typed() {
        let string = CfOwned::string("not a dictionary").unwrap();
        assert!(dictionary_value(string.0, "key").is_none());
        assert!(number_u64(string.0).is_none());
        assert_eq!(cf_string(string.0).as_deref(), Some("not a dictionary"));
    }

    #[test]
    fn exact_signature_requirement_accepts_only_matching_child() {
        let sleep = code_requirement_for_file(Path::new("/bin/sleep")).unwrap();
        let other = code_requirement_for_file(Path::new("/bin/ls")).unwrap();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let matching = verify_code_signature(pid, &[sleep]);
        let different = verify_code_signature(pid, &[other]);
        let _ = child.kill();
        let _ = child.wait();
        assert!(matching.is_ok(), "{matching:?}");
        assert!(different.is_err());
        assert!(verify_code_signature(0, &[]).is_err());
    }
}
