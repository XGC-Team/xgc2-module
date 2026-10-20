//! Hot-plug: change one instance while the others keep running.

mod common;

use common::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xgc2_module_host::host::HostError;

/// Two independent chains: `pa -> stage -> ca` (the one that gets modified) and
/// `pb -> cb` (the bystander).
fn two_chains(f: &Fixture) {
    f.load_module("producer_state");
    f.load_module("consumer");
    f.load("pass_1", &module_version("passthrough", 1));
    f.load("pass_2", &module_version("passthrough", 2));
    f.add(spec("cb", "consumer", 0.0, "{}", &[("state_in", "b")]));
    f.add(spec("pb", "producer_state", 2.0, r#"{"id":2}"#, &[("out", "b")]));
    f.add(spec("ca", "consumer", 0.0, "{}", &[("state_in", "a_out")]));
    f.add(spec("stage", "pass_1", 0.0, "{}", &[("in", "a_in"), ("out", "a_out")]));
    f.add(spec("pa", "producer_state", 2.0, r#"{"id":1}"#, &[("out", "a_in")]));
}

/// Watches a counter and records the longest time it stood still.
struct Stall {
    stop: Arc<AtomicBool>,
    longest_ms: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Stall {
    fn watch(f: &Fixture, instance: &'static str) -> Stall {
        let (stop, longest_ms) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)));
        let host = f.host.clone();
        let (flag, longest) = (stop.clone(), longest_ms.clone());
        let thread = std::thread::spawn(move || {
            let mut last = count(&instance_steps(&host, instance));
            let mut since = Instant::now();
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
                let now = count(&instance_steps(&host, instance));
                if now != last {
                    last = now;
                    since = Instant::now();
                }
                longest.fetch_max(since.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
        });
        Stall { stop, longest_ms, thread: Some(thread) }
    }

    fn finish(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        self.longest_ms.load(Ordering::Relaxed)
    }
}

fn instance_steps(host: &xgc2_module_host::host::ModuleHost, name: &str) -> serde_json::Value {
    instance(host, name)["steps"].clone()
}

#[test]
fn replacing_an_instance_under_load_does_not_disturb_the_others() {
    let f = Fixture::new("replace");
    two_chains(&f);
    wait_until("both chains flow", Duration::from_secs(10), || {
        count(&f.detail("ca")["state_updates"]) > 20 && count(&f.detail("cb")["state_updates"]) > 20
    });
    let bystander = Stall::watch(&f, "cb");
    let producer = Stall::watch(&f, "pa");
    let mut longest_swap = Duration::ZERO;
    for round in 0..20 {
        let module = if round % 2 == 0 { "pass_2" } else { "pass_1" };
        let started = Instant::now();
        f.host.replace_instance("stage", Some(module), None, None).unwrap_or_else(|e| panic!("round {round}: {e}"));
        longest_swap = longest_swap.max(started.elapsed());
        let version = if round % 2 == 0 { 2 } else { 1 };
        wait_until("replacement steps", Duration::from_secs(5), || f.detail("stage")["version"] == version);
    }
    let bystander_stall = bystander.finish();
    let producer_stall = producer.finish();
    assert!(longest_swap < Duration::from_millis(500), "a replacement took {longest_swap:?}");
    assert!(bystander_stall < 100, "the bystander chain stood still for {bystander_stall} ms");
    assert!(producer_stall < 100, "the producer upstream of the replaced stage stood still for {producer_stall} ms");
    // The replaced chain kept its data: sequence never went backwards, nothing was copied.
    wait_until("chain A still flows", Duration::from_secs(5), || count(&f.detail("ca")["state_updates"]) > 100);
    let ca = f.detail("ca");
    assert_eq!(count(&ca["bad_seq"]), 0, "{ca}");
    assert_eq!(count(&ca["zero_copy_bad"]), 0, "{ca}");
    assert_eq!(count(&f.detail("cb")["bad_seq"]), 0);
    assert!(f.host.is_ready());
    // Channels survived every swap with their reader and writer registrations intact.
    for (name, readers, writers) in [("a_in", 1, 1), ("a_out", 1, 1), ("b", 1, 1)] {
        let channel = f.channel(name);
        assert_eq!((channel["readers"].as_u64(), channel["writers"].as_u64()), (Some(readers), Some(writers)), "{channel}");
    }
}

#[test]
fn the_event_backlog_moves_to_the_replacement() {
    let f = Fixture::new("backlog");
    f.load_module("producer_event");
    f.load_module("consumer");
    // Preserve this test's free-dispatch setup: replacement create must finish while the
    // old consumer is in its first slow step, before it can drain the measured backlog.
    let mut consumer = spec("consumer", "consumer", 0.0, r#"{"work_us":150000}"#, &[("event_in", "bus")]);
    consumer.affinity = xgc2_module_host::scheduler::Affinity::Any;
    f.add(consumer);
    let mut producer = spec("producer", "producer_event", 10.0, r#"{"burst":3}"#, &[("out", "bus")]);
    producer.affinity = xgc2_module_host::scheduler::Affinity::Any;
    f.add(producer);
    // The consumer is inside its first (slow) step once it has reported; later events pile up.
    wait_until("first step", Duration::from_secs(5), || count(&f.detail("consumer")["events"]) > 0);
    sleep_ms(35);
    f.host.stop_instance("producer").unwrap();
    let sent = count(&f.detail("producer")["sent"]);
    let seen_by_old = count(&f.detail("consumer")["events"]);
    assert!(seen_by_old < sent, "the test needs unread events: {seen_by_old} of {sent}");
    f.host.replace_instance("consumer", None, None, None).unwrap();
    wait_until("replacement reads the backlog", Duration::from_secs(5), || count(&f.detail("consumer")["events"]) > 0);
    // Exactly the events the old instance had not read; no loss, no duplicate.
    assert_eq!(count(&f.detail("consumer")["events"]), sent - seen_by_old);
    assert_eq!(count(&f.detail("consumer")["event_disorder"]), 0);
}

#[test]
fn a_replacement_that_does_not_fit_is_refused_and_the_old_instance_keeps_running() {
    let f = Fixture::new("incompatible");
    two_chains(&f);
    f.load_module("slow");
    f.load_module("producer_event");
    wait_until("flowing", Duration::from_secs(10), || count(&f.detail("ca")["state_updates"]) > 5);
    let refused = |module: &str| match f.host.replace_instance("stage", Some(module), None, None) {
        Err(HostError::Invalid(message)) => message,
        other => panic!("expected a refusal for {module}, got {other:?}"),
    };
    // `slow` has no `out` port, but channel a_out connects the stage to `ca`.
    let message = refused("slow");
    assert!(message.contains("has no port out") && message.contains("a_out"), "{message}");
    // `producer_event` has an event port named `out`; the channel carries state.
    let message = refused("producer_event");
    assert!(message.contains("event") && message.contains("a_out"), "{message}");
    let stage = f.instance("stage");
    assert_eq!(stage["state"], "running");
    assert_eq!(stage["module"], "pass_1");
    let before = count(&stage["steps"]);
    wait_until("stage keeps stepping", Duration::from_secs(5), || count(&f.instance("stage")["steps"]) > before + 5);
}

#[test]
fn a_replacement_that_cannot_start_restores_the_previous_instance() {
    let f = Fixture::new("restore");
    two_chains(&f);
    wait_until("flowing", Duration::from_secs(10), || count(&f.detail("ca")["state_updates"]) > 5);
    for (config, phase) in [(r#"{"fail_start":1}"#, "start"), (r#"{"fail_create":1}"#, "create")] {
        let error = f.host.replace_instance("stage", Some("pass_2"), Some(config.into()), None).unwrap_err();
        let text = error.to_string();
        assert!(text.contains(phase), "{text}");
        if phase == "start" {
            assert!(text.contains("previous instance was restored"), "{text}");
        }
        let stage = f.instance("stage");
        assert_eq!((stage["state"].as_str(), stage["module"].as_str()), (Some("running"), Some("pass_1")), "{stage}");
        let before = count(&f.detail("ca")["state_updates"]);
        wait_until("data still flows through the restored stage", Duration::from_secs(5), || {
            count(&f.detail("ca")["state_updates"]) > before + 10
        });
    }
    assert_eq!(count(&f.detail("ca")["bad_seq"]), 0);
    assert_eq!(f.detail("stage")["version"], 1);
    assert!(f.host.is_ready());
    // No instance or channel leaked from the failed attempts.
    assert_eq!(f.host.health()["instances"].as_array().unwrap().len(), 5);
    let mut channels: Vec<String> =
        f.host.health()["channels"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_owned()).collect();
    channels.sort();
    assert_eq!(channels, ["a_in", "a_out", "b"], "the discarded replacements left no channel behind");
    assert_eq!(f.channel("a_out")["writers"], 1);
}

#[test]
fn instances_come_and_go_while_the_rest_runs() {
    let f = Fixture::new("add-remove");
    f.load_module("producer_state");
    f.load_module("consumer");
    f.add(spec("producer", "producer_state", 2.0, "{}", &[("out", "s")]));
    f.add(spec("keep", "consumer", 0.0, "{}", &[("state_in", "s")]));
    let stall = Stall::watch(&f, "keep");
    for round in 0..10 {
        let name = format!("guest{round}");
        f.add(spec(&name, "consumer", 0.0, "{}", &[("state_in", "s")]));
        assert_eq!(f.channel("s")["readers"], 2);
        wait_until("guest reads", Duration::from_secs(5), || count(&f.detail(&name)["state_updates"]) > 2);
        f.host.remove_instance(&name).unwrap();
        assert_eq!(f.channel("s")["readers"], 1);
        assert!(matches!(f.host.remove_instance(&name), Err(HostError::NotFound(_))));
    }
    assert!(stall.finish() < 100);
    // The last port of a channel leaves: the channel goes with it.
    f.host.remove_instance("keep").unwrap();
    f.host.remove_instance("producer").unwrap();
    assert!(!has_channel(&f.host, "s"));
    assert!(!has_channel(&f.host, "producer.out"));
    assert_eq!(f.host.health()["instances"].as_array().unwrap().len(), 0);
}

#[test]
fn ports_are_rebound_live() {
    let f = Fixture::new("rebind");
    f.load_module("producer_state");
    f.load_module("consumer");
    f.load("pass_1", &module_version("passthrough", 1));
    f.add(spec("p1", "producer_state", 2.0, r#"{"id":1}"#, &[("out", "s1")]));
    f.add(spec("p2", "producer_state", 2.0, r#"{"id":2}"#, &[("out", "s2")]));
    f.add(spec("reader", "consumer", 0.0, "{}", &[("state_in", "s1")]));
    wait_until("reads producer 1", Duration::from_secs(5), || f.detail("reader")["last_producer"] == 1);
    f.host.bind_port("reader", "state_in", Some("s2")).unwrap();
    wait_until("reads producer 2", Duration::from_secs(5), || f.detail("reader")["last_producer"] == 2);
    assert_eq!((f.channel("s1")["readers"].as_u64(), f.channel("s2")["readers"].as_u64()), (Some(0), Some(1)));
    // Disconnecting the input stops the steps it caused.
    f.host.bind_port("reader", "state_in", None).unwrap();
    let steps = count(&f.instance("reader")["steps"]);
    sleep_ms(60);
    assert_eq!(count(&f.instance("reader")["steps"]), steps);
    assert_eq!(f.channel("s2")["readers"], 0);
    // A required input without a producer makes the entity not ready.
    f.add(spec("stage", "pass_1", 0.0, "{}", &[("in", "nowhere")]));
    let (ready, facts) = f.host.describe();
    assert!(!ready);
    assert!(facts["not_ready"].to_string().contains("required input in (channel nowhere) has no running producer"), "{facts}");
    f.host.bind_port("stage", "in", Some("s1")).unwrap();
    assert!(f.host.is_ready());
    // An output moves back to its private channel when unbound; the old channel loses its writer.
    f.host.bind_port("p1", "out", None).unwrap();
    assert!(has_channel(&f.host, "p1.out"));
    assert_eq!(f.channel("s1")["writers"], 0);
    assert!(!f.host.is_ready(), "the stage's required input lost its producer");
    // Type checks apply to rebinding too.
    let error = f.host.bind_port("reader", "state_in", Some("p1.out")).map(|_| ());
    assert!(error.is_ok());
    f.load_module("producer_event");
    f.add(spec("events", "producer_event", 0.0, "{}", &[("out", "ev")]));
    let error = f.host.bind_port("reader", "state_in", Some("ev")).unwrap_err();
    assert!(matches!(error, HostError::Invalid(_)) && error.to_string().contains("event"), "{error}");
    assert_eq!(f.instance("reader")["state"], "running");
}

#[test]
fn libraries_are_loaded_and_unloaded_at_runtime() {
    let f = Fixture::new("libraries");
    f.load_module("producer_state");
    let path = module("producer_state");
    // The same file cannot be loaded twice, even under another name: dlopen would hand back
    // the code that is already mapped.
    let error = f.host.load_module(Some("again"), &path, None).unwrap_err();
    assert!(matches!(error, HostError::Conflict(_)) && error.to_string().contains("already loaded"), "{error}");
    // A pin that does not match is refused before anything is mapped.
    let other = module("consumer");
    let error = f.host.load_module(Some("pinned"), &other, Some(&"0".repeat(64))).unwrap_err();
    assert!(error.to_string().contains("does not match the pinned"), "{error}");
    // The right pin works, and the library reports what it is.
    let info = f.load("consumer", &other);
    assert_eq!(info["name"], "test_consumer");
    assert_eq!(info["abi"], "2.1");
    assert_eq!(info["ports"].as_array().unwrap().len(), 2);
    let sha = info["sha256"].as_str().unwrap().to_owned();
    f.host.unload_module("consumer").unwrap();
    f.host.load_module(Some("consumer"), &other, Some(&sha.to_uppercase())).unwrap();
    // A library in use cannot be unloaded; after its instances are gone it can.
    f.add(spec("c", "consumer", 0.0, "{}", &[]));
    let error = f.host.unload_module("consumer").unwrap_err();
    assert!(matches!(error, HostError::Conflict(_)) && error.to_string().contains("used by instance(s) c"), "{error}");
    f.host.remove_instance("c").unwrap();
    f.host.unload_module("consumer").unwrap();
    assert!(matches!(f.host.unload_module("consumer"), Err(HostError::NotFound(_))));
    let modules = f.host.modules();
    assert_eq!(modules["modules"].as_array().unwrap().len(), 1);
    // After the unload the file may be loaded again, as new code.
    f.host.load_module(Some("consumer"), &other, None).unwrap();
}
