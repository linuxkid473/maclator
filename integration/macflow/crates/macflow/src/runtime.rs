use crate::{agent::Shared, storage};
use macflow_protocol::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
pub fn start(state: Shared) -> Result<(), String> {
    let dir = storage::user_dir().join("run");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    let path = dir.join("runtime.sock");
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    let listener = UnixListener::bind(&path).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    let count = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            if count.fetch_add(1, Ordering::AcqRel) >= 128 {
                count.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let count = count.clone();
            let state = state.clone();
            std::thread::spawn(move || {
                let _ = session(stream, &state);
                count.fetch_sub(1, Ordering::AcqRel);
            });
        }
    });
    Ok(())
}
fn read_message(reader: &mut BufReader<UnixStream>) -> Result<RuntimeMessage, String> {
    let mut line = Vec::new();
    let n = reader
        .take((MAX_PAYLOAD + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|e| e.to_string())?;
    if n == 0 || n > MAX_PAYLOAD || line.last() != Some(&b'\n') {
        return Err("invalid runtime frame".into());
    }
    serde_json::from_slice(&line).map_err(|e| e.to_string())
}
fn peer(stream: &UnixStream) -> Result<(u32, i32), String> {
    use std::os::fd::AsRawFd;
    let mut uid = 0;
    let mut gid = 0;
    let mut pid = 0i32;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    // Peer credentials are kernel-supplied, not trusted client fields.
    let a = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    let b = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if a != 0 || b != 0 || len as usize != std::mem::size_of::<i32>() {
        Err("runtime peer credentials unavailable".into())
    } else {
        Ok((uid, pid))
    }
}
fn session(mut stream: UnixStream, state: &Shared) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let (uid, pid) = peer(&stream)?;
    if uid != macflow_platform::current_uid() {
        return Err("runtime peer UID mismatch".into());
    }
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let RuntimeMessage::Hello { hello } = read_message(&mut reader)? else {
        return Err("runtime handshake required".into());
    };
    let identity = macflow_platform::identity(pid)?;
    if hello.schema != SCHEMA
        || hello.pid != pid
        || hello.uid != uid
        || hello.start_secs != identity.start_secs
        || hello.start_micros != identity.start_micros
        || hello.source_app.len() > 4096
    {
        return Err("runtime identity mismatch".into());
    }
    let exe = std::path::Path::new(&identity.executable)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if exe != "maclator" {
        return Err("only registered Maclator runtimes are accepted".into());
    }
    if !["compute", "gui", "unknown"].contains(&hello.task_role.as_str()) {
        return Err("invalid runtime role".into());
    }
    state
        .lock()
        .map_err(|_| "state unavailable")?
        .runtime_roles
        .insert(pid, hello.task_role.clone());
    let mut generation = 0;
    let mut previous = String::new();
    let result = (|| -> Result<(), String> {
        loop {
            if macflow_platform::identity(pid)? != identity {
                return Err("runtime exited or identity changed".into());
            }
            let s = state.lock().map_err(|_| "state unavailable")?;
            let foreground = s.status.telemetry.foreground_pid == Some(pid)
                || s.status.telemetry.processes.iter().any(|p| {
                    p.identity.pid == pid
                        && p.parent_pid == s.status.telemetry.foreground_pid.unwrap_or(-1)
                });
            let observe = s.status.config.observe_only || s.status.config.paused;
            let spin = if foreground {
                s.decision.fg_spin_us
            } else {
                s.decision.bg_spin_us
            };
            let mut policy = RuntimePolicy {
                schema: SCHEMA,
                generation: 0,
                observe_only: observe,
                foreground,
                spin_caller_us: spin,
                spin_bridge_us: spin,
                admit_optional: s.decision.admit_optional,
                build_jobs: s.decision.build_jobs,
            };
            drop(s);
            // Send a heartbeat even when unchanged; runtimes expire policy after 10s.
            let encoded = serde_json::to_string(&policy).map_err(|e| e.to_string())?;
            if encoded != previous {
                generation += 1;
                previous = encoded;
            }
            policy.generation = generation;
            let mut bytes = serde_json::to_vec(&policy).map_err(|e| e.to_string())?;
            bytes.push(b'\n');
            stream.write_all(&bytes).map_err(|e| e.to_string())?;
            match read_message(&mut reader)? {
                RuntimeMessage::Stats { mut stats } => {
                    if stats.pid != pid {
                        return Err("runtime stats identity mismatch".into());
                    }
                    stats.source_app = hello.source_app.clone();
                    let mut s = state.lock().map_err(|_| "state unavailable")?;
                    s.status.runtimes.retain(|r| r.pid != pid);
                    s.status.runtimes.push(stats);
                }
                _ => return Err("duplicate handshake".into()),
            }
        }
    })();
    if let Ok(mut s) = state.lock() {
        s.status.runtimes.retain(|r| r.pid != pid);
        s.runtime_roles.remove(&pid);
    }
    result
}
