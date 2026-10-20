//! Handoff latency and CPU of a 1-2 KB frame between two modules of one host, to set against a ROS1 hop of the same size
//! (program/C9/scratch/ros_hop_bench.cpp in the maintenance task directory).
//!
//! `cargo bench --bench frame_handoff`: a producer module writes a frame of `XGC2_BENCH_BYTES` (default 1500) at 500 Hz and
//! 1 kHz; a sink module on the same host keeps the latency of every frame (its step start minus the producer's stamp, one
//! monotonic clock) and the percentiles are read from its health. Sticky and any affinity, four workers.
//! `XGC2_BENCH_SECONDS` (default 15) sets the length of each run.

#[path = "../tests/common/mod.rs"]
mod common;

use common::*;
use std::time::Duration;
use xgc2_module_host::scheduler::Affinity;

fn cpu_seconds() -> f64 {
    // utime and stime of this process, in clock ticks (100 per second on Linux), from /proc/self/stat.
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = stat.rsplit(')').next().unwrap_or("");
    let fields: Vec<&str> = after.split_whitespace().collect();
    let ticks = |i: usize| fields.get(i).and_then(|f| f.parse::<f64>().ok()).unwrap_or(0.0);
    (ticks(11) + ticks(12)) / 100.0
}

fn run(bytes: u32, period_us: u32, affinity: Affinity, seconds: f64) -> (serde_json::Value, f64) {
    let f = Fixture::with("bench", |options| options.workers = 4);
    f.load_module("bench_frame");
    let mut sink = spec("sink", "bench_frame", 0.0, r#"{"role":"sink"}"#, &[("in", "frames")]);
    sink.affinity = affinity;
    f.add(sink);
    let mut producer = spec("producer", "bench_frame", 0.0, &format!(r#"{{"role":"producer","bytes":{bytes},"period_us":{period_us}}}"#), &[("out", "frames")]);
    producer.affinity = affinity;
    f.add(producer);
    std::thread::sleep(Duration::from_millis(1000));
    let before = cpu_seconds();
    let started = std::time::Instant::now();
    std::thread::sleep(Duration::from_secs_f64(seconds));
    let cpu = (cpu_seconds() - before) / started.elapsed().as_secs_f64() * 100.0;
    f.host.stop_instance("sink").unwrap();
    (f.detail("sink"), cpu)
}

fn main() {
    let seconds: f64 = std::env::var("XGC2_BENCH_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(15.0);
    let bytes: u32 = std::env::var("XGC2_BENCH_BYTES").ok().and_then(|s| s.parse().ok()).unwrap_or(1500);
    println!("| affinity | rate | frame bytes | frames | p50 us | p90 us | p99 us | p99.9 us | max us | process CPU |");
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for affinity in [Affinity::Sticky, Affinity::Any] {
        for (rate, period_us) in [("500 Hz", 2000u32), ("1 kHz", 1000u32)] {
            let (d, cpu) = run(bytes, period_us, affinity, seconds);
            let us = |k: &str| d[k].as_f64().unwrap_or(0.0) / 1000.0;
            println!(
                "| {} | {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.2}% of one core |",
                affinity.as_str(),
                rate,
                bytes,
                d["count"],
                us("p50_ns"),
                us("p90_ns"),
                us("p99_ns"),
                us("p999_ns"),
                us("max_ns"),
                cpu
            );
        }
    }
}
