use crate::{
    ipc::{self, Peer},
    storage,
};
use macflow_core::{Engine, failure_rule_selector, selector_matches_process};
use macflow_protocol::*;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
pub type Shared = Arc<Mutex<State>>;

const ACTION_FAILURE_LIMIT: u32 = 3;
const AMBIGUOUS_ACTION_BACKOFF_MS: u64 = 10_000;

#[derive(Default)]
struct ActionFailures {
    consecutive: BTreeMap<String, u32>,
    retry_after: BTreeMap<ProcessIdentity, u64>,
    previous_disabled: BTreeSet<String>,
}
impl ActionFailures {
    fn synchronise(&mut self, disabled: &[String], now: u64) {
        let current: BTreeSet<String> = disabled.iter().cloned().collect();
        for selector in self.previous_disabled.difference(&current) {
            self.consecutive.remove(selector);
        }
        self.previous_disabled = current;
        self.retry_after.retain(|_, deadline| *deadline > now);
    }

    fn allows(&self, identity: &ProcessIdentity, now: u64) -> bool {
        self.retry_after
            .get(identity)
            .is_none_or(|deadline| now >= *deadline)
    }

    fn allows_rule(&self, selector: &str) -> bool {
        self.consecutive
            .get(selector)
            .is_none_or(|count| *count < ACTION_FAILURE_LIMIT)
    }

    fn succeeded(&mut self, selector: &str, identity: &ProcessIdentity) {
        self.consecutive.remove(selector);
        self.retry_after.remove(identity);
    }

    fn failed(&mut self, selector: &str, identity: &ProcessIdentity, now: u64) -> bool {
        // The failed call may have reached the helper. Wait out the maximum
        // lease before any new Apply/Renew attempt; restoration remains safe.
        self.retry_after.insert(
            identity.clone(),
            now.saturating_add(AMBIGUOUS_ACTION_BACKOFF_MS),
        );
        let count = self.consecutive.entry(selector.into()).or_default();
        *count = count.saturating_add(1);
        *count >= ACTION_FAILURE_LIMIT
    }
}

fn failure_selector(identity: &ProcessIdentity, sample: &Telemetry, config: &Config) -> String {
    sample
        .processes
        .iter()
        .find(|p| p.identity == *identity)
        .map(|p| failure_rule_selector(p, config))
        .unwrap_or_else(|| format!("path:{}", identity.executable))
}

fn trim_diagnostic(value: &mut String, maximum: usize) {
    if value.len() > maximum {
        let mut end = maximum;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        value.push('…');
    }
}

fn record_telemetry(ring: &mut VecDeque<Telemetry>, sample: &Telemetry) {
    let mut recorded = sample.clone();
    recorded.processes.sort_by(|a, b| {
        let foreground_a = Some(a.identity.pid) == sample.foreground_pid;
        let foreground_b = Some(b.identity.pid) == sample.foreground_pid;
        foreground_b
            .cmp(&foreground_a)
            .then_with(|| b.cpu_percent.total_cmp(&a.cpu_percent))
    });
    recorded.processes.truncate(32);
    for process in &mut recorded.processes {
        trim_diagnostic(&mut process.identity.executable, 512);
        trim_diagnostic(&mut process.name, 128);
        if let Some(bundle) = &mut process.bundle_id {
            trim_diagnostic(bundle, 256);
        }
        if let Some(reason) = &mut process.protected_reason {
            trim_diagnostic(reason, 256);
        }
    }
    recorded.errors.truncate(8);
    for error in &mut recorded.errors {
        trim_diagnostic(error, 512);
    }
    if let Some(error) = &mut recorded.gpu.error {
        trim_diagnostic(error, 512);
    }
    if let Some(thermal) = &mut recorded.thermal {
        trim_diagnostic(thermal, 64);
    }
    ring.push_back(recorded);
    while ring.len() > 300
        || ring
            .front()
            .is_some_and(|old| sample.at_ms.saturating_sub(old.at_ms) > 300_000)
    {
        ring.pop_front();
    }
}

fn disable_failed_rule(state: &Shared, selector: &str, error: &str) {
    let config = match state.lock() {
        Ok(mut s) => {
            if !s
                .status
                .config
                .disabled_rules
                .iter()
                .any(|key| key == selector)
            {
                s.status.config.disabled_rules.push(selector.into());
            }
            s.event(
                "rule_disabled",
                format!("{selector}: disabled after three consecutive action failures ({error})"),
            );
            s.status.config.clone()
        }
        Err(_) => std::process::exit(70),
    };
    // State is fail-open before disk I/O; the lock is never held during fsync.
    if let Err(error) = storage::save_config(&config) {
        if let Ok(mut s) = state.lock() {
            s.status.config.observe_only = true;
            s.event(
                "configuration_error",
                format!(
                    "Failure quarantine could not be persisted; observation mode enabled: {error}"
                ),
            );
        }
    }
}
pub struct State {
    pub status: Status,
    pub decision: Decision,
    pub events: VecDeque<Event>,
    pub runtime_roles: BTreeMap<i32, String>,
    pub telemetry_samples: VecDeque<Telemetry>,
}
impl State {
    pub fn event(&mut self, kind: &str, message: impl Into<String>) {
        self.events.push_back(Event {
            at_ms: macflow_platform::now_ms(),
            kind: kind.into(),
            message: message.into(),
        });
        while self.events.len() > 300 {
            self.events.pop_front();
        }
    }
    pub fn snapshot(&self) -> Status {
        let mut s = self.status.clone();
        s.events = self.events.iter().rev().take(30).cloned().collect();
        s.telemetry
            .processes
            .sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent));
        s.telemetry.processes.truncate(48);
        s
    }
}
fn helper_requirement() -> Option<String> {
    let root_helper =
        std::path::Path::new("/Library/Application Support/MacFlow/Runtime/macflow-helper");
    let selected = if root_helper.exists() {
        root_helper
    } else {
        std::path::Path::new("/Applications/MacFlow.app/Contents/MacOS/macflow-helper")
    };
    macflow_platform::code_requirement_for_file(selected).ok()
}
fn helper_request(action: Command, requirement: &str) -> Result<serde_json::Value, String> {
    let request = Request {
        schema: SCHEMA,
        id: macflow_platform::now_ms(),
        action,
    };
    let response = ipc::call(
        HELPER_SERVICE,
        true,
        &request,
        Some(requirement),
        Duration::from_millis(800),
    )?;
    if response.ok {
        Ok(response.data)
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "helper operation rejected".into()))
    }
}
fn handle(request: Request, peer: Peer, state: &Shared) -> Response {
    let id = request.id;
    let result = (|| -> Result<serde_json::Value, String> {
        if request.schema != SCHEMA || peer.uid != macflow_platform::current_uid() {
            return Err("request version or user mismatch".into());
        }
        match request.action {
            Command::Status => {
                serde_json::to_value(state.lock().map_err(|_| "state unavailable")?.snapshot())
                    .map_err(|e| e.to_string())
            }
            Command::Export => {
                let (snapshot, samples) = {
                    let s = state.lock().map_err(|_| "state unavailable")?;
                    (s.snapshot(), s.telemetry_samples.clone())
                };
                let dir = storage::user_dir().join("Exports");
                let path = dir.join(format!("MacFlow-{}.json", macflow_platform::now_ms()));
                storage::atomic_json(
                    &path,
                    &serde_json::json!({
                        "schema": SCHEMA, "status": snapshot, "telemetry_samples": samples
                    }),
                )?;
                storage::prune_exports()?;
                Ok(serde_json::json!({"path":path}))
            }
            Command::SetMode { .. }
            | Command::SetObservation { .. }
            | Command::Pause
            | Command::Resume
            | Command::SetRule { .. } => {
                let mut config = state
                    .lock()
                    .map_err(|_| "state unavailable")?
                    .status
                    .config
                    .clone();
                match request.action {
                    Command::SetMode { mode } => config.mode = mode,
                    Command::SetObservation { enabled } => config.observe_only = enabled,
                    Command::Pause => config.paused = true,
                    Command::Resume => config.paused = false,
                    Command::SetRule { selector, policy } => {
                        if selector.is_empty()
                            || selector.len() > 1024
                            || selector.chars().any(char::is_control)
                        {
                            return Err("invalid application selector".into());
                        }
                        let selector = selector.trim().to_owned();
                        let processes = state
                            .lock()
                            .map_err(|_| "state unavailable")?
                            .status
                            .telemetry
                            .processes
                            .clone();
                        let selected: Vec<_> = processes
                            .iter()
                            .filter(|process| selector_matches_process(&selector, process))
                            .collect();
                        config.disabled_rules.retain(|disabled| {
                            disabled != &selector
                                && !selected
                                    .iter()
                                    .any(|process| selector_matches_process(disabled, process))
                                && !(selector.starts_with('/')
                                    && disabled == &format!("path:{selector}"))
                        });
                        config.rules.retain(|r| r.selector != selector);
                        config.rules.push(AppRule { selector, policy });
                    }
                    _ => {}
                }
                storage::save_config(&config)?;
                let mut s = state.lock().map_err(|_| "state unavailable")?;
                s.status.config = config.clone();
                s.event(
                    "configuration",
                    "Configuration updated; leases are reconciled on the next control tick",
                );
                Ok(serde_json::json!(config))
            }
            _ => Err("privileged operation unavailable on user service".into()),
        }
    })();
    match result {
        Ok(v) => Response::success(id, v),
        Err(e) => Response::failure(id, e),
    }
}
pub fn run() -> Result<(), String> {
    let dir = storage::user_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("agent.lock"))
        .map_err(|e| e.to_string())?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another MacFlow controller is running".into());
    }
    let mut config = storage::load_config().unwrap_or_default();
    let signature = serde_json::json!({"boot_compatible_os":std::process::Command::new("/usr/bin/sw_vers").arg("-buildVersion").output().ok().map(|o|String::from_utf8_lossy(&o.stdout).trim().to_owned()),"runtime_requirement":macflow_platform::code_requirement_for_file(&std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default().join("bin/maclator")).ok(),"driver_version":std::fs::read_to_string("/Library/NullMoth/driver-version").ok().map(|s|s.trim().to_owned())});
    let fingerprint = dir.join("compatibility.json");
    if storage::read_json::<serde_json::Value>(&fingerprint)
        .ok()
        .as_ref()
        != Some(&signature)
    {
        config.observe_only = true;
        storage::save_config(&config)?;
        storage::atomic_json(&fingerprint, &signature)?;
    }
    let state = Arc::new(Mutex::new(State {
        status: Status {
            version: env!("CARGO_PKG_VERSION").into(),
            config,
            pressure: false,
            build_jobs: 12,
            admit_optional: true,
            telemetry: Telemetry::default(),
            helper_connected: false,
            active_leases: 0,
            events: vec![],
            runtimes: vec![],
            capabilities: vec![
                Capability {
                    name: "External CPU nice".into(),
                    available: false,
                    detail: "Requires authenticated administrator helper; default observation mode"
                        .into(),
                },
                Capability {
                    name: "GPU utilization / temperature".into(),
                    available: false,
                    detail: "Unknown: driver metric semantics have not been validated".into(),
                },
                Capability {
                    name: "Maclator coordination".into(),
                    available: true,
                    detail: "Versioned local socket, per-thread QoS, GPU bridge atomics".into(),
                },
                Capability {
                    name: "Disk policy".into(),
                    available: true,
                    detail: "Cooperative tasks only; no arbitrary external PID control".into(),
                },
            ],
            pending_verification: vec![
                "Nine-round foreground/background comparison".into(),
                "p95/p99 interaction and verified presentation timing".into(),
                "24-hour stress run".into(),
                "Seven-day daily-use verification".into(),
                "95% native translation performance (separate goal)".into(),
            ],
        },
        decision: Decision::default(),
        events: VecDeque::new(),
        runtime_roles: BTreeMap::new(),
        telemetry_samples: VecDeque::with_capacity(300),
    }));
    crate::runtime::start(state.clone())?;
    // NSWorkspace maintains foreground/application notifications on its
    // initialization run loop. Initialize it on the service's main thread;
    // the XPC listener pumps that run loop while policy work stays off-main.
    let _ = macflow_platform::foreground_pid();
    let control = state.clone();
    std::thread::spawn(move || {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| control_loop(control))).is_err()
        {
            // The helper independently restores leases; launchd must restart a
            // controller whose policy thread has failed instead of leaving a
            // responsive XPC shell around a stopped policy loop.
            eprintln!("policy loop panicked; exiting for recovery");
            std::process::exit(70);
        }
    });
    let req =
        macflow_platform::code_requirement_for_file(&std::env::current_exe().unwrap_or_default())
            .ok();
    // User service accepts only installed GUI and CLI signatures when bundled.
    let mut trusted = Vec::new();
    if let Some(parent) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    {
        for name in ["MacFlowUI", "macflow"] {
            if let Ok(r) = macflow_platform::code_requirement_for_file(&parent.join(name)) {
                trusted.push(r);
            }
        }
    }
    if trusted.is_empty() {
        if let Some(r) = req {
            trusted.push(r);
        }
    }
    ipc::serve(
        AGENT_SERVICE,
        Arc::new(move |r, p| handle(r, p, &state)),
        trusted,
    )
}
fn control_loop(state: Shared) {
    let (history_tx, history_rx) = std::sync::mpsc::sync_channel::<(Status, Vec<Telemetry>)>(2);
    let history_state = state.clone();
    std::thread::spawn(move || {
        for (snapshot, samples) in history_rx {
            if let Err(error) = storage::history_record(&snapshot, &samples) {
                if let Ok(mut s) = history_state.lock() {
                    s.event("history_error", error);
                }
            }
        }
    });
    let mut sampler = macflow_platform::Sampler::new();
    let mut engine = Engine::new();
    let mut leases: BTreeMap<ProcessIdentity, (String, i32)> = BTreeMap::new();
    let mut last_renew = 0;
    let mut last_history = 0;
    let mut last_sample = 0;
    let mut sample = Telemetry::default();
    let requirement = helper_requirement();
    let mut driver_version: Option<String> = None;
    let mut recovery_count: Option<u64> = None;
    let mut gpu_fault: Option<String> = None;
    let mut failures = ActionFailures::default();
    loop {
        // The platform clock uses mach_continuous_time, shared with helper
        // leases; wall-clock corrections cannot stall retries or restoration.
        let tick = macflow_platform::now_ms();
        let new_sample = tick.saturating_sub(last_sample) >= 1000;
        if new_sample {
            sample = sampler.sample();
            last_sample = tick;
            if driver_version.is_some() && sample.gpu.driver_version != driver_version {
                if let Ok(mut s) = state.lock() {
                    s.status.config.observe_only = true;
                    s.event(
                        "compatibility",
                        "Driver version changed; observation mode restored",
                    );
                    let _ = storage::save_config(&s.status.config);
                }
            }
            driver_version = sample.gpu.driver_version.clone();
            if let (Some(before), Some(now)) = (recovery_count, sample.gpu.recovery_count) {
                if now > before {
                    gpu_fault = Some(format!(
                        "GPU recovery count increased {before} -> {now}; GPU optimization disabled"
                    ));
                    if let Ok(mut s) = state.lock() {
                        s.event("gpu_fault", gpu_fault.clone().unwrap_or_default());
                    }
                }
            }
            recovery_count = sample.gpu.recovery_count;
            if let Some(ref fault) = gpu_fault {
                sample.gpu.error = Some(fault.clone());
            }
            if let Ok(s) = state.lock() {
                for p in &mut sample.processes {
                    if p.role == TaskRole::Unknown
                        && s.runtime_roles
                            .get(&p.identity.pid)
                            .is_some_and(|r| r == "compute")
                    {
                        p.role = TaskRole::Compute;
                    }
                }
            }
        }
        let foreground = macflow_platform::foreground_pid();
        let fg_changed = foreground != sample.foreground_pid;
        sample.foreground_pid = foreground;
        let config = match state.lock() {
            Ok(s) => s.status.config.clone(),
            Err(_) => std::process::exit(70),
        };
        let decision = if new_sample {
            engine.evaluate(&sample, &config)
        } else {
            match state.lock() {
                Ok(s) => s.decision.clone(),
                Err(_) => std::process::exit(70),
            }
        };
        failures.synchronise(&config.disabled_rules, tick);
        let mut wanted: BTreeMap<ProcessIdentity, NiceTarget> = decision
            .nice_targets
            .iter()
            .take(8)
            .map(|t| (t.identity.clone(), t.clone()))
            .collect();
        if config.paused || config.observe_only {
            wanted.clear();
        }
        wanted.retain(|identity, _| {
            let selector = failure_selector(identity, &sample, &config);
            failures.allows(identity, tick)
                && failures.allows_rule(&selector)
                && !config.disabled_rules.iter().any(|disabled| {
                    sample
                        .processes
                        .iter()
                        .any(|p| p.identity == *identity && selector_matches_process(disabled, p))
                })
        });
        {
            let mut protected = std::collections::BTreeSet::new();
            if let Some(p) = foreground {
                protected.insert(p);
            }
            // Protect foreground descendants first; then ancestors without their siblings.
            for _ in 0..sample.processes.len().min(32) {
                let before = protected.len();
                for p in &sample.processes {
                    if protected.contains(&p.parent_pid) {
                        protected.insert(p.identity.pid);
                    }
                }
                if before == protected.len() {
                    break;
                }
            }
            let mut ancestor = foreground;
            for _ in 0..32 {
                let Some(pid) = ancestor else {
                    break;
                };
                ancestor = sample
                    .processes
                    .iter()
                    .find(|p| p.identity.pid == pid)
                    .map(|p| p.parent_pid)
                    .filter(|p| *p > 1);
                if let Some(p) = ancestor {
                    if !protected.insert(p) {
                        break;
                    }
                }
            }
            wanted.retain(|id, _| !protected.contains(&id.pid));
        }
        let mut connected = false;
        if let Some(ref req) = requirement {
            if tick.saturating_sub(last_renew) >= 2000 || fg_changed {
                match helper_request(Command::Status, req) {
                    Ok(_) => connected = true,
                    Err(_) => {}
                }
                if connected {
                    let remove: Vec<_> = leases
                        .iter()
                        .filter(|(id, (_, nice))| wanted.get(*id).is_none_or(|t| t.nice != *nice))
                        .map(|(id, v)| (id.clone(), v.0.clone()))
                        .collect();
                    for (id, key) in remove {
                        if helper_request(
                            Command::HelperRevoke {
                                lease_id: Some(key),
                            },
                            req,
                        )
                        .is_ok()
                        {
                            leases.remove(&id);
                        }
                    }
                    for (id, target) in &wanted {
                        let selector = failure_selector(id, &sample, &config);
                        if !failures.allows_rule(&selector) {
                            continue;
                        }
                        let lease_key = format!("{}-{}-{}", id.pid, id.start_secs, id.start_micros);
                        let action = if let Some((key, _)) = leases.get(id) {
                            Command::HelperRenew {
                                lease_id: key.clone(),
                                deadline_ms: macflow_platform::now_ms() + 9500,
                            }
                        } else {
                            Command::HelperApply {
                                identity: id.clone(),
                                nice: target.nice,
                                lease_id: lease_key.clone(),
                                deadline_ms: macflow_platform::now_ms() + 9500,
                            }
                        };
                        match helper_request(action, req) {
                            Ok(_) => {
                                failures.succeeded(&selector, id);
                                leases.insert(id.clone(), (lease_key, target.nice));
                            }
                            Err(e) => {
                                if let Ok(mut s) = state.lock() {
                                    s.event("helper_rejected", format!("{}: {e}", id.pid));
                                }
                                leases.remove(id);
                                if failures.failed(&selector, id, tick) {
                                    disable_failed_rule(&state, &selector, &e);
                                }
                            }
                        }
                    }
                } else {
                    leases.clear();
                }
                last_renew = tick;
            } else {
                connected = state
                    .lock()
                    .map(|s| s.status.helper_connected)
                    .unwrap_or(false);
            }
        }
        if let Ok(mut s) = state.lock() {
            if new_sample {
                record_telemetry(&mut s.telemetry_samples, &sample);
            }
            s.status.telemetry = sample.clone();
            s.status.pressure = decision.pressure;
            s.status.build_jobs = decision.build_jobs;
            s.status.admit_optional = decision.admit_optional;
            s.status.helper_connected = connected;
            s.status.active_leases = leases.len();
            s.status.capabilities[0].available = connected;
            s.decision = decision;
        }
        if tick.saturating_sub(last_history) >= 30_000 {
            if let Ok(s) = state.lock() {
                let snapshot = s.snapshot();
                let samples = s.telemetry_samples.iter().rev().take(30).cloned().collect();
                drop(s);
                // A full history queue may coalesce this periodic snapshot;
                // it cannot delay restoration or the serial policy loop.
                let _ = history_tx.try_send((snapshot, samples));
            }
            last_history = tick;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
pub fn helper_smoke() -> Result<serde_json::Value, String> {
    let requirement = helper_requirement().ok_or("installed signed helper unavailable")?;
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let result = (|| -> Result<serde_json::Value, String> {
        let identity = macflow_platform::identity(child.id() as i32)?;
        let original = macflow_platform::nice(identity.pid)?;
        let target = (original + 5).min(10);
        if target == original {
            return Err(
                "smoke child already has nice >= 10; cannot verify an actual priority change"
                    .into(),
            );
        }
        let key = format!("smoke-{}", std::process::id());
        helper_request(
            Command::HelperApply {
                identity: identity.clone(),
                nice: target,
                lease_id: key.clone(),
                deadline_ms: macflow_platform::now_ms() + 2500,
            },
            &requirement,
        )?;
        if macflow_platform::nice(identity.pid)? != target {
            return Err("applied priority not observed".into());
        }
        helper_request(
            Command::HelperRevoke {
                lease_id: Some(key.clone()),
            },
            &requirement,
        )?;
        if macflow_platform::nice(identity.pid)? != original {
            return Err("explicit restoration failed".into());
        }
        helper_request(
            Command::HelperApply {
                identity: identity.clone(),
                nice: target,
                lease_id: key.clone(),
                deadline_ms: macflow_platform::now_ms() + 1500,
            },
            &requirement,
        )?;
        std::thread::sleep(Duration::from_millis(2100));
        if macflow_platform::nice(identity.pid)? != original {
            return Err("lease expiry restoration failed".into());
        }
        let mut wrong = identity.clone();
        wrong.start_micros = wrong.start_micros.wrapping_add(1);
        expect_helper_rejection(
            Command::HelperApply {
                identity: wrong,
                nice: target,
                lease_id: format!("{key}-identity"),
                deadline_ms: macflow_platform::now_ms() + 1500,
            },
            &requirement,
        )?;
        expect_helper_rejection(
            Command::HelperApply {
                identity: identity.clone(),
                nice: original.saturating_add(6),
                lease_id: format!("{key}-range"),
                deadline_ms: macflow_platform::now_ms() + 1500,
            },
            &requirement,
        )?;
        let protected = macflow_platform::identity(std::process::id() as i32)?;
        expect_helper_rejection(
            Command::HelperApply {
                nice: macflow_platform::nice(protected.pid)?
                    .saturating_add(5)
                    .min(10),
                identity: protected,
                lease_id: format!("{key}-protected"),
                deadline_ms: macflow_platform::now_ms() + 1500,
            },
            &requirement,
        )?;
        Ok(
            serde_json::json!({"pid":identity.pid,"original_nice":original,"applied_nice":target,"explicit_restore":true,"expiry_restore":true,"invalid_identity_rejected":true,"out_of_range_rejected":true,"protected_process_rejected":true}),
        )
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn expect_helper_rejection(action: Command, requirement: &str) -> Result<(), String> {
    let lease_key = match &action {
        Command::HelperApply { lease_id, .. } => Some(lease_id.clone()),
        _ => None,
    };
    let request = Request {
        schema: SCHEMA,
        id: macflow_platform::now_ms(),
        action,
    };
    // A transport timeout is not evidence that the helper rejected a request.
    let response = ipc::call(
        HELPER_SERVICE,
        true,
        &request,
        Some(requirement),
        Duration::from_millis(800),
    )?;
    if !response.ok {
        return Ok(());
    }
    if let Some(key) = lease_key {
        let _ = helper_request(
            Command::HelperRevoke {
                lease_id: Some(key),
            },
            requirement,
        );
    }
    Err("invalid helper request unexpectedly accepted".into())
}

/// A bounded, explicit test action for the signed agent binary. After one
/// Apply, only read the dedicated child; no heartbeat or renewal is sent.
pub fn helper_hold_smoke() -> Result<serde_json::Value, String> {
    let requirement = helper_requirement().ok_or("installed signed helper unavailable")?;
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let result = (|| -> Result<serde_json::Value, String> {
        let identity = macflow_platform::identity(child.id() as i32)?;
        let original = macflow_platform::nice(identity.pid)?;
        let target = original.saturating_add(5).min(10);
        if target == original {
            return Err(
                "smoke child already has nice >= 10; cannot verify an actual priority change"
                    .into(),
            );
        }
        let started = macflow_platform::now_ms();
        let deadline = started + 10_000;
        let key = format!("hold-smoke-{}", std::process::id());
        helper_request(
            Command::HelperApply {
                identity: identity.clone(),
                nice: target,
                lease_id: key.clone(),
                deadline_ms: deadline,
            },
            &requirement,
        )?;
        if macflow_platform::nice(identity.pid)? != target {
            return Err("applied hold-smoke priority not observed".into());
        }
        println!(
            "{}",
            serde_json::json!({"phase":"holding","pid":identity.pid,
            "original_nice":original,"applied_nice":target,"lease_id":key,
            "lease_deadline_ms":deadline,"hold_seconds":15,"renewals":0})
        );
        use std::io::Write;
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut restored_after_ms = None;
        while macflow_platform::now_ms().saturating_sub(started) < 15_000 {
            if macflow_platform::identity(identity.pid)? != identity {
                return Err("smoke child identity changed".into());
            }
            let current = macflow_platform::nice(identity.pid)?;
            if current == original {
                restored_after_ms.get_or_insert(macflow_platform::now_ms().saturating_sub(started));
            } else if current != target {
                return Err(format!("unexpected smoke-child priority {current}"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if macflow_platform::nice(identity.pid)? != original {
            return Err("ten-second lease did not restore by end of fifteen-second hold".into());
        }
        Ok(serde_json::json!({"phase":"completed","pid":identity.pid,
            "original_nice":original,"applied_nice":target,"renewals":0,
            "restored":true,"restored_after_ms":restored_after_ms,
            "hold_ms":macflow_platform::now_ms().saturating_sub(started)}))
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(pid: i32) -> ProcessIdentity {
        ProcessIdentity {
            boot_id: "boot".into(),
            pid,
            start_secs: 20,
            start_micros: 10,
            uid: 501,
            executable: "/usr/bin/clang".into(),
        }
    }

    #[test]
    fn three_consecutive_failures_disable_rule_and_ambiguous_calls_wait_out_lease() {
        let mut failures = ActionFailures::default();
        let identity = identity(100);
        let selector = "path:/usr/bin/clang";
        assert!(!failures.failed(selector, &identity, 1_000));
        assert!(!failures.allows(&identity, 10_999));
        assert!(failures.allows(&identity, 11_000));
        assert!(!failures.failed(selector, &identity, 11_000));
        assert!(failures.failed(selector, &identity, 21_000));
        assert!(!failures.allows_rule(selector));
        assert!(failures.allows_rule("path:/usr/bin/zip"));
    }

    #[test]
    fn success_resets_consecutive_failures_and_user_reenable_resets_circuit() {
        let mut failures = ActionFailures::default();
        let identity = identity(100);
        let selector = "path:/usr/bin/clang";
        assert!(!failures.failed(selector, &identity, 0));
        assert!(!failures.failed(selector, &identity, 10_000));
        failures.succeeded(selector, &identity);
        assert!(failures.allows(&identity, 10_001));
        assert!(!failures.failed(selector, &identity, 10_002));
        assert!(!failures.failed(selector, &identity, 20_002));
        assert!(failures.failed(selector, &identity, 30_002));
        failures.synchronise(&[selector.into()], 30_003);
        failures.synchronise(&[], 40_003);
        assert!(failures.allows_rule(selector));
        assert!(!failures.failed(selector, &identity, 40_004));
    }

    #[test]
    fn retry_quarantine_is_identity_scoped_not_pid_scoped() {
        let mut failures = ActionFailures::default();
        let old = identity(100);
        failures.failed("clang", &old, 0);
        let mut new = old.clone();
        new.start_micros += 1;
        assert!(!failures.allows(&old, 1));
        assert!(failures.allows(&new, 1));
        failures.synchronise(&[], AMBIGUOUS_ACTION_BACKOFF_MS);
        assert!(failures.retry_after.is_empty());
    }

    #[test]
    fn telemetry_ring_is_five_minutes_and_bounded_with_foreground_retained() {
        let mut ring = VecDeque::new();
        let processes: Vec<_> = (2..102)
            .map(|pid| ProcessSample {
                identity: identity(pid),
                parent_pid: 1,
                name: format!("task-{pid}"),
                bundle_id: None,
                nice: 0,
                cpu_percent: f64::from(pid),
                memory_bytes: 0,
                role: TaskRole::Unknown,
                protected_reason: None,
            })
            .collect();
        let mut sample = Telemetry {
            boot_id: "boot".into(),
            foreground_pid: Some(2),
            processes,
            ..Telemetry::default()
        };
        for second in 0..310 {
            sample.at_ms = second * 1000;
            record_telemetry(&mut ring, &sample);
        }
        assert_eq!(ring.len(), 300);
        assert_eq!(ring.front().unwrap().at_ms, 10_000);
        assert!(
            ring.iter()
                .all(|s| s.processes.len() == 32 && s.processes[0].identity.pid == 2)
        );
        sample.at_ms = 1_000_000;
        record_telemetry(&mut ring, &sample);
        assert_eq!(ring.len(), 1);
        assert_eq!(
            sample.processes.len(),
            100,
            "control sample must remain intact"
        );
    }

    #[test]
    fn diagnostic_truncation_is_utf8_safe_and_does_not_mutate_control_identity() {
        let mut ring = VecDeque::new();
        let original = "文件".repeat(512);
        let sample = Telemetry {
            at_ms: 1,
            processes: vec![ProcessSample {
                identity: ProcessIdentity {
                    executable: original.clone(),
                    ..identity(100)
                },
                parent_pid: 1,
                name: "编译".repeat(128),
                bundle_id: None,
                nice: 0,
                cpu_percent: 1.0,
                memory_bytes: 0,
                role: TaskRole::Compute,
                protected_reason: None,
            }],
            errors: vec!["错误".repeat(512); 50],
            ..Telemetry::default()
        };
        record_telemetry(&mut ring, &sample);
        assert_eq!(sample.processes[0].identity.executable, original);
        let recorded = ring.front().unwrap();
        assert!(recorded.processes[0].identity.executable.len() <= 515);
        assert!(recorded.processes[0].name.len() <= 131);
        assert_eq!(recorded.errors.len(), 8);
        assert!(recorded.errors.iter().all(|error| error.len() <= 515));
        serde_json::to_vec(recorded).unwrap();
    }
}
