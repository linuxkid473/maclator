//! Measures this test application's own main-run-loop callback delay.
//! It is NOT global FPS or physical input-to-display latency.
use block2::{DynBlock, RcBlock};
use objc2::{MainThreadOnly, rc::Retained};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEvent,
    NSEventModifierFlags, NSEventType, NSFont, NSTextField, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetMain() -> *const c_void;
    fn CFRunLoopPerformBlock(
        run_loop: *const c_void,
        mode: *const c_void,
        block: &DynBlock<dyn Fn()>,
    );
    fn CFRunLoopWakeUp(run_loop: *const c_void);
    static kCFRunLoopCommonModes: *const c_void;
}
fn percentile(values: &[f64], fraction: f64) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(
            values[((values.len() as f64 * fraction).ceil() as usize)
                .saturating_sub(1)
                .min(values.len() - 1)],
        )
    }
}
fn main() {
    if let Err(error) = run() {
        eprintln!("interaction probe: {error}");
        std::process::exit(1)
    }
}
fn run() -> Result<(), String> {
    let mut duration = 30u64;
    let mut warmup = 0u64;
    let mut label = "unlabelled".to_string();
    let mut output = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--warmup" => {
                warmup = args
                    .next()
                    .ok_or("warmup required")?
                    .parse()
                    .map_err(|_| "invalid warmup")?
            }
            "--duration" => {
                duration = args
                    .next()
                    .ok_or("duration required")?
                    .parse()
                    .map_err(|_| "invalid duration")?
            }
            "--label" => label = args.next().ok_or("label required")?,
            "--json" => {
                output = Some(std::path::PathBuf::from(
                    args.next().ok_or("output required")?,
                ))
            }
            "--help" => {
                println!(
                    "interaction_probe --duration <1..600 seconds> --label <observation/control> [--json file]\nMeasures only this application's main-run-loop queued callback delay; no physical latency or FPS."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    if !(1..=600).contains(&duration) || warmup > 60 || label.len() > 128 {
        return Err("duration or label exceeds bounds".into());
    }
    let mtm = MainThreadMarker::new().ok_or("probe requires main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let window: Retained<NSWindow> = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0., 0.), NSSize::new(600., 220.)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Miniaturizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("MacFlow · 交互队列探针"));
    window.center();
    let text = NSTextField::labelWithString(
        &NSString::from_str(&format!(
            "主线程队列延迟测量 · {label}\n\n{duration} 秒后自动结束，每 10 ms 提交一个回调。\n可以同时运行编译负载并切换窗口。\n该指标不是全局 FPS 或物理输入到显示延迟。"
        )),
        mtm,
    );
    text.setFrame(NSRect::new(NSPoint::new(20., 20.), NSSize::new(560., 170.)));
    text.setFont(Some(&NSFont::systemFontOfSize(16.)));
    window
        .contentView()
        .ok_or("probe content view missing")?
        .addSubview(&text);
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let samples = Arc::new(Mutex::new(Vec::<f64>::new()));
    let pending = Arc::new(AtomicBool::new(false));
    let skipped = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    // CFRunLoopGetMain returns the process-global run loop; it remains alive for
    // the process lifetime. No Cocoa reference crosses this thread boundary.
    let run_loop = unsafe { CFRunLoopGetMain() } as usize;
    if run_loop == 0 {
        return Err("main run loop unavailable".into());
    }
    let (sampler_samples, sampler_pending, sampler_skipped, sampler_failures) = (
        samples.clone(),
        pending.clone(),
        skipped.clone(),
        failures.clone(),
    );
    let started = Instant::now();
    let sampler = std::thread::spawn(move || {
        // Activate the probe before warming up so the shell's foreground
        // parent cannot accidentally protect the CPU fixture as a dependency.
        std::thread::sleep(Duration::from_secs(warmup));
        let measuring = Instant::now();
        while measuring.elapsed() < Duration::from_secs(duration) {
            if !sampler_pending.swap(true, Ordering::AcqRel) {
                let queued = Instant::now();
                let data = sampler_samples.clone();
                let pending = sampler_pending.clone();
                let failures = sampler_failures.clone();
                let callback = RcBlock::new(move || {
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        if let Ok(mut values) = data.lock() {
                            values.push(queued.elapsed().as_secs_f64() * 1000.);
                        } else {
                            failures.fetch_add(1, Ordering::Relaxed);
                        }
                    }));
                    if result.is_err() {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                    pending.store(false, Ordering::Release);
                });
                unsafe {
                    CFRunLoopPerformBlock(
                        run_loop as *const c_void,
                        kCFRunLoopCommonModes,
                        &callback,
                    );
                    CFRunLoopWakeUp(run_loop as *const c_void);
                }
            } else {
                sampler_skipped.fetch_add(1, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let callback = RcBlock::new(move || {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if let Some(mtm) = MainThreadMarker::new() {
                    let app = NSApplication::sharedApplication(mtm);
                    app.stop(None);
                    if let Some(event)=NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(NSEventType::ApplicationDefined,NSPoint::new(0.,0.),NSEventModifierFlags::empty(),0.,0,None,0,0,0){app.postEvent_atStart(&event,true);}
                }
            }));
        });
        unsafe {
            CFRunLoopPerformBlock(run_loop as *const c_void, kCFRunLoopCommonModes, &callback);
            CFRunLoopWakeUp(run_loop as *const c_void);
        }
    });
    app.run();
    sampler.join().map_err(|_| "sampler panicked")?;
    window.orderOut(None);
    let mut values = samples.lock().map_err(|_| "sample lock poisoned")?.clone();
    values.sort_by(f64::total_cmp);
    let report = serde_json::json!({"schema":1,"metric":"probe_main_run_loop_queued_callback_delay_ms","scope":"this test application only","label":label,"requested_duration_seconds":duration,"warmup_seconds":warmup,"elapsed_seconds":started.elapsed().as_secs_f64(),"sampling_interval_ms":10,"samples":values.len(),"samples_ms":&values,"skipped_intervals_while_callback_pending":skipped.load(Ordering::Relaxed),"callback_failures":failures.load(Ordering::Relaxed),"p50_ms":percentile(&values,0.50),"p95_ms":percentile(&values,0.95),"p99_ms":percentile(&values,0.99),"max_ms":values.last(),"global_fps":null,"physical_input_to_display_latency_ms":null});
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    if let Some(path) = output {
        std::fs::write(path, &json).map_err(|e| e.to_string())?;
    }
    println!("{json}");
    if values.is_empty() || failures.load(Ordering::Relaxed) > 0 {
        return Err("probe produced no valid samples or callback errors".into());
    }
    Ok(())
}
