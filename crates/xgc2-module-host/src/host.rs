//! The host core: topology (libraries, instances, channels) and the hot-plug operations.
//!
//! All mutating operations hold the control lock, so they are serialized with each other but
//! never with the running instances: the instances of the entity keep stepping while one of
//! them is added, removed, replaced, rebound or reconfigured. A change to one instance pauses
//! only that instance, waits for its step to return and applies the change at that quiescent
//! point.

use crate::channel::{Channel, CommitHook};
use crate::clock::{Clock, Mode, Update};
use crate::instance::{lock, Completion, Env, Instance, OpKind, Params, State};
use crate::loader::{self, Dir, Module};
use crate::log::Level;
use crate::log_at;
use crate::names;
use crate::plan::{port_channels, ChannelSpec, Hint, Planner};
use crate::scheduler::{Affinity, Placement, Runner, Scheduler, Taken, Worker, MAX_INSTANCES};
use crate::timers::Timers;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[derive(Debug)]
pub enum HostError {
    /// The request is malformed or can never succeed as stated.
    Invalid(String),
    NotFound(String),
    /// The request conflicts with the current state (name taken, busy, pinned).
    Conflict(String),
    /// A call did not finish in time or the host is shutting down.
    Unavailable(String),
    /// A module refused the operation.
    Failed(String),
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostError::Invalid(m) | HostError::NotFound(m) | HostError::Conflict(m) | HostError::Unavailable(m) | HostError::Failed(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for HostError {}

impl From<loader::LoadError> for HostError {
    fn from(error: loader::LoadError) -> Self {
        HostError::Invalid(error.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct ClockSpec {
    pub mode: Mode,
    /// External mode: the state channel that carries the time.
    pub channel: Option<String>,
}

#[derive(Clone, Debug)]
pub struct HostOptions {
    pub entity: String,
    pub workers: usize,
    pub clock: ClockSpec,
    /// How long a hot-plug change waits for an instance's step to finish.
    pub quiesce_timeout: Duration,
    /// How long the control plane waits for one lifecycle operation of a module.
    pub op_timeout: Duration,
    pub hints: HashMap<String, Hint>,
}

impl HostOptions {
    pub fn new(entity: &str) -> Self {
        HostOptions {
            entity: entity.to_owned(),
            workers: default_workers(),
            clock: ClockSpec { mode: Mode::Steady, channel: None },
            quiesce_timeout: Duration::from_secs(2),
            op_timeout: Duration::from_secs(10),
            hints: HashMap::new(),
        }
    }
}

/// min(4, cores).
pub fn default_workers() -> usize {
    std::thread::available_parallelism().map_or(2, usize::from).min(4)
}

/// A request to create one instance.
#[derive(Clone, Debug)]
pub struct InstanceSpec {
    pub name: String,
    /// Registry handle of the module.
    pub module: String,
    pub affinity: Affinity,
    pub config_json: String,
    pub period_ns: i64,
    pub step_budget_ns: Option<i64>,
    pub hang_limit_ns: Option<i64>,
    pub required: bool,
    pub autostart: bool,
    /// Port name to channel name. Unlisted outputs get a private channel `<instance>.<port>`.
    pub bind: BTreeMap<String, String>,
}

impl InstanceSpec {
    pub fn new(name: &str, module: &str) -> Self {
        InstanceSpec {
            name: name.to_owned(),
            module: module.to_owned(),
            affinity: Affinity::Sticky,
            config_json: "{}".to_owned(),
            period_ns: 0,
            step_budget_ns: None,
            hang_limit_ns: None,
            required: true,
            autostart: true,
            bind: BTreeMap::new(),
        }
    }
}

pub const DEFAULT_EVENT_BUDGET_NS: i64 = 50_000_000;
pub const MIN_HANG_LIMIT_NS: i64 = 20_000_000;
const DEFAULT_MIN_HANG_NS: i64 = 100_000_000;

/// Budget defaults: one period for periodic instances, 50 ms otherwise; the hang limit is ten
/// budgets but at least 100 ms.
pub fn effective_limits(period_ns: i64, budget: Option<i64>, hang: Option<i64>) -> (i64, i64) {
    let budget = budget.unwrap_or(if period_ns > 0 { period_ns } else { DEFAULT_EVENT_BUDGET_NS });
    let hang = hang.unwrap_or((budget * 10).max(DEFAULT_MIN_HANG_NS));
    (budget, hang)
}

pub fn check_limits(budget_ns: i64, hang_ns: i64) -> Result<(), String> {
    if budget_ns <= 0 {
        return Err("the step budget must be positive".into());
    }
    if hang_ns < MIN_HANG_LIMIT_NS || hang_ns < budget_ns {
        return Err(format!("the hang limit must be at least {} ms and not below the step budget", MIN_HANG_LIMIT_NS / 1_000_000));
    }
    Ok(())
}

/// Port to channel bindings of a replacement, and the channel specs once they are applied.
type ReplacementPlan = (Vec<(usize, String)>, BTreeMap<String, ChannelSpec>);

pub(crate) struct Topology {
    pub(crate) modules: BTreeMap<String, Arc<Module>>,
    pub(crate) slots: Vec<Option<Arc<Instance>>>,
    names: HashMap<String, u32>,
    /// Instance slots in the order they were added.
    pub(crate) order: Vec<u32>,
    pub(crate) channels: BTreeMap<String, Arc<Channel>>,
}

pub(crate) struct Core {
    pub(crate) options: HostOptions,
    pub(crate) started: Instant,
    pub(crate) clock: Arc<Clock>,
    pub(crate) sched: Arc<Scheduler>,
    pub(crate) readiness_changed: watch::Sender<()>,
    timers: Arc<Timers>,
    timer_thread: Mutex<Option<JoinHandle<()>>>,
    topo: RwLock<Topology>,
    hints: RwLock<HashMap<String, Hint>>,
    /// Queue lengths the whole manifest asks for per event channel.
    depths: RwLock<HashMap<String, u32>>,
    control: Mutex<()>,
    down: AtomicBool,
}

impl Core {
    fn instance_at(&self, idx: u32) -> Option<Arc<Instance>> {
        self.topo.read().unwrap_or_else(|e| e.into_inner()).slots[idx as usize].clone()
    }
}

impl Runner for Core {
    fn run(&self, idx: u32, taken: Taken, worker: &Worker) {
        if let Some(instance) = self.instance_at(idx) {
            instance.run(taken, worker);
        }
    }

    fn hung(&self, idx: u32) {
        if let Some(instance) = self.instance_at(idx) {
            instance.isolate("a module call did not return within the hang limit");
        }
    }
}

/// Handle to a running host. Cloning is cheap; `shutdown` stops it for every clone.
#[derive(Clone)]
pub struct ModuleHost {
    pub(crate) core: Arc<Core>,
}

impl ModuleHost {
    /// Start the worker pool and the timer thread. No module is loaded yet.
    pub fn start(options: HostOptions) -> Result<ModuleHost, HostError> {
        if !names::valid_id(&options.entity) {
            return Err(HostError::Invalid(format!("entity id {:?} must match [A-Za-z0-9._:-]{{1,128}}", options.entity)));
        }
        let clock = Arc::new(Clock::new(options.clock.mode));
        let sched = Scheduler::new(options.workers.max(1));
        let timers = Timers::new(sched.clone(), clock.clone(), MAX_INSTANCES);
        let (readiness_changed, _) = watch::channel(());
        let mut topology = Topology {
            modules: BTreeMap::new(),
            slots: vec![None; MAX_INSTANCES],
            names: HashMap::new(),
            order: Vec::new(),
            channels: BTreeMap::new(),
        };
        if options.clock.mode == Mode::External {
            let name = options.clock.channel.clone().ok_or_else(|| HostError::Invalid("an external clock needs a channel".into()))?;
            let (hook_clock, hook_timers, changed) = (clock.clone(), timers.clone(), readiness_changed.clone());
            let hook: CommitHook = Box::new(move |bytes| {
                if let Ok(raw) = <[u8; 8]>::try_from(bytes) {
                    let ns = i64::from_ne_bytes(raw);
                    let update = hook_clock.set_external(ns);
                    hook_timers.on_clock(update, ns);
                    if update == Update::First {
                        changed.send_replace(());
                    }
                }
            });
            let mut planner = Planner::new(BTreeMap::new(), options.hints.clone());
            planner.add_clock_channel(&name);
            let spec = &planner.specs()[&name];
            let channel = Channel::new(&name, spec.kind, spec.payload.clone(), spec.depth, spec.max_readers, Some(hook))
                .map_err(HostError::Unavailable)?;
            topology.channels.insert(name.clone(), channel);
        }
        let core = Arc::new(Core {
            hints: RwLock::new(options.hints.clone()),
            depths: RwLock::new(HashMap::new()),
            options,
            started: Instant::now(),
            clock,
            sched: sched.clone(),
            readiness_changed,
            timers: timers.clone(),
            timer_thread: Mutex::new(None),
            topo: RwLock::new(topology),
            control: Mutex::new(()),
            down: AtomicBool::new(false),
        });
        sched.start(core.clone());
        let runner = timers.clone();
        let handle = std::thread::Builder::new()
            .name("xgc2-timers".into())
            .spawn(move || runner.run())
            .map_err(|e| HostError::Unavailable(format!("cannot start the timer thread: {e}")))?;
        *lock(&core.timer_thread) = Some(handle);
        Ok(ModuleHost { core })
    }

    pub fn entity(&self) -> &str {
        &self.core.options.entity
    }

    pub fn clock_mode(&self) -> Mode {
        self.core.clock.mode()
    }

    pub(crate) fn topo(&self) -> RwLockReadGuard<'_, Topology> {
        self.core.topo.read().unwrap_or_else(|e| e.into_inner())
    }

    fn topo_mut(&self) -> RwLockWriteGuard<'_, Topology> {
        self.core.topo.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Serialize control operations; fail fast when another one is running.
    fn exclusive(&self) -> Result<MutexGuard<'_, ()>, HostError> {
        if self.core.down.load(Ordering::Acquire) {
            return Err(HostError::Unavailable("the host is shutting down".into()));
        }
        match self.core.control.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => Err(HostError::Conflict("another control operation is in progress".into())),
        }
    }

    fn instance(&self, name: &str) -> Result<Arc<Instance>, HostError> {
        let topo = self.topo();
        topo.names
            .get(name)
            .and_then(|idx| topo.slots[*idx as usize].clone())
            .ok_or_else(|| HostError::NotFound(format!("no instance named {name}")))
    }

    fn module(&self, handle: &str) -> Result<Arc<Module>, HostError> {
        self.topo().modules.get(handle).cloned().ok_or_else(|| HostError::NotFound(format!("no module named {handle}")))
    }

    fn env(&self) -> Env {
        Env {
            clock: self.core.clock.clone(),
            sched: self.core.sched.clone(),
            timers: self.core.timers.clone(),
            readiness_changed: self.core.readiness_changed.clone(),
        }
    }

    fn planner(&self, specs: BTreeMap<String, ChannelSpec>) -> Planner {
        let hints = self.core.hints.read().unwrap_or_else(|e| e.into_inner()).clone();
        let depths = self.core.depths.read().unwrap_or_else(|e| e.into_inner()).clone();
        Planner::new(specs, hints).with_expected_depths(depths)
    }

    /// Channel overrides (queue depth, reader capacity) and the queue lengths the ports of a
    /// whole manifest ask for; both apply to channels created from now on. The manifest
    /// launcher declares them before the first instance.
    pub fn declare_channels(&self, hints: HashMap<String, Hint>, depths: HashMap<String, u32>) {
        *self.core.hints.write().unwrap_or_else(|e| e.into_inner()) = hints;
        *self.core.depths.write().unwrap_or_else(|e| e.into_inner()) = depths;
    }

    /// The loaded module registered under `handle`.
    pub fn loaded_module(&self, handle: &str) -> Result<Arc<Module>, HostError> {
        self.module(handle)
    }

    fn wait(&self, done: &Completion, what: &str) -> Result<(), HostError> {
        match done.wait(self.core.options.op_timeout) {
            Some(Ok(())) => Ok(()),
            Some(Err(message)) => Err(HostError::Failed(format!("{what}: {message}"))),
            None => Err(HostError::Unavailable(format!("{what}: no answer within {:?}", self.core.options.op_timeout))),
        }
    }

    /// Pause one instance and wait until its running task (if any) returned. On success the
    /// instance stays paused; the caller resumes it (or its start operation does).
    fn quiesce(&self, instance: &Instance) -> Result<(), HostError> {
        let was_paused = self.core.sched.is_paused(instance.idx);
        self.core.sched.pause(instance.idx);
        if self.core.sched.wait_not_running(instance.idx, self.core.options.quiesce_timeout) {
            return Ok(());
        }
        if !was_paused && instance.state() != State::Isolated {
            self.core.sched.resume(instance.idx);
        }
        Err(HostError::Conflict(format!(
            "instance {} is busy: its step did not return within {:?}",
            instance.name, self.core.options.quiesce_timeout
        )))
    }

    // ---- libraries --------------------------------------------------------------------

    /// Load a module library under `handle` (default: the module's own name).
    pub fn load_module(&self, handle: Option<&str>, path: &Path, sha256: Option<&str>) -> Result<Value, HostError> {
        let _control = self.exclusive()?;
        if let Some(handle) = handle {
            if !names::valid_name(handle) {
                return Err(HostError::Invalid(format!("module name {handle:?} is not a valid name")));
            }
        }
        if sha256.is_some_and(|pin| !names::valid_sha256(pin)) {
            return Err(HostError::Invalid("sha256 must be 64 hex digits".into()));
        }
        if let Ok(canonical) = path.canonicalize() {
            // dlopen answers a second open of the same path with the code that is already
            // mapped, whatever the file contains now, so a loaded path is never loaded again.
            let loaded = self.topo().modules.iter().find(|(_, module)| module.canonical == canonical).map(|(name, _)| name.clone());
            if let Some(other) = loaded {
                return Err(HostError::Conflict(format!("{} is already loaded as module {other}", canonical.display())));
            }
        }
        let module = loader::load(path, sha256)?;
        let handle = handle.map_or_else(|| module.name.clone(), str::to_owned);
        let mut topo = self.topo_mut();
        if topo.modules.contains_key(&handle) {
            return Err(HostError::Conflict(format!("a module named {handle} is already loaded; pass another name")));
        }
        let module = Arc::new(module);
        let info = crate::observe::module_json(&handle, &module, &[], false);
        log_at!(Level::Info, "host", "loaded module {handle} ({} {}) from {}", module.name, module.version, module.canonical.display());
        topo.modules.insert(handle, module);
        Ok(info)
    }

    /// Unload a library that no instance uses.
    pub fn unload_module(&self, handle: &str) -> Result<(), HostError> {
        let _control = self.exclusive()?;
        let module = {
            let topo = self.topo();
            let module = topo.modules.get(handle).cloned().ok_or_else(|| HostError::NotFound(format!("no module named {handle}")))?;
            let users: Vec<String> = topo
                .slots
                .iter()
                .flatten()
                .filter(|instance| Arc::ptr_eq(&instance.module, &module))
                .map(|instance| instance.name.clone())
                .collect();
            if !users.is_empty() {
                return Err(HostError::Conflict(format!("module {handle} is used by instance(s) {}", users.join(", "))));
            }
            module
        };
        if module.pinned.load(Ordering::Acquire) {
            return Err(HostError::Conflict(format!(
                "module {handle} is pinned by an abandoned instance; the library stays mapped until the process exits"
            )));
        }
        // A worker may still hold the last instance for a moment after it was removed.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Arc::strong_count(&module) > 2 {
            if Instant::now() >= deadline {
                return Err(HostError::Conflict(format!("module {handle} is still referenced by a running task")));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.topo_mut().modules.remove(handle);
        drop(module);
        log_at!(Level::Info, "host", "unloaded module {handle}");
        Ok(())
    }

    // ---- channels ---------------------------------------------------------------------

    /// Bound writer and reader ports per channel name.
    fn bound_counts(topo: &Topology) -> HashMap<String, (u32, u32)> {
        let mut counts: HashMap<String, (u32, u32)> = HashMap::new();
        for instance in topo.slots.iter().flatten() {
            for port in &instance.ports {
                if let Some(channel) = port.bound_channel() {
                    let entry = counts.entry(channel.name().to_owned()).or_default();
                    match port.spec.dir {
                        Dir::Out => entry.0 += 1,
                        Dir::In => entry.1 += 1,
                    }
                }
            }
        }
        counts
    }

    fn topology_specs(&self, topo: &Topology) -> BTreeMap<String, ChannelSpec> {
        let counts = Self::bound_counts(topo);
        topo.channels
            .iter()
            .map(|(name, channel)| {
                let (writers, readers) = counts.get(name).copied().unwrap_or_default();
                let spec = ChannelSpec {
                    kind: channel.kind(),
                    payload: channel.spec().clone(),
                    depth: channel.depth(),
                    max_readers: channel.max_readers(),
                    writers,
                    readers,
                    keep: self.core.options.clock.channel.as_deref() == Some(name.as_str()),
                };
                (name.clone(), spec)
            })
            .collect()
    }

    /// Drop channels that no port is bound to any more.
    fn collect_channels(&self, topo: &mut Topology) {
        let counts = Self::bound_counts(topo);
        let clock = self.core.options.clock.channel.clone();
        topo.channels.retain(|name, _| counts.contains_key(name) || clock.as_deref() == Some(name.as_str()));
    }

    fn channel_for(topo: &mut Topology, name: &str, spec: &ChannelSpec) -> Result<Arc<Channel>, HostError> {
        if let Some(channel) = topo.channels.get(name) {
            return Ok(channel.clone());
        }
        let channel =
            Channel::new(name, spec.kind, spec.payload.clone(), spec.depth, spec.max_readers, None).map_err(HostError::Unavailable)?;
        topo.channels.insert(name.to_owned(), channel.clone());
        Ok(channel)
    }

    /// Connect `instance` to the channels of `bindings`, creating channels as planned.
    fn connect(&self, instance: &Instance, bindings: &[(usize, String)], specs: &BTreeMap<String, ChannelSpec>) -> Result<(), HostError> {
        let mut topo = self.topo_mut();
        let resolved: Result<Vec<(usize, Arc<Channel>)>, HostError> =
            bindings.iter().map(|(port, name)| Self::channel_for(&mut topo, name, &specs[name]).map(|channel| (*port, channel))).collect();
        let result = resolved.and_then(|resolved| instance.bind_all(&resolved).map_err(HostError::Conflict));
        if result.is_err() {
            // Channels created for this attempt must not outlive it.
            self.collect_channels(&mut topo);
        }
        result
    }

    // ---- instances --------------------------------------------------------------------

    pub fn add_instance(&self, spec: InstanceSpec) -> Result<Value, HostError> {
        let _control = self.exclusive()?;
        let instance = self.add_instance_locked(spec)?;
        Ok(self.instance_json(&instance))
    }

    fn add_instance_locked(&self, spec: InstanceSpec) -> Result<Arc<Instance>, HostError> {
        if !names::valid_name(&spec.name) {
            return Err(HostError::Invalid(format!("instance name {:?} is not a valid name", spec.name)));
        }
        let module = self.module(&spec.module)?;
        let (bindings, specs) = {
            let topo = self.topo();
            if topo.names.contains_key(&spec.name) {
                return Err(HostError::Conflict(format!("an instance named {} exists", spec.name)));
            }
            let bindings = port_channels(&spec.name, &module.ports, &spec.bind, &module.name).map_err(HostError::Invalid)?;
            let mut planner = self.planner(self.topology_specs(&topo));
            for (port, channel) in &bindings {
                planner.bind(&spec.name, &module.ports[*port], channel).map_err(HostError::Invalid)?;
            }
            (bindings, planner.into_specs())
        };
        let (budget, hang) = effective_limits(spec.period_ns, spec.step_budget_ns, spec.hang_limit_ns);
        check_limits(budget, hang).map_err(|e| HostError::Invalid(format!("instance {}: {e}", spec.name)))?;
        let params =
            Params { config_json: spec.config_json.clone(), period_ns: spec.period_ns, step_budget_ns: budget, hang_limit_ns: hang };
        let placement = match spec.affinity {
            Affinity::Sticky => Placement::Balanced,
            Affinity::Any => Placement::Any,
        };
        let (idx, generation) =
            self.core.sched.alloc(placement).ok_or_else(|| HostError::Conflict(format!("at most {MAX_INSTANCES} instances")))?;
        let instance = Instance::new(&spec.name, &spec.module, module, spec.required, params, idx, generation, self.env());
        if let Err(error) = self.connect(&instance, &bindings, &specs) {
            self.core.sched.free(idx);
            return Err(error);
        }
        {
            let mut topo = self.topo_mut();
            topo.slots[idx as usize] = Some(instance.clone());
            topo.names.insert(spec.name.clone(), idx);
            topo.order.push(idx);
        }
        let created = instance.post(OpKind::Create);
        let result = self.wait(&created, &format!("create {}", spec.name)).and_then(|()| {
            if spec.autostart {
                let started = instance.post(OpKind::Start);
                self.wait(&started, &format!("start {}", spec.name))
            } else {
                Ok(())
            }
        });
        if let Err(error) = result {
            self.discard(&instance);
            return Err(error);
        }
        log_at!(Level::Info, "host", "instance {} ({}) {}", spec.name, spec.module, if spec.autostart { "started" } else { "created" });
        Ok(instance)
    }

    /// Best-effort stop and destroy, then release everything the instance held. An isolated
    /// instance is leaked on purpose: a stuck thread may still be inside the module and the
    /// module may still call the host, so neither the instance nor its library can be freed.
    fn discard(&self, instance: &Arc<Instance>) {
        if instance.state() != State::Isolated {
            let _ = instance.post(OpKind::Stop).wait(self.core.options.op_timeout);
            let _ = instance.post(OpKind::Destroy).wait(self.core.options.op_timeout);
        }
        instance.unbind_all();
        {
            let mut topo = self.topo_mut();
            topo.slots[instance.idx as usize] = None;
            if topo.names.get(&instance.name) == Some(&instance.idx) {
                topo.names.remove(&instance.name);
            }
            topo.order.retain(|idx| *idx != instance.idx);
            self.collect_channels(&mut topo);
        }
        if instance.state() == State::Isolated {
            instance.module.pinned.store(true, Ordering::Release);
            std::mem::forget(instance.clone());
        }
        self.core.timers.disarm(instance.idx);
        self.core.sched.free(instance.idx);
        self.core.readiness_changed.send_replace(());
    }

    pub fn remove_instance(&self, name: &str) -> Result<(), HostError> {
        let _control = self.exclusive()?;
        let instance = self.instance(name)?;
        if instance.state() != State::Isolated {
            self.quiesce(&instance)?;
        }
        self.discard(&instance);
        log_at!(Level::Info, "host", "instance {name} removed");
        Ok(())
    }

    pub fn start_instance(&self, name: &str) -> Result<(), HostError> {
        let _control = self.exclusive()?;
        let instance = self.instance(name)?;
        let done = instance.post(OpKind::Start);
        self.wait(&done, &format!("start {name}"))
    }

    pub fn stop_instance(&self, name: &str) -> Result<(), HostError> {
        let _control = self.exclusive()?;
        let instance = self.instance(name)?;
        let done = instance.post(OpKind::Stop);
        self.wait(&done, &format!("stop {name}"))
    }

    /// Live configure: the module applies `config_json` between two steps.
    pub fn configure_instance(&self, name: &str, config_json: String) -> Result<(), HostError> {
        let _control = self.exclusive()?;
        let instance = self.instance(name)?;
        let done = instance.post(OpKind::Configure(config_json));
        self.wait(&done, &format!("configure {name}"))
    }

    /// Change period and budgets of an instance without restarting it; absent values stay.
    pub fn set_timing(&self, name: &str, period_ns: Option<i64>, budget_ns: Option<i64>, hang_ns: Option<i64>) -> Result<(), HostError> {
        let _control = self.exclusive()?;
        let instance = self.instance(name)?;
        let current = instance.params();
        let budget = budget_ns.unwrap_or(current.step_budget_ns);
        let hang = hang_ns.unwrap_or(current.hang_limit_ns);
        check_limits(budget, hang).map_err(|e| HostError::Invalid(format!("instance {name}: {e}")))?;
        instance.set_limits(budget, hang);
        if let Some(period) = period_ns {
            instance.set_period_ns(period);
        }
        Ok(())
    }

    /// Replace an instance by a new instance of `module` (default: a fresh instance of the
    /// same module). The new instance takes over the channels, the event backlog and, unless
    /// `config_json` is given, the configuration. If the new instance cannot start, the
    /// previous one is restarted. Omitted affinity preserves the previous affinity; sticky
    /// replacements keep their home slot.
    pub fn replace_instance(
        &self,
        name: &str,
        module: Option<&str>,
        config_json: Option<String>,
        affinity: Option<Affinity>,
    ) -> Result<Value, HostError> {
        let _control = self.exclusive()?;
        let old = self.instance(name)?;
        if old.state() == State::Isolated {
            return Err(HostError::Conflict(format!("instance {name} is isolated; remove it first")));
        }
        let handle = module.map_or_else(|| old.module_handle.clone(), str::to_owned);
        let module = self.module(&handle)?;
        let mut params = old.params();
        if let Some(config) = config_json {
            params.config_json = config;
        }
        let new = self.swap(&old, &handle, module, params, affinity.unwrap_or(old.affinity))?;
        Ok(self.instance_json(&new))
    }

    /// Plan the channels of the replacement: every bound port of the old instance keeps its
    /// channel, new outputs get private channels, and the new module must fit.
    fn plan_replacement(&self, old: &Instance, module: &Module) -> Result<ReplacementPlan, HostError> {
        let name = &old.name;
        let topo = self.topo();
        let mut planner = self.planner(self.topology_specs(&topo));
        for port in &old.ports {
            if let Some(channel) = port.bound_channel() {
                planner.unbind(&port.spec, channel.name());
            }
        }
        let mut bindings = Vec::new();
        for (index, port) in module.ports.iter().enumerate() {
            let existing = old.port_index(&port.name).and_then(|i| old.bound_channel(i));
            let channel = match (existing, port.dir) {
                (Some(channel), _) => channel.name().to_owned(),
                (None, Dir::Out) => format!("{name}.{}", port.name),
                (None, Dir::In) => continue,
            };
            planner.bind(name, port, &channel).map_err(|e| HostError::Invalid(format!("cannot replace {name}: {e}")))?;
            bindings.push((index, channel));
        }
        // A port the new module lacks may only drop a channel nobody else uses.
        for (index, port) in old.ports.iter().enumerate() {
            let Some(channel) = old.bound_channel(index).filter(|_| module.port_index(&port.spec.name).is_none()) else { continue };
            let shared = topo
                .slots
                .iter()
                .flatten()
                .filter(|other| other.idx != old.idx)
                .any(|other| other.ports.iter().any(|p| p.bound_channel().is_some_and(|c| Arc::ptr_eq(&c, &channel))));
            if shared {
                return Err(HostError::Invalid(format!(
                    "cannot replace {name}: module {} has no port {} but channel {} connects it to other instances",
                    module.name,
                    port.spec.name,
                    channel.name()
                )));
            }
        }
        Ok((bindings, planner.into_specs()))
    }

    fn swap(
        &self,
        old: &Arc<Instance>,
        handle: &str,
        module: Arc<Module>,
        params: Params,
        affinity: Affinity,
    ) -> Result<Arc<Instance>, HostError> {
        let name = old.name.clone();
        let (bindings, specs) = self.plan_replacement(old, &module)?;
        // Create the new instance while the old one keeps running.
        let placement = match affinity {
            Affinity::Sticky => self.core.sched.cell(old.idx).home().map_or(Placement::Balanced, Placement::Slot),
            Affinity::Any => Placement::Any,
        };
        let (idx, generation) =
            self.core.sched.alloc(placement).ok_or_else(|| HostError::Conflict(format!("at most {MAX_INSTANCES} instances")))?;
        let new = Instance::new(&name, handle, module, old.required, params, idx, generation, self.env());
        self.topo_mut().slots[idx as usize] = Some(new.clone());
        let created = new.post(OpKind::Create);
        if let Err(error) = self.wait(&created, &format!("create the replacement of {name}")) {
            self.discard(&new);
            return Err(error);
        }
        // Quiescent point of the old instance: from here on a failure restores it.
        if let Err(error) = self.quiesce(old) {
            self.discard(&new);
            return Err(error);
        }
        let mut readers = Some(old.release_readers());
        let stopped = old.post(OpKind::Stop);
        if let Err(error) = self.wait(&stopped, &format!("stop {name} for replacement")) {
            log_at!(Level::Warn, "host", "{error}");
        }
        let old_bindings = old.unbind_all();
        let switched = self.connect(&new, &bindings, &specs).and_then(|()| {
            self.point_name_at(&name, old.idx, new.idx);
            new.adopt_readers(readers.take().unwrap_or_default());
            let started = new.post(OpKind::Start);
            self.wait(&started, &format!("start the replacement of {name}"))
        });
        match switched {
            Ok(()) => {
                let destroyed = old.post(OpKind::Destroy);
                if let Err(error) = self.wait(&destroyed, &format!("destroy the previous {name}")) {
                    log_at!(Level::Warn, "host", "{error}");
                }
                self.discard(old);
                log_at!(Level::Info, "host", "instance {name} replaced by {handle}");
                Ok(new)
            }
            Err(error) => {
                log_at!(Level::Error, "host", "replacing {name} failed ({error}); restoring the previous instance");
                let back = readers.take().unwrap_or_else(|| new.release_readers());
                let _ = new.post(OpKind::Stop).wait(self.core.options.op_timeout);
                new.unbind_all();
                self.point_name_at(&name, new.idx, old.idx);
                let restored = old.bind_all(&old_bindings).is_ok() && {
                    old.adopt_readers(back);
                    let restarted = old.post(OpKind::Start);
                    self.wait(&restarted, &format!("restart the previous {name}")).is_ok()
                };
                self.discard(&new);
                Err(HostError::Failed(format!(
                    "replacing {name} failed: {error}; the previous instance {}",
                    if restored { "was restored" } else { "could not be restarted" }
                )))
            }
        }
    }

    /// `name` now refers to the instance in slot `to` (it referred to `from`).
    fn point_name_at(&self, name: &str, from: u32, to: u32) {
        let mut topo = self.topo_mut();
        topo.names.insert(name.to_owned(), to);
        for slot in topo.order.iter_mut().filter(|slot| **slot == from) {
            *slot = to;
        }
    }

    /// Connect an input port to `channel`, or disconnect it with `None`; an output port with
    /// `None` returns to its private channel `<instance>.<port>`.
    pub fn bind_port(&self, instance: &str, port: &str, channel: Option<&str>) -> Result<(), HostError> {
        let _control = self.exclusive()?;
        let instance = self.instance(instance)?;
        let index =
            instance.port_index(port).ok_or_else(|| HostError::Invalid(format!("instance {} has no port {port}", instance.name)))?;
        let spec = instance.ports[index].spec.clone();
        let target = match (channel, spec.dir) {
            (Some(channel), _) => {
                if !names::valid_name(channel) {
                    return Err(HostError::Invalid(format!("channel name {channel:?} is not a valid name")));
                }
                Some(channel.to_owned())
            }
            (None, Dir::Out) => Some(format!("{}.{}", instance.name, spec.name)),
            (None, Dir::In) => None,
        };
        let current = instance.bound_channel(index);
        if current.as_ref().map(|c| c.name().to_owned()) == target {
            return Ok(());
        }
        let specs = {
            let topo = self.topo();
            let mut planner = self.planner(self.topology_specs(&topo));
            if let Some(current) = &current {
                planner.unbind(&spec, current.name());
            }
            if let Some(target) = &target {
                planner.bind(&instance.name, &spec, target).map_err(HostError::Invalid)?;
            }
            planner.into_specs()
        };
        let was_paused = self.core.sched.is_paused(instance.idx);
        if instance.state() != State::Isolated {
            self.quiesce(&instance)?;
        }
        let result = (|| {
            instance.unbind(index);
            if let Some(target) = &target {
                let new = Self::channel_for(&mut self.topo_mut(), target, &specs[target]);
                let new = match new {
                    Ok(channel) => channel,
                    Err(error) => {
                        if let Some(current) = &current {
                            let _ = instance.bind(index, current.clone());
                        }
                        return Err(error);
                    }
                };
                if let Err(message) = instance.bind(index, new) {
                    // Put the old connection back before reporting.
                    if let Some(current) = &current {
                        let _ = instance.bind(index, current.clone());
                    }
                    return Err(HostError::Conflict(message));
                }
            }
            self.collect_channels(&mut self.topo_mut());
            if instance.state() == State::Running {
                instance.attach_input(index).map_err(HostError::Failed)?;
            }
            Ok(())
        })();
        if !was_paused && instance.state() != State::Isolated {
            self.core.sched.resume(instance.idx);
        }
        if result.is_ok() {
            log_at!(Level::Info, "host", "port {}.{} bound to {}", instance.name, spec.name, target.as_deref().unwrap_or("nothing"));
        }
        self.core.readiness_changed.send_replace(());
        result
    }

    // ---- shutdown ---------------------------------------------------------------------

    /// Stop every instance (newest first), destroy it and stop the threads. Idempotent.
    pub fn shutdown(&self) {
        if self.core.down.swap(true, Ordering::AcqRel) {
            return;
        }
        let _control = lock(&self.core.control);
        let order: Vec<Arc<Instance>> = {
            let topo = self.topo();
            topo.order.iter().rev().filter_map(|idx| topo.slots[*idx as usize].clone()).collect()
        };
        let budget = Duration::from_secs(5).min(self.core.options.op_timeout);
        for instance in order {
            if instance.state() == State::Isolated {
                continue;
            }
            self.core.sched.pause(instance.idx);
            self.core.sched.wait_not_running(instance.idx, self.core.options.quiesce_timeout);
            let _ = instance.post(OpKind::Stop).wait(budget);
            let _ = instance.post(OpKind::Destroy).wait(budget);
            instance.detach_inputs();
        }
        self.core.timers.stop();
        if let Some(handle) = lock(&self.core.timer_thread).take() {
            let _ = handle.join();
        }
        self.core.sched.shutdown(Duration::from_secs(2));
    }
}
