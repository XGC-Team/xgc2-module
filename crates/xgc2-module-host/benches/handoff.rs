//! Handoff latency and CPU cost of a module chain.
//!
//! `cargo bench --bench handoff` runs a 3-module chain (producer at 500 Hz, pass-through stage,
//! sink) and measures, from the modules' own clocks,
//!   hop 1  producer commit -> stage step start
//!   hop 2  stage commit    -> sink step start
//!   total  producer commit -> sink step start
//! together with the CPU time the whole process used, for sticky and any affinity. Further
//! scenarios: a single worker, a fan-out to four sinks, and an idle host. `XGC2_BENCH_SECONDS` (default 20) sets the length
//! of each measurement; `XGC2_BENCH_OUT` names a file that receives the markdown report.

#[path = "../tests/common/mod.rs"]
mod common;

use common::*;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};
use xgc2_module_host::scheduler::Affinity;

struct Percentiles {
    count: usize,
    p50: i64,
    p90: i64,
    p99: i64,
    p999: i64,
    max: i64,
}

fn percentiles(mut values: Vec<i64>) -> Percentiles {
    values.sort_unstable();
    let at = |p: f64| -> i64 {
        if values.is_empty() {
            return 0;
        }
        let rank = (p * values.len() as f64).ceil() as usize;
        values[rank.clamp(1, values.len()) - 1]
    };
    Percentiles {
        count: values.len(),
        p50: at(0.50),
        p90: at(0.90),
        p99: at(0.99),
        p999: at(0.999),
        max: values.last().copied().unwrap_or(0),
    }
}

fn read_i64s(path: &Path) -> Vec<i64> {
    let bytes = std::fs::read(path).unwrap_or_default();
    bytes.chunks_exact(8).map(|chunk| i64::from_ne_bytes(chunk.try_into().unwrap())).collect()
}

struct Usage {
    cpu_seconds: f64,
    voluntary: i64,
    involuntary: i64,
}

fn usage() -> Usage {
    // SAFETY: getrusage fills the zeroed struct.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    Usage {
        cpu_seconds: seconds(usage.ru_utime) + seconds(usage.ru_stime),
        voluntary: usage.ru_nvcsw as i64,
        involuntary: usage.ru_nivcsw as i64,
    }
}

struct Window {
    cpu_percent: f64,
    wakeups_per_s: f64,
    preemptions_per_s: f64,
    /// Voluntary context switches per second of every thread, by thread name.
    by_thread: Vec<(String, f64)>,
}

/// Voluntary context switches of every thread of this process, by thread name.
fn thread_switches() -> std::collections::BTreeMap<String, i64> {
    let mut switches = std::collections::BTreeMap::new();
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else { return switches };
    for task in tasks.flatten() {
        let name = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default().trim().to_owned();
        let status = std::fs::read_to_string(task.path().join("status")).unwrap_or_default();
        let count = status
            .lines()
            .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(0);
        *switches.entry(name).or_insert(0) += count;
    }
    switches
}

/// Run `body` and report the process CPU use during it.
fn measure(body: impl FnOnce()) -> Window {
    let (before, threads_before, started) = (usage(), thread_switches(), Instant::now());
    body();
    let (after, threads_after, wall) = (usage(), thread_switches(), started.elapsed().as_secs_f64());
    let by_thread = threads_after
        .iter()
        .map(|(name, count)| (name.clone(), (count - threads_before.get(name).copied().unwrap_or(0)) as f64 / wall))
        .collect();
    Window {
        by_thread,
        cpu_percent: 100.0 * (after.cpu_seconds - before.cpu_seconds) / wall,
        wakeups_per_s: (after.voluntary - before.voluntary) as f64 / wall,
        preemptions_per_s: (after.involuntary - before.involuntary) as f64 / wall,
    }
}

fn micros(ns: i64) -> f64 {
    ns as f64 / 1000.0
}

/// The table cells of one distribution: samples, p50, p90, p99, p99.9, max.
fn cells(p: &Percentiles) -> String {
    format!(
        "{} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1}",
        p.count,
        micros(p.p50),
        micros(p.p90),
        micros(p.p99),
        micros(p.p999),
        micros(p.max)
    )
}

const WARM_UP_SAMPLES: usize = 500;

fn trimmed(values: Vec<i64>) -> Vec<i64> {
    values.into_iter().skip(WARM_UP_SAMPLES).collect()
}

struct Chain {
    window: Window,
    hop1: Percentiles,
    hop2: Percentiles,
    total: Percentiles,
    steps: [u64; 3],
    host_p50_ns: [u64; 2],
    host_p99_ns: [u64; 2],
    overruns: u64,
    missed: u64,
}

fn chain(workers: usize, seconds: f64, affinity: Affinity) -> Chain {
    let dir = tempfile::tempdir().unwrap();
    let (hop1, hop2, total) = (dir.path().join("hop1.bin"), dir.path().join("hop2.bin"), dir.path().join("total.bin"));
    let f = Fixture::with("bench", |options| options.workers = workers);
    f.load_module("producer_state");
    f.load_module("passthrough");
    f.load_module("consumer");
    let add = |mut instance: xgc2_module_host::host::InstanceSpec| {
        instance.affinity = affinity;
        f.add(instance);
    };
    add(spec(
        "sink",
        "consumer",
        0.0,
        &json!({"latency_file": hop2, "origin_file": total, "report_every": 0}).to_string(),
        &[("state_in", "c")],
    ));
    add(spec("stage", "passthrough", 0.0, &json!({"latency_file": hop1, "report_every": 0}).to_string(), &[("in", "a"), ("out", "c")]));
    add(spec("producer", "producer_state", 2.0, r#"{"report_every":0}"#, &[("out", "a")]));
    sleep_ms(1000);
    let window = measure(|| std::thread::sleep(Duration::from_secs_f64(seconds)));
    let health = f.host.health();
    let find = |name: &str| health["instances"].as_array().unwrap().iter().find(|i| i["name"] == name).unwrap().clone();
    let (producer, stage, sink) = (find("producer"), find("stage"), find("sink"));
    for instance in [&producer, &stage, &sink] {
        assert_eq!(instance["affinity"], affinity.as_str());
    }
    let host_latency = |i: &Value, key: &str| i["handoff_latency"][key].as_u64().unwrap_or(0);
    let steps = [count(&producer["steps"]), count(&stage["steps"]), count(&sink["steps"])];
    let host_p50_ns = [host_latency(&stage, "p50_ns"), host_latency(&sink, "p50_ns")];
    let host_p99_ns = [host_latency(&stage, "p99_ns"), host_latency(&sink, "p99_ns")];
    let overruns = count(&producer["overruns"]) + count(&stage["overruns"]) + count(&sink["overruns"]);
    let missed = count(&producer["missed_periods"]);
    // Stopping the modules makes them write their latency files.
    for name in ["producer", "stage", "sink"] {
        f.host.stop_instance(name).unwrap();
    }
    Chain {
        window,
        hop1: percentiles(trimmed(read_i64s(&hop1))),
        hop2: percentiles(trimmed(read_i64s(&hop2))),
        total: percentiles(trimmed(read_i64s(&total))),
        steps,
        host_p50_ns,
        host_p99_ns,
        overruns,
        missed,
    }
}

struct Fanout {
    window: Window,
    latency: Percentiles,
}

/// One 500 Hz producer, four sinks that all read each sample, with any affinity.
fn fanout(workers: usize, seconds: f64) -> Fanout {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::with("bench-fanout", |options| options.workers = workers);
    f.load_module("producer_state");
    f.load_module("consumer");
    let add = |mut instance: xgc2_module_host::host::InstanceSpec| {
        instance.affinity = Affinity::Any;
        f.add(instance);
    };
    let files: Vec<_> = (0..4).map(|i| dir.path().join(format!("sink{i}.bin"))).collect();
    for (i, file) in files.iter().enumerate() {
        add(spec(
            &format!("sink{i}"),
            "consumer",
            0.0,
            &json!({"latency_file": file, "report_every": 0}).to_string(),
            &[("state_in", "a")],
        ));
    }
    add(spec("producer", "producer_state", 2.0, r#"{"report_every":0}"#, &[("out", "a")]));
    sleep_ms(1000);
    let window = measure(|| std::thread::sleep(Duration::from_secs_f64(seconds)));
    for name in ["producer", "sink0", "sink1", "sink2", "sink3"] {
        f.host.stop_instance(name).unwrap();
    }
    let all: Vec<i64> = files.iter().flat_map(|file| trimmed(read_i64s(file))).collect();
    Fanout { window, latency: percentiles(all) }
}

/// A host with the chain's modules running but nothing to do, and an empty host.
fn idle(seconds: f64, with_instances: bool) -> Window {
    let f = Fixture::with("bench-idle", |options| options.workers = 4);
    if with_instances {
        f.load_module("consumer");
        f.load_module("passthrough");
        f.add(spec("sink", "consumer", 0.0, "{}", &[("state_in", "c")]));
        f.add(spec("stage", "passthrough", 0.0, "{}", &[("in", "a"), ("out", "c")]));
    }
    sleep_ms(500);
    measure(|| std::thread::sleep(Duration::from_secs_f64(seconds)))
}

fn machine() -> String {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let cpuinfo = read("/proc/cpuinfo");
    let model =
        cpuinfo.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split(':').nth(1)).unwrap_or("unknown").trim().to_owned();
    let cores = std::thread::available_parallelism().map_or(0, usize::from);
    let kernel = read("/proc/sys/kernel/osrelease");
    let load = read("/proc/loadavg");
    format!(
        "{model}, {cores} cores, kernel {}, load average before the run: {}",
        kernel.trim(),
        load.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
    )
}

fn main() {
    let seconds: f64 = std::env::var("XGC2_BENCH_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20.0);
    let mut report = String::new();
    writeln!(report, "Machine: {}\n", machine()).unwrap();
    writeln!(report, "Each measurement runs {seconds} s after a 1 s warm-up; times in microseconds.\n").unwrap();

    let mut chains = Vec::new();
    for workers in [4usize, 2, 1] {
        for affinity in [Affinity::Sticky, Affinity::Any] {
            eprintln!("chain, {workers} worker(s), {} affinity ...", affinity.as_str());
            chains.push((workers, affinity, chain(workers, seconds, affinity)));
        }
    }
    writeln!(report, "### Chain: producer (500 Hz) -> stage -> sink\n").unwrap();
    writeln!(report, "| affinity | workers | hop | samples | p50 | p90 | p99 | p99.9 | max |").unwrap();
    writeln!(report, "|---|---|---|---|---|---|---|---|---|").unwrap();
    for (workers, affinity, c) in &chains {
        for (hop, p) in [("producer -> stage", &c.hop1), ("stage -> sink", &c.hop2), ("producer -> sink", &c.total)] {
            writeln!(report, "| {} | {workers} | {hop} | {} |", affinity.as_str(), cells(p)).unwrap();
        }
    }
    writeln!(
        report,
        "\n| affinity | workers | process CPU | thread wake-ups/s | preemptions/s | steps (producer/stage/sink) | overruns | missed periods |"
    )
    .unwrap();
    writeln!(report, "|---|---|---|---|---|---|---|---|").unwrap();
    for (workers, affinity, c) in &chains {
        writeln!(
            report,
            "| {} | {workers} | {:.2}% of one core | {:.0} | {:.1} | {}/{}/{} | {} | {} |",
            affinity.as_str(),
            c.window.cpu_percent,
            c.window.wakeups_per_s,
            c.window.preemptions_per_s,
            c.steps[0],
            c.steps[1],
            c.steps[2],
            c.overruns,
            c.missed
        )
        .unwrap();
    }
    for (workers, affinity, c) in &chains {
        let threads: Vec<String> =
            c.window.by_thread.iter().filter(|(_, rate)| *rate >= 1.0).map(|(name, rate)| format!("{name} {rate:.0}")).collect();
        writeln!(report, "\nThread wake-ups per second with {workers} worker(s), {} affinity: {}.", affinity.as_str(), threads.join(", "))
            .unwrap();
    }
    for (_, affinity, c) in chains.iter().filter(|(workers, _, _)| *workers == 4) {
        writeln!(
        report,
        "\nHost-side histogram of the same hops with 4 workers, {} affinity (bucket error up to 12.5%): stage p50/p99 {:.1}/{:.1}, sink p50/p99 {:.1}/{:.1}.",
        affinity.as_str(),
        micros(c.host_p50_ns[0] as i64),
        micros(c.host_p99_ns[0] as i64),
        micros(c.host_p50_ns[1] as i64),
        micros(c.host_p99_ns[1] as i64)
    )
    .unwrap();
    }

    eprintln!("fan-out ...");
    let fan = fanout(4, seconds);
    writeln!(report, "\n### Fan-out: producer (500 Hz) -> 4 sinks, 4 workers, any affinity\n").unwrap();
    writeln!(report, "| samples | p50 | p90 | p99 | p99.9 | max |").unwrap();
    writeln!(report, "|---|---|---|---|---|---|").unwrap();
    writeln!(report, "| {} |", cells(&fan.latency)).unwrap();
    writeln!(report, "\nProcess CPU: {:.2}% of one core, {:.0} thread wake-ups/s.", fan.window.cpu_percent, fan.window.wakeups_per_s)
        .unwrap();

    eprintln!("idle ...");
    let (empty, loaded) = (idle(seconds.min(10.0), false), idle(seconds.min(10.0), true));
    writeln!(report, "\n### Idle\n").unwrap();
    writeln!(report, "| host | CPU | thread wake-ups/s |").unwrap();
    writeln!(report, "|---|---|---|").unwrap();
    writeln!(report, "| no instances | {:.3}% | {:.0} |", empty.cpu_percent, empty.wakeups_per_s).unwrap();
    writeln!(report, "| 2 started instances, no input and no timer | {:.3}% | {:.0} |", loaded.cpu_percent, loaded.wakeups_per_s).unwrap();

    println!("{report}");
    if let Ok(path) = std::env::var("XGC2_BENCH_OUT") {
        std::fs::write(path, &report).expect("write the report");
    }
}
