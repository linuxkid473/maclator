use crate::{
    ipc::{self, Peer},
    storage,
};
use macflow_protocol::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Serialize, Deserialize)]
struct Lease {
    identity: ProcessIdentity,
    original: i32,
    last: i32,
    deadline_ms: u64,
    controller: ProcessIdentity,
}
#[derive(Default, Serialize, Deserialize)]
struct Journal {
    boot_id: String,
    leases: BTreeMap<String, Lease>,
}
#[derive(Serialize, Deserialize)]
pub struct Trust {
    pub schema: u32,
    pub uid: u32,
    pub requirements: Vec<String>,
}
struct Helper {
    journal: Journal,
    path: PathBuf,
    uid: u32,
}
impl Helper {
    fn persist(&self) -> Result<(), String> {
        storage::atomic_json(&self.path, &self.journal)
    }
    fn revoke(&mut self, key: &str) -> Result<(), String> {
        let Some(lease) = self.journal.leases.get(key).cloned() else {
            return Ok(());
        };
        // Restore only our unchanged write, never an unrelated application update.
        match macflow_platform::identity(lease.identity.pid) {
            Ok(current) if current == lease.identity => {
                if macflow_platform::nice(current.pid)? == lease.last {
                    macflow_platform::set_nice(&current, lease.original)?;
                }
            }
            Ok(_) => {}
            Err(e) => {
                if !definitely_exited(lease.identity.pid) {
                    return Err(format!("retaining recovery record: {e}"));
                }
            }
        }
        self.journal.leases.remove(key);
        self.persist()
    }
    fn expire(&mut self, all: bool) {
        let now = macflow_platform::now_ms();
        let keys: Vec<_> = self
            .journal
            .leases
            .iter()
            .filter(|(_, l)| all || l.deadline_ms <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            if let Err(e) = self.revoke(&key) {
                eprintln!("restore {key}: {e}");
            }
        }
    }
    fn command(&mut self, request: Request, peer: Peer) -> Response {
        let id = request.id;
        let result = (|| -> Result<serde_json::Value, String> {
            if request.schema != SCHEMA || peer.uid != self.uid {
                return Err("request schema or owner mismatch".into());
            }
            // A privileged update holds the exclusive lifecycle lock. New
            // controls and renewals must then fail closed; restoration remains
            // available. Keep the shared lock through persistence and syscall.
            let _lifecycle = if matches!(
                &request.action,
                Command::HelperApply { .. } | Command::HelperRenew { .. }
            ) {
                Some(control_lifecycle_guard()?)
            } else {
                None
            };
            let controller = macflow_platform::identity(peer.pid)?;
            if controller.uid != peer.uid {
                return Err("controller identity mismatch".into());
            }
            match request.action {
                Command::Status => Ok(
                    serde_json::json!({"leases":self.journal.leases.len(),"boot_id":self.journal.boot_id}),
                ),
                Command::HelperQuery { identity } => {
                    self.check_target(&identity)?;
                    Ok(serde_json::json!({"nice":macflow_platform::nice(identity.pid)?}))
                }
                Command::HelperApply {
                    identity,
                    nice,
                    lease_id,
                    deadline_ms,
                } => {
                    self.check_target(&identity)?;
                    self.check_deadline(deadline_ms)?;
                    if lease_id.len() > 128 || lease_id.is_empty() {
                        return Err("invalid lease identifier".into());
                    }
                    if let Some(lease) = self.journal.leases.get(&lease_id) {
                        if lease.identity != identity || lease.controller != controller {
                            return Err("lease identity conflict".into());
                        }
                        if macflow_platform::nice(identity.pid)? != lease.last {
                            return Err("priority changed externally".into());
                        }
                        if nice != lease.last {
                            return Err("revoke lease before changing target".into());
                        }
                        self.journal
                            .leases
                            .get_mut(&lease_id)
                            .ok_or("lease disappeared")?
                            .deadline_ms = deadline_ms;
                        self.persist()?;
                        return Ok(serde_json::json!({"renewed":true}));
                    }
                    let original = macflow_platform::nice(identity.pid)?;
                    if nice < original || nice > 10 || nice > original.saturating_add(5) {
                        return Err("priority request outside allowed range".into());
                    }
                    if self.journal.leases.values().any(|l| l.identity == identity) {
                        return Err("target already leased".into());
                    }
                    self.journal.leases.insert(
                        lease_id.clone(),
                        Lease {
                            identity: identity.clone(),
                            original,
                            last: nice,
                            deadline_ms,
                            controller,
                        },
                    );
                    self.persist()?;
                    if let Err(e) = macflow_platform::set_nice(&identity, nice) {
                        let _ = self.revoke(&lease_id);
                        return Err(e);
                    }
                    Ok(serde_json::json!({"original":original,"applied":nice}))
                }
                Command::HelperRenew {
                    lease_id,
                    deadline_ms,
                } => {
                    self.check_deadline(deadline_ms)?;
                    let lease = self
                        .journal
                        .leases
                        .get_mut(&lease_id)
                        .ok_or("unknown lease")?;
                    if lease.controller != controller {
                        return Err("lease controller changed".into());
                    }
                    if macflow_platform::identity(lease.identity.pid)? != lease.identity
                        || macflow_platform::nice(lease.identity.pid)? != lease.last
                    {
                        return Err("lease identity or priority changed".into());
                    }
                    lease.deadline_ms = deadline_ms;
                    self.persist()?;
                    Ok(serde_json::json!({"renewed":true}))
                }
                Command::HelperRevoke { lease_id } => {
                    if let Some(key) = lease_id {
                        if self
                            .journal
                            .leases
                            .get(&key)
                            .is_some_and(|l| l.controller != controller)
                        {
                            return Err("lease controller changed".into());
                        }
                        self.revoke(&key)?;
                    } else {
                        self.expire(true);
                        if !self.journal.leases.is_empty() {
                            return Err("some leases could not be restored".into());
                        }
                    }
                    Ok(serde_json::json!({"remaining":self.journal.leases.len()}))
                }
                _ => Err("command is not supported by privileged helper".into()),
            }
        })();
        match result {
            Ok(value) => Response::success(id, value),
            Err(e) => Response::failure(id, e),
        }
    }
    fn check_deadline(&self, deadline: u64) -> Result<(), String> {
        let now = macflow_platform::now_ms();
        if deadline <= now || deadline > now + 10_000 {
            Err("lease deadline must be within 10 seconds".into())
        } else {
            Ok(())
        }
    }
    fn check_target(&self, target: &ProcessIdentity) -> Result<(), String> {
        if target.uid != self.uid || target.uid == 0 || target.pid <= 1 {
            return Err("target belongs to protected user".into());
        }
        if let Some(reason) = macflow_platform::protected_reason(target) {
            return Err(format!("protected process: {reason}"));
        }
        if target.executable.starts_with("/System/")
            || target.executable.starts_with("/usr/libexec/")
            || target.executable.contains("MacFlow.app/")
        {
            return Err("protected process".into());
        }
        let name = Path::new(&target.executable)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if [
            "WindowServer",
            "loginwindow",
            "Finder",
            "Dock",
            "SystemUIServer",
            "coreaudiod",
            "kernel_task",
            "launchd",
            "macflow-agent",
            "macflow-helper",
            "MacFlow",
        ]
        .contains(&name)
        {
            return Err("protected process".into());
        }
        if macflow_platform::identity(target.pid)? != *target {
            return Err("process identity changed".into());
        }
        Ok(())
    }
}
fn definitely_exited(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == -1 && *libc::__error() == libc::ESRCH }
}
fn control_lifecycle_guard() -> Result<std::fs::File, String> {
    use std::os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let path = std::path::Path::new("/Library/Application Support/MacFlow/lifecycle.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err("insecure lifecycle lock".into());
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        return Err("maintenance in progress; new controls are disabled".into());
    }
    Ok(file)
}
pub fn run() -> Result<(), String> {
    if macflow_platform::current_uid() != 0 {
        return Err("privileged helper must run as root".into());
    }
    let root = PathBuf::from("/Library/Application Support/MacFlow");
    use std::os::unix::fs::MetadataExt;
    for path in [
        std::path::Path::new("/Library"),
        std::path::Path::new("/Library/Application Support"),
        root.as_path(),
    ] {
        let m = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err("insecure root support directory".into());
        }
    }
    if macflow_platform::boot_id() == "unknown-boot" {
        return Err("boot identity unavailable; recovery journal preserved".into());
    }
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("helper.lock"))
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another privileged helper is running".into());
    }
    let trust: Trust = storage::read_json(&root.join("trust.json"))?;
    let m = std::fs::symlink_metadata(root.join("trust.json")).map_err(|e| e.to_string())?;
    if m.uid() != 0
        || m.mode() & 0o022 != 0
        || trust.schema != SCHEMA
        || trust.uid == 0
        || trust.requirements.is_empty()
    {
        return Err("insecure or invalid helper trust configuration".into());
    }
    let path = root.join("leases.json");
    let mut journal: Journal = if path.exists() {
        storage::read_json(&path)?
    } else {
        Journal::default()
    };
    if journal.boot_id != macflow_platform::boot_id() {
        journal = Journal {
            boot_id: macflow_platform::boot_id(),
            leases: BTreeMap::new(),
        };
    }
    let state = Arc::new(Mutex::new(Helper {
        journal,
        path,
        uid: trust.uid,
    }));
    state
        .lock()
        .map_err(|_| "helper state poisoned")?
        .expire(true);
    let expiry = state.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(200));
            match expiry.lock() {
                Ok(mut h) => h.expire(false),
                Err(_) => std::process::exit(70),
            }
        }
    });
    ipc::serve(
        HELPER_SERVICE,
        Arc::new(move |r, p| match state.lock() {
            Ok(mut h) => h.command(r, p),
            Err(_) => Response::failure(r.id, "helper state unavailable"),
        }),
        trust.requirements,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn instance() -> Helper {
        Helper {
            journal: Journal::default(),
            path: std::env::temp_dir().join("macflow-unused-journal-test"),
            uid: macflow_platform::current_uid(),
        }
    }
    #[test]
    fn deadline_is_bounded_and_target_start_identity_is_required() {
        let h = instance();
        let now = macflow_platform::now_ms();
        assert!(h.check_deadline(now.saturating_sub(1)).is_err());
        assert!(h.check_deadline(now + 20_000).is_err());
        assert!(h.check_deadline(now + 1_000).is_ok());
        let mut identity = macflow_platform::identity(std::process::id() as i32).unwrap();
        identity.start_micros = identity.start_micros.wrapping_add(1);
        assert!(h.check_target(&identity).is_err());
        identity.uid = 0;
        assert!(h.check_target(&identity).is_err());
    }
    #[test]
    fn wrong_schema_or_owner_cannot_mutate_journal() {
        let mut h = instance();
        let response = h.command(
            Request {
                schema: SCHEMA + 1,
                id: 7,
                action: Command::Status,
            },
            Peer {
                pid: std::process::id() as i32,
                uid: h.uid,
            },
        );
        assert!(!response.ok);
        assert_eq!(response.id, 7);
        assert!(h.journal.leases.is_empty());
        let response = h.command(
            Request {
                schema: SCHEMA,
                id: 8,
                action: Command::Status,
            },
            Peer {
                pid: std::process::id() as i32,
                uid: h.uid + 1,
            },
        );
        assert!(!response.ok);
        assert!(h.journal.leases.is_empty());
    }
}
