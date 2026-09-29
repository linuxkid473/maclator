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
