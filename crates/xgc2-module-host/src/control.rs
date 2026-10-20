//! Control plane: XRPC http.v1 over a Unix socket.
//!
//! ```text
//! GET  /v1/describe              identity, entity, readiness; optional wait_ready_ms=0..30000 (unbound)
//! GET  /v1/health                counters per instance and channel
//! GET  /v1/modules               loaded libraries
//! POST /v1/modules/load          {path, name?, sha256?}
//! POST /v1/modules/unload        {module}
//! POST /v1/instances/add         {name, module, config?, period_ms?, step_budget_ms?, hang_limit_ms?,
//!                                 required?, autostart?, affinity?, bind?}
//! POST /v1/instances/remove      {name}
//! POST /v1/instances/replace     {name, module?, config?, affinity?}
//! POST /v1/instances/configure   {name, config?, period_ms?, step_budget_ms?, hang_limit_ms?}
//! POST /v1/instances/start       {name}
//! POST /v1/instances/stop        {name}
//! POST /v1/bindings              {instance, port, channel|null}
//! ```
//!
//! Mutations run on the SDK's blocking pool (they wait for module calls) and are serialized
//! by the host: a second mutation while one is running fails with `conflict` instead of
//! queueing. Reads never wait for a module.

use crate::host::{HostError, InstanceSpec, ModuleHost};
use crate::scheduler::Affinity;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use xgc2_xrpc::{handler, Context, Fault, Limits, Method, Runtime, RuntimeOptions};

pub const SERVICE: &str = "xgc2-module";
pub const API_VERSION: &str = "v1";

pub struct ControlServer {
    runtime: Runtime,
    server: xgc2_xrpc::Host,
    instance_id: String,
    socket: PathBuf,
}

impl ControlServer {
    /// Listen on `socket`, whose directory must be owned by the current user with mode 0700.
    pub fn start(host: ModuleHost, socket: &Path) -> io::Result<ControlServer> {
        let instance_id = xgc2_xrpc::new_instance_id()?;
        let runtime = Runtime::new(RuntimeOptions::default())?;
        let limits = Limits { discovery_routes: vec!["/v1/describe".into()], ..Limits::default() };
        let id = instance_id.clone();
        let server = xgc2_xrpc::Host::bind(
            &runtime,
            socket,
            instance_id.clone(),
            limits,
            true,
            handler(move |context, path, body| {
                let (host, id) = (host.clone(), id.clone());
                async move { route(context, host, id, path, body).await }
            }),
        )?;
        Ok(ControlServer { runtime, server, instance_id, socket: socket.to_owned() })
    }

    /// Random id of this boot; clients pin it with `X-Xrpc-Instance-ID`.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The service reference a supervisor can use to reach the control plane.
    pub fn service_ref(&self) -> Value {
        json!({
            "service": SERVICE,
            "api_version": API_VERSION,
            "instance_id": self.instance_id,
            "profile": "http.v1",
            "endpoint": {"kind": "unix", "address": self.socket},
        })
    }

    /// Stop listening and wait for admitted calls (bounded).
    pub fn close(mut self) -> io::Result<()> {
        let closed = self.server.close();
        let runtime = self.runtime.close(Duration::from_secs(5));
        closed.and(runtime)
    }
}

fn fault(error: HostError) -> Fault {
    let code = match &error {
        HostError::Invalid(_) => "invalid_argument",
        HostError::NotFound(_) => "not_found",
        HostError::Conflict(_) => "conflict",
        HostError::Unavailable(_) => "unavailable",
        HostError::Failed(_) => "internal",
    };
    Fault::new(code, error.to_string())
}

fn request<T: DeserializeOwned>(body: Value) -> Result<T, Fault> {
    serde_json::from_value(body).map_err(|e| Fault::new("invalid_argument", e.to_string()))
}

fn config_json(config: Option<Value>) -> Result<Option<String>, Fault> {
    match config {
        None => Ok(None),
        Some(value @ Value::Object(_)) => Ok(Some(value.to_string())),
        Some(_) => Err(Fault::new("invalid_argument", "config must be a JSON object")),
    }
}

fn ms_to_ns(field: &str, value: Option<f64>) -> Result<Option<i64>, Fault> {
    match value {
        None => Ok(None),
        Some(ms) if ms.is_finite() && ms > 0.0 && ms <= 3_600_000.0 => Ok(Some((ms * 1e6).round() as i64)),
        Some(_) => Err(Fault::new("invalid_argument", format!("{field} must be a positive number of at most 3600000 ms"))),
    }
}

fn describe_wait(query: &str) -> Result<Duration, Fault> {
    if query.is_empty() {
        return Ok(Duration::ZERO);
    }
    let invalid = || Fault::new("invalid_argument", "describe takes only wait_ready_ms between 0 and 30000");
    let value = query.strip_prefix("wait_ready_ms=").ok_or_else(invalid)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let millis: u64 = value.parse().map_err(|_| invalid())?;
    if millis > 30_000 {
        return Err(invalid());
    }
    Ok(Duration::from_millis(millis))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadRequest {
    path: PathBuf,
    name: Option<String>,
    sha256: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnloadRequest {
    module: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddRequest {
    name: String,
    module: String,
    affinity: Option<Affinity>,
    config: Option<Value>,
    period_ms: Option<f64>,
    step_budget_ms: Option<f64>,
    hang_limit_ms: Option<f64>,
    required: Option<bool>,
    autostart: Option<bool>,
    #[serde(default)]
    bind: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameRequest {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceRequest {
    name: String,
    module: Option<String>,
    config: Option<Value>,
    affinity: Option<Affinity>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigureRequest {
    name: String,
    config: Option<Value>,
    period_ms: Option<f64>,
    step_budget_ms: Option<f64>,
    hang_limit_ms: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindRequest {
    instance: String,
    port: String,
    channel: Option<String>,
}

async fn route(context: Context, host: ModuleHost, instance_id: String, path: String, body: Value) -> Result<Value, Fault> {
    if context.method == Method::GET {
        return match path.as_str() {
            "/v1/describe" => {
                let wait = describe_wait(&context.query)?;
                if !wait.is_zero() {
                    host.wait_ready(context.deadline.min(tokio::time::Instant::now() + wait)).await;
                }
                let (ready, facts) = host.describe();
                Ok(json!({"service": SERVICE, "api_version": API_VERSION, "instance_id": instance_id, "ready": ready, "facts": facts}))
            }
            "/v1/health" => Ok(host.health()),
            "/v1/modules" => Ok(host.modules()),
            _ => Err(Fault::new("not_found", format!("unknown query {path}"))),
        };
    }
    if context.method != Method::POST {
        return Err(Fault::new("invalid_argument", "mutations use POST"));
    }
    // Parse before leaving the IO task so malformed requests never occupy the blocking pool.
    let operation = parse_operation(&path, body)?;
    context.blocking(move || perform(&host, operation)).await?.map_err(fault)
}

enum Operation {
    Load(LoadRequest),
    Unload(String),
    Add(InstanceSpec),
    Remove(String),
    Replace { name: String, module: Option<String>, config: Option<String>, affinity: Option<Affinity> },
    Configure { name: String, config: Option<String>, timing: [Option<i64>; 3] },
    Start(String),
    Stop(String),
    Bind(BindRequest),
}

fn parse_operation(path: &str, body: Value) -> Result<Operation, Fault> {
    Ok(match path {
        "/v1/modules/load" => Operation::Load(request(body)?),
        "/v1/modules/unload" => Operation::Unload(request::<UnloadRequest>(body)?.module),
        "/v1/instances/add" => {
            let add: AddRequest = request(body)?;
            let mut spec = InstanceSpec::new(&add.name, &add.module);
            spec.affinity = add.affinity.unwrap_or_default();
            if let Some(config) = config_json(add.config)? {
                spec.config_json = config;
            }
            spec.period_ns = ms_to_ns("period_ms", add.period_ms)?.unwrap_or(0);
            spec.step_budget_ns = ms_to_ns("step_budget_ms", add.step_budget_ms)?;
            spec.hang_limit_ns = ms_to_ns("hang_limit_ms", add.hang_limit_ms)?;
            spec.required = add.required.unwrap_or(true);
            spec.autostart = add.autostart.unwrap_or(true);
            spec.bind = add.bind;
            Operation::Add(spec)
        }
        "/v1/instances/remove" => Operation::Remove(request::<NameRequest>(body)?.name),
        "/v1/instances/replace" => {
            let replace: ReplaceRequest = request(body)?;
            Operation::Replace {
                name: replace.name,
                module: replace.module,
                config: config_json(replace.config)?,
                affinity: replace.affinity,
            }
        }
        "/v1/instances/configure" => {
            let configure: ConfigureRequest = request(body)?;
            let config = config_json(configure.config)?;
            let timing = [
                ms_to_ns("period_ms", configure.period_ms)?,
                ms_to_ns("step_budget_ms", configure.step_budget_ms)?,
                ms_to_ns("hang_limit_ms", configure.hang_limit_ms)?,
            ];
            if config.is_none() && timing.iter().all(Option::is_none) {
                return Err(Fault::new("invalid_argument", "configure needs config, period_ms, step_budget_ms or hang_limit_ms"));
            }
            Operation::Configure { name: configure.name, config, timing }
        }
        "/v1/instances/start" => Operation::Start(request::<NameRequest>(body)?.name),
        "/v1/instances/stop" => Operation::Stop(request::<NameRequest>(body)?.name),
        "/v1/bindings" => Operation::Bind(request(body)?),
        _ => return Err(Fault::new("not_found", format!("unknown operation {path}"))),
    })
}

fn state_of(host: &ModuleHost, name: &str) -> Value {
    let health = host.health();
    health["instances"]
        .as_array()
        .and_then(|list| list.iter().find(|i| i["name"] == name))
        .map(|i| json!({"name": name, "state": i["state"], "health": i["health"]}))
        .unwrap_or_else(|| json!({"name": name}))
}

fn perform(host: &ModuleHost, operation: Operation) -> Result<Value, HostError> {
    match operation {
        Operation::Load(load) => host.load_module(load.name.as_deref(), &load.path, load.sha256.as_deref()),
        Operation::Unload(module) => host.unload_module(&module).map(|()| json!({"unloaded": module})),
        Operation::Add(spec) => host.add_instance(spec),
        Operation::Remove(name) => host.remove_instance(&name).map(|()| json!({"removed": name})),
        Operation::Replace { name, module, config, affinity } => host.replace_instance(&name, module.as_deref(), config, affinity),
        Operation::Configure { name, config, timing: [period, budget, hang] } => {
            if period.is_some() || budget.is_some() || hang.is_some() {
                host.set_timing(&name, period, budget, hang)?;
            }
            if let Some(config) = config {
                host.configure_instance(&name, config)?;
            }
            Ok(state_of(host, &name))
        }
        Operation::Start(name) => host.start_instance(&name).map(|()| state_of(host, &name)),
        Operation::Stop(name) => host.stop_instance(&name).map(|()| state_of(host, &name)),
        Operation::Bind(bind) => host
            .bind_port(&bind.instance, &bind.port, bind.channel.as_deref())
            .map(|()| json!({"instance": bind.instance, "port": bind.port, "channel": bind.channel})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Mode;
    use crate::host::{ClockSpec, HostOptions};
    use std::io::Write;
    use std::net::Shutdown;
    use std::os::unix::{fs::PermissionsExt, net::UnixStream};
    use std::time::Instant;

    struct Fixture {
        host: ModuleHost,
        server: Option<ControlServer>,
        _dir: tempfile::TempDir,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(server) = self.server.take() {
                let _ = server.close();
            }
            self.host.shutdown();
        }
    }

    fn wait_for_waiters(host: &ModuleHost, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while host.core.readiness_changed.receiver_count() != count {
            assert!(Instant::now() < deadline, "readiness waiter count did not become {count}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn cancelled_and_dropped_describe_requests_leave_no_waiters() {
        let dir = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir().unwrap();
        let socket = dir.path().join("module.sock");
        let mut options = HostOptions::new("readiness-test");
        options.workers = 1;
        options.clock = ClockSpec { mode: Mode::External, channel: Some("clock".into()), phase_t0_ns: None };
        let host = ModuleHost::start(options).unwrap();
        let server = ControlServer::start(host.clone(), &socket).unwrap();
        let mut fixture = Fixture { host, server: Some(server), _dir: dir };
        assert_eq!(fixture.host.core.readiness_changed.receiver_count(), 0);
        for cancel in [false, true] {
            let mut stream = UnixStream::connect(&socket).unwrap();
            stream
                .write_all(b"GET /v1/describe?wait_ready_ms=30000 HTTP/1.1\r\nHost: local\r\nX-Request-ID: readiness:drop\r\nX-Xrpc-Timeout-Ms: 30000\r\n\r\n")
                .unwrap();
            wait_for_waiters(&fixture.host, 1);
            if cancel {
                stream.shutdown(Shutdown::Both).unwrap();
            }
            drop(stream);
            wait_for_waiters(&fixture.host, 0);
        }
        // Closing the owner also cancels a held request and releases its receiver.
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .write_all(b"GET /v1/describe?wait_ready_ms=30000 HTTP/1.1\r\nHost: local\r\nX-Request-ID: readiness:close\r\nX-Xrpc-Timeout-Ms: 30000\r\n\r\n")
            .unwrap();
        wait_for_waiters(&fixture.host, 1);
        fixture.server.take().unwrap().close().unwrap();
        assert_eq!(fixture.host.core.readiness_changed.receiver_count(), 0);
    }
}
