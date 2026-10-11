//! Random hot-plug under load. A producer chain and a bystander chain run at 500 Hz while a
//! driver replaces, adds, removes, rebinds, reconfigures, stops and starts instances in a
//! random order. Invariants: no deadlock, every request either succeeds or fails cleanly, data
//! never goes backwards or gets copied, and the entity ends up healthy.
//! XGC2_SOAK_SECONDS (default 3) sets the length.

mod common;

use common::*;
use std::time::{Duration, Instant};
use xgc2_module_host::host::{HostError, ModuleHost};

/// xorshift64*: enough for picking operations.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn must_succeed_or_be_refused(what: &str, result: Result<impl std::fmt::Debug, HostError>) {
    match result {
        Ok(_) => {}
        // A refusal for a legitimate reason (busy, name in use, nothing to remove) is fine; a
        // module failure or a timeout is not.
        Err(HostError::Conflict(_) | HostError::NotFound(_) | HostError::Invalid(_)) => {}
        Err(other) => panic!("{what}: {other}"),
    }
}

#[test]
fn random_hot_plug_under_load() {
    let seconds: f64 = std::env::var("XGC2_SOAK_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(3.0);
    let f = Fixture::new("soak");
    f.load_module("producer_state");
    f.load_module("producer_event");
    f.load_module("consumer");
    f.load("pass_1", &module_version("passthrough", 1));
    f.load("pass_2", &module_version("passthrough", 2));
    // Chain A is modified all the time, chain B is the bystander, events flow through E.
    f.add(spec("ca", "consumer", 0.0, "{}", &[("state_in", "a_out"), ("event_in", "e")]));
    f.add(spec("stage", "pass_1", 0.0, "{}", &[("in", "a_in"), ("out", "a_out")]));
    f.add(spec("pa", "producer_state", 2.0, r#"{"id":1}"#, &[("out", "a_in")]));
    f.add(spec("cb", "consumer", 0.0, "{}", &[("state_in", "b")]));
    f.add(spec("pb", "producer_state", 2.0, r#"{"id":2}"#, &[("out", "b")]));
    f.add(spec("ev", "producer_event", 3.0, r#"{"burst":2}"#, &[("out", "e")]));
    wait_until("flowing", Duration::from_secs(10), || count(&f.detail("ca")["state_updates"]) > 20);

    let host: &ModuleHost = &f.host;
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut guests = 0u32;
    let mut operations = [0u32; 8];
    while Instant::now() < deadline {
        let choice = random.below(8);
        operations[choice as usize] += 1;
        match choice {
            0 | 1 => {
                let module = if random.below(2) == 0 { "pass_1" } else { "pass_2" };
                must_succeed_or_be_refused("replace stage", host.replace_instance("stage", Some(module), None, None));
            }
            2 => {
                guests += 1;
                let name = format!("guest{guests}");
                let channel = if random.below(2) == 0 { "a_out" } else { "b" };
                must_succeed_or_be_refused("add guest", host.add_instance(spec(&name, "consumer", 0.0, "{}", &[("state_in", channel)])));
            }
            3 => {
                let health = host.health();
                let guest =
                    health["instances"].as_array().unwrap().iter().filter_map(|i| i["name"].as_str()).find(|n| n.starts_with("guest"));
                if let Some(name) = guest.map(str::to_owned) {
                    must_succeed_or_be_refused("remove guest", host.remove_instance(&name));
                }
            }
            4 => must_succeed_or_be_refused(
                "rebind",
                host.bind_port("cb", "state_in", Some(if random.below(2) == 0 { "b" } else { "a_out" })),
            ),
            5 => must_succeed_or_be_refused("configure", host.configure_instance("ca", format!(r#"{{"work_us":{}}}"#, random.below(300)))),
            6 => {
                must_succeed_or_be_refused("stop", host.stop_instance("ca"));
                must_succeed_or_be_refused("start", host.start_instance("ca"));
            }
            _ => {
                must_succeed_or_be_refused("stop producer", host.stop_instance("pa"));
                sleep_ms(random.below(5));
                must_succeed_or_be_refused("start producer", host.start_instance("pa"));
            }
        }
        sleep_ms(random.below(4));
    }
    eprintln!("operations by kind: {operations:?}");
    // Everything that is still there runs, and data still moves.
    wait_until("ready again", Duration::from_secs(10), || host.is_ready());
    let health = host.health();
    for instance in health["instances"].as_array().unwrap() {
        assert_eq!(instance["state"], "running", "{instance}");
        assert_ne!(instance["health"], "failed", "{instance}");
        assert_eq!(count(&instance["misuse"]), 0, "{instance}");
    }
    for name in ["ca", "cb"] {
        let before = count(&f.detail(name)["state_updates"]);
        wait_until("data moves", Duration::from_secs(10), || count(&f.detail(name)["state_updates"]) > before + 10);
        let detail = f.detail(name);
        assert_eq!(count(&detail["zero_copy_bad"]), 0, "{name}: {detail}");
        assert_eq!(count(&detail["repeat_bad"]), 0, "{name}: {detail}");
    }
    for channel in health["channels"].as_array().unwrap() {
        assert_eq!(count(&channel["stalls"]), 0, "{channel}");
    }
    assert!(operations.iter().filter(|n| **n > 0).count() >= 6, "the driver used too few kinds of operation: {operations:?}");
}
