//! Native AppKit UI written in Rust. Cocoa objects never leave the main thread.
use crate::ipc;
use macflow_protocol::{
    AGENT_SERVICE, Command, MemoryPressure, Mode, Request, RulePolicy, SCHEMA, Status, TaskRole,
};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString, NSTimer};
use std::{
    cell::{OnceCell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc::{self, Receiver, SyncSender},
    time::{Duration, Instant},
};

#[derive(Debug)]
enum Work {
    Request(Command, bool),
    Lifecycle(&'static str),
}
struct Update {
    status: Option<Status>,
    history: Option<String>,
    message: Option<String>,
    quit: bool,
}
struct UiIvars {
    window: OnceCell<Retained<NSWindow>>,
    pages: OnceCell<Vec<Retained<NSTextView>>>,
    banner: OnceCell<Retained<NSTextField>>,
    rule_input: OnceCell<Retained<NSTextField>>,
    status_item: OnceCell<Retained<NSStatusItem>>,
    buttons: OnceCell<Vec<Retained<NSButton>>>,
    status: RefCell<Option<Status>>,
    work: SyncSender<Work>,
    updates: RefCell<Receiver<Update>>,
    action_message: RefCell<String>,
    last_status: RefCell<Option<Instant>>,
    stale_marked: RefCell<bool>,
    history: RefCell<String>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = UiIvars]
    struct Controller;
    unsafe impl NSObjectProtocol for Controller {}
    impl Controller {
        #[unsafe(method(action:))]
        fn action(&self, sender: &AnyObject) {
            if catch_unwind(AssertUnwindSafe(|| self.perform_action(sender))).is_err() {
                self.set_message("界面操作异常，已阻止异常进入系统回调；请暂停调度并导出诊断。");
            }
        }
        #[unsafe(method(refresh:))]
        fn refresh(&self, _timer: &NSTimer) {
            if catch_unwind(AssertUnwindSafe(|| self.consume_updates())).is_err() {
                self.set_message("界面刷新异常；后台租约恢复机制独立运行。");
            }
        }
    }
);
impl Controller {
    fn new(
        mtm: MainThreadMarker,
        work: SyncSender<Work>,
        updates: Receiver<Update>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(UiIvars {
            window: OnceCell::new(),
            pages: OnceCell::new(),
            banner: OnceCell::new(),
            rule_input: OnceCell::new(),
            status_item: OnceCell::new(),
            buttons: OnceCell::new(),
            status: RefCell::new(None),
            work,
            updates: RefCell::new(updates),
            action_message: RefCell::new("连接本机服务中……".into()),
            last_status: RefCell::new(None),
            stale_marked: RefCell::new(false),
            history: RefCell::new("聚合历史正在加载。".into()),
        });
        unsafe { msg_send![super(this), init] }
    }
    fn set_message(&self, text: &str) {
        *self.ivars().action_message.borrow_mut() = text.into();
        if let Some(banner) = self.ivars().banner.get() {
            banner.setStringValue(&NSString::from_str(text));
        }
    }
    fn enqueue(&self, command: Work) {
        match self.ivars().work.try_send(command) {
            Ok(()) => self.set_message("正在执行；结果将在此处显示。"),
            Err(_) => self.set_message("操作队列繁忙，请等待当前操作完成。"),
        }
    }
    fn perform_action(&self, sender: &AnyObject) {
        // Only buttons/menu items created below target this fixed selector.
        let tag: isize = unsafe { msg_send![sender, tag] };
        let state = self.ivars().status.borrow();
        match tag {
            0 => {
                if let Some(window) = self.ivars().window.get() {
                    window.makeKeyAndOrderFront(None);
                }
                #[allow(deprecated)]
                NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
            }
            1..=3 => self.enqueue(Work::Request(
                Command::SetMode {
                    mode: match tag {
                        1 => Mode::Auto,
                        2 => Mode::Smooth,
                        _ => Mode::Performance,
                    },
                },
                false,
            )),
            4 => {
                if let Some(status) = state.as_ref() {
                    self.enqueue(Work::Request(
                        Command::SetObservation {
                            enabled: !status.config.observe_only,
                        },
                        false,
                    ));
                } else {
                    self.set_message("服务尚未连接，无法切换观察模式。");
                }
            }
            5 => {
                if let Some(status) = state.as_ref() {
                    self.enqueue(Work::Request(
                        if status.config.paused {
                            Command::Resume
                        } else {
                            Command::Pause
                        },
                        false,
                    ));
                } else {
                    self.set_message("服务尚未连接，无法切换暂停状态。");
                }
            }
            6 | 7 => {
                let selector = self
                    .ivars()
                    .rule_input
                    .get()
                    .map(|f| f.stringValue().to_string())
                    .unwrap_or_default();
                let selector = selector.trim();
                if selector.is_empty() || selector.len() > 1024 {
                    self.set_message(
                        "请输入应用 bundle ID 或完整可执行文件路径。规则不使用通配符。",
                    );
                    return;
                }
                self.enqueue(Work::Request(
                    Command::SetRule {
                        selector: selector.into(),
                        policy: if tag == 6 {
                            RulePolicy::Protect
                        } else {
                            RulePolicy::AllowBackground
                        },
                    },
                    false,
                ));
            }
            8 => self.enqueue(Work::Request(Command::Export, false)),
            9 => self.enqueue(Work::Lifecycle("install")),
            10 => self.enqueue(Work::Lifecycle("uninstall")),
            11 => self.enqueue(Work::Lifecycle("rollback")),
            12 => self.enqueue(Work::Request(Command::Pause, true)),
            _ => self.set_message("未知操作已忽略。"),
        }
    }
    fn consume_updates(&self) {
        while let Ok(update) = self.ivars().updates.borrow_mut().try_recv() {
            if let Some(message) = update.message {
                self.set_message(&message);
            }
            if let Some(status) = update.status {
                if let Some(history) = update.history {
                    *self.ivars().history.borrow_mut() = history;
                }
                *self.ivars().last_status.borrow_mut() = Some(Instant::now());
                *self.ivars().stale_marked.borrow_mut() = false;
                let mut text = render_pages(&status);
                text[2].push_str("\n\n");
                text[2].push_str(&self.ivars().history.borrow());
                if let Some(pages) = self.ivars().pages.get() {
                    for (view, text) in pages.iter().zip(text.iter()) {
                        view.setString(&NSString::from_str(text));
                    }
                }
                if let Some(item) = self.ivars().status_item.get() {
                    if let Some(button) = item.button(self.mtm()) {
                        button.setTitle(&NSString::from_str(if status.config.paused {
                            "MF 暂停"
                        } else if status.config.observe_only {
                            "MF 观察"
                        } else if status.pressure {
                            "MF 调度"
                        } else {
                            "MacFlow"
                        }));
                    }
                }
                if let Some(buttons) = self.ivars().buttons.get() {
                    buttons[3].setTitle(&NSString::from_str(if status.config.observe_only {
                        "启用管理"
                    } else {
                        "仅观察"
                    }));
                    buttons[4].setTitle(&NSString::from_str(if status.config.paused {
                        "恢复"
                    } else {
                        "暂停"
                    }));
                }
                if self
                    .ivars()
                    .action_message
                    .borrow()
                    .starts_with("服务状态未知")
                    || self.ivars().action_message.borrow().starts_with("连接本机")
                    || self
                        .ivars()
                        .action_message
                        .borrow()
                        .starts_with("状态已过期")
                {
                    self.set_message(if status.config.observe_only {
                        "服务已连接 · 当前仅观察，发布验收尚未完成"
                    } else {
                        "服务已连接 · 管理策略运行中，发布验收尚未完成"
                    });
                }
                *self.ivars().status.borrow_mut() = Some(status);
            }
            if update.quit {
                NSApplication::sharedApplication(self.mtm()).terminate(None);
            }
        }
        if self
            .ivars()
            .last_status
            .borrow()
            .is_some_and(|at| at.elapsed() > Duration::from_secs(7))
            && !*self.ivars().stale_marked.borrow()
        {
            *self.ivars().stale_marked.borrow_mut() = true;
            self.set_message("状态已过期；页面保留最后一次采样，当前服务状态未知。");
            if let Some(item) = self.ivars().status_item.get() {
                if let Some(button) = item.button(self.mtm()) {
                    button.setTitle(&NSString::from_str("MF 未知"));
                }
            }
            if let Some(status) = self.ivars().status.borrow().as_ref() {
                if let Some(pages) = self.ivars().pages.get() {
                    for (view, text) in pages.iter().zip(render_pages(status).iter()) {
                        view.setString(&NSString::from_str(&format!(
                            "⚠ 数据已过期，以下是最后一次采样

{text}"
                        )));
                    }
                }
            }
        }
    }
}

fn byte_label(bytes: Option<u64>) -> String {
    bytes
        .map(|v| format!("{:.2} GiB", v as f64 / 1073741824.0))
        .unwrap_or_else(|| "未知".into())
}
fn integer_label(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "未知".into())
}
fn percent(value: Option<f64>) -> String {
    value
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| format!("{v:.1}%"))
        .unwrap_or_else(|| "未知".into())
}
fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Auto => "自动",
        Mode::Smooth => "流畅优先",
        Mode::Performance => "性能优先",
    }
}
fn memory_name(value: MemoryPressure) -> &'static str {
    match value {
        MemoryPressure::Unknown => "未知",
        MemoryPressure::Normal => "正常",
        MemoryPressure::Warning => "警告",
        MemoryPressure::Critical => "严重",
    }
}
fn role_name(value: TaskRole) -> &'static str {
    match value {
        TaskRole::Foreground => "前台保护",
        TaskRole::Protected => "永久或系统保护",
        TaskRole::Compute => "可管理后台计算",
        TaskRole::Unknown => "用途未知，仅监测",
    }
}
fn rule_name(value: RulePolicy) -> &'static str {
    match value {
        RulePolicy::Protect => "永久保护",
        RulePolicy::AllowBackground => "允许后台管理",
    }
}
fn render_pages(status: &Status) -> [String; 5] {
    let t = &status.telemetry;
    let now = macflow_platform::now_ms();
    let sample_fresh = t.at_ms > 0 && now.saturating_sub(t.at_ms) <= 7000;
    let protection = if status.config.paused {
        "暂停，正在撤销本软件策略"
    } else if status.config.observe_only {
        "观察模式，未施加新调度"
    } else {
        "运行管理已启用"
    };
    let overview = format!(
        "MacFlow {} · {}\n\n{}\n资源争用：{}\nCPU：{}\n内存压力：{}\n编译并发建议：{}\n可延迟任务：{}\n管理员助手：{}\n当前租约：{}\n\n策略依据\n前台任务和通话、音视频、输入法保持保护；未知任务仅监测。\n全机 CPU >85% 且存在可管理后台任务，连续两个窗口后进入争用。\n内存压力与驱动故障分别处理。\n\n验证状态\n{}",
        status.version,
        mode_name(status.config.mode),
        protection,
        if !sample_fresh {
            "未知（采样过期）"
        } else if status.pressure {
            "存在"
        } else {
            "未检测到"
        },
        percent(t.cpu_percent),
        memory_name(t.memory_pressure),
        status.build_jobs,
        if status.admit_optional {
            "允许"
        } else {
            "暂缓"
        },
        if status.helper_connected {
            "已连接"
        } else {
            "未连接"
        },
        status.active_leases,
        status
            .pending_verification
            .iter()
            .map(|s| format!("• {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let mut processes = t.processes.iter().collect::<Vec<_>>();
    processes.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent));
    let mut applications = String::from("应用与进程（CPU 占单核百分比；用途不明确时仅监测）\n\n");
    for p in processes.iter().take(100) {
        applications.push_str(&format!(
            "{} · PID {} · CPU {:.1}% · {:.0} MiB · nice {}\n  {}{}\n  {}\n",
            p.name,
            p.identity.pid,
            p.cpu_percent,
            p.memory_bytes as f64 / 1048576.0,
            p.nice,
            role_name(p.role),
            p.protected_reason
                .as_ref()
                .map(|s| format!(" / {s}"))
                .unwrap_or_default(),
            p.bundle_id.as_deref().unwrap_or(&p.identity.executable)
        ));
    }
    applications.push_str("\n保存的应用规则\n");
    for rule in &status.config.rules {
        applications.push_str(&format!("{}：{}\n", rule_name(rule.policy), rule.selector));
    }
    applications
        .push_str("\n下方输入 bundle ID 或完整可执行文件路径后，可永久保护或允许后台管理。\n");
    let performance = format!(
        "当前系统采样 · 时间戳 {} ms\n\nCPU：{}\n内存压力：{}\n压缩内存：{}\nSwap：{}\n每采样增量（页）：压缩 {} / 解压 {} / 换入 {} / 换出 {}\n热状态：{}\n供电信息：{}（未经验证时不参与策略）\n\nGPU\n驱动版本：{}\nVRAM 使用：{}\n恢复计数：{}\n利用率：{}\n温度：{}\n\nMaclator 运行时\n{}\n\n没有经过验证的全局 FPS 或输入到显示延迟数据。\n95% 原生性能目标需要独立基准验证。",
        t.at_ms,
        percent(t.cpu_percent),
        memory_name(t.memory_pressure),
        byte_label(t.compressed_bytes),
        byte_label(t.swap_used_bytes),
        integer_label(t.compressed_pages_delta),
        integer_label(t.decompressed_pages_delta),
        integer_label(t.swap_in_pages_delta),
        integer_label(t.swap_out_pages_delta),
        t.thermal.as_deref().unwrap_or("未知"),
        match t.ac_power {
            Some(true) => "接电",
            Some(false) => "电池",
            None => "未知",
        },
        t.gpu.driver_version.as_deref().unwrap_or("未知"),
        byte_label(t.gpu.vram_used_bytes),
        t.gpu
            .recovery_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "未知".into()),
        percent(t.gpu.utilization_percent),
        t.gpu
            .temperature_c
            .filter(|v| v.is_finite())
            .map(|v| format!("{v:.1} °C"))
            .unwrap_or_else(|| "未知".into()),
        status
            .runtimes
            .iter()
            .map(|r| format!(
                "PID {} / {}\n采样 {} · 桥接调用 {} · 等待 {:.1} ms · QoS 失败 {}",
                r.pid,
                r.source_app,
                r.samples,
                r.bridge_calls,
                r.bridge_wait_ns as f64 / 1_000_000.0,
                r.qos_failures
            ))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    let diagnostics = format!(
        "系统能力\n{}\n\n采样异常\n{}\n\nGPU 异常\n{}\n\n最近事件\n{}\n\n待完成验证\n{}",
        status
            .capabilities
            .iter()
            .map(|c| format!(
                "{} · {}：{}",
                if c.available {
                    "可用"
                } else {
                    "未启用/未知"
                },
                c.name,
                c.detail
            ))
            .collect::<Vec<_>>()
            .join("\n"),
        t.errors.join("\n"),
        t.gpu.error.as_deref().unwrap_or("未报告"),
        status
            .events
            .iter()
            .rev()
            .take(70)
            .map(|e| format!("{} [{}] {}", e.at_ms, e.kind, e.message))
            .collect::<Vec<_>>()
            .join("\n"),
        status.pending_verification.join("\n")
    );
    let settings = format!(
        "运行配置\n\n模式：{}\n观察模式：{}\n暂停：{}\n\n恢复策略\n心跳 2 秒、租约 10 秒；控制器断联后助手撤销本软件施加的优先级。\n其他软件修改优先级时停止接管。暂停、退出、卸载先撤销租约。\n\n保留现有 EFI、ACPI、内核驱动及永不休眠设置。\n原生 GPU 调度能力不可用时仅监测，不自动重启 WindowServer。\n\n生命周期操作\n安装 / 注册服务：安装登录服务及申请助手注册。\n卸载：停止管理并清理本软件服务。\n回滚：恢复前一软件版本，不替换 Maclator 或系统驱动。\n需要管理员确认时在系统窗口自行完成。\n\n日志仅保存本机，导出按钮返回报告位置。\n\n禁用规则\n{}",
        mode_name(status.config.mode),
        status.config.observe_only,
        status.config.paused,
        status.config.disabled_rules.join("\n")
    );
    [overview, applications, performance, diagnostics, settings]
}

fn run_cli(command: &'static str) -> Result<String, String> {
    // Fixed arguments, no shell expansion and no caller-provided executable.
    if !matches!(command, "install" | "uninstall" | "rollback") {
        return Err("unknown lifecycle action".into());
    }
    let app = std::env::current_exe().map_err(|e| e.to_string())?;
    let cli = app
        .parent()
        .ok_or("application executable directory missing")?
        .join("macflow");
    let output = std::process::Command::new(cli)
        .arg(command)
        .output()
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(if output.status.success() {
        &output.stdout
    } else {
        &output.stderr
    })
    .trim()
    .to_string();
    if output.status.success() {
        Ok(if text.is_empty() {
            "操作完成".into()
        } else {
            text
        })
    } else {
        Err(format!("{}：{}", output.status, text))
    }
}
fn agent_requirement() -> Result<String, String> {
    let installed = std::path::Path::new("/Applications/MacFlow.app/Contents/MacOS/macflow-agent");
    let path = if installed.is_file() {
        installed.to_path_buf()
    } else {
        std::env::current_exe()
            .map_err(|e| e.to_string())?
            .parent()
            .ok_or("application directory missing")?
            .join("macflow-agent")
    };
    macflow_platform::code_requirement_for_file(&path)
        .map_err(|e| format!("无法验证本机服务签名：{e}"))
}
fn agent_request(
    command: Command,
    id: u64,
    requirement: &Result<String, String>,
) -> Result<macflow_protocol::Response, String> {
    let requirement = requirement.as_ref().map_err(Clone::clone)?;
    ipc::call(
        AGENT_SERVICE,
        false,
        &Request {
            schema: SCHEMA,
            id,
            action: command,
        },
        Some(requirement),
        Duration::from_secs(3),
    )
}
fn worker(work: Receiver<Work>, updates: SyncSender<Update>) {
    let mut id = 1u64;
    let mut requirement = agent_requirement();
    let mut next_refresh = Instant::now();
    let mut next_history = Instant::now();
    loop {
        let wait = next_refresh.saturating_duration_since(Instant::now());
        match work.recv_timeout(wait) {
            Ok(Work::Request(command, quit)) => {
                id = id.wrapping_add(1);
                let result = agent_request(command, id, &requirement);
                let (message, allow_quit) = match result {
                    Ok(r) if r.ok => (
                        if r.data.is_null() {
                            "操作完成".into()
                        } else {
                            format!("操作完成：{}", r.data)
                        },
                        quit,
                    ),
                    Ok(r) => (
                        format!(
                            "操作未完成：{}",
                            r.error.unwrap_or_else(|| "未知错误".into())
                        ),
                        false,
                    ),
                    Err(e) => (e, false),
                };
                if updates
                    .send(Update {
                        status: None,
                        history: None,
                        message: Some(message),
                        quit: allow_quit,
                    })
                    .is_err()
                {
                    break;
                }
                if allow_quit {
                    break;
                }
                next_refresh = Instant::now();
            }
            Ok(Work::Lifecycle(command)) => {
                let message = match run_cli(command) {
                    Ok(s) => s,
                    Err(e) => format!("操作未完成：{e}"),
                };
                requirement = agent_requirement();
                if updates
                    .send(Update {
                        status: None,
                        history: None,
                        message: Some(message),
                        quit: false,
                    })
                    .is_err()
                {
                    break;
                }
                next_refresh = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                id = id.wrapping_add(1);
                let result = agent_request(Command::Status, id, &requirement).and_then(|r| {
                    if r.ok {
                        serde_json::from_value::<Status>(r.data)
                            .map_err(|e| format!("状态解析失败：{e}"))
                    } else {
                        Err(r.error.unwrap_or_else(|| "服务返回失败".into()))
                    }
                });
                let update = match result {
                    Ok(status) => {
                        let history = if Instant::now() >= next_history {
                            next_history = Instant::now() + Duration::from_secs(30);
                            Some(
                                crate::storage::recent_history_text()
                                    .unwrap_or_else(|e| format!("历史读取失败：{e}")),
                            )
                        } else {
                            None
                        };
                        Update {
                            status: Some(status),
                            history,
                            message: None,
                            quit: false,
                        }
                    }
                    Err(e) => Update {
                        status: None,
                        history: None,
                        message: Some(format!("服务状态未知：{e}")),
                        quit: false,
                    },
                };
                // Status refresh may coalesce, command results above may not.
                if matches!(
                    updates.try_send(update),
                    Err(mpsc::TrySendError::Disconnected(_))
                ) {
                    break;
                }
                next_refresh = Instant::now() + Duration::from_secs(2);
            }
        }
    }
}
fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}
fn add_button(
    controller: &Controller,
    parent: &NSView,
    title: &str,
    tag: isize,
    frame: NSRect,
) -> Retained<NSButton> {
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(controller),
            Some(sel!(action:)),
            controller.mtm(),
        )
    };
    button.setTag(tag);
    button.setFrame(frame);
    parent.addSubview(&button);
    button
}
fn add_menu_item(controller: &Controller, menu: &NSMenu, title: &str, tag: isize, key: &str) {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(controller.mtm()),
            &NSString::from_str(title),
            Some(sel!(action:)),
            &NSString::from_str(key),
        )
    };
    item.setTag(tag);
    unsafe { item.setTarget(Some(controller)) };
    menu.addItem(&item);
}

pub fn run() -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("MacFlow UI requires the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let (work_tx, work_rx) = mpsc::sync_channel(16);
    let (update_tx, update_rx) = mpsc::sync_channel(16);
    let controller = Controller::new(mtm, work_tx, update_rx);
    std::thread::Builder::new()
        .name("macflow-ui-io".into())
        .spawn(move || worker(work_rx, update_tx))
        .map_err(|e| e.to_string())?;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0., 0., 960., 680.),
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        window.setReleasedWhenClosed(false);
        window.setContentMinSize(NSSize::new(840., 620.))
    };
    window.setTitle(&NSString::from_str("MacFlow · 整机资源管理"));
    window.center();
    let content = window.contentView().ok_or("window content view missing")?;
    let title = NSTextField::labelWithString(&NSString::from_str("MacFlow"), mtm);
    title.setFrame(rect(24., 625., 250., 36.));
    title.setFont(Some(&NSFont::boldSystemFontOfSize(26.)));
    title.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
    content.addSubview(&title);
    let banner = NSTextField::labelWithString(&NSString::from_str("连接本机服务中……"), mtm);
    banner.setFrame(rect(24., 594., 910., 26.));
    banner.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    content.addSubview(&banner);
    controller
        .ivars()
        .banner
        .set(banner)
        .map_err(|_| "banner initialized twice")?;
    let tabs = NSTabView::initWithFrame(NSTabView::alloc(mtm), rect(20., 65., 920., 520.));
    tabs.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    let mut pages = Vec::new();
    for (index, label) in ["总览", "应用", "性能", "诊断", "设置"].iter().enumerate() {
        let panel = NSView::initWithFrame(NSView::alloc(mtm), rect(0., 0., 890., 475.));
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(
                12.,
                if index == 1 || index == 4 { 64. } else { 12. },
                864.,
                if index == 1 || index == 4 { 399. } else { 451. },
            ),
        );
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        scroll.setHasVerticalScroller(true);
        let text = NSTextView::initWithFrame(NSTextView::alloc(mtm), rect(0., 0., 844., 450.));
        text.setEditable(false);
        text.setSelectable(true);
        text.setFont(Some(&NSFont::systemFontOfSize(14.)));
        text.setString(&NSString::from_str(
            "等待经过验证的本机状态。若服务尚未安装，请打开设置页安装/注册服务。",
        ));
        text.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        scroll.setDocumentView(Some(&text));
        panel.addSubview(&scroll);
        if index == 1 {
            let input =
                NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(12., 15., 555., 30.));
            input.setPlaceholderString(Some(&NSString::from_str(
                "应用 bundle ID 或完整可执行文件路径",
            )));
            input.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
            panel.addSubview(&input);
            controller
                .ivars()
                .rule_input
                .set(input)
                .map_err(|_| "rule field initialized twice")?;
            add_button(
                &controller,
                &panel,
                "永久保护",
                6,
                rect(579., 15., 125., 30.),
            );
            add_button(
                &controller,
                &panel,
                "允许后台管理",
                7,
                rect(711., 15., 160., 30.),
            );
        }
        if index == 4 {
            add_button(
                &controller,
                &panel,
                "安装 / 注册服务",
                9,
                rect(12., 15., 185., 32.),
            );
            add_button(
                &controller,
                &panel,
                "回滚软件",
                11,
                rect(212., 15., 140., 32.),
            );
            add_button(
                &controller,
                &panel,
                "卸载服务",
                10,
                rect(369., 15., 140., 32.),
            );
        }
        let item = unsafe { NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), None) };
        item.setLabel(&NSString::from_str(label));
        item.setView(Some(&panel));
        tabs.addTabViewItem(&item);
        pages.push(text);
    }
    content.addSubview(&tabs);
    controller
        .ivars()
        .pages
        .set(pages)
        .map_err(|_| "pages initialized twice")?;
    let mut buttons = Vec::new();
    for (index, (title, tag, width)) in [
        ("自动", 1, 100.),
        ("流畅优先", 2, 120.),
        ("性能优先", 3, 120.),
        ("启用管理", 4, 130.),
        ("暂停", 5, 100.),
        ("导出诊断", 8, 130.),
    ]
    .iter()
    .enumerate()
    {
        let x = [24., 135., 266., 397., 538., 649.][index];
        let b = add_button(
            &controller,
            &content,
            title,
            *tag,
            rect(x, 20., *width, 32.),
        );
        buttons.push(b);
    }
    controller
        .ivars()
        .buttons
        .set(buttons)
        .map_err(|_| "buttons initialized twice")?;
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    for (title, tag) in [
        ("打开控制面板", 0),
        ("自动模式", 1),
        ("流畅优先", 2),
        ("性能优先", 3),
        ("观察 / 管理", 4),
        ("暂停 / 恢复", 5),
        ("导出诊断", 8),
        ("退出并恢复", 12),
    ] {
        add_menu_item(&controller, &menu, title, tag, "");
    }
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.);
    if let Some(button) = item.button(mtm) {
        button.setTitle(&NSString::from_str("MF 观察"));
    }
    item.setMenu(Some(&menu));
    controller
        .ivars()
        .status_item
        .set(item)
        .map_err(|_| "status item initialized twice")?;
    let main_menu = NSMenu::new(mtm);
    let app_menu = NSMenu::new(mtm);
    add_menu_item(&controller, &app_menu, "打开 MacFlow", 0, "");
    add_menu_item(&controller, &app_menu, "退出并恢复", 12, "q");
    let app_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("MacFlow"),
            None,
            &NSString::from_str(""),
        )
    };
    app_item.setSubmenu(Some(&app_menu));
    main_menu.addItem(&app_item);
    app.setMainMenu(Some(&main_menu));
    controller
        .ivars()
        .window
        .set(window.clone())
        .map_err(|_| "window initialized twice")?;
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.2,
            &controller,
            sel!(refresh:),
            None,
            true,
        )
    };
    timer.setTolerance(0.05);
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    app.run();
    timer.invalidate();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_metrics_are_not_zero() {
        assert_eq!(percent(None), "未知");
        assert_eq!(byte_label(None), "未知");
        assert_eq!(percent(Some(f64::NAN)), "未知");
    }
    #[test]
    fn metric_format_is_explicit() {
        assert_eq!(percent(Some(85.2)), "85.2%");
        assert_eq!(byte_label(Some(1073741824)), "1.00 GiB");
    }
}
