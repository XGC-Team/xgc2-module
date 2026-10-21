//! The frame links and a TRO-shaped agent in the real host: ABI minor 1 deadlines on a shared utc grid, a solver
//! thread that wakes its instance, and algorithm-owned messages carried by peer_link (UDP) and shm_bridge (shared
//! memory) between two hosts of one process.

mod common;

use common::*;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use xgc2_module_host::clock::Mode;
use xgc2_module_host::host::{ClockSpec, InstanceSpec};

fn utc_now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as i64
}

fn utc_host(entity: &str, phase_t0_ns: i64) -> Fixture {
    Fixture::with(entity, move |options| options.clock = ClockSpec { mode: Mode::Utc, channel: None, phase_t0_ns: Some(phase_t0_ns) })
}

fn agent_and_link(f: &Fixture, tag: &str, agent_config: &str, agent_so: &Path, link_so: &Path, link_config: &str) {
    // Paths are already built. Loading must not enter the compiler lock: the other
    // link test may be compiling, and that wait would land after the shared grid starts.
    f.load("agent", agent_so);
    f.load("link", link_so);
    let rx = format!("{tag}_rx");
    let tx = format!("{tag}_tx");
    let stats = format!("{tag}_stats");
    let mut l = InstanceSpec::new("link", "link");
    l.config_json = link_config.to_owned();
    l.bind = [("tx", &tx), ("rx", &rx), ("stats", &stats)].iter().map(|(p, c)| ((*p).to_owned(), (*c).clone())).collect();
    f.add(l);
    let mut a = InstanceSpec::new("agent", "agent");
    a.config_json = agent_config.to_owned();
    a.bind = [("rx", &rx), ("tx", &tx)].iter().map(|(p, c)| ((*p).to_owned(), (*c).clone())).collect();
    f.add(a);
}

fn number(detail: &Value, key: &str) -> i64 {
    detail[key].as_i64().unwrap_or_else(|| panic!("{key} missing in {detail}"))
}

/// Run two agents for `seconds` and check what a period of the TRO method needs from the platform.
fn exchange(link: &str, config_for: impl Fn(u16) -> String) {
    // Compile before choosing T0. The lead below is only enough to start both hosts;
    // a compile after T0 makes the first host emit periods the second one never sees.
    let agent_so = module("tro_shaped");
    let link_so = link_module(link);
    let period_ns = 100_000_000i64;
    // The grid origin: a boundary of the 100 ms period about half a second ahead, the same on both hosts.
    let phase = (utc_now_ns() / period_ns + 6) * period_ns;
    let a = utc_host("a", phase);
    let b = utc_host("b", phase);
    let agent = |id: u16| format!(r#"{{"self":{id},"peers":2,"period_ms":100,"w1_ms":20,"w2_ms":60,"solve_ms":8,"plan_bytes":1238}}"#);
    agent_and_link(&a, "a", &agent(1), &agent_so, &link_so, &config_for(1));
    agent_and_link(&b, "b", &agent(2), &agent_so, &link_so, &config_for(2));
    std::thread::sleep(Duration::from_millis(((phase - utc_now_ns()).max(0) / 1_000_000) as u64 + 3_000));
    let (da, db) = (a.detail("agent"), b.detail("agent"));
    for (name, own, other) in [("a", &da, &db), ("b", &db, &da)] {
        let periods = number(own, "beacons_sent");
        assert!(periods >= 25, "{name}: {periods} periods in 3 s\n{own}");
        // Beacons broadcast, plans to the partner; the link may lose the last one in flight.
        assert!(number(own, "beacons_rx") >= number(other, "beacons_sent") - 2, "{name}: beacons\n{own}\n{other}");
        assert!(number(own, "plans_rx") >= number(other, "plans_sent") - 2, "{name}: plans\n{own}\n{other}");
        assert_eq!(number(own, "corrupt"), 0, "{name}: a frame arrived changed\n{own}");
        assert_eq!(number(own, "tx_refused"), 0, "{name}\n{own}");
        assert_eq!(number(own, "solver_late"), 0, "{name}: the solver missed its window\n{own}");
        // Deadlines: stepped on the grid, a few hundred microseconds late at most on an idle machine.
        assert!(number(own, "deadline_steps") >= periods * 3 - 6, "{name}: the schedule machine was woken by wake_at\n{own}");
        assert!(number(own, "late_mean_us") < 5_000, "{name}: mean lateness of the deadline steps\n{own}");
        assert!(number(own, "window_misses") * 10 <= number(own, "plans_rx").max(1), "{name}: plans outside their window\n{own}");
        assert!(number(own, "solves") >= periods - 2, "{name}\n{own}");
    }
}

#[test]
fn two_hosts_run_the_tro_shaped_windows_over_udp() {
    let (pa, pb) = (free_udp_port(), free_udp_port());
    exchange("peer_link", |id| {
        let (own, other, other_id) = if id == 1 { (pa, pb, 2) } else { (pb, pa, 1) };
        format!(r#"{{"self":{id},"bind":"127.0.0.1:{own}","peers":{{"{other_id}":"127.0.0.1:{other}"}},"stats_period_ms":500}}"#)
    });
}

#[test]
fn two_hosts_run_the_tro_shaped_windows_over_shared_memory() {
    let segment = format!("/xgc2-frame-links-{}", std::process::id());
    exchange("shm_bridge", |id| {
        // Endpoint ids are inbox indices: the agents use 1 and 2, so three inboxes; endpoint 1 owns the segment.
        format!(r#"{{"shm_name":"{segment}","self":{id},"endpoints":3,"owner":{},"stats_period_ms":500,"attach_timeout_ms":3000}}"#, id == 1)
    });
}

#[test]
fn the_link_statistics_reach_the_host() {
    let f = Fixture::new("stats");
    f.load("agent", &module("tro_shaped"));
    f.load("link", &link_module("peer_link"));
    let port = free_udp_port();
    let mut l = InstanceSpec::new("link", "link");
    l.config_json = format!(r#"{{"self":1,"bind":"127.0.0.1:{port}","stats_period_ms":100}}"#);
    f.add(l);
    wait_until("the stats period fires", Duration::from_secs(5), || count(&f.channel("link.stats")["commits"]) >= 2);
    // The reported health names the counters.
    assert!(f.detail("link")["tx_frames"].is_number());
    assert_eq!(f.instance("link")["period_ns"], 100_000_000);
}
