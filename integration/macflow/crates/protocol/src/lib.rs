#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
pub const SCHEMA: u32 = 1;
pub const MAX_PAYLOAD: usize = 65536;
pub const AGENT_SERVICE: &str = "org.macflow.agent";
pub const HELPER_SERVICE: &str = "org.macflow.helper";
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Auto,
    Smooth,
    Performance,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    #[default]
    Unknown,
    Normal,
    Warning,
    Critical,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskRole {
    Foreground,
    Protected,
    Compute,
    #[default]
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub boot_id: String,
    pub pid: i32,
    pub start_secs: u64,
    pub start_micros: u64,
    pub uid: u32,
    pub executable: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSample {
    pub identity: ProcessIdentity,
    pub parent_pid: i32,
    pub name: String,
    pub bundle_id: Option<String>,
    pub nice: i32,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub role: TaskRole,
    pub protected_reason: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuTelemetry {
    pub driver_version: Option<String>,
    pub vram_used_bytes: Option<u64>,
    pub recovery_count: Option<u64>,
    pub utilization_percent: Option<f64>,
    pub temperature_c: Option<f64>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Telemetry {
    pub at_ms: u64,
    pub boot_id: String,
    pub cpu_percent: Option<f64>,
    pub memory_pressure: MemoryPressure,
    pub compressed_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    #[serde(default)]
    pub compressed_pages_delta: Option<u64>,
    #[serde(default)]
    pub decompressed_pages_delta: Option<u64>,
    #[serde(default)]
    pub swap_in_pages_delta: Option<u64>,
    #[serde(default)]
    pub swap_out_pages_delta: Option<u64>,
    pub thermal: Option<String>,
    pub ac_power: Option<bool>,
    pub gpu: GpuTelemetry,
    pub foreground_pid: Option<i32>,
    pub processes: Vec<ProcessSample>,
    pub errors: Vec<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RulePolicy {
    Protect,
    AllowBackground,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppRule {
    pub selector: String,
    pub policy: RulePolicy,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub mode: Mode,
    pub observe_only: bool,
    pub paused: bool,
    pub rules: Vec<AppRule>,
    pub disabled_rules: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            mode: Mode::Auto,
            observe_only: true,
            paused: false,
            rules: vec![],
            disabled_rules: vec![],
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub at_ms: u64,
    pub kind: String,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub name: String,
    pub available: bool,
    pub detail: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStats {
    pub pid: i32,
    pub source_app: String,
    pub samples: u64,
    pub bridge_calls: u64,
    pub bridge_wait_ns: u64,
    pub qos_failures: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub config: Config,
    pub pressure: bool,
    pub build_jobs: u32,
    pub admit_optional: bool,
    pub telemetry: Telemetry,
    pub helper_connected: bool,
    pub active_leases: usize,
    pub events: Vec<Event>,
    pub runtimes: Vec<RuntimeStats>,
    pub capabilities: Vec<Capability>,
    pub pending_verification: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NiceTarget {
    pub identity: ProcessIdentity,
    pub nice: i32,
    pub reason: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Decision {
    pub pressure: bool,
    pub build_jobs: u32,
    pub admit_optional: bool,
    pub fg_spin_us: u32,
    pub bg_spin_us: u32,
    pub nice_targets: Vec<NiceTarget>,
    pub protected_pids: Vec<i32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema: u32,
    pub id: u64,
    pub action: Command,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Status,
    SetMode {
        mode: Mode,
    },
    SetObservation {
        enabled: bool,
    },
    Pause,
    Resume,
    SetRule {
        selector: String,
        policy: RulePolicy,
    },
    Export,
    HelperQuery {
        identity: ProcessIdentity,
    },
    HelperApply {
        identity: ProcessIdentity,
        nice: i32,
        lease_id: String,
        deadline_ms: u64,
    },
    HelperRenew {
        lease_id: String,
        deadline_ms: u64,
    },
    HelperRevoke {
        lease_id: Option<String>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub schema: u32,
    pub id: u64,
    pub ok: bool,
    pub data: serde_json::Value,
    pub error: Option<String>,
}
impl Response {
    pub fn success(id: u64, data: serde_json::Value) -> Self {
        Self {
            schema: SCHEMA,
            id,
            ok: true,
            data,
            error: None,
        }
    }
    pub fn failure(id: u64, error: impl Into<String>) -> Self {
        Self {
            schema: SCHEMA,
            id,
            ok: false,
            data: serde_json::Value::Null,
            error: Some(error.into()),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHello {
    pub schema: u32,
    pub pid: i32,
    pub uid: u32,
    pub start_secs: u64,
    pub start_micros: u64,
    pub source_app: String,
    pub task_role: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePolicy {
    pub schema: u32,
    pub generation: u64,
    pub observe_only: bool,
    pub foreground: bool,
    pub spin_caller_us: u32,
    pub spin_bridge_us: u32,
    pub admit_optional: bool,
    pub build_jobs: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeMessage {
    Hello { hello: RuntimeHello },
    Stats { stats: RuntimeStats },
}
