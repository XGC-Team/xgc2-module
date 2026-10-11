//! The control plane over a Unix socket: every endpoint, error codes, fencing, readiness.

mod common;

use common::*;
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use xgc2_module_host::clock::Mode;
use xgc2_module_host::control::ControlServer;
use xgc2_module_host::host::{ClockSpec, HostOptions};
use xgc2_xrpc::{BlockingClient, CallError, Method, Runtime, RuntimeOptions};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Control {
    fixture: Fixture,
    server: Option<ControlServer>,
    runtime: Runtime,
    client: BlockingClient,
    unfenced: BlockingClient,
    _dir: tempfile::TempDir,
}

impl Control {
    fn new() -> Control {
        Self::with(|_| {})
    }

    fn with(tweak: impl FnOnce(&mut HostOptions)) -> Control {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().unwrap();
        let socket = dir.path().join("module.sock");
        let fixture = Fixture::with("control-test", tweak);
        let server = ControlServer::start(fixture.host.clone(), &socket).expect("control server");
        let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
        let client = BlockingClient::unix(&runtime, &socket, server.instance_id()).unwrap();
        let unfenced = BlockingClient::unix(&runtime, &socket, "").unwrap();
        Control { fixture, server: Some(server), runtime, client, unfenced, _dir: dir }
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, CallError> {
        self.client.call(path, body, TIMEOUT)
    }

    fn get(&self, path: &str) -> Value {
        self.client.request(Method::GET, path, None, TIMEOUT, None).unwrap_or_else(|e| panic!("GET {path}: {e}"))
    }

    /// The SDK client rejects query strings; use a native unbound HTTP exchange.
    fn describe(&self, target: &str, budget: Duration) -> io::Result<(u16, Value)> {
        let mut stream = UnixStream::connect(self.server.as_ref().unwrap().socket())?;
        stream.set_read_timeout(Some(budget + Duration::from_secs(1)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: local\r\nConnection: close\r\nX-Request-ID: describe:test\r\nX-Xrpc-Timeout-Ms: {}\r\n\r\n",
            budget.as_millis()
        )?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        let (headers, body) =
            response.split_once("\r\n\r\n").ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no describe response"))?;
        let status = headers
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP status"))?;
        let body = serde_json::from_str(body).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok((status, body))
    }

    fn ok(&self, path: &str, body: Value) -> Value {
        self.post(path, body.clone()).unwrap_or_else(|e| panic!("POST {path} {body}: {e}"))
    }

    fn path_of(&self, name: &str) -> String {
        module(name).to_str().unwrap().to_owned()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            let _ = server.close();
        }
        let _ = self.runtime.close(Duration::from_secs(2));
    }
}

/// (code, message) of an error answer.
fn failure(error: CallError) -> (String, String) {
    let body: Value = serde_json::from_str(&error.message).unwrap_or(Value::Null);
    match body["error"]["code"].as_str() {
        Some(code) => (code.to_owned(), body["error"]["message"].as_str().unwrap_or("").to_owned()),
        None => ("transport".to_owned(), error.message),
    }
}

/// Both unbound discovery calls must stay held until the change makes the host ready.
fn describe_when_ready(c: &Control, change: impl FnOnce()) -> Value {
    assert!(!c.fixture.host.is_ready());
    std::thread::scope(|scope| {
        let (done, replies) = mpsc::channel();
        for _ in 0..2 {
            let done = done.clone();
            scope.spawn(move || {
                let result = c.describe("/v1/describe?wait_ready_ms=30000", Duration::from_secs(3));
                let _ = done.send(result);
            });
        }
        drop(done);
        assert!(matches!(replies.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)));
        change();
        let mut described = Value::Null;
        for _ in 0..2 {
            let (status, body) = replies.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
            assert_eq!(status, 200, "{body}");
            described = body;
            assert_eq!(described["ready"], true);
            assert_eq!(described["instance_id"], c.server.as_ref().unwrap().instance_id());
        }
        described
    })
}

#[test]
fn describe_is_the_only_unfenced_call_and_carries_identity_and_readiness() {
    let c = Control::new();
    let described = c.unfenced.request(Method::GET, "/v1/describe", None, TIMEOUT, None).unwrap();
    assert_eq!(described["service"], "xgc2-module");
    assert_eq!(described["api_version"], "v1");
    assert_eq!(described["instance_id"], c.server.as_ref().unwrap().instance_id());
    assert_eq!(described["ready"], true, "an empty entity has nothing missing");
    assert_eq!(described["facts"]["entity"], "control-test");
    assert_eq!(described["facts"]["abi"], json!({"major": 2, "minor": 0}));
    assert_eq!(described["facts"]["clock"]["mode"], "steady");
    // Everything else needs the instance id of this boot. A client without one is told so by
    // the server; a client pinned to another boot (a restarted host) is refused too.
    let (code, message) = failure(c.unfenced.request(Method::GET, "/v1/health", None, TIMEOUT, None).unwrap_err());
    assert_eq!((code.as_str(), message.as_str()), ("conflict", "service instance changed"));
    let stale = BlockingClient::unix(&c.runtime, c.server.as_ref().unwrap().socket(), "0123456789abcdef").unwrap();
    let refused = stale.request(Method::GET, "/v1/health", None, TIMEOUT, None).unwrap_err();
    assert!(refused.message.contains("instance"), "{refused}");
    assert_eq!(c.get("/v1/health")["entity"], "control-test");
}

#[test]
fn every_endpoint_round_trips() {
    let c = Control::new();
    // Libraries.
    let loaded = c.ok("/v1/modules/load", json!({"path": c.path_of("producer_state"), "name": "producer"}));
    assert_eq!((loaded["module"].as_str(), loaded["name"].as_str()), (Some("producer"), Some("test_state_producer")));
    // The pin is accepted in either case.
    c.ok("/v1/modules/load", json!({"path": c.path_of("consumer"), "name": "consumer", "sha256": sha_of("consumer").to_uppercase()}));
    let listed = c.get("/v1/modules");
    assert_eq!(listed["modules"].as_array().unwrap().len(), 2);
    // Instances.
    let added = c.ok(
        "/v1/instances/add",
        json!({"name": "p", "module": "producer", "period_ms": 2, "config": {"id": 4}, "bind": {"out": "samples"}}),
    );
    assert_eq!((added["state"].as_str(), added["period_ns"].as_u64()), (Some("running"), Some(2_000_000)));
    c.ok("/v1/instances/add", json!({"name": "c", "module": "consumer", "bind": {"state_in": "samples"}, "required": false}));
    wait_until("data flows", TIMEOUT, || count(&c.fixture.detail("c")["state_updates"]) > 10);
    let health = c.get("/v1/health");
    let consumer = health["instances"].as_array().unwrap().iter().find(|i| i["name"] == "c").unwrap();
    for key in ["steps", "step_time", "handoff_latency", "wakeups", "coalesced_dirties", "overruns", "timer_fires", "ports"] {
        assert!(consumer.get(key).is_some(), "health lacks {key}: {consumer}");
    }
    assert!(consumer["step_time"]["p99_ns"].as_u64().unwrap() >= consumer["step_time"]["p50_ns"].as_u64().unwrap());
    let samples = health["channels"].as_array().unwrap().iter().find(|ch| ch["name"] == "samples").unwrap();
    assert!(samples["commits"].as_u64().unwrap() > 10);
    // Configure: module configuration and timing in one call.
    let configured = c.ok("/v1/instances/configure", json!({"name": "c", "config": {"work_us": 100}, "step_budget_ms": 80}));
    assert_eq!(configured["state"], "running");
    wait_until("configured", TIMEOUT, || c.fixture.detail("c")["work_us"] == 100);
    assert_eq!(c.fixture.instance("c")["step_budget_ns"], 80_000_000);
    c.ok("/v1/instances/configure", json!({"name": "p", "period_ms": 10}));
    assert_eq!(c.fixture.instance("p")["period_ns"], 10_000_000);
    // Stop and start.
    assert_eq!(c.ok("/v1/instances/stop", json!({"name": "c"}))["state"], "stopped");
    assert_eq!(c.ok("/v1/instances/start", json!({"name": "c"}))["state"], "running");
    // Replace keeps the name, the channels and (without a new config) the configuration.
    let replaced = c.ok("/v1/instances/replace", json!({"name": "c"}));
    assert_eq!((replaced["name"].as_str(), replaced["state"].as_str()), (Some("c"), Some("running")));
    wait_until("replacement steps", TIMEOUT, || c.fixture.detail("c")["work_us"] == 100);
    // Bindings.
    c.ok("/v1/instances/add", json!({"name": "p2", "module": "producer", "period_ms": 2, "config": {"id": 5}, "bind": {"out": "other"}}));
    let moved = c.ok("/v1/bindings", json!({"instance": "c", "port": "state_in", "channel": "other"}));
    assert_eq!(moved["channel"], "other");
    wait_until("rebound", TIMEOUT, || c.fixture.detail("c")["last_producer"] == 5);
    c.ok("/v1/bindings", json!({"instance": "c", "port": "state_in", "channel": null}));
    // Removal and unload.
    assert_eq!(c.ok("/v1/instances/remove", json!({"name": "c"}))["removed"], "c");
    assert_eq!(c.ok("/v1/modules/unload", json!({"module": "consumer"}))["unloaded"], "consumer");
    assert_eq!(c.get("/v1/modules")["modules"].as_array().unwrap().len(), 1);
}

#[test]
fn affinity_is_exposed_validated_and_preserved_or_changed_on_replace() {
    let c = Control::new();
    c.ok("/v1/modules/load", json!({"path": c.path_of("thread_probe"), "name": "probe"}));
    let added = c.ok("/v1/instances/add", json!({"name": "sticky", "module": "probe", "config": {"strict": false}}));
    assert_eq!(added["affinity"], "sticky");
    assert_eq!(added["worker"], 0);
    let replaced = c.ok("/v1/instances/replace", json!({"name": "sticky"}));
    assert_eq!(replaced["affinity"], "sticky");
    assert_eq!(replaced["worker"], added["worker"]);
    let added = c.ok("/v1/instances/add", json!({"name": "any", "module": "probe", "affinity": "any", "config": {"strict": false}}));
    assert_eq!(added["affinity"], "any");
    assert_eq!(c.ok("/v1/instances/replace", json!({"name": "any"}))["affinity"], "any");
    let sticky = c.ok("/v1/instances/replace", json!({"name": "any", "affinity": "sticky"}));
    assert_eq!(sticky["affinity"], "sticky");
    assert_eq!(sticky["worker"], 1);
    assert_eq!(c.get("/v1/health")["workers"]["sticky"], json!([1, 1]));
    let free = c.ok("/v1/instances/replace", json!({"name": "sticky", "affinity": "any"}));
    assert_eq!(free["affinity"], "any");
    assert_eq!(c.get("/v1/health")["workers"]["sticky"], json!([0, 1]));
    for path in ["/v1/health", "/v1/describe"] {
        let answer = c.get(path);
        let instances = if path == "/v1/describe" { &answer["facts"]["instances"] } else { &answer["instances"] };
        let probe = instances.as_array().unwrap().iter().find(|i| i["name"] == "any").unwrap();
        assert_eq!(probe["affinity"], "sticky");
        assert_eq!(probe["worker"], 1);
        assert_eq!(probe["worker_moves"], 0);
    }
    for path in ["/v1/instances/add", "/v1/instances/replace"] {
        let (code, message) = failure(c.post(path, json!({"name": "any", "module": "probe", "affinity": "free"})).unwrap_err());
        assert_eq!(code, "invalid_argument");
        assert!(message.contains("sticky") && message.contains("any"), "{message}");
    }
}

fn sha_of(name: &str) -> String {
    xgc2_module_host::loader::sha256_hex(&std::fs::read(module(name)).unwrap())
}

#[test]
fn errors_use_the_xrpc_vocabulary() {
    let c = Control::new();
    c.ok("/v1/modules/load", json!({"path": c.path_of("producer_state"), "name": "producer"}));
    c.ok("/v1/modules/load", json!({"path": c.path_of("consumer"), "name": "consumer"}));
    c.ok("/v1/instances/add", json!({"name": "p", "module": "producer", "bind": {"out": "s"}}));
    c.ok("/v1/instances/add", json!({"name": "r", "module": "consumer", "bind": {"state_in": "s"}}));
    let cases: Vec<(&str, Value, &str, &str)> = vec![
        ("/v1/instances/add", json!({"name": "p", "module": "producer"}), "conflict", "exists"),
        ("/v1/instances/add", json!({"name": "q", "module": "producer", "bind": {"out": "s"}}), "invalid_argument", "already has a writer"),
        ("/v1/instances/add", json!({"name": "q", "module": "nope"}), "not_found", "no module named nope"),
        ("/v1/instances/add", json!({"name": "q", "module": "producer", "bogus": 1}), "invalid_argument", "unknown field"),
        ("/v1/instances/add", json!({"name": "q", "module": "producer", "config": [1]}), "invalid_argument", "JSON object"),
        ("/v1/instances/add", json!({"name": "q", "module": "producer", "period_ms": -2}), "invalid_argument", "period_ms"),
        ("/v1/instances/add", json!({"name": "q", "module": "producer", "bind": {"zzz": "x"}}), "invalid_argument", "no port zzz"),
        ("/v1/instances/add", json!({"name": "bad name", "module": "producer"}), "invalid_argument", "not a valid name"),
        ("/v1/instances/add", json!({"module": "producer"}), "invalid_argument", "name"),
        ("/v1/instances/remove", json!({"name": "ghost"}), "not_found", "no instance named ghost"),
        ("/v1/instances/configure", json!({"name": "p"}), "invalid_argument", "needs config"),
        ("/v1/instances/replace", json!({"name": "p", "module": "consumer"}), "invalid_argument", "has no port out"),
        ("/v1/modules/unload", json!({"module": "producer"}), "conflict", "used by instance(s) p"),
        ("/v1/modules/unload", json!({"module": "ghost"}), "not_found", "no module named ghost"),
        ("/v1/modules/load", json!({"path": "relative/libx.so"}), "invalid_argument", "libx.so"),
        ("/v1/modules/load", json!({"path": c.path_of("slow"), "name": "slow", "sha256": "00"}), "invalid_argument", "64 hex digits"),
        (
            "/v1/modules/load",
            json!({"path": c.path_of("slow"), "name": "slow", "sha256": "0".repeat(64)}),
            "invalid_argument",
            "does not match",
        ),
        ("/v1/modules/load", json!({"path": c.path_of("consumer"), "name": "again"}), "conflict", "already loaded as module consumer"),
        ("/v1/bindings", json!({"instance": "p", "port": "out", "channel": "bad name"}), "invalid_argument", "not a valid name"),
        ("/v1/bindings", json!({"instance": "p", "port": "ghost", "channel": "x"}), "invalid_argument", "no port ghost"),
        ("/v1/nothing", json!({}), "not_found", "unknown operation"),
    ];
    for (path, body, expected_code, expected_text) in cases {
        let (code, message) = failure(c.post(path, body.clone()).expect_err(&format!("{path} {body} should fail")));
        assert_eq!(code, expected_code, "{path} {body}: {message}");
        assert!(message.contains(expected_text), "{path} {body}: {message}");
    }
    let (code, _) = failure(c.client.request(Method::GET, "/v1/unknown", None, TIMEOUT, None).unwrap_err());
    assert_eq!(code, "not_found");
    // The failed requests changed nothing.
    assert_eq!(c.fixture.host.health()["instances"].as_array().unwrap().len(), 2);
}

#[test]
fn readiness_follows_started_instances_and_their_producers() {
    let c = Control::new();
    c.ok("/v1/modules/load", json!({"path": c.path_of("producer_state"), "name": "producer"}));
    c.ok("/v1/modules/load", json!({"path": c.path_of("passthrough"), "name": "stage"}));
    let described = |c: &Control| c.unfenced.request(Method::GET, "/v1/describe", None, TIMEOUT, None).unwrap();
    c.ok("/v1/instances/add", json!({"name": "stage", "module": "stage", "bind": {"in": "raw"}}));
    let before = described(&c);
    assert_eq!(before["ready"], false);
    let reasons = before["facts"]["not_ready"].to_string();
    assert!(reasons.contains("required input in (channel raw) has no running producer"), "{reasons}");
    let stage = before["facts"]["instances"].as_array().unwrap().iter().find(|i| i["name"] == "stage").unwrap().clone();
    assert_eq!((stage["state"].as_str(), stage["ready"].as_bool()), (Some("running"), Some(false)));
    c.ok("/v1/instances/add", json!({"name": "src", "module": "producer", "bind": {"out": "raw"}, "autostart": false}));
    assert_eq!(described(&c)["ready"], false, "a producer that is not started does not count");
    let ready = describe_when_ready(&c, || {
        c.ok("/v1/instances/start", json!({"name": "src"}));
    });
    assert!(ready["facts"]["instances"].as_array().unwrap().iter().all(|instance| instance["state"] == "running"));
    c.ok("/v1/bindings", json!({"instance": "stage", "port": "in", "channel": null}));
    describe_when_ready(&c, || {
        c.ok("/v1/bindings", json!({"instance": "stage", "port": "in", "channel": "raw"}));
    });
    c.ok("/v1/instances/stop", json!({"name": "src"}));
    assert_eq!(described(&c)["ready"], false);
    let empty = describe_when_ready(&c, || {
        c.ok("/v1/instances/remove", json!({"name": "src"}));
        assert!(!c.fixture.host.is_ready());
        c.ok("/v1/instances/remove", json!({"name": "stage"}));
    });
    assert_eq!(empty["facts"]["instances"], json!([]));
}

#[test]
fn describe_wait_is_released_by_the_first_external_clock_sample() {
    let c = Control::with(|options| {
        options.clock = ClockSpec { mode: Mode::External, channel: Some("clock".into()) };
    });
    c.ok("/v1/modules/load", json!({"path": c.path_of("sim_clock"), "name": "clock"}));
    c.ok(
        "/v1/instances/add",
        json!({"name": "clock", "module": "clock", "autostart": false, "required": false,
               "config": {"interval_us": 100000, "count": 1}, "bind": {"time": "clock"}}),
    );
    let described = describe_when_ready(&c, || {
        c.ok("/v1/instances/start", json!({"name": "clock"}));
    });
    assert_eq!(described["facts"]["clock"]["valid"], true);
    assert_eq!(described["facts"]["clock"]["now_ns"], 10_000_000);
}

#[test]
fn describe_is_immediate_when_ready_or_without_a_wait() {
    let ready = Control::new();
    for target in ["/v1/describe?wait_ready_ms=0", "/v1/describe?wait_ready_ms=30000"] {
        let (status, described) = ready.describe(target, Duration::from_secs(1)).unwrap();
        assert_eq!(status, 200);
        assert_eq!(described["ready"], true);
    }
    let not_ready = Control::with(|options| {
        options.clock = ClockSpec { mode: Mode::External, channel: Some("clock".into()) };
    });
    for target in ["/v1/describe", "/v1/describe?wait_ready_ms=0"] {
        let (status, described) = not_ready.describe(target, Duration::from_secs(1)).unwrap();
        assert_eq!(status, 200);
        assert_eq!(described["ready"], false);
    }
}

#[test]
fn describe_wait_expires_with_the_current_not_ready_body() {
    let c = Control::with(|options| {
        options.clock = ClockSpec { mode: Mode::External, channel: Some("clock".into()) };
    });
    let started = Instant::now();
    let (status, described) = c.describe("/v1/describe?wait_ready_ms=120", Duration::from_secs(2)).unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(120) && elapsed < Duration::from_secs(1), "{elapsed:?}");
    assert_eq!(status, 200);
    assert_eq!(described["ready"], false);
    assert_eq!(described["facts"]["clock"]["valid"], false);
    assert_eq!(described["facts"]["not_ready"], json!(["the external clock has not published a time yet"]));
}

#[test]
fn describe_wait_is_bounded_by_the_request_deadline() {
    let c = Control::with(|options| {
        options.clock = ClockSpec { mode: Mode::External, channel: Some("clock".into()) };
    });
    let started = Instant::now();
    let response = c.describe("/v1/describe?wait_ready_ms=30000", Duration::from_millis(120));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(100) && elapsed < Duration::from_secs(1), "{elapsed:?}");
    match response {
        Ok((status, body)) => {
            assert_eq!(status, 504, "{body}");
            assert_eq!(body["error"]["code"], "deadline_exceeded");
        }
        Err(error) => assert!(
            matches!(error.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe),
            "{error}"
        ),
    }
}

#[test]
fn describe_rejects_invalid_wait_queries_even_when_ready() {
    let c = Control::new();
    for query in [
        "wait_ready_ms=",
        "wait_ready_ms=abc",
        "wait_ready_ms=-1",
        "wait_ready_ms=+1",
        "wait_ready_ms=1.5",
        "wait_ready_ms=30001",
        "wait_ready_ms=18446744073709551616",
        "wait_ready_ms=1&wait_ready_ms=2",
        "wait_ready_ms=1&other=2",
        "other=1",
    ] {
        let target = format!("/v1/describe?{query}");
        let (status, body) = c.describe(&target, TIMEOUT).unwrap();
        assert_eq!(status, 400, "{target}: {body}");
        assert_eq!(body["error"]["code"], "invalid_argument", "{target}");
    }
}

#[test]
fn a_second_mutation_is_refused_while_one_is_running() {
    let c = Control::new();
    c.ok("/v1/modules/load", json!({"path": c.path_of("slow"), "name": "slow"}));
    c.ok("/v1/instances/add", json!({"name": "slow", "module": "slow", "period_ms": 1000, "config": {"sleep_us": 1200000}, "step_budget_ms": 2000, "hang_limit_ms": 5000}));
    c.ok("/v1/instances/add", json!({"name": "other", "module": "slow"}));
    wait_until("slow step started", TIMEOUT, || c.fixture.instance("slow")["state"] == "running");
    // The first step starts after one period and then sleeps 1.2 s; stop must wait for it.
    sleep_ms(1100);
    std::thread::scope(|scope| {
        let stopping = scope.spawn(|| c.post("/v1/instances/stop", json!({"name": "slow"})));
        sleep_ms(200);
        let (code, message) = failure(c.post("/v1/instances/configure", json!({"name": "other", "config": {"sleep_us": 1}})).unwrap_err());
        assert_eq!(code, "conflict", "{message}");
        assert!(message.contains("another control operation is in progress"), "{message}");
        // Reads are never blocked by a mutation.
        assert_eq!(c.get("/v1/health")["entity"], "control-test");
        assert_eq!(stopping.join().unwrap().unwrap()["state"], "stopped");
    });
    c.ok("/v1/instances/configure", json!({"name": "other", "config": {"sleep_us": 1}}));
}

#[test]
fn closing_the_server_removes_the_endpoint() {
    let mut c = Control::new();
    let socket = c.server.as_ref().unwrap().socket().to_owned();
    assert!(socket.exists());
    c.server.take().unwrap().close().unwrap();
    assert!(c.client.request(Method::GET, "/v1/health", None, Duration::from_secs(1), None).is_err());
    assert!(!socket.exists(), "the socket file is removed with the lease");
}
