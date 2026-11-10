//! Guest path redirection.
//!
//! On an Intel Mac the arm64 dyld shared cache is not installed; the user
//! supplies it (e.g. extracted from an Apple Silicon IPSW of the same macOS
//! version) and points Maclator at it with `--sysroot`. dyld's lookups in the
//! standard cache directories are redirected there.

use std::sync::Mutex;
use std::path::{Component, Path, PathBuf};

static CACHE_DIR: Mutex<Option<String>> = Mutex::new(None);
static OVERLAY_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

// Keep experimental Xcode components local to the emulated process. These
// prefixes contain the unmodified payloads of Xcode's prerequisite packages.
const OVERLAY_PREFIXES: &[&str] = &[
    "/Library/Developer/PrivateFrameworks",
    "/Library/Developer/DeviceKit",
    "/Library/Developer/CoreSimulator",
    "/Library/Developer/CoreDevice",
    "/Library/Apple",
    "/System/Library/CoreServices/CoreTypes.bundle",
];

const CACHE_PREFIXES: &[&str] = &[
    "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld",
    "/System/Cryptexes/OS/System/Library/dyld",
    "/System/Library/dyld",
];

pub fn set_cache_dir(d: &str) {
    *CACHE_DIR.lock().unwrap() = Some(d.trim_end_matches('/').to_string());
}

pub fn set_overlay_root(d: &str) -> std::io::Result<()> {
    let root = std::fs::canonicalize(d)?;
    if !root.is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "overlay root is not a directory"));
    }
    *OVERLAY_ROOT.lock().unwrap() = Some(root);
    Ok(())
}

pub fn overlay_root() -> Option<PathBuf> {
    OVERLAY_ROOT.lock().unwrap().clone()
}

fn normalized_absolute(path: &str) -> Option<PathBuf> {
    if !path.starts_with('/') {
        return None;
    }
    let mut result = PathBuf::from("/");
    for c in Path::new(path).components() {
        match c {
            Component::RootDir | Component::CurDir => {},
            Component::Normal(part) => result.push(part),
            Component::ParentDir => { if !result.pop() { return None; } },
            Component::Prefix(_) => return None,
        }
    }
    Some(result)
}

fn component_path(path: &Path) -> bool {
    OVERLAY_PREFIXES.iter().any(|p| path.starts_with(p))
}

fn overlay_path(root: &Path, path: &str) -> Option<PathBuf> {
    let normalized = normalized_absolute(path)?;
    if !component_path(&normalized) {
        return None;
    }
    let candidate = root.join(normalized.strip_prefix("/").ok()?);
    // Do not follow the final symlink here: readlink/stat must still be routed
    // when the original payload contains a symlink to another guest path.
    std::fs::symlink_metadata(&candidate).ok()?;
    Some(candidate)
}

/// Translate a physical overlay path returned by the host into its guest name.
pub fn guest_path(path: &str) -> Option<String> {
    let root = overlay_root()?;
    let relative = Path::new(path).strip_prefix(root).ok()?;
    let guest = Path::new("/").join(relative);
    component_path(&guest).then(|| guest.to_string_lossy().into_owned())
}

/// If `path` should be redirected, return the replacement.
pub fn redirect(path: &str) -> Option<String> {
    if let Some(dir) = CACHE_DIR.lock().unwrap().clone() {
        for p in CACHE_PREFIXES {
            if let Some(rest) = path.strip_prefix(p) {
                if rest.is_empty() || rest.starts_with('/') {
                    return Some(format!("{dir}{rest}"));
                }
            }
        }
    }
    if let Some(root) = overlay_root() {
        if let Some(candidate) = overlay_path(&root, path) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

pub fn cache_dir() -> Option<String> {
    CACHE_DIR.lock().unwrap().clone()
}

/// Find a directory holding the arm64 dyld shared cache when `--sysroot` was not given:
/// `$MACLATOR_SYSROOT`, then `~/.maclator/sysroot` (a directory, or a file containing one),
/// then any `~/maclator-sysroot/*` subdirectory that has a `dyld_shared_cache_arm64e`.
pub fn discover_sysroot() -> Option<String> {
    let has_cache = |d: &std::path::Path| d.join("dyld_shared_cache_arm64e").is_file();
    if let Ok(d) = std::env::var("MACLATOR_SYSROOT") {
        if has_cache(std::path::Path::new(&d)) {
            return Some(d);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let cfg = std::path::Path::new(&home).join(".maclator/sysroot");
    if cfg.is_dir() && has_cache(&cfg) {
        return Some(cfg.to_string_lossy().into_owned());
    }
    if let Ok(t) = std::fs::read_to_string(&cfg) {
        let p = std::path::PathBuf::from(t.trim());
        if has_cache(&p) {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    let root = std::path::Path::new(&home).join("maclator-sysroot");
    let mut dirs: Vec<_> = std::fs::read_dir(&root).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| has_cache(p)).collect();
    dirs.sort();
    dirs.pop().map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_routes_only_existing_component_paths() {
        let root = std::env::temp_dir().join(format!("maclator-overlay-test-{}", std::process::id()));
        let dir = root.join("Library/Developer/PrivateFrameworks/Example.framework");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Info.plist");
        std::fs::write(&file, b"original package data").unwrap();
        assert_eq!(overlay_path(&root, "/Library/Developer/PrivateFrameworks/Example.framework/Info.plist"), Some(file));
        assert!(overlay_path(&root, "/Library/Developer/PrivateFrameworks/Missing.framework").is_none());
        assert!(overlay_path(&root, "/Library/Developer/PrivateFrameworksOther/Info.plist").is_none());
        assert!(overlay_path(&root, "/Library/Developer/PrivateFrameworks/../../Apple").is_none());
        assert_eq!(overlay_path(&root, "/Library/Developer/PrivateFrameworks/Example.framework/../Example.framework/Info.plist"), Some(dir.join("Info.plist")));
        assert!(normalized_absolute("/../../Library").is_none());
        assert!(overlay_path(&root, "Library/Developer/PrivateFrameworks/Example.framework").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
