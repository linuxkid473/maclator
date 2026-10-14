//! Bounded CPU-only fixture. It deliberately has no MacFlow-prefixed executable name.
use std::hint::black_box;
use std::sync::{Arc, Barrier, OnceLock};
use std::time::{Duration, Instant};

const ITERATIONS_PER_UNIT: u64 = 4096;

fn main() {
    if let Err(error) = run() {
        eprintln!("compute fixture: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut seconds = 18u64;
    let mut threads = 30usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--duration" => {
                seconds = args
                    .next()
                    .ok_or("duration required")?
                    .parse()
                    .map_err(|_| "invalid duration")?
            }
            "--threads" => {
                threads = args
                    .next()
                    .ok_or("thread count required")?
                    .parse()
                    .map_err(|_| "invalid thread count")?
            }
            "--help" => {
                println!(
                    "compute_fixture --duration <1..30 seconds> --threads <1..30>\nCPU-only timed fixture; JSON output reports completed units and checksum."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    if !(1..=30).contains(&seconds) || !(1..=30).contains(&threads) {
        return Err("duration or thread count exceeds fixture bounds".into());
    }
    let barrier = Arc::new(Barrier::new(threads + 1));
    let start = Arc::new(OnceLock::<Instant>::new());
    let mut workers = Vec::with_capacity(threads);
    for index in 0..threads {
        let barrier = barrier.clone();
        let start = start.clone();
        workers.push(
            std::thread::Builder::new()
                .name(format!("fixture-{index}"))
                .spawn(move || {
                    let mut state =
                        black_box(0x9e37_79b9_7f4a_7c15u64.wrapping_mul(index as u64 + 1));
                    let mut units = 0u64;
                    barrier.wait();
                    let started = *start.get().expect("start published before barrier release");
                    while started.elapsed() < Duration::from_secs(seconds) {
                        for step in 0..ITERATIONS_PER_UNIT {
                            state ^= state >> 12;
                            state ^= state << 25;
                            state ^= state >> 27;
                            state = state
                                .wrapping_mul(0x2545_f491_4f6c_dd1d)
                                .rotate_left((step & 31) as u32);
                        }
                        state = black_box(state);
                        units += 1;
                    }
                    (units, state)
                })
                .map_err(|e| format!("worker allocation failed: {e}"))?,
        );
    }
    let started = Instant::now();
    start.set(started).map_err(|_| "start already published")?;
    barrier.wait();
    let mut units = 0u64;
    let mut checksum = 0u64;
    for (index, worker) in workers.into_iter().enumerate() {
        let (completed, state) = worker.join().map_err(|_| "fixture worker panicked")?;
        units = units.checked_add(completed).ok_or("unit count overflow")?;
        checksum ^= state.rotate_left((index & 63) as u32);
    }
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "{}",
        serde_json::json!({
            "schema": 1,
            "metric": "timed_cpu_fixture_completed_units",
            "pid": std::process::id(),
            "threads": threads,
            "requested_duration_seconds": seconds,
            "elapsed_seconds": elapsed,
            "iterations_per_unit": ITERATIONS_PER_UNIT,
            "completed_units": units,
            "units_per_second": units as f64 / elapsed,
            "checksum_hex": format!("{checksum:016x}"),
            "scope": "this bounded CPU fixture only; not application throughput or native translation efficiency"
        })
    );
    Ok(())
}
