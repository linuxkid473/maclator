#![forbid(unsafe_code)]
//! Deterministic, fail-open scheduling policy. This crate never changes a process.
//!
//! Samples are expected once per second. A missing interval, a boot change, or a
//! backwards timestamp discards pressure history. Unknown CPU or memory metrics
//! cannot create a pressure event. Protection is recomputed on every sample.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use macflow_protocol::{
    AppRule, Config, Decision, MemoryPressure, Mode, NiceTarget, ProcessIdentity, ProcessSample,
    RulePolicy, SCHEMA, TaskRole, Telemetry,
};

const WINDOW_MS: u64 = 5_000;
const MAX_SAMPLE_GAP_MS: u64 = 2_500;
const RECOVERY_MS: u64 = 15_000;
const MIN_WINDOW_SAMPLES: u32 = 4;

#[derive(Debug)]
struct ProcessState {
    baseline_nice: i32,
    proposed_nice: Option<i32>,
    last_seen_ms: u64,
    consecutive_samples: u32,
    externally_changed: bool,
}

#[derive(Debug, Default)]
struct CpuWindow {
    start_ms: u64,
    samples: u32,
    total: f64,
    all_have_candidate: bool,
}

/// A single-owner policy state machine. Use one instance per logged-in user.
#[derive(Debug, Default)]
pub struct Engine {
    boot_id: String,
    last_sample_ms: Option<u64>,
    window: Option<CpuWindow>,
    hot_windows: u32,
    memory_warning_samples: u32,
    cpu_pressure: bool,
    memory_pressure: bool,
    recovery_start_ms: Option<u64>,
    processes: HashMap<ProcessIdentity, ProcessState>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns desired state, rather than commands. The caller must restore
    /// leases absent from `nice_targets`, including on pause or observation.
    pub fn evaluate(&mut self, sample: &Telemetry, config: &Config) -> Decision {
        let mut decision = mode_defaults(config.mode, false);
        let protections = protected_processes(sample, config);
        decision.protected_pids = protections.iter().copied().collect();
        decision.protected_pids.sort_unstable();

        let repeated_or_backwards = self.last_sample_ms.is_some_and(|last| sample.at_ms <= last);
        let gap = self
            .last_sample_ms
            .is_some_and(|last| sample.at_ms.saturating_sub(last) > MAX_SAMPLE_GAP_MS);
        let boot_changed = self.boot_id != sample.boot_id;
        if sample.boot_id.is_empty() || repeated_or_backwards {
            self.clear_pressure();
            // A rejected sample must not become the anchor for later samples.
            return decision;
        }
        if boot_changed || gap {
            self.clear_pressure();
            self.processes.clear();
        }
        self.boot_id.clone_from(&sample.boot_id);
        self.last_sample_ms = Some(sample.at_ms);

        self.record_processes(sample);
        let candidates: Vec<&ProcessSample> = sample
            .processes
            .iter()
            .filter(|process| {
                !protections.contains(&process.identity.pid)
                    && stable_identity(process, &sample.boot_id)
                    && process.cpu_percent.is_finite()
                    && process.cpu_percent > 0.0
                    && is_background_compute(process, config)
                    && self.processes.get(&process.identity).is_some_and(|state| {
                        state.consecutive_samples >= 2 && !state.externally_changed
                    })
            })
            .collect();

        let cpu = sample
            .cpu_percent
            .filter(|percent| percent.is_finite() && (0.0..=100.0).contains(percent));
        if let Some(cpu) = cpu {
            self.record_cpu(sample.at_ms, cpu, !candidates.is_empty());
        } else {
            self.window = None;
            self.hot_windows = 0;
            self.cpu_pressure = false;
        }

        match sample.memory_pressure {
            MemoryPressure::Warning | MemoryPressure::Critical => {
                self.memory_warning_samples = self.memory_warning_samples.saturating_add(1);
                if self.memory_warning_samples >= 3 {
                    self.memory_pressure = true;
                }
            }
            MemoryPressure::Normal => self.memory_warning_samples = 0,
            MemoryPressure::Unknown => {
                self.memory_warning_samples = 0;
                self.memory_pressure = false;
            }
        }

        // Recovery requires known healthy memory plus a fresh CPU measurement.
        // A foreground-only load is not a reason to retain background control.
        let normal = cpu.is_some_and(|cpu| cpu <= 85.0 || candidates.is_empty())
            && sample.memory_pressure == MemoryPressure::Normal;
        if self.cpu_pressure || self.memory_pressure {
            if normal {
                let since = *self.recovery_start_ms.get_or_insert(sample.at_ms);
                if sample.at_ms.saturating_sub(since) >= RECOVERY_MS {
                    self.cpu_pressure = false;
                    self.memory_pressure = false;
                    self.hot_windows = 0;
                    self.recovery_start_ms = None;
                }
            } else {
                self.recovery_start_ms = None;
            }
        } else {
            self.recovery_start_ms = None;
        }

        let pressure = self.cpu_pressure || self.memory_pressure;
        let protected_pids = decision.protected_pids;
        decision = mode_defaults(config.mode, pressure);
        decision.protected_pids = protected_pids;
        decision.admit_optional = !self.memory_pressure;

        // A reported driver failure stops bridge spinning immediately. No
        // other GPU metric is trusted here: no temperature/utilization guesses.
        if sample.gpu.error.is_some() {
            decision.fg_spin_us = 0;
            decision.bg_spin_us = 0;
        }

        if config.observe_only || config.paused || config.schema != SCHEMA {
            return decision;
        }
        // Memory pressure gates admission; lowering CPU priority alone does
        // not release memory and should not be used as a memory remedy.
        if self.cpu_pressure && cpu.is_some() {
            for process in candidates {
                if let Some(state) = self.processes.get_mut(&process.identity) {
                    let target = (state.baseline_nice + 5).min(10);
                    if target > state.baseline_nice && (-20..=20).contains(&process.nice) {
                        state.proposed_nice = Some(target);
                        decision.nice_targets.push(NiceTarget {
                            identity: process.identity.clone(),
                            nice: target,
                            reason: "持续 CPU 争用：降低已识别后台计算任务的优先级".into(),
                        });
                    }
                }
            }
            decision
                .nice_targets
                .sort_by_key(|target| target.identity.pid);
        }
        decision
    }

    fn clear_pressure(&mut self) {
        self.window = None;
        self.hot_windows = 0;
        self.memory_warning_samples = 0;
        self.cpu_pressure = false;
        self.memory_pressure = false;
        self.recovery_start_ms = None;
    }

    fn record_processes(&mut self, sample: &Telemetry) {
        let present: HashSet<&ProcessIdentity> = sample
            .processes
            .iter()
            .map(|process| &process.identity)
            .collect();
        self.processes
            .retain(|identity, _| present.contains(identity));
        for process in &sample.processes {
            if !stable_identity(process, &sample.boot_id) || !(-20..=20).contains(&process.nice) {
                continue;
            }
            let state = self
                .processes
                .entry(process.identity.clone())
                .or_insert(ProcessState {
                    baseline_nice: process.nice,
                    proposed_nice: None,
                    last_seen_ms: sample.at_ms,
                    consecutive_samples: 0,
                    externally_changed: false,
                });
            if process.nice != state.baseline_nice && Some(process.nice) != state.proposed_nice {
                // Do not fight a manual or third-party priority adjustment.
                // Retain this protection for the lifetime of this identity.
                state.externally_changed = true;
            }
            if sample.at_ms.saturating_sub(state.last_seen_ms) <= MAX_SAMPLE_GAP_MS {
                state.consecutive_samples = state.consecutive_samples.saturating_add(1);
            } else {
                state.consecutive_samples = 1;
            }
            state.last_seen_ms = sample.at_ms;
        }
    }

    fn record_cpu(&mut self, at_ms: u64, cpu: f64, has_candidate: bool) {
        if !has_candidate {
            self.window = None;
            self.hot_windows = 0;
            return;
        }
        if let Some(window) = &self.window {
            if at_ms.saturating_sub(window.start_ms) >= WINDOW_MS {
                let hot = window.samples >= MIN_WINDOW_SAMPLES
                    && window.all_have_candidate
                    && window.total / f64::from(window.samples) > 85.0;
                if hot {
                    self.hot_windows = self.hot_windows.saturating_add(1);
                    if self.hot_windows >= 2 {
                        self.cpu_pressure = true;
                    }
                } else {
                    self.hot_windows = 0;
                }
                self.window = None;
            }
        }
        let window = self.window.get_or_insert(CpuWindow {
            start_ms: at_ms,
            samples: 0,
            total: 0.0,
            all_have_candidate: true,
        });
        window.samples = window.samples.saturating_add(1);
        window.total += cpu;
        window.all_have_candidate &= has_candidate;
    }
}

fn mode_defaults(mode: Mode, pressure: bool) -> Decision {
    let (normal_jobs, pressure_jobs, fg_spin, bg_spin) = match mode {
        Mode::Auto => (12, 8, 5, 0),
        Mode::Smooth => (8, 4, 5, 0),
        Mode::Performance => (16, 12, 20, 5),
    };
    Decision {
        pressure,
        build_jobs: if pressure { pressure_jobs } else { normal_jobs },
        admit_optional: true,
        fg_spin_us: if pressure { 0 } else { fg_spin },
        bg_spin_us: if pressure { 0 } else { bg_spin },
        ..Decision::default()
    }
}

fn stable_identity(process: &ProcessSample, boot_id: &str) -> bool {
    process.identity.pid > 1
        && process.identity.uid != 0
        && process.identity.boot_id == boot_id
        && (process.identity.start_secs != 0 || process.identity.start_micros != 0)
        && process.identity.start_micros < 1_000_000
        && Path::new(&process.identity.executable).is_absolute()
}

fn executable_name(process: &ProcessSample) -> &str {
    Path::new(&process.identity.executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
}

fn rule_matches(rule: &AppRule, process: &ProcessSample) -> bool {
    let selector = rule.selector.trim();
    if let Some(bundle) = selector.strip_prefix("bundle:") {
        return process.bundle_id.as_deref() == Some(bundle);
    }
    if let Some(path) = selector.strip_prefix("path:") {
        return process.identity.executable == path;
    }
    if let Some(name) = selector.strip_prefix("name:") {
        return executable_name(process) == name || process.name == name;
    }
    !selector.is_empty()
        && (process.bundle_id.as_deref() == Some(selector)
            || process.identity.executable == selector
            || executable_name(process) == selector
            || process.name == selector)
}

fn active_rule<'a>(rule: &'a AppRule, config: &Config) -> Option<&'a AppRule> {
    (rule.policy == RulePolicy::Protect || !config.disabled_rules.contains(&rule.selector))
        .then_some(rule)
}

fn is_background_compute(process: &ProcessSample, config: &Config) -> bool {
    // A failed optimization must not silently reactivate through another
    // matching allow rule, an explicit cooperative role, or built-in detection.
    if config
        .disabled_rules
        .iter()
        .any(|selector| selector_matches_process(selector, process))
    {
        return false;
    }
    process.role == TaskRole::Compute
        || config
            .rules
            .iter()
            .filter_map(|rule| active_rule(rule, config))
            .any(|rule| rule.policy == RulePolicy::AllowBackground && rule_matches(rule, process))
        || matches!(
            executable_name(process),
            "clang"
                | "clang++"
                | "rustc"
                | "xcodebuild"
                | "swiftc"
                | "swift-frontend"
                | "cc1"
                | "7z"
                | "7za"
                | "zip"
        )
}

/// The matching explicit optimization rule, or an executable-scoped built-in
/// rule identifier. Failure quarantine never disables protection rules.
pub fn failure_rule_selector(process: &ProcessSample, config: &Config) -> String {
    config
        .rules
        .iter()
        .find(|rule| rule.policy == RulePolicy::AllowBackground && rule_matches(rule, process))
        .map(|rule| rule.selector.clone())
        .unwrap_or_else(|| format!("path:{}", process.identity.executable))
}

pub fn selector_matches_process(selector: &str, process: &ProcessSample) -> bool {
    rule_matches(
        &AppRule {
            selector: selector.into(),
            policy: RulePolicy::AllowBackground,
        },
        process,
    )
}

fn protected_processes(sample: &Telemetry, config: &Config) -> HashSet<i32> {
    let by_pid: HashMap<i32, &ProcessSample> = sample
        .processes
        .iter()
        .map(|process| (process.identity.pid, process))
        .collect();
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for process in &sample.processes {
        children
            .entry(process.parent_pid)
            .or_default()
            .push(process.identity.pid);
    }
    let mut protected = HashSet::new();
    let mut family_roots = Vec::new();
    for process in &sample.processes {
        let pid = process.identity.pid;
        let protected_rule = config
            .rules
            .iter()
            .filter_map(|rule| active_rule(rule, config))
            .any(|rule| rule.policy == RulePolicy::Protect && rule_matches(rule, process));
        let is_foreground =
            Some(pid) == sample.foreground_pid || process.role == TaskRole::Foreground;
        let explicitly_protected = process.role == TaskRole::Protected
            || process.protected_reason.is_some()
            || protected_rule;
        if is_foreground || explicitly_protected {
            protected.insert(pid);
            family_roots.push(pid);
        }
        if process.identity.uid == 0 || pid <= 1 || is_system_or_realtime(process) {
            protected.insert(pid);
        }
    }

    // Descendants of foreground/protected applications can be synchronous
    // dependencies. Walk a bounded graph, also tolerating process-list cycles.
    let mut descendant_queue = family_roots.clone();
    while let Some(parent) = descendant_queue.pop() {
        if let Some(descendants) = children.get(&parent) {
            for &pid in descendants {
                if protected.insert(pid) {
                    descendant_queue.push(pid);
                }
            }
        }
    }
    // Protect ancestors themselves, but do not expand their other children:
    // launchd and shared shell/launcher ancestors must not protect the world.
    for root in family_roots {
        let mut next = root;
        let mut visited = HashSet::new();
        while visited.insert(next) {
            let Some(process) = by_pid.get(&next) else {
                break;
            };
            let parent = process.parent_pid;
            if parent <= 1 || parent == next {
                break;
            }
            protected.insert(parent);
            next = parent;
        }
    }
    protected
}

fn is_system_or_realtime(process: &ProcessSample) -> bool {
    let path = &process.identity.executable;
    path.starts_with("/System/Library/")
        || matches!(
            executable_name(process),
            "launchd"
                | "kernel_task"
                | "WindowServer"
                | "loginwindow"
                | "coreaudiod"
                | "audiod"
                | "bluetoothd"
                | "tccd"
                | "MacFlow"
                | "macflow"
                | "macflow-agent"
                | "macflow-helper"
                | "sshd"
                | "ARDAgent"
                | "screensharingd"
                | "uuremote"
        )
        || process.bundle_id.as_deref().is_some_and(|bundle| {
            matches!(
                bundle,
                "com.apple.Music" | "com.apple.FaceTime" | "com.apple.QuickTimePlayer"
            ) || bundle.starts_with("com.apple.inputmethod.")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: i32, parent: i32, executable: &str) -> ProcessSample {
        ProcessSample {
            identity: ProcessIdentity {
                boot_id: "test-boot".into(),
                pid,
                start_secs: 50,
                start_micros: 1234,
                uid: 501,
                executable: executable.into(),
            },
            parent_pid: parent,
            name: Path::new(executable)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            bundle_id: None,
            nice: 0,
            cpu_percent: 10.0,
            memory_bytes: 1024,
            role: TaskRole::Unknown,
            protected_reason: None,
        }
    }

    fn sample(at_ms: u64, cpu: Option<f64>) -> Telemetry {
        Telemetry {
            at_ms,
            boot_id: "test-boot".into(),
            cpu_percent: cpu,
            memory_pressure: MemoryPressure::Normal,
            foreground_pid: Some(100),
            processes: vec![
                process(100, 1, "/Applications/Editor.app/Editor"),
                process(200, 1, "/usr/bin/clang"),
            ],
            ..Telemetry::default()
        }
    }

    fn control() -> Config {
        Config {
            observe_only: false,
            ..Config::default()
        }
    }

    fn hot(engine: &mut Engine, config: &Config) -> Decision {
        // First observation establishes stable process identities. Two fully
        // eligible subsequent five-second windows then begin at one second.
        let mut result = Decision::default();
        for second in 0..=15 {
            result = engine.evaluate(&sample(second * 1000, Some(90.0)), config);
        }
        result
    }

    #[test]
    fn requires_two_complete_windows_with_stable_compute_candidate() {
        let mut engine = Engine::new();
        let config = control();
        for second in 0..11 {
            let decision = engine.evaluate(&sample(second * 1000, Some(90.0)), &config);
            assert!(!decision.pressure, "pressure activated at {second}s");
            assert!(decision.nice_targets.is_empty());
        }
        let decision = engine.evaluate(&sample(11_000, Some(90.0)), &config);
        assert!(decision.pressure);
        assert_eq!(decision.nice_targets.len(), 1);
        assert_eq!(decision.nice_targets[0].identity.pid, 200);
        assert_eq!(decision.nice_targets[0].nice, 5);
        assert_eq!(decision.build_jobs, 8);
    }

    #[test]
    fn exact_threshold_is_not_pressure_and_idle_compute_does_not_trigger() {
        for cpu in [0.0, 85.0] {
            let mut engine = Engine::new();
            for second in 0..30 {
                assert!(
                    !engine
                        .evaluate(&sample(second * 1000, Some(cpu)), &control())
                        .pressure
                );
            }
        }
        let mut engine = Engine::new();
        for second in 0..30 {
            let mut telemetry = sample(second * 1000, Some(99.0));
            telemetry.processes[1].cpu_percent = 0.0;
            assert!(!engine.evaluate(&telemetry, &control()).pressure);
        }
    }

    #[test]
    fn memory_requires_three_samples_and_recovery_fifteen_seconds() {
        let mut engine = Engine::new();
        for second in 0..3 {
            let mut telemetry = sample(second * 1000, Some(20.0));
            telemetry.memory_pressure = MemoryPressure::Warning;
            let decision = engine.evaluate(&telemetry, &control());
            assert_eq!(decision.pressure, second == 2);
            assert_eq!(decision.admit_optional, second != 2);
            assert!(
                decision.nice_targets.is_empty(),
                "memory-only pressure must not nice"
            );
        }
        for second in 3..18 {
            let decision = engine.evaluate(&sample(second * 1000, Some(20.0)), &control());
            assert!(decision.pressure);
        }
        let recovered = engine.evaluate(&sample(18_000, Some(20.0)), &control());
        assert!(!recovered.pressure);
        assert!(recovered.admit_optional);
    }

    #[test]
    fn intermittent_memory_warnings_never_accumulate() {
        let mut engine = Engine::new();
        for second in 0..30 {
            let mut telemetry = sample(second * 1000, Some(20.0));
            telemetry.memory_pressure = if second % 3 == 2 {
                MemoryPressure::Unknown
            } else {
                MemoryPressure::Critical
            };
            assert!(!engine.evaluate(&telemetry, &control()).pressure);
        }
    }

    #[test]
    fn invalid_and_unknown_cpu_cannot_trigger() {
        for cpu in [
            None,
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(-1.0),
            Some(101.0),
        ] {
            let mut engine = Engine::new();
            for second in 0..30 {
                let decision = engine.evaluate(&sample(second * 1000, cpu), &control());
                assert!(!decision.pressure);
                assert!(decision.nice_targets.is_empty());
            }
        }
    }

    #[test]
    fn gaps_backwards_and_boot_changes_fail_open() {
        let mut engine = Engine::new();
        assert!(hot(&mut engine, &control()).pressure);
        let gap = engine.evaluate(&sample(20_000, Some(95.0)), &control());
        assert!(!gap.pressure);
        assert!(gap.nice_targets.is_empty());
        assert!(
            !engine
                .evaluate(&sample(19_000, Some(95.0)), &control())
                .pressure
        );
        let mut reboot = sample(21_000, Some(95.0));
        reboot.boot_id = "new-boot".into();
        assert!(!engine.evaluate(&reboot, &control()).pressure);
    }

    #[test]
    fn foreground_descendants_and_ancestors_are_protected_without_all_siblings() {
        let mut engine = Engine::new();
        let mut decision = Decision::default();
        for second in 0..=15 {
            let mut telemetry = sample(second * 1000, Some(95.0));
            telemetry.processes[0].parent_pid = 50;
            telemetry.processes.push(process(50, 1, "/usr/bin/zsh"));
            telemetry
                .processes
                .push(process(300, 100, "/usr/bin/clang"));
            telemetry
                .processes
                .push(process(301, 300, "/usr/bin/swiftc"));
            telemetry.processes[1].parent_pid = 50;
            decision = engine.evaluate(&telemetry, &control());
        }
        assert_eq!(decision.protected_pids, vec![50, 100, 300, 301]);
        assert_eq!(decision.nice_targets.len(), 1);
        assert_eq!(decision.nice_targets[0].identity.pid, 200);
    }

    #[test]
    fn foreground_switch_releases_targets_immediately() {
        let mut engine = Engine::new();
        assert_eq!(hot(&mut engine, &control()).nice_targets.len(), 1);
        let mut telemetry = sample(16_000, Some(95.0));
        telemetry.foreground_pid = Some(200);
        let decision = engine.evaluate(&telemetry, &control());
        assert!(decision.nice_targets.is_empty());
        assert!(decision.protected_pids.contains(&200));
    }

    #[test]
    fn protected_roles_and_rules_override_allow_background() {
        let mut config = control();
        config.rules = vec![
            AppRule {
                selector: "name:clang".into(),
                policy: RulePolicy::AllowBackground,
            },
            AppRule {
                selector: "clang".into(),
                policy: RulePolicy::Protect,
            },
        ];
        let mut engine = Engine::new();
        assert!(hot(&mut engine, &config).nice_targets.is_empty());
        config.disabled_rules.push("clang".into());
        let mut engine = Engine::new();
        assert!(hot(&mut engine, &config).nice_targets.is_empty());
        // Disabling an optimization must never disable a protection rule.
        config
            .rules
            .retain(|rule| rule.policy != RulePolicy::Protect);
        config.disabled_rules.clear();
        let mut engine = Engine::new();
        assert_eq!(hot(&mut engine, &config).nice_targets.len(), 1);
        for role in [TaskRole::Protected, TaskRole::Foreground] {
            let mut engine = Engine::new();
            for second in 0..=15 {
                let mut telemetry = sample(second * 1000, Some(95.0));
                telemetry.processes[1].role = role;
                assert!(
                    engine
                        .evaluate(&telemetry, &control())
                        .nice_targets
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn system_realtime_unknown_and_gui_maclator_are_not_candidates() {
        for executable in [
            "/Users/test/bin/maclator",
            "/usr/bin/unknown",
            "/System/Library/clang",
        ] {
            let mut engine = Engine::new();
            for second in 0..=15 {
                let mut telemetry = sample(second * 1000, Some(95.0));
                telemetry.processes[1] = process(200, 1, executable);
                assert!(
                    engine
                        .evaluate(&telemetry, &control())
                        .nice_targets
                        .is_empty()
                );
            }
        }
        let mut engine = Engine::new();
        let mut result = Decision::default();
        for second in 0..=15 {
            let mut telemetry = sample(second * 1000, Some(95.0));
            telemetry.processes[1] = process(200, 1, "/Users/test/bin/maclator");
            telemetry.processes[1].role = TaskRole::Compute;
            result = engine.evaluate(&telemetry, &control());
        }
        assert_eq!(result.nice_targets.len(), 1);
    }

    #[test]
    fn nice_never_ratchets_or_exceeds_increase_cap() {
        let mut engine = Engine::new();
        assert_eq!(hot(&mut engine, &control()).nice_targets[0].nice, 5);
        for second in 16..300 {
            let mut telemetry = sample(second * 1000, Some(95.0));
            telemetry.processes[1].nice = 5;
            assert_eq!(
                engine.evaluate(&telemetry, &control()).nice_targets[0].nice,
                5
            );
        }
        for original in [-20, 8, 10, 15] {
            let mut engine = Engine::new();
            let mut result = Decision::default();
            for second in 0..=15 {
                let mut telemetry = sample(second * 1000, Some(95.0));
                telemetry.processes[1].nice = original;
                result = engine.evaluate(&telemetry, &control());
            }
            if original < 10 {
                assert_eq!(result.nice_targets[0].nice, (original + 5).min(10));
            } else {
                assert!(result.nice_targets.is_empty());
            }
        }
    }

    #[test]
    fn external_priority_change_is_not_overwritten() {
        let mut engine = Engine::new();
        assert_eq!(hot(&mut engine, &control()).nice_targets.len(), 1);
        let mut telemetry = sample(16_000, Some(95.0));
        telemetry.processes[1].nice = 3;
        assert!(
            engine
                .evaluate(&telemetry, &control())
                .nice_targets
                .is_empty()
        );
        telemetry.at_ms = 17_000;
        telemetry.processes[1].nice = 0;
        assert!(
            engine
                .evaluate(&telemetry, &control())
                .nice_targets
                .is_empty()
        );
    }

    #[test]
    fn observe_pause_and_unknown_schema_emit_no_changes() {
        for config in [
            Config::default(),
            Config {
                paused: true,
                ..control()
            },
            Config {
                schema: SCHEMA + 1,
                ..control()
            },
        ] {
            let decision = hot(&mut Engine::new(), &config);
            assert!(decision.nice_targets.is_empty());
        }
    }

    #[test]
    fn modes_have_explicit_jobs_spins_and_gpu_failure_stops_spins() {
        for (mode, jobs, pressure_jobs, foreground, background) in [
            (Mode::Auto, 12, 8, 5, 0),
            (Mode::Smooth, 8, 4, 5, 0),
            (Mode::Performance, 16, 12, 20, 5),
        ] {
            let config = Config { mode, ..control() };
            let mut engine = Engine::new();
            let normal = engine.evaluate(&sample(0, Some(20.0)), &config);
            assert_eq!(
                (normal.build_jobs, normal.fg_spin_us, normal.bg_spin_us),
                (jobs, foreground, background)
            );
            let mut pressure = Decision::default();
            for second in 1..=15 {
                pressure = engine.evaluate(&sample(second * 1000, Some(95.0)), &config);
            }
            assert_eq!(pressure.build_jobs, pressure_jobs);
            assert_eq!((pressure.fg_spin_us, pressure.bg_spin_us), (0, 0));
            let mut engine = Engine::new();
            let mut telemetry = sample(0, Some(20.0));
            telemetry.gpu.error = Some("driver recovery".into());
            let failed = engine.evaluate(&telemetry, &config);
            assert_eq!((failed.fg_spin_us, failed.bg_spin_us), (0, 0));
        }
    }

    #[test]
    fn invalid_or_reused_identity_needs_new_stability() {
        let mut engine = Engine::new();
        assert!(hot(&mut engine, &control()).pressure);
        let mut telemetry = sample(16_000, Some(95.0));
        telemetry.processes[1].identity.start_secs += 1;
        assert!(
            engine
                .evaluate(&telemetry, &control())
                .nice_targets
                .is_empty()
        );
        telemetry.at_ms = 17_000;
        assert_eq!(
            engine.evaluate(&telemetry, &control()).nice_targets.len(),
            1
        );
        telemetry.at_ms = 18_000;
        telemetry.processes[1].identity.start_micros = 1_000_000;
        assert!(
            engine
                .evaluate(&telemetry, &control())
                .nice_targets
                .is_empty()
        );
    }

    #[test]
    fn cpu_recovery_timer_restarts_after_a_new_hot_sample() {
        let mut engine = Engine::new();
        assert!(hot(&mut engine, &control()).pressure);
        for second in 16..30 {
            assert!(
                engine
                    .evaluate(&sample(second * 1000, Some(20.0)), &control())
                    .pressure
            );
        }
        assert!(
            engine
                .evaluate(&sample(30_000, Some(95.0)), &control())
                .pressure
        );
        for second in 31..46 {
            assert!(
                engine
                    .evaluate(&sample(second * 1000, Some(20.0)), &control())
                    .pressure
            );
        }
        let recovered = engine.evaluate(&sample(46_000, Some(20.0)), &control());
        assert!(!recovered.pressure);
        assert!(recovered.nice_targets.is_empty());
        assert_eq!(recovered.build_jobs, 12);
    }

    #[test]
    fn sparse_sampling_and_missing_stable_identity_do_not_create_pressure() {
        let mut engine = Engine::new();
        for step in 0..30 {
            assert!(
                !engine
                    .evaluate(&sample(step * 2000, Some(95.0)), &control())
                    .pressure
            );
        }
        for invalid in ["uid", "start", "path", "boot"] {
            let mut engine = Engine::new();
            for second in 0..=15 {
                let mut telemetry = sample(second * 1000, Some(95.0));
                let identity = &mut telemetry.processes[1].identity;
                match invalid {
                    "uid" => identity.uid = 0,
                    "start" => {
                        identity.start_secs = 0;
                        identity.start_micros = 0;
                    }
                    "path" => identity.executable = "clang".into(),
                    "boot" => identity.boot_id = "old-boot".into(),
                    _ => unreachable!(),
                }
                let decision = engine.evaluate(&telemetry, &control());
                assert!(!decision.pressure, "invalid identity field {invalid}");
                assert!(decision.nice_targets.is_empty());
            }
        }
    }

    #[test]
    fn disabled_optimization_blocks_builtin_and_cooperative_fallbacks() {
        for role in [TaskRole::Unknown, TaskRole::Compute] {
            let config = Config {
                disabled_rules: vec!["path:/usr/bin/clang".into()],
                rules: vec![AppRule {
                    selector: "name:clang".into(),
                    policy: RulePolicy::AllowBackground,
                }],
                ..control()
            };
            let mut engine = Engine::new();
            for second in 0..=15 {
                let mut telemetry = sample(second * 1000, Some(95.0));
                telemetry.processes[1].role = role;
                assert!(engine.evaluate(&telemetry, &config).nice_targets.is_empty());
            }
        }
    }

    #[test]
    fn failure_selector_is_scoped_and_disabled_protection_still_wins() {
        let process = process(200, 1, "/usr/bin/clang");
        assert_eq!(
            failure_rule_selector(&process, &control()),
            "path:/usr/bin/clang"
        );
        let mut config = Config {
            rules: vec![AppRule {
                selector: "clang".into(),
                policy: RulePolicy::AllowBackground,
            }],
            ..control()
        };
        assert_eq!(failure_rule_selector(&process, &config), "clang");
        config.rules[0].policy = RulePolicy::Protect;
        config.disabled_rules.push("clang".into());
        let decision = hot(&mut Engine::new(), &config);
        assert!(decision.protected_pids.contains(&200));
        assert!(decision.nice_targets.is_empty());
    }
}
