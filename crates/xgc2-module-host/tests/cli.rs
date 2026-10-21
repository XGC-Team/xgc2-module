//! The `xgc2-module-host` binary: `--check`, start-up, the control socket and signals.

mod common;

use common::*;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use xgc2_xrpc::{BlockingClient, Method, Runtime, RuntimeOptions};

const BIN: &str = env!("CARGO_BIN_EXE_xgc2-module-host");

/// The host binary, behind `XGC2_HOST_RUNNER` when set (scripts/bionic-test.sh runs it on the
/// glibc 2.27 loader, like the test processes).
fn host() -> Command {
    match std::env::var("XGC2_HOST_RUNNER") {
        Ok(runner) if !runner.is_empty() => {
            let mut words = runner.split_whitespace();
            let mut command = Command::new(words.next().unwrap());
            command.args(words).arg(BIN);
            command
        }
        _ => Command::new(BIN),
    }
}

fn manifest(dir: &std::path::Path, socket: Option<&std::path::Path>) -> std::path::PathBuf {
    let control = socket.map_or(String::new(), |s| format!("[control]\nsocket = \"{}\"\n", s.display()));
    let text = format!(
        "entity = \"cli-test\"\n{control}[host]\nworkers = 2\n[[module]]\nname = \"p\"\npath = \"{}\"\n[[module]]\nname = \"c\"\npath = \"{}\"\n[[instance]]\nname = \"src\"\nmodule = \"p\"\nperiod_ms = 5\n[instance.bind]\nout = \"s\"\n[[instance]]\nname = \"sink\"\nmodule = \"c\"\n[instance.bind]\nstate_in = \"s\"\n",
        module("producer_state").display(),
        module("consumer").display()
    );
    let path = dir.join("entity.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().unwrap()
}

fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "process did not exit within {within:?}");
        sleep_ms(10);
    }
}

#[test]
fn check_validates_without_starting() {
    let dir = private_dir();
    let path = manifest(dir.path(), None);
    let output = host().arg("--manifest").arg(&path).arg("--check").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["entity"], "cli-test");
    assert_eq!(report["instances"], serde_json::json!(["src", "sink"]));
    assert_eq!(report["channels"][0]["name"], "s");

    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "entity = \"x\"\n[[instance]]\nname = \"i\"\nmodule = \"nope\"\nperiod_ms = 0\n").unwrap();
    let output = host().arg("--manifest").arg(&bad).arg("--check").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("module \"nope\" is not declared") && stderr.contains("period_ms"), "{stderr}");
}

#[test]
fn usage_errors_exit_with_two() {
    for args in
        [vec![], vec!["--bogus"], vec!["--manifest"], vec!["--manifest", "/nonexistent/m.toml"], vec!["--workers", "x", "--manifest", "m"]]
    {
        let output = host().args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn it_serves_the_control_plane_until_sigterm() {
    let dir = private_dir();
    let socket = dir.path().join("module.sock");
    let path = manifest(dir.path(), Some(&socket));
    let mut child = host().arg("--manifest").arg(&path).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut first).unwrap();
    let announced: Value = serde_json::from_str(&first).unwrap_or_else(|e| panic!("first line {first:?}: {e}"));
    let service = &announced["service_ref"];
    assert_eq!((service["service"].as_str(), service["profile"].as_str()), (Some("xgc2-module"), Some("http.v1")));
    assert_eq!(service["endpoint"]["address"], socket.to_str().unwrap());
    let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
    let id = service["instance_id"].as_str().unwrap();
    let client = BlockingClient::unix(&runtime, &socket, id).unwrap();
    let health = || client.request(Method::GET, "/v1/health", None, Duration::from_secs(5), None).unwrap();
    wait_until("the entity runs", Duration::from_secs(10), || {
        health()["instances"].as_array().is_some_and(|list| list.iter().any(|i| i["name"] == "sink" && count(&i["steps"]) > 5))
    });
    let discovery = BlockingClient::unix(&runtime, &socket, "").unwrap();
    let described = discovery.request(Method::GET, "/v1/describe", None, Duration::from_secs(5), None).unwrap();
    assert_eq!((described["ready"].as_bool(), described["instance_id"].as_str()), (Some(true), Some(id)));
    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = wait_exit(&mut child, Duration::from_secs(10));
    assert_eq!(status.code(), Some(0));
    assert!(!socket.exists());
    drop(client);
    let _ = runtime;
}

/// Run the host on `manifest` with the given `--log-level` until it has logged that instance `src`
/// started, then stop it with SIGTERM; returns everything it wrote to stderr.
fn stderr_until_started(manifest: &std::path::Path, level: &str) -> String {
    let mut child =
        host().arg("--manifest").arg(manifest).args(["--log-level", level]).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (lines, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let mut log = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !log.contains("instance src (p) started") {
        let wait = deadline.saturating_duration_since(Instant::now());
        match received.recv_timeout(wait) {
            Ok(line) => log.push_str(&format!("{line}\n")),
            Err(e) => panic!("no start-up line ({e}); stderr so far:\n{log}"),
        }
    }
    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(wait_exit(&mut child, Duration::from_secs(10)).code(), Some(0));
    while let Ok(line) = received.recv_timeout(Duration::from_secs(5)) {
        log.push_str(&format!("{line}\n"));
    }
    log
}

#[test]
fn log_lines_of_modules_follow_the_log_level() {
    let dir = private_dir();
    let path = manifest(dir.path(), None);
    // The producer logs "producer started" at level 0 (debug) from its start function.
    let debug = stderr_until_started(&path, "debug");
    assert!(debug.lines().any(|line| line.contains(" DEBUG src: producer started")), "{debug}");
    let info = stderr_until_started(&path, "info");
    assert!(info.contains("instance src (p) started") && !info.contains("producer started"), "{info}");
}

#[test]
fn a_failing_start_exits_without_serving() {
    let dir = private_dir();
    let socket = dir.path().join("module.sock");
    let text = format!(
        "entity = \"x\"\n[control]\nsocket = \"{}\"\n[[module]]\nname = \"m\"\npath = \"/nonexistent/libm.so\"\n",
        socket.display()
    );
    let path = dir.path().join("bad.toml");
    std::fs::write(&path, text).unwrap();
    let output = host().arg("--manifest").arg(&path).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("libm.so"));
    assert!(!socket.exists());
}

fn object_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

#[test]
fn describe_lib_prints_descriptor_facts() {
    let path = module("consumer");
    let output = host().arg("--describe-lib").arg(&path).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        object_keys(&document),
        ["abi_major", "abi_minor", "name", "ports", "sha256", "version"]
    );
    assert_eq!(document["name"], "test_consumer");
    assert_eq!(document["version"], "1.0.0");
    assert_eq!(document["abi_major"], 2);
    assert_eq!(document["abi_minor"], 1);
    let sha = xgc2_module_host::loader::sha256_hex(&std::fs::read(&path).unwrap());
    assert_eq!(document["sha256"], sha);
    let ports = document["ports"].as_array().unwrap();
    assert_eq!(ports.len(), 2);
    assert_eq!(object_keys(&ports[0]), ["align", "depth", "dir", "flags", "kind", "name", "required", "schema", "size"]);
    assert_eq!(
        &ports[0],
        &serde_json::json!({"name": "state_in", "dir": "in", "kind": "state", "schema": "test.sample.v1", "required": false, "depth": 0, "size": 64, "align": 8, "flags": 0})
    );
    assert_eq!(
        &ports[1],
        &serde_json::json!({"name": "event_in", "dir": "in", "kind": "event", "schema": "test.event.v1", "required": false, "depth": 16, "size": 16, "align": 8, "flags": 0})
    );

    let passed = host().arg("--describe-lib").arg(module("passthrough")).output().unwrap();
    assert!(passed.status.success(), "{}", String::from_utf8_lossy(&passed.stderr));
    let passed: Value = serde_json::from_slice(&passed.stdout).unwrap();
    assert_eq!(passed["name"], "test_passthrough");
    assert_eq!(passed["ports"][0]["required"], true);
    assert_eq!(passed["ports"][0]["flags"], xgc2_module_host::abi::PORT_REQUIRED);
    assert_eq!(passed["ports"][1]["flags"], 0);

    let clock = host().arg("--describe-lib").arg(module("sim_clock")).output().unwrap();
    assert!(clock.status.success(), "{}", String::from_utf8_lossy(&clock.stderr));
    let clock: Value = serde_json::from_slice(&clock.stdout).unwrap();
    assert_eq!(clock["ports"][0]["dir"], "out");
    assert_eq!(clock["ports"][0]["required"], false);
    assert_eq!(clock["ports"][0]["schema"], "xgc2.clock.v1");
    assert_eq!(clock["ports"][0]["size"], 8);
    assert_eq!(clock["ports"][0]["flags"], xgc2_module_host::abi::PORT_ASYNC_WRITER);
}

#[test]
fn describe_lib_reports_loader_errors_and_rejects_other_modes() {
    let abi = host().arg("--describe-lib").arg(broken("ABI1")).output().unwrap();
    assert_eq!(abi.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&abi.stderr).contains("ABI major 1"));

    let missing = host().arg("--describe-lib").arg(build("layout_probe.c", "layout_probe", &[])).output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("xgc2_module_entry"));

    let output = host().arg("--describe-lib").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--describe-lib needs a value"));

    let dir = private_dir();
    let sentinel = dir.path().join("sentinel");
    let source = dir.path().join("sentinel.c");
    std::fs::write(
        &source,
        r#"
#include <stdio.h>
#include <xgc2/module.h>
static void mark(const char* word) {
    FILE* file = fopen(SENTINEL, "a");
    if (!file) return;
    fputs(word, file);
    fputc('\n', file);
    fclose(file);
}
__attribute__((constructor)) static void on_load(void) { mark("constructor"); }
static xgc2_status create(const xgc2_host_api* host, void* ctx, const xgc2_config* config, xgc2_instance** out) {
    (void)host; (void)ctx; (void)config; (void)out; mark("create"); return XGC2_ERR_INTERNAL;
}
static xgc2_status configure(xgc2_instance* instance, const xgc2_config* config) {
    (void)instance; (void)config; mark("configure"); return XGC2_ERR_INTERNAL;
}
static xgc2_status start(xgc2_instance* instance) { (void)instance; mark("start"); return XGC2_ERR_INTERNAL; }
static xgc2_status step(xgc2_instance* instance, const xgc2_step_ctx* ctx) {
    (void)instance; (void)ctx; mark("step"); return XGC2_ERR_INTERNAL;
}
static xgc2_status stop(xgc2_instance* instance) { (void)instance; mark("stop"); return XGC2_ERR_INTERNAL; }
static void destroy(xgc2_instance* instance) { (void)instance; mark("destroy"); }
static const xgc2_port_desc PORTS[] = {{"out", XGC2_PORT_OUT, XGC2_PORT_STATE, "xgc2.sentinel.v1", 8, 8, 0, 0}};
static const xgc2_module_desc DESC = {XGC2_MODULE_ABI_MAJOR, XGC2_MODULE_ABI_MINOR, "describe_sentinel", "1.0.0", PORTS, 1,
                                      create, configure, start, step, stop, destroy};
__attribute__((visibility("default"))) const xgc2_module_desc* xgc2_module_entry(void) {
    mark("entry");
    return &DESC;
}
"#,
    )
    .unwrap();
    let library = build_file(&source, "describe_sentinel", &[&format!("SENTINEL=\"{}\"", sentinel.display())], &[]);
    let manifest = dir.path().join("unused.toml");
    let socket = dir.path().join("unused.sock");
    let path = library.to_str().unwrap();
    let sentinel_text = sentinel.to_str().unwrap();
    let manifest_text = manifest.to_str().unwrap();
    let socket_text = socket.to_str().unwrap();
    for args in [
        vec!["--describe-lib", path, "--manifest", manifest_text],
        vec!["--manifest", manifest_text, "--describe-lib", path],
        vec!["--describe-lib", path, "--check"],
        vec!["--check", "--describe-lib", path],
        vec!["--describe-lib", path, "--control-socket", socket_text],
        vec!["--control-socket", socket_text, "--describe-lib", path],
        vec!["--describe-lib", path, "--workers", "2"],
        vec!["--workers", "2", "--describe-lib", path],
    ] {
        let output = host().args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?} {}", String::from_utf8_lossy(&output.stderr));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("cannot be combined"), "{args:?}: {stderr}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!sentinel.exists(), "{args:?} loaded the library");
    }

    let output = host().arg("--describe-lib").arg(&library).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["name"], "describe_sentinel");
    assert_eq!(document["abi_major"], xgc2_module_host::abi::ABI_MAJOR);
    let text = std::fs::read_to_string(sentinel_text).unwrap();
    assert_eq!(text, "constructor\nentry\n", "{text}");
}
