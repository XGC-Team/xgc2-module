//! Shared test support: builds the C test modules and wraps a running host.
#![allow(dead_code)]

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use xgc2_module_host::host::{HostOptions, InstanceSpec, ModuleHost};

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repository root")
}

fn modules_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("xgc2-test-modules");
    fs::create_dir_all(&dir).expect("module output directory");
    dir
}

fn newest_input(root: &Path, source: &Path) -> SystemTime {
    let mut newest = fs::metadata(source).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    let mut headers = vec![root.join("include/xgc2/module.h"), root.join("include/xgc2/frame.h"), root.join("tests/modules/common.h")];
    for dir in ["sdk/include/xgc2/sdk", "modules/common"] {
        if let Ok(entries) = fs::read_dir(root.join(dir)) {
            headers.extend(entries.flatten().map(|e| e.path()));
        }
    }
    for header in headers {
        if let Ok(modified) = fs::metadata(header).and_then(|m| m.modified()) {
            newest = newest.max(modified);
        }
    }
    newest
}

static BUILD: Mutex<()> = Mutex::new(());

/// Compile `tests/modules/<source>` into `lib<output>.so` (once per process and input
/// state) and return its path. `CC` and `CXX` select the compilers.
pub fn build(source: &str, output: &str, defines: &[&str]) -> PathBuf {
    let source_path = repo_root().join("tests/modules").join(source);
    build_file(&source_path, output, defines, &[])
}

/// Like [`build`] for any source file of the repository (the link modules under `modules/`), with extra
/// compiler arguments such as libraries.
pub fn build_file(source_path: &Path, output: &str, defines: &[&str], extra: &[&str]) -> PathBuf {
    let root = repo_root();
    let source = source_path.to_str().expect("source path");
    let target = modules_dir().join(format!("lib{output}.so"));
    let _guard = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    let current = fs::metadata(&target).and_then(|m| m.modified()).is_ok_and(|built| built >= newest_input(&root, &source_path));
    if current {
        return target;
    }
    let cxx = source.ends_with(".cpp");
    let source_name = source_path.file_name().and_then(|n| n.to_str()).unwrap_or(source);
    let compiler = std::env::var(if cxx { "CXX" } else { "CC" }).unwrap_or_else(|_| (if cxx { "c++" } else { "cc" }).to_owned());
    let temporary = modules_dir().join(format!("lib{output}.so.{}.tmp", std::process::id()));
    let mut command = Command::new(&compiler);
    command
        .args(["-O2", "-fPIC", "-shared", "-pthread", "-Wall", "-Wextra", "-Werror", "-D_GNU_SOURCE"])
        .arg(if cxx { "-std=c++14" } else { "-std=c11" })
        .arg("-I")
        .arg(root.join("include"))
        .arg("-I")
        .arg(root.join("sdk/include"))
        .arg("-I")
        .arg(root.join("tests/modules"));
    for define in defines {
        command.arg(format!("-D{define}"));
    }
    let result = command.arg(source_path).arg("-o").arg(&temporary).args(extra).output().unwrap_or_else(|e| panic!("cannot run {compiler}: {e}"));
    assert!(result.status.success(), "{compiler} failed on {source_name}:\n{}", String::from_utf8_lossy(&result.stderr));
    fs::rename(&temporary, &target).expect("install test module");
    target
}

/// A test module by its source name (`consumer` builds `tests/modules/consumer.c`).
pub fn module(name: &str) -> PathBuf {
    let source = if matches!(name, "wake_thread" | "tro_shaped") { format!("{name}.cpp") } else { format!("{name}.c") };
    build(&source, name, &[])
}

pub fn module_version(name: &str, version: u32) -> PathBuf {
    build(&format!("{name}.c"), &format!("{name}_{version}"), &[&format!("VERSION={version}")])
}

pub fn broken(variant: &str) -> PathBuf {
    build("broken.c", &format!("broken_{}", variant.to_lowercase()), &[&format!("VARIANT_{variant}")])
}

/// A started host that shuts down when dropped.
pub struct Fixture {
    pub host: ModuleHost,
}

impl Fixture {
    pub fn new(entity: &str) -> Fixture {
        Self::with(entity, |_| {})
    }

    pub fn with(entity: &str, tweak: impl FnOnce(&mut HostOptions)) -> Fixture {
        let mut options = HostOptions::new(entity);
        options.workers = 2;
        options.quiesce_timeout = Duration::from_secs(2);
        options.op_timeout = Duration::from_secs(10);
        tweak(&mut options);
        Fixture { host: ModuleHost::start(options).expect("host starts") }
    }

    /// Load `modules/<file>` under `handle`.
    pub fn load(&self, handle: &str, file: &Path) -> Value {
        self.host.load_module(Some(handle), file, None).unwrap_or_else(|e| panic!("load {handle}: {e}"))
    }

    pub fn load_module(&self, name: &str) -> Value {
        self.load(name, &module(name))
    }

    pub fn add(&self, spec: InstanceSpec) {
        let name = spec.name.clone();
        self.host.add_instance(spec).unwrap_or_else(|e| panic!("add {name}: {e}"));
    }

    pub fn instance(&self, name: &str) -> Value {
        instance(&self.host, name)
    }

    pub fn channel(&self, name: &str) -> Value {
        channel(&self.host, name)
    }

    /// The JSON text a test module passes to `report`.
    pub fn detail(&self, name: &str) -> Value {
        detail(&self.host, name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.host.shutdown();
    }
}

pub fn spec(name: &str, module: &str, period_ms: f64, config: &str, bind: &[(&str, &str)]) -> InstanceSpec {
    let mut spec = InstanceSpec::new(name, module);
    spec.config_json = config.to_owned();
    spec.period_ns = (period_ms * 1e6) as i64;
    spec.bind = bind.iter().map(|(port, channel)| ((*port).to_owned(), (*channel).to_owned())).collect();
    spec
}

pub fn instance(host: &ModuleHost, name: &str) -> Value {
    host.health()["instances"]
        .as_array()
        .and_then(|list| list.iter().find(|i| i["name"] == name).cloned())
        .unwrap_or_else(|| panic!("no instance {name} in health"))
}

pub fn channel(host: &ModuleHost, name: &str) -> Value {
    host.health()["channels"]
        .as_array()
        .and_then(|list| list.iter().find(|c| c["name"] == name).cloned())
        .unwrap_or_else(|| panic!("no channel {name} in health"))
}

pub fn has_channel(host: &ModuleHost, name: &str) -> bool {
    host.health()["channels"].as_array().is_some_and(|list| list.iter().any(|c| c["name"] == name))
}

pub fn detail(host: &ModuleHost, name: &str) -> Value {
    let text = instance(host, name)["reported"]["detail"].as_str().unwrap_or("{}").to_owned();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

pub fn count(value: &Value) -> u64 {
    value.as_u64().unwrap_or(0)
}

pub fn wait_until(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out after {timeout:?} waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// A frame link module of `modules/` (`peer_link` or `shm_bridge`), built with the SDK, jsoncpp and librt.
pub fn link_module(name: &str) -> PathBuf {
    let source = repo_root().join("modules").join(name).join(format!("{name}.cpp"));
    build_file(&source, &format!("xgc2_{name}"), &[], &["-ljsoncpp", "-lrt"])
}

/// A UDP port on loopback that is free right now.
pub fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").expect("free port").local_addr().expect("local address").port()
}
