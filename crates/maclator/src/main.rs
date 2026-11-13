//! Maclator: run arm64 macOS programs on x86-64 Macs.

mod aot;
mod bundle;
mod cache;
mod commpage;
mod engine;
mod gpu;
mod guestmem;
mod hostsys;
mod iokit;
mod jit;
mod loader;
mod macho;
mod paths;
mod signals;
mod slices;
mod spawn;
mod symbols;
mod syscalls;
mod threads;
mod workq;

use std::sync::atomic::Ordering;

fn usage() -> ! {
    eprintln!("usage: maclator [options] <arm64-program> [args...]");
    eprintln!("       maclator --aot <arm64-program>     translate ahead of time and exit");
    eprintln!("options:");
    eprintln!("  --interp        interpreter only (no JIT/AOT)");
    eprintln!("  --no-aot        JIT only: don't load or build AOT translations");
    eprintln!("  --dyld PATH     arm64 dyld to use (default /usr/lib/dyld)");
    eprintln!("  --sysroot DIR   directory with the arm64 dyld shared cache files");
    eprintln!("  --overlay-root DIR  local Xcode component payloads (experimental)");
    eprintln!("  --trace         trace system calls");
    eprintln!("  --gpu           route the guest's Metal API to the host GPU");
    eprintln!("  --gpu-only SUBSTR  give the GPU only to guest processes whose arguments contain SUBSTR");
    eprintln!("  --gpu-process  same as --gpu --gpu-only --type=gpu-process");
    eprintln!("  --no-gpu        never load the GPU bridge (overrides automatic detection)");
    eprintln!("  --print-plan    print the resolved launch plan to stderr, then run");
    eprintln!("  --dry-run       print the resolved launch plan to stderr and exit");
    std::process::exit(2);
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut dyld = std::env::var("MACLATOR_DYLD").unwrap_or_else(|_| "/usr/lib/dyld".to_string());
    let mut no_aot = false;
    let mut gpu_enabled: Option<bool> = None;
    let mut gpu_scope: Option<String> = None;
    let mut gpu_flag = false;
    let mut print_plan = false;
    let mut dry_run = false;
    while let Some(a) = args.first().cloned() {
        match a.as_str() {
            "--trace-file" => {
                // Trace to /tmp/maclator-trace-<pid>.log (helper processes have no usable stderr).
                syscalls::TRACE.store(true, Ordering::Relaxed);
                std::env::set_var("MACLATOR_TRACE_FILE", "1");
                args.remove(0);
            }
            "--dump-on-fault" => {
                std::env::set_var("MACLATOR_DUMP_ON_FAULT", "1");
                args.remove(0);
            }
            "--trace" => {
                syscalls::TRACE.store(true, Ordering::Relaxed);
                args.remove(0);
            }
            "--dyld" => {
                args.remove(0);
                if args.is_empty() {
                    usage();
                }
                dyld = args.remove(0);
            }
            "--gpu" => {
                // Route the guest's Metal API to the host GPU (see gpu.rs). Unchanged semantics:
                // every guest process gets the bridge unless --gpu-only narrows it.
                gpu_enabled = Some(true);
                gpu_flag = true;
                args.remove(0);
            }
            "--gpu-process" => {
                // Convenience for the common case: the bridge for the GPU process only.
                gpu_enabled = Some(true);
                gpu_scope = Some("--type=gpu-process".to_string());
                gpu_flag = true;
                args.remove(0);
            }
            "--no-gpu" => {
                // Explicitly off, including against automatic detection and any inherited
                // MACLATOR_GPU in the environment.
                gpu_enabled = Some(false);
                gpu_flag = true;
                args.remove(0);
            }
            "--print-plan" => {
                print_plan = true;
                args.remove(0);
            }
            "--dry-run" => {
                dry_run = true;
                args.remove(0);
            }
            "--gpu-only" => {
                // Only guest processes whose arguments contain SUBSTR get the GPU (e.g. --type=gpu-process);
                // others run untouched (saves the bridge start-up cost in every helper process).
                // As before, this narrows the bridge but does not enable it on its own.
                args.remove(0);
                if args.is_empty() {
                    usage();
                }
                gpu_scope = Some(args.remove(0));
                gpu_flag = true;
            }
            "--interp" => {
                jit::JIT_ENABLED.store(false, Ordering::Relaxed);
                aot::set_enabled(false);
                args.remove(0);
            }
            "--no-aot" => {
                aot::set_enabled(false);
                no_aot = true;
                args.remove(0);
            }
            "--sysroot" => {
                args.remove(0);
                if args.is_empty() {
                    usage();
                }
                paths::set_cache_dir(&args.remove(0));
            }
            "--overlay-root" => {
                args.remove(0);
                if args.is_empty() {
                    usage();
                }
                if let Err(e) = paths::set_overlay_root(&args.remove(0)) {
                    eprintln!("maclator: overlay: {e}");
                    std::process::exit(1);
                }
            }
            "--aot" => {
                args.remove(0);
                if args.is_empty() {
                    usage();
                }
                let exe = match resolve_target(&args[0]) {
                    Ok(target) => target.executable().to_string(),
                    Err(e) => {
                        eprintln!("maclator: {e}");
                        std::process::exit(1);
                    }
                };
                match aot::build_standalone(&exe) {
                    Ok(n) => {
                        eprintln!("maclator: translated {n} blocks of {exe} ahead of time");
                        std::process::exit(0);
                    }
                    Err(e) => {
                        eprintln!("maclator: aot: {e}");
                        std::process::exit(1);
                    }
                }
            }
            "--jit-dump" => {
                aot::set_enabled(false);
                let words: Vec<u32> = args[1..].iter().filter_map(|w| u32::from_str_radix(w.trim_start_matches("0x"), 16).ok()).collect();
                jit::selftest::dump_block(&words);
                std::process::exit(0);
            }
            "--selftest-jit" => {
                let class = args.get(1).cloned().unwrap_or_else(|| "dpreg".into());
                let n = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(20000);
                aot::set_enabled(false);
                jit::selftest::run_selftest(&class, n);
                std::process::exit(0);
            }
            "-h" | "--help" => usage(),
            _ => break,
        }
    }
    if args.is_empty() {
        usage();
    }
    if paths::cache_dir().is_none() && !std::path::Path::new("/System/Library/dyld/dyld_shared_cache_arm64e").exists() {
        if let Some(d) = paths::discover_sysroot() {
            paths::set_cache_dir(&d);
        }
    }
    if std::env::var_os("MACLATOR_TRACE").is_some() {
        syscalls::TRACE.store(true, Ordering::Relaxed);
    }
    let target = match resolve_target(&args[0]) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("maclator: {e}");
            std::process::exit(1);
        }
    };
    let exe = target.executable().to_string();

    // Explicit flags win over anything inherited through the environment (see design D5); the
    // environment only fills in what the command line left alone.
    let mut source = if gpu_flag { "explicit" } else { "auto" };
    if gpu_enabled.is_none() && std::env::var_os("MACLATOR_GPU").is_some() {
        gpu_enabled = Some(true);
        source = "explicit";
    }
    if gpu_scope.is_none() {
        if let Ok(scope) = std::env::var("MACLATOR_GPU_ONLY") {
            if !scope.is_empty() {
                gpu_scope = Some(scope);
            }
        }
    }

    let shim = gpu::shim_path();
    let mut reason = "explicit";
    if gpu_enabled.is_none() {
        // Automatic detection only applies to the new form (a bundle path); the historical
        // "path to the executable inside the bundle" keeps behaving exactly as before.
        match &target {
            Target::Executable(_) => reason = "not-in-bundle",
            Target::Bundle { root, .. } => {
                if !slices::arm64_only(&exe) {
                    reason = "not-arm64";
                } else if !bundle::is_chromium_family(root) {
                    reason = "not-chromium";
                } else if shim.is_none() {
                    reason = "bridge-missing";
                    eprintln!("maclator: chromium-family app, but the GPU bridge library was not found; continuing without it");
                } else {
                    gpu_enabled = Some(true);
                    if gpu_scope.is_none() {
                        gpu_scope = Some("--type=gpu-process".to_string());
                    }
                    source = "auto";
                    reason = "chromium-family";
                    eprintln!("maclator: chromium-family app; enabling Metal bridge (--no-gpu to disable)");
                }
            }
        }
    }

    // Normalise the environment so it describes exactly one state: children read it through
    // spawn's forwarding instead, but keeping it truthful avoids surprises for other readers.
    match gpu_enabled {
        Some(true) => {
            std::env::set_var("MACLATOR_GPU", "1");
            match &gpu_scope {
                Some(scope) => std::env::set_var("MACLATOR_GPU_ONLY", scope),
                None => {
                    std::env::remove_var("MACLATOR_GPU_ONLY");
                }
            }
        }
        Some(false) | None => {
            std::env::remove_var("MACLATOR_GPU");
            std::env::remove_var("MACLATOR_GPU_ONLY");
        }
    }
    let gpu_active = gpu_enabled == Some(true)
        && gpu_scope
            .as_ref()
            .map(|scope| args.iter().skip(1).any(|a| a.contains(scope.as_str())))
            .unwrap_or(true);

    {
        // Options that re-launched arm64 children must inherit. Built after the decision so the
        // decision itself reaches them (design D9).
        let mut fwd: Vec<String> = vec!["--dyld".into(), dyld.clone()];
        if let Some(d) = paths::cache_dir() {
            fwd.push("--sysroot".into());
            fwd.push(d);
        }
        if let Some(root) = paths::overlay_root() {
            fwd.push("--overlay-root".into());
            fwd.push(root.to_string_lossy().into_owned());
        }
        if !jit::JIT_ENABLED.load(Ordering::Relaxed) {
            fwd.push("--interp".into());
        }
        if no_aot {
            fwd.push("--no-aot".into());
        }
        match gpu_enabled {
            Some(true) => {
                fwd.push("--gpu".into());
                if let Some(scope) = &gpu_scope {
                    fwd.push("--gpu-only".into());
                    fwd.push(scope.clone());
                }
            }
            Some(false) => fwd.push("--no-gpu".into()),
            None => {}
        }
        if std::env::var_os("MACLATOR_TRACE_FILE").is_some() {
            fwd.push("--trace-file".into());
        } else if syscalls::TRACE.load(Ordering::Relaxed) {
            fwd.push("--trace".into());
        }
        if std::env::var_os("MACLATOR_DUMP_ON_FAULT").is_some() {
            fwd.push("--dump-on-fault".into());
        }
        spawn::configure(fwd);
    }

    if print_plan || dry_run {
        emit_plan(&Plan {
            target: &exe,
            gpu: gpu_label(gpu_enabled, gpu_scope.as_deref()),
            source,
            reason,
            shim: shim.as_deref(),
            argv: &args[1..],
        });
        if dry_run {
            std::process::exit(0);
        }
    }

    let mut envp: Vec<String> = std::env::vars()
        .filter(|(k, _)| !k.starts_with("MACLATOR_") || matches!(k.as_str(), "MACLATOR_DUMP_ON_FAULT" | "MACLATOR_GPU_BRIDGE" | "MACLATOR_JIT_THRESHOLD" | "MACLATOR_STATS" | "MACLATOR_FALLBACK_HIST"))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    if gpu_active && !gpu::inject(&mut envp) {
        eprintln!("maclator: GPU bridge: libmclmetal.dylib not found (looked next to maclator, in ./gpu, ~/.maclator/gpu, $MACLATOR_GPU_DIR)");
    }
    if std::env::var_os("MACLATOR_TRACE_FILE").is_some() {
        unsafe {
            let path = format!("/tmp/maclator-trace-{}.log\0", libc::getpid());
            let fd = libc::open(path.as_ptr() as *const libc::c_char, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644);
            if fd >= 0 {
                libc::dup2(fd, 2);
            }
        }
    }
    if gpu_active {
        // The host frameworks must be initialised before the guest runs: CoreFoundation's
        // initialiser disturbs the guest's LaunchServices/XPC state when run mid-flight.
        gpu::preload();
    }
    engine::install_debug_handler();
    let cp = commpage::CommPage::install();
    if syscalls::TRACE.load(Ordering::Relaxed) {
        eprintln!("[maclator] commpage {} at {:#x}", if cp.in_place { "in place" } else { "redirected" }, cp.base);
    }
    let (loaded, mut cpu) = match loader::load(&exe, &dyld, &args, &envp) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("maclator: {e}");
            std::process::exit(1);
        }
    };
    let (lo, hi) = loaded.main.macho.vm_range();
    symbols::add_image(lo.wrapping_add(loaded.main.slide), hi.wrapping_add(loaded.main.slide), exe.rsplit('/').next().unwrap_or(&exe));
    let (lo, hi) = loaded.dyld.macho.vm_range();
    symbols::add_image(lo.wrapping_add(loaded.dyld.slide), hi.wrapping_add(loaded.dyld.slide), "dyld");
    if syscalls::TRACE.load(Ordering::Relaxed) {
        eprintln!("[maclator] main at {:#x}, dyld entry {:#x}, sp {:#x}", loaded.main.base, loaded.entry, loaded.sp);
    }
    {
        let m = &loaded.main.macho;
        let (lo, hi) = m.vm_range();
        let entries = aot::function_starts(&exe, m, loaded.main.slide);
        aot::register_main(&exe, m.uuid, lo.wrapping_add(loaded.main.slide), hi.wrapping_add(loaded.main.slide), entries);
        let d = &loaded.dyld.macho;
        let (lo, hi) = d.vm_range();
        aot::register_dyld(&dyld, d.uuid, lo.wrapping_add(loaded.dyld.slide), hi.wrapping_add(loaded.dyld.slide));
    }
    engine::run_thread(&mut cpu);
    // The main thread ended (pthread_exit on main): wait for other threads.
    while threads::LIVE_THREADS.load(Ordering::SeqCst) > 1 {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::process::exit(0);
}

fn resolve_path(p: &str) -> String {
    if let Some(physical) = paths::redirect(p) {
        return physical;
    }
    if p.contains('/') {
        return std::fs::canonicalize(p).map(|x| x.to_string_lossy().into_owned()).unwrap_or_else(|_| p.to_string());
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let cand = format!("{dir}/{p}");
            if std::path::Path::new(&cand).is_file() {
                return std::fs::canonicalize(&cand).map(|x| x.to_string_lossy().into_owned()).unwrap_or(cand);
            }
        }
    }
    p.to_string()
}

/// What the user pointed maclator at.
enum Target {
    /// A plain executable path (or a PATH lookup): historical behaviour, no automatic decisions.
    Executable(String),
    /// An application bundle: resolved to its main executable, eligible for automatic decisions.
    Bundle { root: std::path::PathBuf, exe: String },
}

impl Target {
    fn executable(&self) -> &str {
        match self {
            Target::Executable(path) => path,
            Target::Bundle { exe, .. } => exe,
        }
    }
}

/// A launch request as maclator resolved it, for `--print-plan` / `--dry-run`.
struct Plan<'a> {
    target: &'a str,
    gpu: &'a str,
    source: &'a str,
    reason: &'a str,
    shim: Option<&'a str>,
    argv: &'a [String],
}

fn gpu_label(enabled: Option<bool>, scope: Option<&str>) -> &'static str {
    match (enabled, scope) {
        (Some(true), Some(_)) => "scoped",
        (Some(true), None) => "all",
        (Some(false), _) => "off",
        (None, _) => "unset",
    }
}

fn emit_plan(p: &Plan<'_>) {
    eprintln!("target={}", p.target);
    eprintln!("gpu={}", p.gpu);
    eprintln!("source={}", p.source);
    eprintln!("reason={}", p.reason);
    eprintln!("shim={}", p.shim.unwrap_or("none"));
    eprintln!("argv={}", p.argv.join(" "));
}

/// Resolve the launch target: an application bundle becomes its main executable, anything else
/// goes through the usual path/PATH handling. `resolve_path` canonicalises first, so symlinked
/// and trailing-slash bundle paths behave like the physical path.
fn resolve_target(arg: &str) -> Result<Target, String> {
    let path = resolve_path(arg);
    let candidate = std::path::Path::new(&path);
    if candidate.is_dir() && bundle::is_bundle(candidate) {
        let exe = bundle::resolve_executable(candidate)?;
        return Ok(Target::Bundle {
            root: candidate.to_path_buf(),
            exe: exe.to_string_lossy().into_owned(),
        });
    }
    Ok(Target::Executable(path))
}
