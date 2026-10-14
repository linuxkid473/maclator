use macflow_protocol::*;
use std::{path::PathBuf, time::Duration};
fn main() {
    if let Err(e) = run() {
        eprintln!("MacFlow: {e}");
        std::process::exit(1)
    }
}
fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = args.first().map(String::as_str).unwrap_or("status");
    if arg == "install-root" {
        let app = args
            .windows(2)
            .find(|w| w[0] == "--app")
            .map(|w| PathBuf::from(&w[1]))
            .ok_or("--app required")?;
        let uid = args
            .windows(2)
            .find(|w| w[0] == "--uid")
            .and_then(|w| w[1].parse().ok())
            .ok_or("--uid required")?;
        return macflow::lifecycle::install_root(&app, uid);
    }
    if arg == "uninstall-root" {
        return macflow::lifecycle::remove_root();
    }
    if arg == "rollback-root" {
        return macflow::lifecycle::rollback_root();
    }
    if arg == "install" {
        let source = args
            .get(1)
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe().ok().and_then(|p| {
                    p.ancestors()
                        .find(|p| p.extension().is_some_and(|x| x == "app"))
                        .map(|p| p.to_path_buf())
                })
            })
            .ok_or("provide path to MacFlow.app")?;
        macflow::lifecycle::request_admin_install(&source)?;
        let status = std::process::Command::new(
            PathBuf::from(macflow::lifecycle::APP).join("Contents/MacOS/macflow"),
        )
        .arg("register-services")
        .status()
        .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("service registration failed".into());
        }
        std::process::Command::new("/usr/bin/open")
            .arg(macflow::lifecycle::APP)
            .status()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    if arg == "register-services" {
        println!("{}", macflow::lifecycle::register_or_fallback()?);
        return Ok(());
    }
    if arg == "unregister-services" {
        let _ = macflow_platform::unregister_user_service();
        macflow::lifecycle::stop_user_agent()?;
        return Ok(());
    }
    if ["uninstall", "rollback"].contains(&arg) {
        let own = std::env::current_exe().map_err(|e| e.to_string())?;
        let status = std::process::Command::new(&own)
            .arg("pause")
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("could not pause controller; restore service first".into());
        }
        std::thread::sleep(Duration::from_secs(11));
        macflow::lifecycle::stop_user_agent()?;
        let _ = macflow_platform::unregister_user_service();
        macflow::lifecycle::admin_operation(if arg == "uninstall" {
            "uninstall-root"
        } else {
            "rollback-root"
        })?;
        if arg == "rollback" {
            macflow::lifecycle::register_or_fallback()?;
        }
        return Ok(());
    }
    if arg == "build" || arg == "aot" {
        let request = Request {
            schema: SCHEMA,
            id: macflow_platform::now_ms(),
            action: Command::Status,
        };
        let requirement = agent_requirement()?;
        let response = macflow::ipc::call(
            AGENT_SERVICE,
            false,
            &request,
            Some(&requirement),
            Duration::from_secs(3),
        )?;
        if !response.ok {
            return Err(response.error.unwrap_or_default());
        }
        let status: Status = serde_json::from_value(response.data).map_err(|e| e.to_string())?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME unavailable")?;
        let _aot_lock;
        let mut command;
        if arg == "aot" {
            if !status.admit_optional {
                return Err(
                    "memory pressure blocks new optional AOT work; retry after recovery".into(),
                );
            }
            if args.len() != 2 {
                return Err("aot requires exactly one ARM executable path".into());
            }
            use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(macflow::storage::user_dir().join("aot.lock"))
                .map_err(|e| e.to_string())?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err("one explicit AOT task is already running".into());
            }
            _aot_lock = Some(file);
            command = std::process::Command::new(home.join("bin/maclator"));
            command.arg("--aot").arg(&args[1]);
        } else {
            if args
                .iter()
                .skip(1)
                .any(|a| a == "-jobs" || a.starts_with("-jobs="))
            {
                return Err(
                    "MacFlow supplies -jobs from the current mode; remove the explicit override"
                        .into(),
                );
            }
            _aot_lock = None;
            command = std::process::Command::new(home.join("bin/xcodebuild-maclator"));
            command.arg("-jobs").arg(status.build_jobs.to_string());
            command.args(args.iter().skip(1).filter(|a| a.as_str() != "--"));
        }
        command.env("MACLATOR_TASK_ROLE", "compute");
        macflow_platform::apply_own_qos(0x11)?;
        macflow_platform::apply_background_io()?;
        let exit = command.status().map_err(|e| e.to_string())?;
        std::process::exit(exit.code().unwrap_or(1));
    }
    if arg == "--help" || arg == "help" {
        println!(
            "macflow status [--json]\nmacflow mode auto|smooth|performance\nmacflow observe on|off\nmacflow pause|resume|export\nmacflow rule protect|background <bundle-id-or-executable>\nmacflow build [xcodebuild arguments]\nmacflow aot <ARM executable>\nmacflow install [MacFlow.app]\nmacflow uninstall|rollback\nmacflow register-services|unregister-services"
        );
        return Ok(());
    }
    let action = match arg {
        "status" => Command::Status,
        "pause" => Command::Pause,
        "resume" => Command::Resume,
        "export" => Command::Export,
        "mode" => Command::SetMode {
            mode: match args.get(1).map(String::as_str) {
                Some("auto") => Mode::Auto,
                Some("smooth") => Mode::Smooth,
                Some("performance") => Mode::Performance,
                _ => return Err("mode must be auto, smooth or performance".into()),
            },
        },
        "observe" => Command::SetObservation {
            enabled: match args.get(1).map(String::as_str) {
                Some("on") => true,
                Some("off") => false,
                _ => return Err("observe must be on or off".into()),
            },
        },
        "rule" => Command::SetRule {
            selector: args.get(2).ok_or("application selector required")?.clone(),
            policy: match args.get(1).map(String::as_str) {
                Some("protect") => RulePolicy::Protect,
                Some("background") => RulePolicy::AllowBackground,
                _ => return Err("rule must be protect or background".into()),
            },
        },
        _ => return Err("unknown command; use --help".into()),
    };
    let request = Request {
        schema: SCHEMA,
        id: macflow_platform::now_ms(),
        action,
    };
    let requirement = agent_requirement()?;
    let response = macflow::ipc::call(
        AGENT_SERVICE,
        false,
        &request,
        Some(&requirement),
        Duration::from_secs(3),
    )?;
    if !response.ok {
        return Err(response.error.unwrap_or_else(|| "operation failed".into()));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&response.data).map_err(|e| e.to_string())?
    );
    Ok(())
}
fn agent_requirement() -> Result<String, String> {
    let own = std::env::current_exe().map_err(|e| e.to_string())?;
    let parent = own.parent().ok_or("missing binary directory")?;
    macflow_platform::code_requirement_for_file(&parent.join("macflow-agent"))
}
