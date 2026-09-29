//! Maclator: run arm64 macOS programs on x86-64 Macs.

mod aot;
mod cache;
mod commpage;
mod engine;
mod guestmem;
mod hostsys;
mod iokit;
mod jit;
mod loader;
mod macho;
mod paths;
mod signals;
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
    eprintln!("  --trace         trace system calls");
    std::process::exit(2);
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut dyld = std::env::var("MACLATOR_DYLD").unwrap_or_else(|_| "/usr/lib/dyld".to_string());
    while let Some(a) = args.first().cloned() {
        match a.as_str() {
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
            "--interp" => {
                jit::JIT_ENABLED.store(false, Ordering::Relaxed);
                aot::set_enabled(false);
                args.remove(0);
            }
            "--no-aot" => {
                aot::set_enabled(false);
                args.remove(0);
            }
            "--sysroot" => {
                args.remove(0);
                if args.is_empty() {
                    usage();
                }
                paths::set_cache_dir(&args.remove(0));
            }
            "--aot" => {
                args.remove(0);
                if args.is_empty() {
                    usage();
                }
                let exe = resolve_path(&args[0]);
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
    if std::env::var_os("MACLATOR_TRACE").is_some() {
        syscalls::TRACE.store(true, Ordering::Relaxed);
    }
    let exe = resolve_path(&args[0]);
    let envp: Vec<String> = std::env::vars()
        .filter(|(k, _)| !k.starts_with("MACLATOR_"))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

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
