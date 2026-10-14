//! Resident-component accounting, excluding this finite measurement process.
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
fn main() {
    let seconds = std::env::args()
        .nth(1)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(10, 120);
    let mut sampler = macflow_platform::Sampler::new();
    let mut cpu = Vec::new();
    let mut memory = Vec::new();
    let mut system_cpu = Vec::new();
    let mut identities: Option<BTreeSet<_>> = None;
    let mut stable = true;
    let mut rows = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        let sample = sampler.sample();
        let residents: Vec<_> = sample
            .processes
            .iter()
            .filter(|p| {
                let path = &p.identity.executable;
                (path.starts_with("/Applications/MacFlow.app/Contents/MacOS/")
                    || path.starts_with("/Library/Application Support/MacFlow/Runtime/"))
                    && ["MacFlowUI", "macflow-agent", "macflow-helper"].contains(
                        &std::path::Path::new(path)
                            .file_name()
                            .and_then(|v| v.to_str())
                            .unwrap_or(""),
                    )
            })
            .collect();
        let live: BTreeSet<_> = residents.iter().map(|p| p.identity.clone()).collect();
        if let Some(previous) = &identities {
            stable &= previous == &live;
        } else {
            identities = Some(live);
        }
        stable &= residents.len() == 3;
        if sample.cpu_percent.is_some() {
            cpu.push(residents.iter().map(|p| p.cpu_percent).sum::<f64>());
            memory.push(residents.iter().map(|p| p.memory_bytes).sum::<u64>());
            system_cpu.push(sample.cpu_percent.unwrap());
        }
        let components: BTreeMap<_, _> = residents.iter().map(|p| (p.name.clone(), serde_json::json!({"pid":p.identity.pid,"cpu_one_core_percent":p.cpu_percent,"physical_footprint_bytes":p.memory_bytes}))).collect();
        rows.push(serde_json::json!({"at_ms":sample.at_ms,"system_cpu_percent":sample.cpu_percent,"components":components}));
        std::thread::sleep(Duration::from_secs(1));
    }
    let average = if cpu.is_empty() {
        None
    } else {
        Some(cpu.iter().sum::<f64>() / cpu.len() as f64)
    };
    let peak = memory.iter().copied().max();
    let system_average = if system_cpu.is_empty() {
        None
    } else {
        Some(system_cpu.iter().sum::<f64>() / system_cpu.len() as f64)
    };
    println!(
        "{}",
        serde_json::json!({"duration_seconds":start.elapsed().as_secs_f64(),"samples":cpu.len(),"stable_three_resident_identities":stable,"average_one_core_cpu_percent":average,"peak_combined_physical_footprint_bytes":peak,"system_average_cpu_percent":system_average,"idle_environment_observed":system_average.is_some_and(|v| v<10.0),"cpu_below_one_core_1_percent":stable && average.is_some_and(|v|v<1.0),"memory_le_150_mib":stable && peak.is_some_and(|v|v<=150*1024*1024),"raw":rows})
    );
}
