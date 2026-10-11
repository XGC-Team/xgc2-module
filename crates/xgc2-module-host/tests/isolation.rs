//! A module call that does not return: the instance is isolated, a replacement worker keeps
//! the pool at full size, the other instances keep running.

mod common;

use common::*;
use std::time::{Duration, Instant};
use xgc2_module_host::host::{HostError, InstanceSpec};

fn hanging(name: &str, config: &str, input: &str, output: &str) -> InstanceSpec {
    let mut spec = spec(name, "hang", 0.0, config, &[("in", input), ("out", output)]);
    spec.hang_limit_ns = Some(100_000_000);
    spec
}

fn setup(f: &Fixture) {
    f.load_module("producer_state");
    f.load_module("consumer");
    f.load_module("hang");
    f.add(spec("cb", "consumer", 0.0, "{}", &[("state_in", "b")]));
    f.add(spec("pb", "producer_state", 2.0, r#"{"id":2}"#, &[("out", "b")]));
    f.add(spec("reader", "consumer", 0.0, "{}", &[("state_in", "hang_out")]));
    f.add(spec("src", "producer_state", 2.0, r#"{"id":1}"#, &[("out", "hang_in")]));
}

fn workers(f: &Fixture) -> (u64, u64, u64) {
    let w = &f.host.health()["workers"];
    (count(&w["configured"]), count(&w["live"]), count(&w["abandoned"]))
}

#[test]
fn a_hung_instance_is_isolated_and_everything_else_keeps_running() {
    let f = Fixture::new("hang");
    setup(&f);
    f.add(hanging("h", r#"{"hang_after":3}"#, "hang_in", "hang_out"));
    let started = Instant::now();
    wait_until("isolation", Duration::from_secs(5), || f.instance("h")["state"] == "isolated");
    let detection = started.elapsed();
    assert!(detection < Duration::from_millis(600), "isolation took {detection:?} for a 100 ms hang limit");
    let h = f.instance("h");
    assert_eq!(h["health"], "failed");
    assert!(h["last_error"].as_str().unwrap().contains("isolated"), "{h}");
    // The pool replaced the stuck worker: same number of live workers, one abandoned.
    assert_eq!(workers(&f), (2, 2, 1));
    // Outputs are stale, readers still get the last sample; the entity is not ready.
    wait_until("stale", Duration::from_secs(2), || f.channel("hang_out")["stale"] == true);
    let (ready, facts) = f.host.describe();
    assert!(!ready && facts["not_ready"].to_string().contains("instance h is isolated"), "{facts}");
    // Bystanders are untouched.
    let steps = count(&f.instance("cb")["steps"]);
    wait_until("bystander steps", Duration::from_secs(5), || count(&f.instance("cb")["steps"]) > steps + 20);
    assert_eq!(f.instance("cb")["state"], "running");
    // The upstream producer is not held back by the isolated reader either: its pin and
    // reader slot were released.
    assert_eq!(f.channel("hang_in")["readers"], 0);
    let commits = count(&f.channel("hang_in")["commits"]);
    wait_until("producer continues", Duration::from_secs(5), || count(&f.channel("hang_in")["commits"]) > commits + 20);
    assert_eq!(count(&f.channel("hang_in")["stalls"]), 0);
    // The library cannot be unloaded while the instance exists, nor afterwards.
    let error = f.host.unload_module("hang").unwrap_err();
    assert!(error.to_string().contains("used by instance(s) h"), "{error}");
    // Removing an isolated instance does not call into the stuck module.
    let removing = Instant::now();
    f.host.remove_instance("h").unwrap();
    assert!(removing.elapsed() < Duration::from_millis(500), "removal waited for the hung call");
    assert!(f.host.health()["instances"].as_array().unwrap().iter().all(|i| i["name"] != "h"));
    let error = f.host.unload_module("hang").unwrap_err();
    assert!(matches!(error, HostError::Conflict(_)) && error.to_string().contains("pinned"), "{error}");
    assert_eq!(f.host.modules()["modules"].as_array().unwrap().iter().find(|m| m["module"] == "hang").unwrap()["pinned"], true);
    // The slot is free for a new instance, which gets worker time like everyone else.
    f.add(spec("fresh", "consumer", 0.0, "{}", &[("state_in", "b")]));
    wait_until("fresh instance steps", Duration::from_secs(5), || count(&f.instance("fresh")["steps"]) > 3);
    assert_eq!(workers(&f), (2, 2, 1));
}

#[test]
fn a_single_worker_pool_survives_a_hang() {
    let f = Fixture::with("one-worker", |options| options.workers = 1);
    setup(&f);
    wait_until("flowing", Duration::from_secs(5), || count(&f.instance("cb")["steps"]) > 5);
    f.add(hanging("h", r#"{"hang_after":2}"#, "hang_in", "hang_out"));
    wait_until("isolation", Duration::from_secs(5), || f.instance("h")["state"] == "isolated");
    // Until the watchdog spawned a replacement nobody could run; now the others do.
    let steps = count(&f.instance("cb")["steps"]);
    wait_until("bystander resumes", Duration::from_secs(5), || count(&f.instance("cb")["steps"]) > steps + 20);
    assert_eq!(workers(&f), (1, 1, 1));
}

#[test]
fn a_hung_call_that_finally_returns_changes_nothing() {
    let f = Fixture::new("late-return");
    setup(&f);
    // Sleeps 400 ms in its 3rd step, 300 ms past the hang limit.
    f.add(hanging("h", r#"{"hang_after":3,"hang_ms":400}"#, "hang_in", "hang_out"));
    wait_until("isolation", Duration::from_secs(5), || f.instance("h")["state"] == "isolated");
    let steps = count(&f.instance("h")["steps"]);
    let commits = count(&f.channel("hang_out")["commits"]);
    sleep_ms(700);
    // The call returned long ago; the instance stays isolated and writes nothing more.
    let h = f.instance("h");
    assert_eq!(h["state"], "isolated");
    assert_eq!(count(&h["steps"]), steps);
    assert_eq!(count(&f.channel("hang_out")["commits"]), commits);
    assert_eq!(workers(&f), (2, 2, 1));
    f.host.remove_instance("h").unwrap();
    assert!(f.host.is_ready());
}

#[test]
fn every_hang_costs_a_worker_but_not_capacity() {
    let f = Fixture::new("repeated");
    setup(&f);
    for round in 1..=3u64 {
        let name = format!("h{round}");
        f.add(hanging(&name, r#"{"hang_after":1}"#, "hang_in", &format!("out{round}")));
        wait_until("isolation", Duration::from_secs(5), || f.instance(&name)["state"] == "isolated");
        assert_eq!(workers(&f), (2, 2, round));
        f.host.remove_instance(&name).unwrap();
    }
    let steps = count(&f.instance("cb")["steps"]);
    wait_until("still running", Duration::from_secs(5), || count(&f.instance("cb")["steps"]) > steps + 20);
}

#[test]
fn a_replacement_waits_for_nothing_when_the_old_instance_is_stuck() {
    let f = Fixture::new("replace-stuck");
    setup(&f);
    f.add(hanging("h", r#"{"hang_after":3}"#, "hang_in", "hang_out"));
    wait_until("isolation", Duration::from_secs(5), || f.instance("h")["state"] == "isolated");
    let error = f.host.replace_instance("h", None, None, None).unwrap_err();
    assert!(matches!(error, HostError::Conflict(_)) && error.to_string().contains("isolated; remove it first"), "{error}");
}
