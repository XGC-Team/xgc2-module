//! ABI minor 1 at the host: wake_at_ns deadlines, the utc clock, phase_t0_ns and the timer grid.

mod common;

use common::*;
use std::time::Duration;
use xgc2_module_host::clock::Mode;
use xgc2_module_host::host::{ClockSpec, HostError, HostOptions, ModuleHost};

fn probe(f: &Fixture, name: &str, config: &str) {
    f.load_module("deadline_probe");
    f.add(spec(name, "deadline_probe", 0.0, config, &[]));
}

fn utc(phase: Option<i64>) -> impl FnOnce(&mut HostOptions) {
    move |options| options.clock = ClockSpec { mode: Mode::Utc, channel: None, phase_t0_ns: phase }
}

#[test]
fn a_deadline_wakes_the_instance_once_and_never_early() {
    let f = Fixture::new("deadline");
    probe(&f, "p", r#"{"deadline_after_ms":30}"#);
    wait_until("the deadline step", Duration::from_secs(5), || count(&f.detail("p")["deadline_steps"]) >= 1);
    sleep_ms(100);
    let detail = f.detail("p");
    assert_eq!(count(&detail["deadline_steps"]), 1, "{detail}");
    assert_eq!(count(&detail["timer_steps"]), 0, "a deadline is not a timer expiration: {detail}");
    assert_eq!(count(&detail["wake_steps"]), 0, "a deadline is not a wake(): {detail}");
    let late = detail["min_late_ns"].as_i64().unwrap();
    assert!(late >= 0, "the step ran {late} ns before the requested instant");
    assert!(detail["max_late_ns"].as_i64().unwrap() < 200_000_000, "{detail}");
}

#[test]
fn a_chain_of_deadlines_keeps_the_instance_stepping() {
    let f = Fixture::new("chain");
    probe(&f, "p", r#"{"deadline_after_ms":5,"rearm_ms":10}"#);
    sleep_ms(400);
    let detail = f.detail("p");
    let steps = count(&detail["deadline_steps"]);
    assert!((10..=45).contains(&steps), "{detail}");
    assert!(detail["min_late_ns"].as_i64().unwrap() >= 0, "{detail}");
    assert_eq!(count(&detail["timer_steps"]), 0);
}

#[test]
fn a_new_request_replaces_the_old_one_and_a_negative_one_cancels() {
    let f = Fixture::new("replace");
    probe(&f, "r", r#"{"replace_after_ms":20}"#);
    probe_more(&f, "c", r#"{"cancel":1}"#);
    wait_until("the replacing deadline", Duration::from_secs(5), || count(&f.detail("r")["deadline_steps"]) == 1);
    sleep_ms(150);
    assert_eq!(count(&f.detail("r")["deadline_steps"]), 1, "the replaced hour-long deadline must not fire");
    assert_eq!(f.detail("c"), serde_json::Value::Null, "the cancelled deadline must not step the instance: it never stepped");
}

fn probe_more(f: &Fixture, name: &str, config: &str) {
    f.add(spec(name, "deadline_probe", 0.0, config, &[]));
}

#[test]
fn a_deadline_in_the_past_fires_at_once() {
    let f = Fixture::new("past");
    probe(&f, "p", r#"{"past":1}"#);
    wait_until("the deadline step", Duration::from_secs(5), || count(&f.detail("p")["deadline_steps"]) >= 1);
}

#[test]
fn the_utc_clock_reads_the_wall_clock_and_announces_its_phase() {
    let f = Fixture::with("utc", utc(Some(1_760_000_000_000_000_037)));
    probe(&f, "p", r#"{"deadline_after_ms":10}"#);
    wait_until("the deadline step", Duration::from_secs(5), || count(&f.detail("p")["deadline_steps"]) >= 1);
    let detail = f.detail("p");
    assert_eq!(detail["phase_t0_ns"], 1_760_000_000_000_000_037i64);
    // Nanoseconds of UTC (after 2020), not an uptime.
    assert!(detail["last_now_ns"].as_i64().unwrap() > 1_577_836_800_000_000_000, "{detail}");
    let (_, facts) = f.host.describe();
    assert_eq!(facts["clock"]["mode"], "utc");
    assert_eq!(facts["clock"]["valid"], true);
    assert_eq!(facts["clock"]["phase_t0_ns"], 1_760_000_000_000_000_037i64);
}

#[test]
fn utc_without_a_phase_has_a_grid_of_multiples_of_the_period() {
    let f = Fixture::with("utc0", utc(None));
    probe(&f, "p", r#"{"deadline_after_ms":10}"#);
    wait_until("the deadline step", Duration::from_secs(5), || count(&f.detail("p")["deadline_steps"]) >= 1);
    assert_eq!(f.detail("p")["phase_t0_ns"], 0);
    assert_eq!(f.host.describe().1["clock"]["phase_t0_ns"], 0);
}

#[test]
fn a_steady_host_has_no_phase() {
    let f = Fixture::new("steady");
    probe(&f, "p", r#"{"deadline_after_ms":10}"#);
    wait_until("the deadline step", Duration::from_secs(5), || count(&f.detail("p")["deadline_steps"]) >= 1);
    assert_eq!(f.detail("p")["phase_t0_ns"], 0);
    assert!(f.host.describe().1["clock"]["phase_t0_ns"].is_null());
    let mut options = HostOptions::new("bad");
    options.clock = ClockSpec { mode: Mode::Steady, channel: None, phase_t0_ns: Some(5) };
    match ModuleHost::start(options) {
        Err(HostError::Invalid(message)) => assert!(message.contains("no timer grid"), "{message}"),
        other => panic!("a steady clock with a phase must be refused, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn the_simulated_clock_timer_follows_the_phase_grid() {
    // T0 = 50 ms, period 100 ms, samples 10 ms apart: expirations at 50, 150, 250, 350 and 450 ms
    // (five). Without a phase the first sample anchors the timer and there are four (see
    // scheduling.rs, external_clock_runs_timers_on_simulated_time).
    let f = Fixture::with("simgrid", |options| {
        options.clock = ClockSpec { mode: Mode::External, channel: Some("clock".into()), phase_t0_ns: Some(50_000_000) };
    });
    f.load_module("sim_clock");
    f.load_module("producer_state");
    f.add(spec("tick", "producer_state", 100.0, "{}", &[]));
    sleep_ms(30);
    f.add(spec("clock", "sim_clock", 0.0, r#"{"step_ms":10,"interval_us":2000,"count":50}"#, &[("time", "clock")]));
    wait_until("simulation finished", Duration::from_secs(10), || f.host.describe().1["clock"]["now_ns"] == 500_000_000);
    sleep_ms(50);
    let tick = f.instance("tick");
    assert_eq!(count(&tick["timer_fires"]), 5, "{tick}");
    assert_eq!(f.host.describe().1["clock"]["phase_t0_ns"], 50_000_000);
}
