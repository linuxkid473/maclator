//! macOS application bundles: locating the main executable and telling Chromium-family
//! bundles from native ones.
//!
//! Callers canonicalise the path first (`paths::redirect` / `resolve_path`), so symlinked
//! and trailing-slash forms behave the same as the physical path.

use std::path::{Path, PathBuf};

fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// True when `dir` looks like an application bundle: a directory with `Contents/Info.plist`.
pub fn is_bundle(dir: &Path) -> bool {
    dir.is_dir() && dir.join("Contents/Info.plist").is_file()
}

fn executables(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join("Contents/MacOS")) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| is_executable_file(p)).collect();
    out.sort();
    out
}

/// `CFBundleExecutable` from an XML `Info.plist`. Binary plists are deliberately not parsed.
fn plist_executable(root: &Path) -> Option<String> {
    const KEY: &str = "<key>CFBundleExecutable</key>";
    let text = std::fs::read_to_string(root.join("Contents/Info.plist")).ok()?;
    let after = &text[text.find(KEY)? + KEY.len()..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")? + start;
    let name = after[start..end].trim();
    if name.is_empty() { None } else { Some(name.to_string()) }
}

/// Resolve a bundle's main executable: the only executable in `Contents/MacOS`, otherwise
/// the one named by `CFBundleExecutable`, otherwise the one matching the bundle name.
/// Never guesses between several candidates.
pub fn resolve_executable(root: &Path) -> Result<PathBuf, String> {
    let exes = executables(root);
    if exes.len() == 1 {
        return Ok(exes[0].clone());
    }
    if let Some(name) = plist_executable(root) {
        let candidate = root.join("Contents/MacOS").join(&name);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    if let Some(stem) = root.file_stem().and_then(|s| s.to_str()) {
        let candidate = root.join("Contents/MacOS").join(stem);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    if exes.is_empty() {
        return Err(format!("{} has no executable in Contents/MacOS", root.display()));
    }
    let list: Vec<String> = exes.iter().map(|p| p.display().to_string()).collect();
    Err(format!(
        "{} has several executables in Contents/MacOS ({})",
        root.display(),
        list.join(", ")
    ))
}

/// Chromium family (Electron, Chrome, Edge, Brave, …): the bundle ships
/// `Contents/Frameworks/<X> Framework.framework`.
pub fn is_chromium_family(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join("Contents/Frameworks")) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name().to_string_lossy().ends_with(" Framework.framework") && e.path().is_dir()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const XML_PLIST: &str = "<plist><dict><key>CFBundleExecutable</key><string>Chosen</string></dict></plist>";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("maclator-bundle-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn app(&self, name: &str, plist: &[u8]) -> PathBuf {
            let app = self.0.join(format!("{name}.app"));
            fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            fs::write(app.join("Contents/Info.plist"), plist).unwrap();
            app
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn add_executable(app: &Path, name: &str) -> PathBuf {
        let p = app.join("Contents/MacOS").join(name);
        fs::write(&p, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn unique_executable_wins_without_reading_the_plist() {
        let s = Scratch::new("unique");
        let app = s.app("Solo", b"not a plist at all");
        let exe = add_executable(&app, "Solo");
        assert!(is_bundle(&app));
        assert_eq!(resolve_executable(&app).unwrap(), exe);
    }

    #[test]
    fn binary_plist_falls_back_to_the_unique_executable() {
        let s = Scratch::new("binary");
        let app = s.app("Bin", b"bplist00\x00\x01\x02\x03");
        let exe = add_executable(&app, "Bin");
        assert_eq!(resolve_executable(&app).unwrap(), exe);
    }

    #[test]
    fn xml_plist_chooses_between_several_executables() {
        let s = Scratch::new("xml");
        let app = s.app("Multi", XML_PLIST.as_bytes());
        add_executable(&app, "Other");
        let chosen = add_executable(&app, "Chosen");
        assert_eq!(resolve_executable(&app).unwrap(), chosen);
    }

    #[test]
    fn ambiguous_bundle_is_an_error() {
        let s = Scratch::new("ambiguous");
        let app = s.app("Mess", b"bplist00");
        add_executable(&app, "One");
        add_executable(&app, "Two");
        let err = resolve_executable(&app).unwrap_err();
        assert!(err.contains("several executables"), "{err}");
        assert!(err.contains("One") && err.contains("Two"), "{err}");
    }

    #[test]
    fn bundle_without_executables_is_an_error() {
        let s = Scratch::new("empty");
        let app = s.app("Empty", XML_PLIST.as_bytes());
        assert!(resolve_executable(&app).is_err());
    }

    #[test]
    fn chromium_family_needs_a_space_delimited_framework() {
        let s = Scratch::new("family");
        let app = s.app("Electronish", XML_PLIST.as_bytes());
        assert!(!is_chromium_family(&app), "no Frameworks directory yet");
        fs::create_dir_all(app.join("Contents/Frameworks/Squirrel.framework")).unwrap();
        assert!(!is_chromium_family(&app), "Squirrel.framework is not a Family");
        fs::create_dir_all(app.join("Contents/Frameworks/Foo.framework")).unwrap();
        assert!(!is_chromium_family(&app), "Foo.framework has no space");
        fs::create_dir_all(app.join("Contents/Frameworks/Electron Framework.framework")).unwrap();
        assert!(is_chromium_family(&app));
        // A plain file with the right name is not a framework either.
        let other = s.app("Fileish", XML_PLIST.as_bytes());
        fs::create_dir_all(other.join("Contents/Frameworks")).unwrap();
        fs::write(other.join("Contents/Frameworks/Edge Framework.framework"), b"x").unwrap();
        assert!(!is_chromium_family(&other));
    }
}
