use macflow_protocol::Config;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub fn user_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
        .join("Library/Application Support/MacFlow")
}
pub fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    atomic_write(path, &bytes, 0o600)
}
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let parent = path.parent().ok_or("missing parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    if fs::symlink_metadata(parent)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("refusing symlink directory".into());
    }
    let temp = parent.join(format!(
        ".macflow-{}-{}.tmp",
        std::process::id(),
        macflow_platform::now_ms()
    ));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temp).map_err(|e| e.to_string())?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&temp, path).map_err(|e| e.to_string())?;
        fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !m.is_file() || m.len() > 8 * 1024 * 1024 {
        return Err("invalid or oversized JSON file".into());
    }
    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
pub fn load_config() -> Result<Config, String> {
    let path = user_dir().join("config.json");
    if !path.exists() {
        return Ok(Config::default());
    }
    let config: Config = read_json(&path)?;
    if config.schema != macflow_protocol::SCHEMA {
        return Err("configuration schema unsupported".into());
    }
    Ok(config)
}
pub fn save_config(config: &Config) -> Result<(), String> {
    atomic_json(&user_dir().join("config.json"), config)
}

/// Prune local diagnostic files, never configuration, trust or recovery data.
/// The 450 MiB history budget plus 40 MiB exports leaves headroom below 500 MiB.
pub fn prune_exports() -> Result<(), String> {
    prune_files(&user_dir().join("Exports"), 40 * 1024 * 1024)
}
fn prune_files(dir: &Path, budget: u64) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if metadata.is_file() {
            entries.push((entry.path(), metadata.len(), metadata.modified().ok()));
        }
    }
    entries.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
    let mut total = entries.iter().map(|e| e.1).sum::<u64>();
    for (path, bytes, _) in entries {
        if total <= budget {
            break;
        }
        fs::remove_file(path).map_err(|e| e.to_string())?;
        total = total.saturating_sub(bytes);
    }
    Ok(())
}

pub fn history_record(
    status: &macflow_protocol::Status,
    samples: &[macflow_protocol::Telemetry],
) -> Result<(), String> {
    let dir = user_dir().join("history");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = dir.join(format!("day-{}.jsonl", epoch / 86400));
    let mut value = serde_json::to_value(status).map_err(|e| e.to_string())?;
    let cpu: Vec<f64> = samples
        .iter()
        .filter_map(|s| s.cpu_percent)
        .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
        .collect();
    value["telemetry"]["cpu_percent"] = if cpu.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!(cpu.iter().sum::<f64>() / cpu.len() as f64)
    };
    value["recorded_epoch_seconds"] = serde_json::json!(epoch);
    value["aggregation_samples"] = serde_json::json!(samples.len());
    value["aggregation_window_seconds"] = serde_json::json!(30);
    // History is aggregated; process names/paths are confined to explicit exports.
    value["telemetry"]["processes"] = serde_json::json!([]);
    value["events"] = serde_json::json!([]);
    let mut bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| e.to_string())?;
    f.write_all(&bytes).map_err(|e| e.to_string())?;
    let mut files: Vec<_> = fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok().map(|m| (entry.path(), m.len())))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut total: u64 = files.iter().map(|x| x.1).sum();
    for (p, size) in files {
        let day = p
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("day-"))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if total > 450 * 1024 * 1024 || day.saturating_add(7) <= epoch / 86400 {
            fs::remove_file(p).map_err(|e| e.to_string())?;
            total = total.saturating_sub(size);
        }
    }
    Ok(())
}

/// A bounded tail read for the UI worker; never runs on AppKit's main thread.
pub fn recent_history_text() -> Result<String, String> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let dir = user_dir().join("history");
    if !dir.exists() {
        return Ok("聚合历史尚未生成。".into());
    }
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("day-") && n.ends_with(".jsonl"))
        })
        .map(|e| e.path())
        .collect();
    paths.sort();
    let Some(path) = paths.last() else {
        return Ok("聚合历史尚未生成。".into());
    };
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let start = len.saturating_sub(128 * 1024);
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut text = String::new();
    file.take(128 * 1024)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let mut result =
        String::from("最近聚合历史 · 每 30 秒\n时间 (Unix 秒)     CPU 平均      内存压力\n");
    for row in rows
        .iter()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let epoch = row["recorded_epoch_seconds"]
            .as_u64()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "旧版记录".into());
        let cpu = row["telemetry"]["cpu_percent"]
            .as_f64()
            .filter(|v| v.is_finite())
            .map(|v| format!("{v:.1}%"))
            .unwrap_or_else(|| "未知".into());
        result.push_str(&format!(
            "{epoch}    {cpu}    {}\n",
            row["telemetry"]["memory_pressure"]
                .as_str()
                .unwrap_or("未知")
        ));
    }
    result.push_str("七天历史保存在本机 history 目录；环形细粒度数据通过导出查看。\n");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "macflow-{label}-{}-{}",
            std::process::id(),
            macflow_platform::now_ms()
        ))
    }
    #[test]
    fn atomic_json_rejects_symlinks_and_bounds_input() {
        let root = test_dir("json");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.json");
        atomic_json(&path, &serde_json::json!({"value":7})).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let v: serde_json::Value = read_json(&path).unwrap();
        assert_eq!(v["value"], 7);
        std::os::unix::fs::symlink(&path, root.join("link.json")).unwrap();
        assert!(read_json::<serde_json::Value>(&root.join("link.json")).is_err());
        let oversized = root.join("large.json");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(8 * 1024 * 1024 + 1)
            .unwrap();
        assert!(read_json::<serde_json::Value>(&oversized).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn diagnostic_quota_keeps_recovery_outside_pruned_directory() {
        let root = test_dir("quota");
        let exports = root.join("Exports");
        fs::create_dir_all(&exports).unwrap();
        fs::write(root.join("leases.json"), b"recovery").unwrap();
        for n in 0..4 {
            fs::write(exports.join(format!("{n}.json")), [0u8; 100]).unwrap();
        }
        prune_files(&exports, 200).unwrap();
        let total: u64 = fs::read_dir(&exports)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert!(total <= 200);
        assert_eq!(fs::read(root.join("leases.json")).unwrap(), b"recovery");
        fs::remove_dir_all(root).unwrap();
    }
}
