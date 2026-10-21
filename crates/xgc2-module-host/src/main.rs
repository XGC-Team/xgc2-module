//! `xgc2-module-host --manifest FILE [--control-socket PATH] [--workers N] [--log-level LEVEL] [--check]`
//! `xgc2-module-host --describe-lib PATH`
//!
//! Runs the modules of one entity until SIGINT or SIGTERM. With `--check` the manifest is
//! validated (libraries are loaded and every channel is planned) and nothing is started.
//! `--describe-lib` loads one library and prints its descriptor. dlopen still runs ELF
//! constructors and `xgc2_module_entry`; the vtable lifecycle is not called.
//! Exit codes: 0 clean shutdown, successful check, or description; 2 startup or usage error.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use xgc2_module_host::abi;
use xgc2_module_host::control::ControlServer;
use xgc2_module_host::launch;
use xgc2_module_host::loader::{self, Dir};
use xgc2_module_host::log::{self, Level};
use xgc2_module_host::manifest::Manifest;

const USAGE: &str = "\
usage: xgc2-module-host --manifest FILE [--control-socket PATH] [--workers N] [--log-level debug|info|warn|error] [--check]
       xgc2-module-host --describe-lib PATH";
const MANIFEST_LIMIT: u64 = 1 << 20;

struct Args {
    manifest: PathBuf,
    control_socket: Option<PathBuf>,
    workers: Option<usize>,
    check: bool,
    describe_lib: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut manifest = None;
    let mut describe_lib = None;
    let mut control_socket = None;
    let mut workers = None;
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value\n{USAGE}"));
        match arg.as_str() {
            "--manifest" => manifest = Some(PathBuf::from(value("--manifest")?)),
            "--describe-lib" => describe_lib = Some(PathBuf::from(value("--describe-lib")?)),
            "--control-socket" => control_socket = Some(PathBuf::from(value("--control-socket")?)),
            "--workers" => {
                let text = value("--workers")?;
                workers = Some(text.parse::<usize>().map_err(|_| format!("--workers {text:?} is not a number"))?);
            }
            "--log-level" => {
                let text = value("--log-level")?;
                log::set_level(Level::parse(&text).ok_or_else(|| format!("--log-level {text:?} is not debug, info, warn or error"))?);
            }
            "--check" => check = true,
            "--help" | "-h" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if describe_lib.is_some() && (manifest.is_some() || control_socket.is_some() || workers.is_some() || check) {
        return Err(format!(
            "--describe-lib cannot be combined with --manifest, --check, --control-socket or --workers\n{USAGE}"
        ));
    }
    Ok(Args {
        manifest: match describe_lib {
            Some(_) => PathBuf::new(),
            None => manifest.ok_or_else(|| format!("--manifest is required\n{USAGE}"))?,
        },
        control_socket,
        workers,
        check,
        describe_lib,
    })
}

fn describe_lib(path: &Path) -> Result<(), String> {
    let module = loader::load(path, None).map_err(|error| error.to_string())?;
    let ports: Vec<_> = module.ports.iter().map(|port| {
        let flags = u32::from(port.required) * abi::PORT_REQUIRED | u32::from(port.async_writer) * abi::PORT_ASYNC_WRITER;
        serde_json::json!({
            "name": port.name,
            "dir": match port.dir {
                Dir::In => "in",
                Dir::Out => "out",
            },
            "kind": port.kind.as_str(),
            "schema": port.payload.schema,
            "required": port.required,
            "depth": port.queue_depth,
            "size": port.payload.size,
            "align": port.payload.align,
            "flags": flags,
        })
    }).collect();
    let document = serde_json::json!({
        "name": module.name,
        "version": module.version,
        "abi_major": abi::ABI_MAJOR,
        "abi_minor": module.abi_minor,
        "sha256": module.sha256,
        "ports": ports,
    });
    println!("{}", serde_json::to_string_pretty(&document).map_err(|error| error.to_string())?);
    Ok(())
}

fn read_manifest(path: &PathBuf) -> Result<Manifest, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let metadata = file.metadata().map_err(|e| format!("{}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MANIFEST_LIMIT {
        return Err(format!("{}: not a regular file of at most {MANIFEST_LIMIT} bytes", path.display()));
    }
    let mut text = String::new();
    file.take(MANIFEST_LIMIT).read_to_string(&mut text).map_err(|e| format!("{}: {e}", path.display()))?;
    let absolute = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let base = absolute.parent().map(PathBuf::from).unwrap_or_default();
    Manifest::parse(&text, &base).map_err(|e| format!("{}:\n{e}", path.display()))
}

/// Block SIGINT/SIGTERM in this thread (and every thread spawned later) and wait for one.
fn block_signals() -> libc::sigset_t {
    // SAFETY: plain libc calls on a locally owned set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

fn wait_for_signal(set: &libc::sigset_t) -> i32 {
    let mut signal = 0;
    // SAFETY: `set` is a valid set and `signal` a valid out pointer.
    unsafe { libc::sigwait(set, &mut signal) };
    signal
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    if let Some(path) = &args.describe_lib {
        return describe_lib(path);
    }
    let mut manifest = read_manifest(&args.manifest)?;
    if let Some(workers) = args.workers {
        manifest.host.workers = Some(workers);
    }
    if args.check {
        let report = launch::check(&manifest).map_err(|e| format!("{}:\n{e}", args.manifest.display()))?;
        println!("{}", serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?);
        return Ok(());
    }
    let signals = block_signals();
    let host = launch::launch(&manifest).map_err(|e| format!("start-up failed: {e}"))?;
    let socket = args.control_socket.or_else(|| manifest.control.socket.clone());
    let control = match socket {
        Some(socket) => match ControlServer::start(host.clone(), &socket) {
            Ok(server) => {
                println!("{}", serde_json::json!({"service_ref": server.service_ref()}));
                Some(server)
            }
            Err(error) => {
                host.shutdown();
                return Err(format!("control socket {}: {error}", socket.display()));
            }
        },
        None => None,
    };
    log::emit(Level::Info, "host", &format!("entity {} is up; waiting for SIGINT or SIGTERM", host.entity()));
    let signal = wait_for_signal(&signals);
    log::emit(Level::Info, "host", &format!("signal {signal}: shutting down"));
    if let Some(control) = control {
        if let Err(error) = control.close() {
            log::emit(Level::Warn, "host", &format!("control plane did not close cleanly: {error}"));
        }
    }
    host.shutdown();
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("xgc2-module-host: {message}");
            ExitCode::from(2)
        }
    }
}
