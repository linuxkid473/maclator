//! Reversible installation and service maintenance for the local signed build.
use crate::{helper::Trust, storage};
use macflow_protocol::*;
use std::{
    collections::BTreeMap,
    fs,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
pub const APP: &str = "/Applications/MacFlow.app";
const ROOT: &str = "/Library/Application Support/MacFlow";
const DAEMON: &str = "/Library/LaunchDaemons/org.macflow.helper.plist";
const CONTROL: &str = "/Library/Application Support/MacFlow/Runtime/macflow-control";
const HELPER: &str = "/Library/Application Support/MacFlow/Runtime/macflow-helper";
const ROLES: [&str; 4] = ["MacFlowUI", "macflow", "macflow-agent", "macflow-helper"];
fn command(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}
fn text(path: &Path) -> Result<&str, String> {
    let value = path.to_str().ok_or("path is not UTF-8")?;
    if value.chars().any(char::is_control) {
        Err("control characters in installation paths are unsupported".into())
    } else {
        Ok(value)
    }
}
fn metadata_directory(path: &Path) -> Result<(), String> {
    let m = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err(format!("insecure root directory: {}", path.display()));
    }
    Ok(())
}
fn validate_root_dir(path: &Path) -> Result<(), String> {
    for p in [
        Path::new("/Library"),
        Path::new("/Library/Application Support"),
        path,
    ] {
        metadata_directory(p)?;
    }
    Ok(())
}
fn root_file(path: &Path) -> Result<(), String> {
    let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !m.is_file() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err(format!("insecure root file: {}", path.display()));
    }
    Ok(())
}
fn ensure_directory(path: &Path) -> Result<(), String> {
    if !path.try_exists().map_err(|e| e.to_string())? {
        fs::DirBuilder::new()
            .mode(0o755)
            .create(path)
            .map_err(|e| e.to_string())?;
    }
    metadata_directory(path)
}
fn prepare_root() -> Result<PathBuf, String> {
    metadata_directory(Path::new("/Library"))?;
    metadata_directory(Path::new("/Library/Application Support"))?;
    let root = PathBuf::from(ROOT);
    ensure_directory(&root)?;
    ensure_directory(&root.join("Backups"))?;
    ensure_directory(&root.join("Runtime"))?;
    Ok(root)
}
fn maintenance_lock(root: &Path) -> Result<fs::File, String> {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("lifecycle.lock"))
        .map_err(|e| e.to_string())?;
    let m = lock.metadata().map_err(|e| e.to_string())?;
    if !m.is_file() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err("insecure lifecycle lock".into());
    }
    // The helper takes a nonblocking shared lock for apply/renew. Holding this
    // exclusive lock denies new leases but allows status, revoke and expiry.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another lifecycle operation is active".into());
    }
    Ok(lock)
}
fn require_empty_journal(root: &Path) -> Result<(), String> {
    let path = root.join("leases.json");
    if fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return Ok(());
    }
    let value: serde_json::Value = storage::read_json(&path)?;
    let leases = value["leases"]
        .as_object()
        .ok_or("invalid recovery journal; refusing to remove it")?;
    if !leases.is_empty() {
        return Err("active leases remain; wait for restoration".into());
    }
    Ok(())
}
fn service_loaded(target: &str) -> Result<bool, String> {
    let result = Command::new("/bin/launchctl")
        .args(["print", target])
        .output()
        .map_err(|e| e.to_string())?;
    if result.status.success() {
        return Ok(true);
    }
    let error = String::from_utf8_lossy(&result.stderr);
    if error.contains("Could not find service")
        || error.contains("Could not find domain")
        || error.contains("No such process")
        || error.contains("No such file or directory")
    {
        Ok(false)
    } else {
        Err(format!("cannot inspect service {target}: {}", error.trim()))
    }
}
fn stop_job(target: &str) -> Result<(), String> {
    if service_loaded(target)? {
        command("/bin/launchctl", &["bootout", target])?;
        if service_loaded(target)? {
            return Err(format!("service still loaded: {target}"));
        }
    }
    Ok(())
}
fn owner_uid(root: &Path, default: u32) -> Result<u32, String> {
    let path = root.join("trust.json");
    if !path.try_exists().map_err(|e| e.to_string())? {
        return Ok(default);
    }
    root_file(&path)?;
    let trust: Trust = storage::read_json(&path)?;
    if trust.schema != SCHEMA || trust.uid == 0 {
        return Err("invalid existing installation owner".into());
    }
    Ok(trust.uid)
}
fn user_command(uid: u32, arg: &str) -> Result<(), String> {
    let user = format!("#{uid}");
    let id = uid.to_string();
    // launchctl switches bootstrap namespace; sudo explicitly switches UID,
    // groups and HOME before executing any user-owned application file.
    command(
        "/bin/launchctl",
        &[
            "asuser",
            &id,
            "/usr/bin/sudo",
            "-H",
            "-u",
            &user,
            "--",
            &format!("{APP}/Contents/MacOS/macflow"),
            arg,
        ],
    )
}
fn quiesce(uid: u32, root: &Path) -> Result<(), String> {
    let target = format!("gui/{uid}/org.macflow.agent");
    if service_loaded(&target)? {
        if let Err(error) = user_command(uid, "pause") {
            eprintln!(
                "MacFlow controller pause unavailable: {error}; stopping controller and awaiting lease expiry"
            );
        }
        // Stop only the controller now. Older CLIs unregister both services,
        // so their cleanup command must wait until helper restoration finishes.
        stop_job(&target)?;
    }
    let deadline = Instant::now() + Duration::from_secs(13);
    loop {
        match require_empty_journal(root) {
            Ok(()) => {
                if Path::new(APP).join("Contents/MacOS/macflow").is_file() {
                    if let Err(error) = user_command(uid, "unregister-services") {
                        eprintln!(
                            "MacFlow registration cleanup after journal restoration: {error}"
                        );
                    }
                }
                return Ok(());
            }
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(format!("refusing maintenance: {error}"));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
fn bundle_requirements(bundle: &Path) -> Result<BTreeMap<String, String>, String> {
    command(
        "/usr/bin/codesign",
        &["--verify", "--deep", "--strict", text(bundle)?],
    )?;
    let mut result = BTreeMap::new();
    for role in ROLES {
        let executable = bundle.join("Contents/MacOS").join(role);
        if !fs::symlink_metadata(&executable)
            .map_err(|e| e.to_string())?
            .is_file()
        {
            return Err("bundle executable is not a regular file".into());
        }
        command(
            "/usr/bin/codesign",
            &["--verify", "--strict", text(&executable)?],
        )?;
        result.insert(
            role.into(),
            macflow_platform::code_requirement_for_file(&executable)?,
        );
    }
    Ok(result)
}
fn write_marker(
    root: &Path,
    uid: u32,
    requirements: &BTreeMap<String, String>,
    state: &str,
) -> Result<(), String> {
    let marker = serde_json::json!({"schema":SCHEMA,"uid":uid,"state":state,"service_backend":"launchd_local","binary_requirements":requirements,"reason":"Explicit local ad-hoc compatibility backend; Developer ID notarization not available"});
    // Non-secret status must be readable by register_or_fallback. Exact helper
    // trust remains a separate root-only file.
    storage::atomic_write(
        &root.join("installation.json"),
        &serde_json::to_vec_pretty(&marker).map_err(|e| e.to_string())?,
        0o644,
    )
}
#[derive(Clone)]
struct SavedFile {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    mode: u32,
}
impl SavedFile {
    fn capture(path: PathBuf, mode: u32) -> Result<Self, String> {
        let bytes = match fs::symlink_metadata(&path) {
            Ok(m) => {
                if !m.is_file()
                    || m.uid() != 0
                    || m.mode() & 0o022 != 0
                    || m.len() > 32 * 1024 * 1024
                {
                    return Err(format!("unsafe maintenance file: {}", path.display()));
                }
                Some(fs::read(&path).map_err(|e| e.to_string())?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        Ok(Self { path, bytes, mode })
    }
    fn restore(&self) -> Result<(), String> {
        match &self.bytes {
            Some(bytes) => storage::atomic_write(&self.path, bytes, self.mode),
            None => {
                if self.path.try_exists().map_err(|e| e.to_string())? {
                    fs::remove_file(&self.path).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
        }
    }
}
/// A failed publication restores the old bundle, while completed backups remain
/// recoverable. The old directory is not deleted until explicitly uninstalled.
struct BundleSwap {
    destination: PathBuf,
    backup: Option<PathBuf>,
    published: bool,
}
impl BundleSwap {
    fn publish(
        staging: &Path,
        destination: &Path,
        backup: Option<PathBuf>,
    ) -> Result<Self, String> {
        if let Some(ref old) = backup {
            fs::rename(destination, old).map_err(|e| e.to_string())?;
        }
        if let Err(error) = fs::rename(staging, destination) {
            if let Some(ref old) = backup {
                fs::rename(old, destination).map_err(|restore| {
                    format!("publish failed: {error}; restore failed: {restore}")
                })?;
            }
            return Err(error.to_string());
        }
        Ok(Self {
            destination: destination.into(),
            backup,
            published: true,
        })
    }
    fn restore(&mut self) -> Result<(), String> {
        if self.published {
            fs::remove_dir_all(&self.destination).map_err(|e| e.to_string())?;
            if let Some(ref old) = self.backup {
                fs::rename(old, &self.destination).map_err(|e| e.to_string())?;
            }
            self.published = false;
        }
        Ok(())
    }
}
fn daemon_xml() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>org.macflow.helper</string><key>ProgramArguments</key><array><string>{HELPER}</string></array><key>MachServices</key><dict><key>org.macflow.helper</key><true/></dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>10</integer></dict></plist>"
    )
}
fn install_runtime(staging: &Path, requirements: &BTreeMap<String, String>) -> Result<(), String> {
    for (role, destination) in [("macflow-helper", HELPER), ("macflow", CONTROL)] {
        let source = staging.join("Contents/MacOS").join(role);
        let bytes = fs::read(&source).map_err(|e| e.to_string())?;
        storage::atomic_write(Path::new(destination), &bytes, 0o755)?;
        command("/usr/bin/codesign", &["--verify", "--strict", destination])?;
        if macflow_platform::code_requirement_for_file(Path::new(destination))?
            != requirements[role]
        {
            return Err("protected runtime signature changed".into());
        }
    }
    Ok(())
}
pub fn install_root(source: &Path, uid: u32) -> Result<(), String> {
    if macflow_platform::current_uid() != 0 || uid == 0 {
        return Err(
            "install-root requires administrator privileges and a non-root target UID".into(),
        );
    }
    let source = source.canonicalize().map_err(|e| e.to_string())?;
    if source.file_name().and_then(|s| s.to_str()) != Some("MacFlow.app") {
        return Err("expected MacFlow.app bundle".into());
    }
    let requirements = bundle_requirements(&source)?;
    let root = prepare_root()?;
    let _lock = maintenance_lock(&root)?;
    let existing = Path::new(APP).try_exists().map_err(|e| e.to_string())?;
    if existing {
        metadata_directory(Path::new(APP))?;
    }
    let old_uid = owner_uid(&root, uid)?;
    let helper_was_loaded = service_loaded("system/org.macflow.helper")?;
    quiesce(old_uid, &root)?;
    if uid != old_uid {
        quiesce(uid, &root)?;
    }
    // The lock freezes helper apply/renew, so the journal cannot acquire new
    // records after this check while services are being replaced.
    require_empty_journal(&root)?;
    let saved = [
        ("trust.json", 0o600),
        ("installation.json", 0o644),
        ("previous.json", 0o600),
    ]
    .into_iter()
    .map(|(n, m)| SavedFile::capture(root.join(n), m))
    .chain(
        [
            (PathBuf::from(HELPER), 0o755),
            (PathBuf::from(CONTROL), 0o755),
            (PathBuf::from(DAEMON), 0o644),
        ]
        .into_iter()
        .map(|(p, m)| SavedFile::capture(p, m)),
    )
    .collect::<Result<Vec<_>, _>>()?;
    let staging_parent = root.join(format!(
        ".stage-{}-{}",
        std::process::id(),
        macflow_platform::now_ms()
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging_parent)
        .map_err(|e| e.to_string())?;
    let staging = staging_parent.join("MacFlow.app");
    let mut swap = None;
    let result = (|| -> Result<(), String> {
        command("/usr/bin/ditto", &[text(&source)?, text(&staging)?])?;
        command("/usr/sbin/chown", &["-R", "root:wheel", text(&staging)?])?;
        command("/bin/chmod", &["-R", "go-w", text(&staging)?])?;
        if bundle_requirements(&staging)? != requirements {
            return Err("bundle changed during installation".into());
        }
        stop_job("system/org.macflow.helper")?;
        require_empty_journal(&root)?;
        install_runtime(&staging, &requirements)?;
        let backup = if existing {
            let dir = root.join("Backups").join(format!(
                "{}-{}",
                macflow_platform::now_ms(),
                std::process::id()
            ));
            fs::DirBuilder::new()
                .mode(0o755)
                .create(&dir)
                .map_err(|e| e.to_string())?;
            Some(dir.join("MacFlow.app"))
        } else {
            None
        };
        swap = Some(BundleSwap::publish(&staging, Path::new(APP), backup)?);
        if let Some(path) = swap.as_ref().and_then(|s| s.backup.as_ref()) {
            storage::atomic_json(
                &root.join("previous.json"),
                &serde_json::json!({"path":path,"uid":old_uid}),
            )?;
        }
        storage::atomic_json(
            &root.join("trust.json"),
            &Trust {
                schema: SCHEMA,
                uid,
                requirements: vec![requirements["macflow-agent"].clone()],
            },
        )?;
        write_marker(&root, uid, &requirements, "installed_paused")?;
        storage::atomic_write(Path::new(DAEMON), daemon_xml().as_bytes(), 0o644)?;
        command("/bin/launchctl", &["bootstrap", "system", DAEMON])?;
        Ok(())
    })();
    if let Err(error) = result {
        let mut recovery_errors = Vec::new();
        if let Err(e) = stop_job("system/org.macflow.helper") {
            recovery_errors.push(e);
        }
        if let Some(ref mut published) = swap {
            if let Err(e) = published.restore() {
                recovery_errors.push(e);
            }
        }
        for file in &saved {
            if let Err(e) = file.restore() {
                recovery_errors.push(e);
            }
        }
        if helper_was_loaded && Path::new(DAEMON).is_file() {
            if let Err(e) = command("/bin/launchctl", &["bootstrap", "system", DAEMON]) {
                recovery_errors.push(e);
            }
        }
        let _ = fs::remove_dir_all(&staging_parent);
        return Err(format!(
            "installation failed: {error}; old software restored where possible; controller remains paused{}",
            if recovery_errors.is_empty() {
                String::new()
            } else {
                format!("; recovery errors: {}", recovery_errors.join("; "))
            }
        ));
    }
    fs::remove_dir_all(&staging_parent).map_err(|e| e.to_string())?;
    // The unprivileged caller registers the installed agent after this root
    // transaction returns. It stays paused until compatibility is reviewed.
    Ok(())
}
pub fn remove_root() -> Result<(), String> {
    if macflow_platform::current_uid() != 0 {
        return Err("uninstall-root requires administrator privileges".into());
    }
    let root = PathBuf::from(ROOT);
    validate_root_dir(&root)?;
    let _lock = maintenance_lock(&root)?;
    let uid = owner_uid(&root, 0)?;
    if uid != 0 {
        quiesce(uid, &root)?;
    }
    require_empty_journal(&root)?;
    stop_job("system/org.macflow.helper")?;
    require_empty_journal(&root)?;
    if Path::new(APP).try_exists().map_err(|e| e.to_string())? {
        metadata_directory(Path::new(APP))?;
        fs::remove_dir_all(APP).map_err(|e| e.to_string())?;
    }
    for name in ["trust.json", "leases.json", "installation.json"] {
        let path = root.join(name);
        if path.try_exists().map_err(|e| e.to_string())? {
            root_file(&path)?;
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
    }
    for path in [Path::new(DAEMON), Path::new(HELPER), Path::new(CONTROL)] {
        if path.try_exists().map_err(|e| e.to_string())? {
            root_file(path)?;
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
fn backup_path(backups: &Path, path: &Path) -> Result<PathBuf, String> {
    let root = backups.canonicalize().map_err(|e| e.to_string())?;
    let resolved = path.canonicalize().map_err(|e| e.to_string())?;
    if !resolved.starts_with(&root)
        || resolved == root
        || resolved.file_name().and_then(|s| s.to_str()) != Some("MacFlow.app")
    {
        return Err("backup escapes protected backup directory".into());
    }
    Ok(resolved)
}
pub fn rollback_root() -> Result<(), String> {
    if macflow_platform::current_uid() != 0 {
        return Err("rollback-root requires administrator privileges".into());
    }
    let root = PathBuf::from(ROOT);
    validate_root_dir(&root)?;
    root_file(&root.join("previous.json"))?;
    let record: serde_json::Value = storage::read_json(&root.join("previous.json"))?;
    let path = backup_path(
        &root.join("Backups"),
        Path::new(record["path"].as_str().ok_or("no previous bundle")?),
    )?;
    for ancestor in path.ancestors() {
        if ancestor == root {
            break;
        }
        metadata_directory(ancestor)?;
    }
    let uid = record["uid"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n != 0)
        .ok_or("invalid backup UID")?;
    install_root(&path, uid)
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn authorized_command(line: &str) -> Result<(), String> {
    let escaped = line.replace('\\', "\\\\").replace('"', "\\\"");
    command(
        "/usr/bin/osascript",
        &[
            "-e",
            &format!("do shell script \"{escaped}\" with administrator privileges"),
        ],
    )
}
fn protected_control() -> Result<Option<PathBuf>, String> {
    let path = PathBuf::from(CONTROL);
    if !path.try_exists().map_err(|e| e.to_string())? {
        return Ok(None);
    }
    validate_root_dir(Path::new(ROOT))?;
    metadata_directory(Path::new(ROOT).join("Runtime").as_path())?;
    root_file(&path)?;
    command("/usr/bin/codesign", &["--verify", "--strict", text(&path)?])?;
    Ok(Some(path))
}
pub fn request_admin_install(source: &Path) -> Result<(), String> {
    // Prefer the protected existing maintenance executable. The chosen source
    // bundle is copied and verified again under root before publication.
    let executable =
        protected_control()?.unwrap_or(std::env::current_exe().map_err(|e| e.to_string())?);
    authorized_command(&format!(
        "{} install-root --app {} --uid {}",
        shell_quote(text(&executable)?),
        shell_quote(text(source)?),
        macflow_platform::current_uid()
    ))
}
pub fn admin_operation(operation: &str) -> Result<(), String> {
    if !["uninstall-root", "rollback-root"].contains(&operation) {
        return Err("unsupported administrator operation".into());
    }
    let executable=protected_control()?.ok_or("protected maintenance executable missing; reinstall the current package before maintenance")?;
    authorized_command(&format!("{} {operation}", shell_quote(text(&executable)?)))
}
pub fn register_or_fallback() -> Result<String, String> {
    let marker = Path::new(ROOT).join("installation.json");
    if marker.try_exists().map_err(|e| e.to_string())? {
        validate_root_dir(Path::new(ROOT))?;
        root_file(&marker)?;
        let value: serde_json::Value = storage::read_json(&marker)?;
        if value["schema"] != SCHEMA
            || value["uid"].as_u64() != Some(macflow_platform::current_uid() as u64)
        {
            return Err("installation marker version or target UID mismatch".into());
        }
        if value["service_backend"] == "launchd_local" {
            start_user_agent()?;
            return Ok(
                "已注册本机 ad-hoc 构建的 launchd 兼容服务；管理员助手使用受保护的 Runtime 副本"
                    .into(),
            );
        }
    }
    macflow_platform::register_services()
}
pub fn start_user_agent() -> Result<(), String> {
    if macflow_platform::current_uid() == 0 {
        return Err("user agent registration requires the target user's session".into());
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME unavailable")?;
    let dir = home.join("Library/LaunchAgents");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if fs::symlink_metadata(&dir)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("user LaunchAgents directory is a symlink".into());
    }
    let path = dir.join("org.macflow.agent.plist");
    let domain = format!("gui/{}", macflow_platform::current_uid());
    let target = format!("{domain}/org.macflow.agent");
    // Re-registration first pauses the existing controller; maintenance must
    // never restart an active controller before its leases have been revoked.
    if service_loaded(&target)? {
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        command(text(&executable)?, &["pause"])?;
        std::thread::sleep(Duration::from_secs(11));
        stop_job(&target)?;
    }
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>org.macflow.agent</string><key>ProgramArguments</key><array><string>{APP}/Contents/MacOS/macflow-agent</string></array><key>MachServices</key><dict><key>org.macflow.agent</key><true/></dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>10</integer></dict></plist>"
    );
    storage::atomic_write(&path, xml.as_bytes(), 0o644)?;
    command("/bin/launchctl", &["bootstrap", &domain, text(&path)?])
}
pub fn stop_user_agent() -> Result<(), String> {
    if macflow_platform::current_uid() == 0 {
        return Err("user agent removal requires the target user's session".into());
    }
    stop_job(&format!(
        "gui/{}/org.macflow.agent",
        macflow_platform::current_uid()
    ))?;
    let path = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME unavailable")?
        .join("Library/LaunchAgents/org.macflow.agent.plist");
    if path.try_exists().map_err(|e| e.to_string())? {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "macflow-lifecycle-{}-{}-{}",
            std::process::id(),
            label,
            macflow_platform::now_ms()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn publication_failure_restores_previous_bundle() {
        let root = fixture("publish");
        let destination = root.join("MacFlow.app");
        let backup = root.join("old.app");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("marker"), b"previous").unwrap();
        assert!(
            BundleSwap::publish(&root.join("missing"), &destination, Some(backup.clone())).is_err()
        );
        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"previous");
        assert!(!backup.exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn successful_swap_can_be_compensated() {
        let root = fixture("compensate");
        let destination = root.join("MacFlow.app");
        let staging = root.join("new.app");
        let backup = root.join("old.app");
        for (path, bytes) in [
            (&destination, b"old".as_slice()),
            (&staging, b"new".as_slice()),
        ] {
            fs::create_dir(path).unwrap();
            fs::write(path.join("marker"), bytes).unwrap();
        }
        let mut transaction = BundleSwap::publish(&staging, &destination, Some(backup)).unwrap();
        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"new");
        transaction.restore().unwrap();
        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"old");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rollback_rejects_parent_escape_and_symlink() {
        let root = fixture("backups");
        let backups = root.join("Backups");
        let outside = root.join("outside/MacFlow.app");
        fs::create_dir(&backups).unwrap();
        fs::create_dir_all(&outside).unwrap();
        assert!(backup_path(&backups, &backups.join("../outside/MacFlow.app")).is_err());
        std::os::unix::fs::symlink(&outside, backups.join("linked")).unwrap();
        assert!(backup_path(&backups, &backups.join("linked")).is_err());
        let valid = backups.join("1/MacFlow.app");
        fs::create_dir_all(&valid).unwrap();
        assert_eq!(
            backup_path(&backups, &valid).unwrap(),
            valid.canonicalize().unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn maintenance_refuses_active_or_malformed_journal() {
        let root = fixture("journal");
        assert!(require_empty_journal(&root).is_ok());
        storage::atomic_json(
            &root.join("leases.json"),
            &serde_json::json!({"leases":{"one":{"original":0}}}),
        )
        .unwrap();
        assert!(require_empty_journal(&root).is_err());
        storage::atomic_json(
            &root.join("leases.json"),
            &serde_json::json!({"leases":null}),
        )
        .unwrap();
        assert!(require_empty_journal(&root).is_err());
        storage::atomic_json(&root.join("leases.json"), &serde_json::json!({"leases":{}})).unwrap();
        assert!(require_empty_journal(&root).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn public_marker_is_readable_without_exposing_root_trust() {
        let root = fixture("marker");
        write_marker(&root, 501, &BTreeMap::new(), "installed_paused").unwrap();
        assert_eq!(
            fs::metadata(root.join("installation.json")).unwrap().mode() & 0o777,
            0o644
        );
        assert!(!root.join("trust.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn privileged_helper_path_is_outside_group_writable_applications() {
        let xml = daemon_xml();
        assert!(xml.contains(HELPER));
        assert!(!xml.contains(&format!("{APP}/Contents/MacOS/macflow-helper")));
        assert!(HELPER.starts_with(ROOT) && CONTROL.starts_with(ROOT));
    }
    #[test]
    fn shell_arguments_preserve_literal_quotes_and_substitutions() {
        assert_eq!(
            shell_quote("path'$(printf unsafe)"),
            "'path'\\''$(printf unsafe)'"
        );
        assert!(text(Path::new("bad\npath")).is_err());
    }
}
