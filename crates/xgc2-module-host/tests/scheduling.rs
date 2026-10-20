//! When instances run: period timers, wake() from module threads, the external clock,
//! step budgets, live configure and stop/start.

mod common;

use common::*;
use std::time::{Duration, Instant};
use xgc2_module_host::clock::Mode;
use xgc2_module_host::host::ClockSpec;

#[test]
fn period_timer_steps_an_instance_at_its_rate() {
    let f = Fixture::new("timers");
    f.load_module("producer_state");
    f.add(spec("producer", "producer_state", 5.0, "{}", &[]));
    let started = Instant::now();
    sleep_ms(500);
    let health = f.instance("producer");
    let elapsed = started.elapsed().as_secs_f64();
    let steps = count(&health["steps"]);
    // 5 ms period: about 100 steps in 0.5 s; a shared machine may lose some but never gain.
    assert!((60..=102).contains(&steps), "{steps} steps in {elapsed:.3} s");
    assert!(count(&health["timer_fires"]) >= steps, "{health}");
    assert_eq!(count(&health["step_time"]["count"]), steps);
    assert!(count(&health["step_time"]["p99_ns"]) < 5_000_000, "{health}");
    assert_eq!(health["period_ns"], 5_000_000);
    // Changing the period live takes effect without restarting the instance.
    f.host.set_timing("producer", Some(20_000_000), None, None).unwrap();
    let before = count(&f.instance("producer")["steps"]);
    sleep_ms(400);
    let after = count(&f.instance("producer")["steps"]);
    assert!((12..=21).contains(&(after - before)), "{} steps in 400 ms at 20 ms", after - before);
}

#[test]
fn a_module_can_choose_its_own_period_in_start() {
    let f = Fixture::new("module-period");
    f.load_module("producer_state");
    // The manifest gives no period; the module asks for 4 ms from start().
    f.add(spec("chosen", "producer_state", 0.0, r#"{"period_us":4000}"#, &[]));
    // The module's request replaces the period of the manifest, too.
    f.add(spec("overridden", "producer_state", 500.0, r#"{"period_us":4000}"#, &[]));
    f.add(spec("untouched", "producer_state", 0.0, "{}", &[]));
    sleep_ms(400);
    for name in ["chosen", "overridden"] {
        let health = f.instance(name);
        assert_eq!(health["period_ns"], 4_000_000, "{name}");
        let steps = count(&health["steps"]);
        assert!((30..=101).contains(&steps), "{name}: {steps} steps in 400 ms at 4 ms");
    }
    let health = f.instance("untouched");
    assert_eq!((health["period_ns"].as_i64(), count(&health["steps"])), (Some(0), 0), "no timer, nothing to run it: {health}");
    // A period of 0 disarms the timer.
    f.host.set_timing("chosen", Some(0), None, None).unwrap();
    sleep_ms(30);
    let before = count(&f.instance("chosen")["steps"]);
    sleep_ms(100);
    assert_eq!(count(&f.instance("chosen")["steps"]), before, "no step after the timer was disarmed");
}

#[test]
fn a_module_thread_wakes_its_instance_and_writes_asynchronously() {
    let f = Fixture::new("wake");
    f.load_module("wake_thread");
    f.load_module("consumer");
    f.add(spec("solver", "wake_thread", 0.0, r#"{"interval_us":2000}"#, &[("ticks", "ticks")]));
    f.add(spec("reader", "consumer", 0.0, "{}", &[("state_in", "ticks")]));
    wait_until("wake steps", Duration::from_secs(10), || count(&f.detail("solver")["wake_steps"]) >= 20);
    let solver = f.instance("solver");
    assert!(count(&solver["wakeups"]) >= 20, "{solver}");
    assert_eq!(count(&solver["timer_fires"]), 0);
    // The thread's samples reach the reader in place, through the async writer port.
    wait_until("reader sees ticks", Duration::from_secs(10), || count(&f.detail("reader")["state_updates"]) >= 10);
    assert_eq!(count(&f.detail("reader")["zero_copy_bad"]), 0);
    // stop() joins the thread; afterwards nothing is stepped or written any more.
    f.host.stop_instance("solver").unwrap();
    let commits = count(&f.channel("ticks")["commits"]);
    sleep_ms(50);
    assert_eq!(count(&f.channel("ticks")["commits"]), commits);
    assert_eq!(f.instance("solver")["state"], "stopped");
    f.host.start_instance("solver").unwrap();
    wait_until("ticks resume", Duration::from_secs(10), || count(&f.channel("ticks")["commits"]) > commits + 5);
}

#[test]
fn external_clock_runs_timers_on_simulated_time() {
    let f = Fixture::with("sim", |options| {
        options.clock = ClockSpec { mode: Mode::External, channel: Some("clock".into()), phase_t0_ns: None };
    });
    f.load_module("sim_clock");
    f.load_module("producer_state");
    // Not ready until the first time sample arrives.
    f.add(spec("tick", "producer_state", 100.0, "{}", &[]));
    let (ready, facts) = f.host.describe();
    assert!(!ready, "{facts}");
    assert_eq!(facts["clock"]["mode"], "external");
    assert_eq!(facts["clock"]["valid"], false);
    assert!(facts["not_ready"].to_string().contains("external clock"));
    sleep_ms(50);
    assert_eq!(count(&f.instance("tick")["steps"]), 0, "no time, no timer");
    // 50 samples, 10 simulated ms apart, one per wall millisecond: 500 simulated ms in ~50 ms.
    let started = Instant::now();
    f.add(spec("clock", "sim_clock", 0.0, r#"{"step_ms":10,"interval_us":1000,"count":50}"#, &[("time", "clock")]));
    wait_until("simulation finished", Duration::from_secs(10), || f.host.describe().1["clock"]["now_ns"] == 500_000_000);
    let wall = started.elapsed();
    sleep_ms(50);
    let tick = f.instance("tick");
    // The first sample (10 ms) anchors the timer; expirations at 110, 210, 310 and 410 ms.
    assert_eq!(count(&tick["timer_fires"]), 4, "{tick}");
    assert_eq!(count(&tick["steps"]), 4, "{tick}");
    assert!(wall < Duration::from_millis(450), "simulated 500 ms took {wall:?} of wall time");
    let (ready, facts) = f.host.describe();
    assert!(ready, "{facts}");
    assert_eq!(facts["clock"]["valid"], true);
    // The producer's stamps are in simulated time.
    assert_eq!(f.channel("tick.out")["commits"], 4);
}

#[test]
fn a_step_over_budget_degrades_the_instance_and_it_recovers() {
    let f = Fixture::new("budget");
    f.load_module("slow");
    f.add(spec("slow", "slow", 10.0, r#"{"sleep_us":30000}"#, &[]));
    wait_until("overruns", Duration::from_secs(10), || count(&f.instance("slow")["overruns"]) >= 3);
    let slow = f.instance("slow");
    assert_eq!(slow["health"], "degraded");
    assert_eq!(slow["state"], "running", "over budget is reported, not fatal");
    assert!(count(&slow["missed_periods"]) > 0, "periods that passed during a slow step are counted: {slow}");
    assert!(count(&slow["step_time"]["p50_ns"]) >= 30_000_000);
    assert!(f.host.is_ready(), "a degraded instance is still running");
    // A live configure makes the step fast again: the next in-budget step clears the flag.
    f.host.configure_instance("slow", r#"{"sleep_us":100}"#.into()).unwrap();
    wait_until("recovered", Duration::from_secs(10), || f.instance("slow")["health"] == "ok");
    assert_eq!(f.detail("slow")["sleep_us"], 100);
}

#[test]
fn live_configure_is_applied_between_steps_and_visible_to_the_next_one() {
    let f = Fixture::new("configure");
    f.load_module("consumer");
    f.load_module("producer_state");
    f.add(spec("producer", "producer_state", 5.0, "{}", &[("out", "s")]));
    f.add(spec("consumer", "consumer", 0.0, r#"{"work_us":0}"#, &[("state_in", "s")]));
    wait_until("running", Duration::from_secs(5), || count(&f.detail("consumer")["steps"]) > 3);
    assert_eq!(count(&f.detail("consumer")["config_steps"]), 0);
    f.host.configure_instance("consumer", r#"{"work_us":200}"#.into()).unwrap();
    wait_until("configure step", Duration::from_secs(5), || count(&f.detail("consumer")["config_steps"]) == 1);
    assert_eq!(f.detail("consumer")["work_us"], 200);
    // The module can refuse a configuration; the old one stays in force.
    f.load_module("slow");
    f.add(spec("slow", "slow", 20.0, r#"{"sleep_us":10}"#, &[]));
    let error = f.host.configure_instance("slow", r#"{"reject":1}"#.into()).unwrap_err();
    assert!(error.to_string().contains("configure returned status 1"), "{error}");
    assert_eq!(f.instance("slow")["state"], "running");
    sleep_ms(60);
    assert_eq!(f.detail("slow")["sleep_us"], 10);
}

#[test]
fn stopped_instances_do_not_step_and_do_not_hold_event_writers_back() {
    let f = Fixture::new("stop-start");
    f.load_module("producer_event");
    f.load_module("consumer");
    f.add(spec("producer", "producer_event", 2.0, r#"{"burst":4}"#, &[("out", "bus")]));
    f.add(spec("consumer", "consumer", 0.0, "{}", &[("event_in", "bus")]));
    wait_until("events flow", Duration::from_secs(5), || count(&f.detail("consumer")["events"]) > 40);
    f.host.stop_instance("consumer").unwrap();
    assert_eq!(f.instance("consumer")["state"], "stopped");
    let steps = count(&f.instance("consumer")["steps"]);
    // Nobody reads: a stopped reader releases its cursor, so the writer is never throttled.
    sleep_ms(100);
    assert_eq!(count(&f.channel("bus")["drops"]), 0);
    assert_eq!(f.channel("bus")["readers"], 0);
    assert_eq!(count(&f.instance("consumer")["steps"]), steps);
    f.host.start_instance("consumer").unwrap();
    let events = count(&f.detail("consumer")["events"]);
    wait_until("events resume", Duration::from_secs(5), || count(&f.detail("consumer")["events"]) > events + 20);
    assert_eq!(count(&f.detail("consumer")["event_disorder"]), 0);
}

#[test]
fn a_failing_step_marks_the_instance_failed_and_its_outputs_stale() {
    let f = Fixture::new("failure");
    f.load_module("slow");
    f.load_module("producer_state");
    f.load_module("passthrough");
    f.add(spec("producer", "producer_state", 5.0, "{}", &[("out", "in")]));
    f.add(spec("stage", "passthrough", 0.0, "{}", &[("in", "in"), ("out", "out")]));
    wait_until("flowing", Duration::from_secs(5), || count(&f.channel("out")["commits"]) > 3);
    // slow.c fails once, at its 3rd step.
    f.add(spec("flaky", "slow", 5.0, r#"{"fail_at":3}"#, &[]));
    wait_until("failure", Duration::from_secs(5), || f.instance("flaky")["state"] == "failed");
    let flaky = f.instance("flaky");
    assert_eq!(flaky["health"], "failed");
    assert!(flaky["last_error"].as_str().unwrap().contains("internal error"));
    assert!(!f.host.is_ready(), "a required instance failed");
    let steps = count(&flaky["steps"]);
    sleep_ms(50);
    assert_eq!(count(&f.instance("flaky")["steps"]), steps, "a failed instance is not stepped");
    // The rest of the entity is unaffected.
    assert_eq!(f.instance("stage")["state"], "running");
    // stop + start restarts it (the failure was a one-time condition).
    f.host.stop_instance("flaky").unwrap();
    f.host.start_instance("flaky").unwrap();
    wait_until("flaky runs again", Duration::from_secs(5), || count(&f.instance("flaky")["steps"]) > steps + 3);
    assert_eq!(f.instance("flaky")["health"], "ok");
}

#[test]
fn outputs_of_a_failed_producer_are_flagged_stale_for_their_readers() {
    let f = Fixture::new("stale");
    f.load_module("producer_state");
    f.load_module("consumer");
    f.add(spec("producer", "producer_state", 5.0, r#"{"fail_at":5}"#, &[("out", "samples")]));
    f.add(spec("consumer", "consumer", 0.0, "{}", &[("state_in", "samples")]));
    wait_until("producer failed", Duration::from_secs(5), || f.instance("producer")["state"] == "failed");
    let channel = f.channel("samples");
    assert_eq!(channel["stale"], true);
    assert_eq!(count(&channel["commits"]), 5);
    // The last sample stays readable; a reader that looks at it again is counted.
    f.host.stop_instance("consumer").unwrap();
    f.host.start_instance("consumer").unwrap();
    wait_until("stale read", Duration::from_secs(5), || count(&f.channel("samples")["stale_reads"]) > 0);
    assert_eq!(f.detail("consumer")["last_state_seq"], 5);
    // Restarting the producer publishes again and clears the flag.
    f.host.stop_instance("producer").unwrap();
    f.host.configure_instance("producer", r#"{"fail_at":0}"#.into()).unwrap();
    f.host.start_instance("producer").unwrap();
    wait_until("fresh samples", Duration::from_secs(5), || f.channel("samples")["stale"] == false);
}

#[test]
fn a_module_can_report_itself_degraded_or_failed() {
    let f = Fixture::new("report");
    f.load_module("slow");
    f.add(spec("degraded", "slow", 5.0, r#"{"report_at":2,"report_health":1}"#, &[]));
    f.add(spec("failing", "slow", 5.0, r#"{"report_at":2,"report_health":2}"#, &[]));
    wait_until("reports", Duration::from_secs(5), || {
        f.instance("degraded")["health"] == "degraded" && f.instance("failing")["state"] == "failed"
    });
    assert_eq!(f.instance("degraded")["state"], "running");
    assert!(f.instance("failing")["last_error"].as_str().unwrap().contains("reported failure"));
    assert_eq!(f.instance("degraded")["reported"]["health"], 1);
}
