//! OS-thread affinity across lifecycle calls, dispatch sources, hot-plug and isolation.

mod common;

use common::*;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use xgc2_module_host::scheduler::Affinity;

const TIMEOUT: Duration = Duration::from_secs(5);

fn assert_trace(path: &Path) -> u64 {
    let trace = std::fs::read_to_string(path).unwrap();
    let calls: Vec<_> = trace.lines().map(|line| line.split_once(' ').unwrap()).collect();
    let threads: HashSet<_> = calls.iter().map(|(_, tid)| *tid).collect();
    assert_eq!(threads.len(), 1, "{trace}");
    for expected in ["create", "configure", "start", "step", "stop", "destroy"] {
        assert!(calls.iter().any(|(call, _)| *call == expected), "missing {expected}: {trace}");
    }
    threads.into_iter().next().unwrap().parse().unwrap()
}

#[test]
fn sticky_lifecycle_stays_on_one_os_thread_for_timers_inputs_and_async_wakes() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::with("sticky-sources", |options| options.workers = 4);
    f.load_module("thread_probe");
    // Three balanced home slots: input handoffs cross worker threads.
    for (name, period, wake, bind) in [
        ("sink", 0.0, 0, vec![("in", "stage.out")]),
        ("stage", 0.0, 0, vec![("in", "source.out")]),
        ("source", 2.0, 0, vec![]),
        ("waker", 0.0, 2, vec![]),
    ] {
        let trace = dir.path().join(name);
        f.add(spec(name, "thread_probe", period, &json!({"trace": trace, "wake_ms": wake}).to_string(), &bind));
    }
    wait_until("all dispatch sources", TIMEOUT, || {
        ["sink", "stage", "source", "waker"].iter().all(|name| count(&f.instance(name)["steps"]) >= 30)
    });
    assert!(count(&f.instance("sink")["input_commits"]) >= 30);
    assert!(count(&f.instance("source")["timer_fires"]) >= 30);
    assert!(count(&f.instance("waker")["wakeups"]) >= 30);
    for name in ["sink", "stage", "source", "waker"] {
        let before = f.detail(name)["tid"].clone();
        let home = f.instance(name)["worker"].clone();
        f.host.configure_instance(name, "{}".into()).unwrap();
        f.host.stop_instance(name).unwrap();
        f.host.start_instance(name).unwrap();
        assert_eq!(f.detail(name)["tid"], before);
        assert_eq!(f.instance(name)["worker"], home);
        assert_eq!(f.instance(name)["affinity"], "sticky");
        assert_eq!(f.instance(name)["worker_moves"], 0);
        assert_eq!(f.instance(name)["state"], "running");
        f.host.remove_instance(name).unwrap();
        assert_eq!(json!(assert_trace(&dir.path().join(name))), before);
    }
}

#[test]
fn any_affinity_observes_migration_between_os_threads() {
    let f = Fixture::with("any-migration", |options| options.workers = 4);
    f.load_module("thread_probe");
    let mut probe = spec("probe", "thread_probe", 2.0, r#"{"strict":false}"#, &[]);
    probe.affinity = Affinity::Any;
    f.add(probe);
    wait_until("OS-thread migration", TIMEOUT, || count(&f.detail("probe")["moves"]) > 5);
    assert_eq!(f.instance("probe")["affinity"], "any");
    assert!(count(&f.instance("probe")["worker_moves"]) > 5);
    assert_eq!(f.host.health()["workers"]["sticky"], json!([0, 0, 0, 0]));
    assert_eq!(f.instance("probe")["state"], "running");
}

#[test]
fn balanced_slots_survive_replacement_and_freed_slots_are_reused() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::with("balanced-hotplug", |options| options.workers = 4);
    f.load_module("thread_probe");
    for index in 0..8 {
        let name = format!("probe{index}");
        f.add(spec(&name, "thread_probe", 2.0, &json!({"trace": dir.path().join(&name)}).to_string(), &[]));
        assert_eq!(f.instance(&name)["worker"], index % 4);
    }
    assert_eq!(f.host.health()["workers"]["sticky"], json!([2, 2, 2, 2]));
    let home = f.instance("probe2")["worker"].clone();
    let tid = f.detail("probe2")["tid"].clone();
    f.host.configure_instance("probe2", json!({"trace": dir.path().join("probe2")}).to_string()).unwrap();
    for _ in 0..5 {
        let replaced = f.host.replace_instance("probe2", None, None, None).unwrap();
        assert_eq!(replaced["worker"], home);
        assert_eq!(f.detail("probe2")["tid"], tid);
        assert_eq!(f.host.health()["workers"]["sticky"], json!([2, 2, 2, 2]));
    }
    f.host.stop_instance("probe2").unwrap();
    f.host.start_instance("probe2").unwrap();
    wait_until("replacement steps", TIMEOUT, || count(&f.instance("probe2")["steps"]) >= 10);
    f.host.remove_instance("probe2").unwrap();
    assert_eq!(json!(assert_trace(&dir.path().join("probe2"))), tid);
    assert_eq!(f.host.health()["workers"]["sticky"], json!([2, 2, 1, 2]));
    f.add(spec("fresh", "thread_probe", 0.0, "{}", &[]));
    assert_eq!(f.instance("fresh")["worker"], home);
    assert_eq!(f.host.health()["workers"]["sticky"], json!([2, 2, 2, 2]));
}

#[test]
fn hung_worker_delays_only_its_sticky_lane_and_replacement_keeps_the_slot() {
    let f = Fixture::new("sticky-hang");
    f.load_module("thread_probe");
    f.load_module("hang");
    let mut hung = spec("hung", "hang", 0.0, r#"{"hang_after":1,"hang_ms":700}"#, &[]);
    hung.step_budget_ns = Some(10_000_000);
    hung.hang_limit_ns = Some(400_000_000);
    f.add(hung); // home 0; enable its timer only after the neighbours are running.
    f.add(spec("other", "thread_probe", 2.0, "{}", &[])); // home 1
    f.add(spec("neighbour", "thread_probe", 2.0, r#"{"strict":false}"#, &[])); // home 0
    let mut any = spec("free", "thread_probe", 2.0, r#"{"strict":false}"#, &[]);
    any.affinity = Affinity::Any;
    f.add(any);
    wait_until("neighbour started", TIMEOUT, || count(&f.instance("neighbour")["steps"]) >= 10);
    let old_tid = f.detail("neighbour")["tid"].clone();
    f.host.set_timing("hung", Some(2_000_000), None, None).unwrap();
    wait_until("hung call entered", TIMEOUT, || count(&f.channel("hung.out")["commits"]) == 1);
    let neighbour_steps = count(&f.instance("neighbour")["steps"]);
    let other_steps = count(&f.instance("other")["steps"]);
    let any_steps = count(&f.instance("free")["steps"]);
    sleep_ms(60);
    assert_eq!(f.instance("hung")["state"], "running");
    assert_eq!(count(&f.instance("neighbour")["steps"]), neighbour_steps);
    assert!(count(&f.instance("other")["steps"]) > other_steps);
    assert!(count(&f.instance("free")["steps"]) > any_steps);
    wait_until("isolation and lane recovery", TIMEOUT, || {
        f.instance("hung")["state"] == "isolated" && count(&f.instance("neighbour")["steps"]) > neighbour_steps + 10
    });
    assert_eq!(f.instance("neighbour")["worker"], 0);
    assert_eq!(f.instance("neighbour")["worker_moves"], 1);
    assert_ne!(f.detail("neighbour")["tid"], old_tid);
    assert_eq!(f.instance("other")["worker_moves"], 0);
    assert_eq!(f.host.health()["workers"]["live"], 2);
    assert_eq!(f.host.health()["workers"]["abandoned"], 1);
    // Let the finite hung call return and its abandoned thread exit before teardown.
    sleep_ms(750);
    assert_eq!(f.instance("neighbour")["worker_moves"], 1);
}
