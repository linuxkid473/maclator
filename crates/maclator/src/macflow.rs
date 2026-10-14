//! Cooperative MacFlow adapter. Socket work only runs on its own host thread;
//! guest execution and GPU RPCs read a packed atomic snapshot without locks.
use macflow_protocol::{
    RuntimeHello, RuntimeMessage, RuntimePolicy, RuntimeStats, MAX_PAYLOAD, SCHEMA,
};
use std::cell::Cell;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const LEASE_MS: u64 = 10_000;
const ACTIVE: u64 = 1 << 32;
const FOREGROUND: u64 = 1 << 33;
const MAX_SPIN_US: u32 = 1_000;
static STARTED: AtomicBool = AtomicBool::new(false);
static EPOCH: OnceLock<Instant> = OnceLock::new();
static POLICY: AtomicU64 = AtomicU64::new(0);
static POLICY_AT: AtomicU64 = AtomicU64::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static SAMPLES: AtomicU64 = AtomicU64::new(0);
static BRIDGE_CALLS: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAIT_NS: AtomicU64 = AtomicU64::new(0);
static QOS_FAILURES: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Qos {
    pub class: u32,
    pub relative: i32,
}

impl Qos {
    pub fn packed(self) -> u64 {
        u64::from(self.class) | (u64::from(self.relative as u8) << 8)
    }

    pub fn from_packed(packed: u64) -> Option<Self> {
        let value = Self {
            class: (packed & 0xff) as u32,
            relative: ((packed >> 8) as u8 as i8) as i32,
        };
        (matches!(value.class, 9 | 17 | 21 | 25 | 33) && (-15..=0).contains(&value.relative))
            .then_some(value)
    }
}

thread_local! {
    static HOST_QOS: Cell<Option<Qos>> = const { Cell::new(None) };
}

/// XNU's six private thread-QoS ranks, mapped to the public host API. Public
/// maintenance QoS does not exist; maintenance therefore uses background.
pub fn qos_for_rank(rank: u32, relative: i32) -> Option<Qos> {
    let class = match rank {
        1 | 2 => 9,
        3 => 17,
        4 => 21,
        5 => 25,
        6 => 33,
        _ => return None,
    };
    (-15..=0)
        .contains(&relative)
        .then_some(Qos { class, relative })
}

/// priority_private.h encodes QoS as one bit in bits 8..13 and relpri-1 in
/// the signed low byte. Reject malformed masks instead of passing raw guest
/// priorities to the host kernel and exposing libdispatch to host EINVAL.
pub fn decode_priority(priority: u64) -> Option<Qos> {
    let bits = ((priority >> 8) & 0x3f) as u32;
    if bits.count_ones() != 1 {
        return None;
    }
    let rank = bits.trailing_zeros() + 1;
    let relative = (priority as u8 as i8) as i32 + 1;
    qos_for_rank(rank, relative)
}

/// Apply a public QoS value only to the calling host pthread. Failures are
/// diagnostic counters, not guest syscall failures; vouchers stay untouched.
pub fn set_self_qos(qos: Qos) {
    if Qos::from_packed(qos.packed()) != Some(qos) {
        QOS_FAILURES.fetch_add(1, Ordering::Relaxed);
        return;
    }
    HOST_QOS.with(|current| {
        if current.get() == Some(qos) {
            return;
        }
        let class = match qos.class {
            9 => libc::qos_class_t::QOS_CLASS_BACKGROUND,
            17 => libc::qos_class_t::QOS_CLASS_UTILITY,
            21 => libc::qos_class_t::QOS_CLASS_DEFAULT,
            25 => libc::qos_class_t::QOS_CLASS_USER_INITIATED,
            33 => libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE,
            _ => return,
        };
        let result = unsafe { libc::pthread_set_qos_class_self_np(class, qos.relative) };
        if result == 0 {
            current.set(Some(qos));
        } else {
            QOS_FAILURES.fetch_add(1, Ordering::Relaxed);
        }
    });
}

pub fn set_guest_priority(priority: u64) {
    if priority == 0 {
        // The guest explicitly clears requested QoS. The public setter cannot
        // accept UNSPECIFIED, so return to the ordinary default host class.
        set_self_qos(Qos {
            class: 21,
            relative: 0,
        });
    } else if let Some(qos) = decode_priority(priority) {
        set_self_qos(qos);
    } else {
        QOS_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn set_guest_start_flags(flags: u64) {
    const PTHREAD_START_QOSCLASS: u64 = 0x0800_0000;
    if flags & PTHREAD_START_QOSCLASS != 0 {
        set_guest_priority(flags & 0x00ff_ffff);
    }
}

pub fn set_workqueue_flags(flags: u64) {
    const WQ_FLAG_THREAD_PRIO_QOS: u64 = 0x4000;
    if flags & WQ_FLAG_THREAD_PRIO_QOS != 0 {
        if let Some(qos) = qos_for_rank((flags & 0x7) as u32, 0) {
            set_self_qos(qos);
        } else {
            QOS_FAILURES.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Requested QoS, sampled from the owning pthread; never from another guest
/// thread. An unspecified host thread is represented by public default QoS.
pub fn current_qos() -> Qos {
    HOST_QOS.with(|current| {
        if let Some(qos) = current.get() {
            return qos;
        }
        let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
        let mut relative = 0i32;
        let result = unsafe {
            libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative)
        };
        let qos = if result == 0 {
            Qos::from_packed(u64::from(class as u32) | (u64::from(relative as u8) << 8)).unwrap_or(
                Qos {
                    class: 21,
                    relative: 0,
                },
            )
        } else {
            QOS_FAILURES.fetch_add(1, Ordering::Relaxed);
            Qos {
                class: 21,
                relative: 0,
            }
        };
        current.set(Some(qos));
        qos
    })
}

fn elapsed_ms() -> u64 {
    EPOCH
        .get()
        .map(|start| start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn pack_policy(policy: &RuntimePolicy) -> Option<u64> {
    if policy.schema != SCHEMA
        || policy.spin_caller_us > MAX_SPIN_US
        || policy.spin_bridge_us > MAX_SPIN_US
        || policy.build_jobs == 0
        || policy.build_jobs > 64
    {
        return None;
    }
    let mut packed = u64::from(policy.spin_caller_us) | (u64::from(policy.spin_bridge_us) << 16);
    if policy.foreground {
        packed |= FOREGROUND;
    }
    if !policy.observe_only {
        packed |= ACTIVE;
    }
    Some(packed)
}

/// None means keep the previous standalone MACLATOR_GPU_SPIN setting. The
/// independent heartbeat thread and this read both enforce a ten-second lease.
pub fn spin_override() -> Option<(Duration, Duration)> {
    let packed = POLICY.load(Ordering::Acquire);
    let age = elapsed_ms().saturating_sub(POLICY_AT.load(Ordering::Acquire));
    spin_for_snapshot(packed, age)
}

fn spin_for_snapshot(packed: u64, age: u64) -> Option<(Duration, Duration)> {
    if packed & ACTIVE == 0 || age >= LEASE_MS {
        return None;
    }
    Some((
        Duration::from_micros(packed & 0xffff),
        Duration::from_micros((packed >> 16) & 0xffff),
    ))
}

pub fn record_bridge_wait(wait: Duration) {
    BRIDGE_CALLS.fetch_add(1, Ordering::Relaxed);
    BRIDGE_WAIT_NS.fetch_add(
        wait.as_nanos().min(u128::from(u64::MAX)) as u64,
        Ordering::Relaxed,
    );
}

fn own_identity() -> io::Result<(i32, u32, u64, u64)> {
    let pid = unsafe { libc::getpid() };
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if bytes != size as i32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete process identity",
        ));
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid as u32 || info.pbi_start_tvusec >= 1_000_000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid process identity",
        ));
    }
    Ok((
        pid,
        info.pbi_uid,
        info.pbi_start_tvsec,
        info.pbi_start_tvusec,
    ))
}

fn socket_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| Path::new(&home).join("Library/Application Support/MacFlow/run/runtime.sock"))
}

fn connect(path: &Path, uid: u32) -> io::Result<UnixStream> {
    let metadata = std::fs::symlink_metadata(path)?;
    let directory =
        std::fs::symlink_metadata(path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing socket directory")
        })?)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || !directory.is_dir()
        || directory.uid() != uid
        || directory.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "untrusted runtime socket",
        ));
    }
    let stream = UnixStream::connect(path)?;
    let mut peer_uid = 0;
    let mut peer_gid = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) };
    if result != 0 || peer_uid != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "wrong runtime socket peer",
        ));
    }
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    Ok(stream)
}

fn send(stream: &mut UnixStream, message: &RuntimeMessage) -> io::Result<()> {
    let mut data = serde_json::to_vec(message).map_err(io::Error::other)?;
    if data.len() >= MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime message too large",
        ));
    }
    data.push(b'\n');
    stream.write_all(&data)
}

fn session(mut stream: UnixStream, hello: &RuntimeHello) -> io::Result<()> {
    send(
        &mut stream,
        &RuntimeMessage::Hello {
            hello: hello.clone(),
        },
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut pending = Vec::with_capacity(512);
    let mut latest_generation = None;
    let mut stats_at = Instant::now();
    loop {
        let available = MAX_PAYLOAD.saturating_sub(pending.len());
        if available == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "policy exceeds message limit",
            ));
        }
        let result = reader
            .by_ref()
            .take(available as u64)
            .read_until(b'\n', &mut pending);
        match result {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "runtime service disconnected",
                ))
            }
            Ok(_) if pending.last() == Some(&b'\n') => {
                let policy: RuntimePolicy =
                    serde_json::from_slice(&pending).map_err(io::Error::other)?;
                let packed = pack_policy(&policy).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid runtime policy")
                })?;
                if latest_generation.is_some_and(|last| policy.generation < last) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "stale policy generation",
                    ));
                }
                latest_generation = Some(policy.generation);
                GENERATION.store(policy.generation, Ordering::Release);
                POLICY_AT.store(elapsed_ms(), Ordering::Release);
                POLICY.store(packed, Ordering::Release);
                SAMPLES.fetch_add(1, Ordering::Relaxed);
                pending.clear();
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        if elapsed_ms().saturating_sub(POLICY_AT.load(Ordering::Acquire)) >= LEASE_MS {
            POLICY.store(0, Ordering::Release);
        }
        if stats_at.elapsed() >= Duration::from_secs(1) {
            send(
                &mut stream,
                &RuntimeMessage::Stats {
                    stats: RuntimeStats {
                        pid: hello.pid,
                        source_app: hello.source_app.clone(),
                        samples: SAMPLES.load(Ordering::Relaxed),
                        bridge_calls: BRIDGE_CALLS.load(Ordering::Relaxed),
                        bridge_wait_ns: BRIDGE_WAIT_NS.load(Ordering::Relaxed),
                        qos_failures: QOS_FAILURES.load(Ordering::Relaxed),
                    },
                },
            )?;
            stats_at = Instant::now();
        }
    }
}

/// Launch before guest execution. Failures preserve standalone behavior and
/// retry at a bounded rate. All serialization and socket allocation happens here.
pub fn start(source_executable: &str) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(path) = socket_path() else { return };
    let Ok((pid, uid, start_secs, start_micros)) = own_identity() else {
        return;
    };
    EPOCH.get_or_init(Instant::now);
    let hello = RuntimeHello {
        schema: SCHEMA,
        pid,
        uid,
        start_secs,
        start_micros,
        source_app: std::env::var("MACLATOR_SOURCE_APP")
            .unwrap_or_else(|_| source_executable.into()),
        task_role: std::env::var("MACLATOR_TASK_ROLE").unwrap_or_else(|_| "unknown".into()),
    };
    let _ = std::thread::Builder::new()
        .name("maclator-macflow".into())
        .stack_size(512 * 1024)
        .spawn(move || {
            set_self_qos(Qos {
                class: 17,
                relative: 0,
            });
            loop {
                if let Ok(stream) = connect(&path, uid) {
                    let _ = session(stream, &hello);
                    // Immediate release on detected EOF/error is safer than
                    // waiting out the upper-bound ten-second fallback lease.
                    POLICY.store(0, Ordering::Release);
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RuntimePolicy {
        RuntimePolicy {
            schema: SCHEMA,
            generation: 1,
            observe_only: false,
            foreground: true,
            spin_caller_us: 5,
            spin_bridge_us: 5,
            admit_optional: true,
            build_jobs: 12,
        }
    }

    #[test]
    fn all_guest_qos_classes_and_relative_priorities_decode() {
        for (rank, class) in [(1, 9), (2, 9), (3, 17), (4, 21), (5, 25), (6, 33)] {
            for relative in [-15, -7, 0] {
                let priority = (1 << (rank + 7)) | u64::from((relative as i8 - 1) as u8);
                let expected = Qos { class, relative };
                assert_eq!(decode_priority(priority), Some(expected));
                assert_eq!(Qos::from_packed(expected.packed()), Some(expected));
            }
        }
        for priority in [0, 0x03ff, 0x4000, 0x80ff, 0x0800, 0x08ef] {
            assert!(decode_priority(priority).is_none(), "{priority:#x}");
        }
    }

    #[test]
    fn observe_policy_disables_override_and_validation_is_bounded() {
        let mut value = policy();
        assert_ne!(pack_policy(&value).unwrap() & ACTIVE, 0);
        value.observe_only = true;
        assert_eq!(pack_policy(&value).unwrap() & ACTIVE, 0);
        value.spin_caller_us = MAX_SPIN_US + 1;
        assert!(pack_policy(&value).is_none());
        value = policy();
        value.schema += 1;
        assert!(pack_policy(&value).is_none());
        value = policy();
        value.build_jobs = 0;
        assert!(pack_policy(&value).is_none());
    }

    #[test]
    fn expired_or_observation_policy_preserves_standalone_spin() {
        let value = policy();
        let packed = pack_policy(&value).unwrap();
        assert_eq!(
            spin_for_snapshot(packed, LEASE_MS - 1),
            Some((Duration::from_micros(5), Duration::from_micros(5)))
        );
        assert!(spin_for_snapshot(packed, LEASE_MS).is_none());
        assert!(spin_for_snapshot(packed, u64::MAX).is_none());
        assert!(spin_for_snapshot(0, 0).is_none());
        let value = RuntimePolicy {
            observe_only: true,
            ..value
        };
        assert!(spin_for_snapshot(pack_policy(&value).unwrap(), 0).is_none());
    }

    #[test]
    fn qos_is_applied_on_native_owner_thread() {
        std::thread::spawn(|| {
            for class in [9, 17, 21, 25, 33] {
                let expected = Qos {
                    class,
                    relative: -2,
                };
                set_self_qos(expected);
                let mut actual = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
                let mut relative = 0;
                assert_eq!(
                    unsafe {
                        libc::pthread_get_qos_class_np(
                            libc::pthread_self(),
                            &mut actual,
                            &mut relative,
                        )
                    },
                    0
                );
                assert_eq!(actual as u32, class);
                assert_eq!(relative, -2);
                assert_eq!(current_qos(), expected);
            }
        })
        .join()
        .unwrap();
    }
}
