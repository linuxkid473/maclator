//! Guest path redirection.
//!
//! On an Intel Mac the arm64 dyld shared cache is not installed; the user
//! supplies it (e.g. extracted from an Apple Silicon IPSW of the same macOS
//! version) and points Maclator at it with `--sysroot`. dyld's lookups in the
//! standard cache directories are redirected there.

use std::sync::Mutex;

static CACHE_DIR: Mutex<Option<String>> = Mutex::new(None);

const CACHE_PREFIXES: &[&str] = &[
    "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld",
    "/System/Cryptexes/OS/System/Library/dyld",
    "/System/Library/dyld",
];

pub fn set_cache_dir(d: &str) {
    *CACHE_DIR.lock().unwrap() = Some(d.trim_end_matches('/').to_string());
}

/// If `path` should be redirected, return the replacement.
pub fn redirect(path: &str) -> Option<String> {
    let dir = CACHE_DIR.lock().unwrap().clone()?;
    for p in CACHE_PREFIXES {
        if let Some(rest) = path.strip_prefix(p) {
            if rest.is_empty() || rest.starts_with('/') {
                return Some(format!("{dir}{rest}"));
            }
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
