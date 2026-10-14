//! Optional ARM-side application icon override. Native entry bundles and source
//! apps keep their own identities; this only sets AppKit's running-app image.
use std::path::PathBuf;

pub fn shim_path() -> Option<String> {
    if let Some(path) = std::env::var_os("MACLATOR_ICON_SHIM") {
        let path = PathBuf::from(path);
        return path.is_file().then(|| path.to_string_lossy().into_owned());
    }
    let path = std::env::current_exe().ok()?.parent()?.join("libmclappicon.dylib");
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

pub fn inject(icon: &str, envp: &mut Vec<String>) -> Result<(), String> {
    let path = std::fs::canonicalize(icon).map_err(|e| format!("icon {icon}: {e}"))?;
    if !path.is_file() {
        return Err(format!("icon is not a file: {}", path.display()));
    }
    let shim = shim_path().ok_or_else(|| "libmclappicon.dylib not found next to maclator".to_string())?;
    // Child exec/spawn removes this shim, so CLI tools do not load Foundation.
    envp.retain(|e| !e.starts_with("MACLATOR_APP_ICON="));
    envp.push(format!("MACLATOR_APP_ICON={}", path.to_string_lossy()));
    if let Some(e) = envp.iter_mut().find(|e| e.starts_with("DYLD_INSERT_LIBRARIES=")) {
        let value = e.strip_prefix("DYLD_INSERT_LIBRARIES=").unwrap();
        if !value.split(':').any(|entry| entry == shim) {
            e.push(':');
            e.push_str(&shim);
        }
    } else {
        envp.push(format!("DYLD_INSERT_LIBRARIES={shim}"));
    }
    Ok(())
}
