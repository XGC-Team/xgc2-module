//! One module instance: its ports, parameters, lifecycle operations and step execution.
//!
//! The scheduler runs an instance as a serial task. A task first executes the queued
//! lifecycle operations (create, configure, start, stop, destroy) in order, then at most one
//! step. Because both happen on the instance's own task, a live `configure` or a `stop` is
//! applied exactly between two steps, with no extra locking against the module.

use crate::abi::{self, HostApi, Status, StepCtx};
use crate::api;
use crate::channel::{Channel, Reader, Subscriber, View, WriteError, Writer};
use crate::clock::{steady_ns, Clock};
use crate::loader::{Dir, Module, PortSpec};
use crate::log::Level;
use crate::log_at;
use crate::metrics::Histogram;
use crate::scheduler::{Affinity, Scheduler, Taken, Worker, REASON_INPUT, REASON_OPS, REASON_TIMER, REASON_WAKE};
use crate::timers::{Timers, MIN_PERIOD_NS};
use std::collections::VecDeque;
use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Lifecycle calls (create, start, stop, destroy) get at least this long before the instance
/// counts as hung, whatever its step hang limit is.
pub const LIFECYCLE_MIN_LIMIT_NS: i64 = 5_000_000_000;

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    /// Record exists; the module instance is not created yet.
    New = 0,
    Created,
    Starting,
    Running,
    Stopping,
    Stopped,
    /// The module reported failure or returned an internal error; no further steps.
    Failed,
    /// A module call exceeded the hang limit; the instance is abandoned.
    Isolated,
    Removed,
}

impl State {
    fn from_u8(value: u8) -> State {
        match value {
            0 => State::New,
            1 => State::Created,
            2 => State::Starting,
            3 => State::Running,
            4 => State::Stopping,
            5 => State::Stopped,
            6 => State::Failed,
            7 => State::Isolated,
            _ => State::Removed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            State::New => "new",
            State::Created => "created",
            State::Starting => "starting",
            State::Running => "running",
            State::Stopping => "stopping",
            State::Stopped => "stopped",
            State::Failed => "failed",
            State::Isolated => "isolated",
            State::Removed => "removed",
        }
    }

    pub fn is_live(self) -> bool {
        matches!(self, State::Running)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Ok,
    Degraded,
    Failed,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Degraded => "degraded",
            Health::Failed => "failed",
        }
    }
}

/// Live-changeable parameters of an instance.
#[derive(Clone, Debug)]
pub struct Params {
    /// JSON object text passed to create and configure.
    pub config_json: String,
    /// 0 = no period timer.
    pub period_ns: i64,
    pub step_budget_ns: i64,
    pub hang_limit_ns: i64,
}

/// Period and budgets of an instance, readable without a lock.
pub struct Timing {
    period_ns: AtomicI64,
    step_budget_ns: AtomicI64,
    hang_limit_ns: AtomicI64,
}

pub enum OpKind {
    Create,
    Configure(String),
    Start,
    Stop,
    Destroy,
}

/// Completion of a lifecycle operation, awaited by the control plane.
#[derive(Default)]
pub struct Completion {
    result: Mutex<Option<Result<(), String>>>,
    done: Condvar,
}

impl Completion {
    pub fn finish(&self, result: Result<(), String>) {
        *lock(&self.result) = Some(result);
        self.done.notify_all();
    }

    /// `None` when the operation did not finish in time.
    pub fn wait(&self, timeout: Duration) -> Option<Result<(), String>> {
        let deadline = Instant::now() + timeout;
        let mut result = lock(&self.result);
        loop {
            if let Some(done) = result.take() {
                return Some(done);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            result = self.done.wait_timeout(result, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
        }
    }
}

struct Op {
    kind: OpKind,
    done: Arc<Completion>,
}

/// What a port is connected to.
pub enum PortIo {
    Unbound,
    Out {
        channel: Arc<Channel>,
        writer: Writer,
    },
    /// `reader` is present while the instance is started.
    In {
        channel: Arc<Channel>,
        reader: Option<Reader>,
    },
}

/// A reader taken from a stopping instance, waiting to be adopted by its replacement.
pub struct Released {
    port: String,
    channel: Arc<Channel>,
    reader: Reader,
}

pub struct PortRt {
    pub spec: PortSpec,
    /// Bit in `changed_inputs` (inputs only).
    pub input_bit: Option<u8>,
    pub io: Mutex<PortIo>,
}

impl PortRt {
    /// The channel this port is bound to.
    pub fn bound_channel(&self) -> Option<Arc<Channel>> {
        match &*lock(&self.io) {
            PortIo::Unbound => None,
            PortIo::Out { channel, .. } | PortIo::In { channel, .. } => Some(channel.clone()),
        }
    }
}

/// Delivers input commits of one instance to the scheduler.
pub struct InputWake {
    sched: Arc<Scheduler>,
    idx: u32,
    generation: u32,
}

impl Subscriber for InputWake {
    fn notify(&self, bit: u64, commit_ns: i64) {
        self.sched.mark_dirty(self.idx, self.generation, bit, commit_ns);
    }
}

#[derive(Default)]
pub struct Stats {
    pub steps: AtomicU64,
    pub step_time: Histogram,
    /// Oldest unprocessed input commit to step start.
    pub handoff: Histogram,
    pub overruns: AtomicU64,
    pub step_errors: AtomicU64,
    /// Runs whose dirty bits had nothing new to read.
    pub spurious: AtomicU64,
    /// Calls the module made against the API contract (a second write_begin, bad ports).
    pub misuse: AtomicU64,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub health: i32,
    pub detail: String,
}

/// Shared services every instance needs.
#[derive(Clone)]
pub struct Env {
    pub clock: Arc<Clock>,
    pub sched: Arc<Scheduler>,
    pub timers: Arc<Timers>,
    pub readiness_changed: tokio::sync::watch::Sender<()>,
}

pub struct Instance {
    pub name: String,
    /// Registry handle of the module this instance was created from.
    pub module_handle: String,
    pub idx: u32,
    pub generation: u32,
    pub module: Arc<Module>,
    pub required: bool,
    pub affinity: Affinity,
    /// Pool slot that last ran this instance (the home slot before the first sticky run).
    worker_slot: AtomicUsize,
    last_worker_id: AtomicUsize,
    pub worker_moves: AtomicU64,
    pub ports: Vec<PortRt>,
    pub stats: Stats,
    env: Env,
    /// JSON object text of the current configuration.
    config: Mutex<String>,
    /// Period and budgets are read on every step, so they are plain atomics.
    timing: Timing,
    state: AtomicU8,
    last_error: Mutex<Option<String>>,
    handle: AtomicPtr<abi::Instance>,
    ops: Mutex<VecDeque<Op>>,
    /// Completion of the operation being executed, so isolation can fail it.
    inflight: Mutex<Option<Arc<Completion>>>,
    config_applied: AtomicBool,
    /// The module's `start` returned OK and `stop` has not been called since.
    started: AtomicBool,
    changed: AtomicU64,
    step_index: AtomicU64,
    wake: Arc<InputWake>,
    report: Mutex<Report>,
    reported_health: AtomicI32,
    over_budget: AtomicBool,
}

impl Instance {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &str,
        module_handle: &str,
        module: Arc<Module>,
        required: bool,
        params: Params,
        idx: u32,
        generation: u32,
        env: Env,
    ) -> Arc<Instance> {
        let ports = module
            .ports
            .iter()
            .enumerate()
            .map(|(index, spec)| PortRt { spec: spec.clone(), input_bit: module.input_bit(index), io: Mutex::new(PortIo::Unbound) })
            .collect();
        let home = env.sched.cell(idx).home();
        Arc::new(Instance {
            name: name.to_owned(),
            module_handle: module_handle.to_owned(),
            idx,
            generation,
            module,
            required,
            affinity: if home.is_some() { Affinity::Sticky } else { Affinity::Any },
            worker_slot: AtomicUsize::new(home.unwrap_or(usize::MAX)),
            last_worker_id: AtomicUsize::new(usize::MAX),
            worker_moves: AtomicU64::new(0),
            ports,
            stats: Stats::default(),
            wake: Arc::new(InputWake { sched: env.sched.clone(), idx, generation }),
            env,
            config: Mutex::new(params.config_json),
            timing: Timing {
                period_ns: AtomicI64::new(params.period_ns),
                step_budget_ns: AtomicI64::new(params.step_budget_ns),
                hang_limit_ns: AtomicI64::new(params.hang_limit_ns),
            },
            state: AtomicU8::new(State::New as u8),
            last_error: Mutex::new(None),
            handle: AtomicPtr::new(std::ptr::null_mut()),
            ops: Mutex::new(VecDeque::new()),
            inflight: Mutex::new(None),
            config_applied: AtomicBool::new(false),
            started: AtomicBool::new(false),
            changed: AtomicU64::new(0),
            step_index: AtomicU64::new(0),
            report: Mutex::new(Report::default()),
            reported_health: AtomicI32::new(abi::HEALTH_OK),
            over_budget: AtomicBool::new(false),
        })
    }

    // ---- accessors -------------------------------------------------------------------

    pub fn state(&self) -> State {
        State::from_u8(self.state.load(Ordering::Acquire))
    }

    pub fn worker_slot(&self) -> Option<usize> {
        let slot = self.worker_slot.load(Ordering::Relaxed);
        (slot != usize::MAX).then_some(slot)
    }

    fn set_state(&self, state: State) {
        self.state.store(state as u8, Ordering::Release);
        self.env.readiness_changed.send_replace(());
    }

    /// Snapshot of the configuration and timing (control plane only).
    pub fn params(&self) -> Params {
        Params {
            config_json: lock(&self.config).clone(),
            period_ns: self.timing.period_ns.load(Ordering::Relaxed),
            step_budget_ns: self.timing.step_budget_ns.load(Ordering::Relaxed),
            hang_limit_ns: self.timing.hang_limit_ns.load(Ordering::Relaxed),
        }
    }

    /// New budgets, applied from the next step on.
    pub fn set_limits(&self, step_budget_ns: i64, hang_limit_ns: i64) {
        self.timing.step_budget_ns.store(step_budget_ns, Ordering::Relaxed);
        self.timing.hang_limit_ns.store(hang_limit_ns, Ordering::Relaxed);
    }

    pub fn last_error(&self) -> Option<String> {
        lock(&self.last_error).clone()
    }

    fn fail_with(&self, message: String) {
        log_at!(Level::Error, &self.name, "{message}");
        *lock(&self.last_error) = Some(message);
    }

    pub fn report(&self) -> Report {
        lock(&self.report).clone()
    }

    pub fn health(&self) -> Health {
        let reported = self.reported_health.load(Ordering::Acquire);
        match self.state() {
            State::Failed | State::Isolated => Health::Failed,
            _ if reported >= abi::HEALTH_FAILED => Health::Failed,
            _ if self.over_budget.load(Ordering::Acquire) || reported == abi::HEALTH_DEGRADED => Health::Degraded,
            _ => Health::Ok,
        }
    }

    pub fn over_budget(&self) -> bool {
        self.over_budget.load(Ordering::Acquire)
    }

    pub fn step_index(&self) -> u64 {
        self.step_index.load(Ordering::Relaxed)
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    pub fn host_ctx(&self) -> *mut c_void {
        self as *const Instance as *mut c_void
    }

    pub fn port_index(&self, name: &str) -> Option<usize> {
        self.ports.iter().position(|port| port.spec.name == name)
    }

    /// Channel a port is bound to.
    pub fn bound_channel(&self, port: usize) -> Option<Arc<Channel>> {
        self.ports[port].bound_channel()
    }

    // ---- binding (control plane; the instance is paused or not yet started) -----------

    /// Connect a port to a channel. An output port registers as a writer of the channel,
    /// which a state channel accepts once.
    pub fn bind(&self, port: usize, channel: Arc<Channel>) -> Result<(), String> {
        let rt = &self.ports[port];
        let mut io = lock(&rt.io);
        if !matches!(*io, PortIo::Unbound) {
            return Err(format!("port {} is already bound", rt.spec.name));
        }
        *io = match rt.spec.dir {
            Dir::Out => {
                channel.add_writer().map_err(|_| format!("channel {} already has a writer", channel.name()))?;
                PortIo::Out { channel, writer: Writer::default() }
            }
            Dir::In => PortIo::In { channel, reader: None },
        };
        Ok(())
    }

    /// Disconnect a port: a pending write is abandoned, an attached reader detaches and an
    /// output stops being a writer of its channel.
    pub fn unbind(&self, port: usize) -> Option<Arc<Channel>> {
        match std::mem::replace(&mut *lock(&self.ports[port].io), PortIo::Unbound) {
            PortIo::Unbound => None,
            PortIo::Out { channel, mut writer } => {
                channel.abort(&mut writer);
                channel.remove_writer();
                Some(channel)
            }
            PortIo::In { channel, reader } => {
                if let Some(reader) = reader {
                    channel.detach_reader(reader);
                }
                Some(channel)
            }
        }
    }

    /// Disconnect every port; returns what was bound (port index, channel).
    pub fn unbind_all(&self) -> Vec<(usize, Arc<Channel>)> {
        (0..self.ports.len()).filter_map(|port| self.unbind(port).map(|channel| (port, channel))).collect()
    }

    /// Connect several ports; on failure none stays connected.
    pub fn bind_all(&self, bindings: &[(usize, Arc<Channel>)]) -> Result<(), String> {
        for (done, (port, channel)) in bindings.iter().enumerate() {
            if let Err(message) = self.bind(*port, channel.clone()) {
                for (port, _) in &bindings[..done] {
                    self.unbind(*port);
                }
                return Err(message);
            }
        }
        Ok(())
    }

    /// Hot replace, old side: take the attached readers out without detaching them, so event
    /// cursors keep their position. Pins are released and the ports stay bound.
    pub fn release_readers(&self) -> Vec<Released> {
        let mut released = Vec::new();
        for port in &self.ports {
            if let PortIo::In { channel, reader } = &mut *lock(&port.io) {
                if let Some(mut taken) = reader.take() {
                    channel.end_step(&mut taken);
                    released.push(Released { port: port.spec.name.clone(), channel: channel.clone(), reader: taken });
                }
            }
        }
        released
    }

    /// Hot replace, new side: take over readers released by the instance this one replaces.
    /// Their commits now wake this instance. A reader whose port does not exist here, or is
    /// bound to another channel, detaches.
    pub fn adopt_readers(&self, readers: Vec<Released>) {
        for mut released in readers {
            let target = self.port_index(&released.port).and_then(|index| self.ports[index].input_bit.map(|bit| (index, bit)));
            if let Some((index, bit)) = target {
                if let PortIo::In { channel, reader } = &mut *lock(&self.ports[index].io) {
                    if Arc::ptr_eq(channel, &released.channel) && reader.is_none() {
                        channel.retarget_reader(&mut released.reader, self.wake.clone(), 1u64 << bit);
                        // What the previous instance left unread is this instance's first input.
                        if channel.has_unread(&released.reader) {
                            self.env.sched.mark_dirty(self.idx, self.generation, 1u64 << bit, steady_ns());
                        }
                        *reader = Some(released.reader);
                        continue;
                    }
                }
            }
            released.channel.detach_reader(released.reader);
        }
    }

    /// Attach the reader of one input port (the instance is started).
    pub fn attach_input(&self, port: usize) -> Result<(), String> {
        let rt = &self.ports[port];
        let Some(bit) = rt.input_bit else { return Ok(()) };
        let mut io = lock(&rt.io);
        if let PortIo::In { channel, reader } = &mut *io {
            if reader.is_none() {
                let attached = channel
                    .attach_reader(self.wake.clone(), 1u64 << bit)
                    .map_err(|_| format!("port {}: channel {} has no free reader slot", rt.spec.name, channel.name()))?;
                if channel.has_unread(&attached) {
                    self.env.sched.mark_dirty(self.idx, self.generation, 1u64 << bit, steady_ns());
                }
                *reader = Some(attached);
            }
        }
        Ok(())
    }

    fn attach_inputs(&self) -> Result<(), String> {
        for port in 0..self.ports.len() {
            if let Err(message) = self.attach_input(port) {
                self.detach_inputs();
                return Err(message);
            }
        }
        Ok(())
    }

    /// Detach all readers: pins are released and event cursors stop holding back writers.
    pub fn detach_inputs(&self) {
        for port in &self.ports {
            if let PortIo::In { channel, reader } = &mut *lock(&port.io) {
                if let Some(reader) = reader.take() {
                    channel.detach_reader(reader);
                }
            }
        }
    }

    fn end_step_io(&self) {
        for port in &self.ports {
            if port.input_bit.is_some() {
                if let PortIo::In { channel, reader: Some(reader) } = &mut *lock(&port.io) {
                    channel.end_step(reader);
                }
            }
        }
    }

    fn mark_outputs_stale(&self, stale: bool) {
        for port in &self.ports {
            if let PortIo::Out { channel, .. } = &*lock(&port.io) {
                channel.set_stale(stale);
            }
        }
    }

    // ---- host API (called by the module) ----------------------------------------------

    fn writable(&self) -> bool {
        !matches!(self.state(), State::Isolated | State::Removed)
    }

    pub fn write_begin(&self, port: u32) -> *mut c_void {
        let Some(rt) = self.ports.get(port as usize).filter(|rt| rt.spec.dir == Dir::Out) else {
            self.stats.misuse.fetch_add(1, Ordering::Relaxed);
            return std::ptr::null_mut();
        };
        if !self.writable() {
            return std::ptr::null_mut();
        }
        let mut io = lock(&rt.io);
        let PortIo::Out { channel, writer } = &mut *io else { return std::ptr::null_mut() };
        match channel.begin_write(writer) {
            Ok(pointer) => pointer as *mut c_void,
            Err(WriteError::Pending) => {
                self.stats.misuse.fetch_add(1, Ordering::Relaxed);
                std::ptr::null_mut()
            }
            Err(_) => std::ptr::null_mut(),
        }
    }

    pub fn write_commit(&self, port: u32, stamp_ns: i64) -> Status {
        let Some(rt) = self.ports.get(port as usize).filter(|rt| rt.spec.dir == Dir::Out) else {
            self.stats.misuse.fetch_add(1, Ordering::Relaxed);
            return abi::ERR_INVALID;
        };
        let mut io = lock(&rt.io);
        let PortIo::Out { channel, writer } = &mut *io else { return abi::ERR_STATE };
        match channel.commit(writer, stamp_ns, steady_ns()) {
            Ok(_) => abi::OK,
            Err(_) => {
                self.stats.misuse.fetch_add(1, Ordering::Relaxed);
                abi::ERR_STATE
            }
        }
    }

    pub fn write_abort(&self, port: u32) {
        if let Some(rt) = self.ports.get(port as usize).filter(|rt| rt.spec.dir == Dir::Out) {
            if let PortIo::Out { channel, writer } = &mut *lock(&rt.io) {
                channel.abort(writer);
            }
        }
    }

    pub fn read(&self, port: u32, latest: bool, out: &mut abi::SampleView) -> Status {
        let Some(rt) = self.ports.get(port as usize).filter(|rt| rt.spec.dir == Dir::In) else {
            self.stats.misuse.fetch_add(1, Ordering::Relaxed);
            return abi::ERR_INVALID;
        };
        if matches!(self.state(), State::Isolated | State::Removed) {
            return abi::ERR_STATE;
        }
        let mut io = lock(&rt.io);
        let PortIo::In { channel, reader: Some(reader) } = &mut *io else { return abi::ERR_NODATA };
        if (channel.kind() == crate::channel::Kind::State) != latest {
            self.stats.misuse.fetch_add(1, Ordering::Relaxed);
            return abi::ERR_INVALID;
        }
        let view: Option<View> = if latest { channel.read_latest(reader) } else { channel.read_next(reader) };
        match view {
            Some(view) => {
                *out = abi::SampleView { data: view.data as *const c_void, size: view.size, seq: view.seq, stamp_ns: view.stamp_ns };
                abi::OK
            }
            None => abi::ERR_NODATA,
        }
    }

    pub fn changed(&self, port: u32) -> bool {
        self.ports.get(port as usize).and_then(|rt| rt.input_bit).is_some_and(|bit| (self.changed.load(Ordering::Acquire) >> bit) & 1 == 1)
    }

    pub fn wake(&self) {
        self.env.sched.wake(self.idx);
    }

    pub fn set_period_ns(&self, period_ns: i64) {
        let period_ns = if period_ns <= 0 { 0 } else { period_ns.max(MIN_PERIOD_NS) };
        self.timing.period_ns.store(period_ns, Ordering::Relaxed);
        if self.state() == State::Running {
            if period_ns == 0 {
                self.env.timers.disarm(self.idx);
            } else {
                self.env.timers.arm(self.idx, self.generation, period_ns);
            }
        }
    }

    /// `report(health, detail)`: the module's own view of its health.
    pub fn set_report(&self, health: i32, detail: &str) {
        let health = health.clamp(abi::HEALTH_OK, abi::HEALTH_FAILED);
        {
            let mut report = lock(&self.report);
            report.health = health;
            report.detail.clear();
            report.detail.extend(detail.chars().take(512));
        }
        self.reported_health.store(health, Ordering::Release);
        if health == abi::HEALTH_FAILED && matches!(self.state(), State::Running | State::Starting) {
            self.enter_failed(format!("module reported failure: {detail}"));
        }
    }

    // ---- lifecycle operations ---------------------------------------------------------

    /// Queue a lifecycle operation; the instance's task executes it between steps.
    pub fn post(&self, kind: OpKind) -> Arc<Completion> {
        let done = Arc::new(Completion::default());
        lock(&self.ops).push_back(Op { kind, done: done.clone() });
        self.env.sched.raise(self.idx, REASON_OPS);
        done
    }

    fn lifecycle_limit(&self) -> i64 {
        self.timing.hang_limit_ns.load(Ordering::Relaxed).max(LIFECYCLE_MIN_LIMIT_NS)
    }

    /// Stop dispatching steps; outputs go stale; readers detach so they cannot hold writers
    /// back. Safe from any thread: the readers are released at the end of the task.
    fn enter_failed(&self, message: String) {
        self.fail_with(message);
        self.set_state(State::Failed);
        self.env.sched.pause(self.idx);
        self.env.timers.disarm(self.idx);
        self.mark_outputs_stale(true);
        self.env.sched.raise(self.idx, REASON_OPS);
    }

    /// The instance is stuck inside a module call (watchdog thread). Its cell is dead, its
    /// pending operations fail, its readers and writers are released and its outputs are
    /// flagged stale. The module instance is never destroyed and `self` is never freed while a
    /// thread may still be inside the module (see `Core::remove_instance`).
    pub fn isolate(&self, reason: &str) {
        if matches!(self.state(), State::Isolated | State::Removed) {
            return;
        }
        let message = format!("isolated: {reason}");
        log_at!(Level::Error, &self.name, "{message}");
        *lock(&self.last_error) = Some(message);
        self.set_state(State::Isolated);
        self.env.sched.mark_dead(self.idx);
        self.env.timers.disarm(self.idx);
        for op in lock(&self.ops).drain(..) {
            op.done.finish(Err(format!("instance is isolated: {reason}")));
        }
        if let Some(done) = lock(&self.inflight).take() {
            done.finish(Err(format!("instance is isolated: {reason}")));
        }
        for port in &self.ports {
            match &mut *lock(&port.io) {
                PortIo::In { channel, reader } => {
                    if let Some(reader) = reader.take() {
                        channel.detach_reader(reader);
                    }
                }
                PortIo::Out { channel, writer } => {
                    channel.abort(writer);
                    channel.set_stale(true);
                }
                PortIo::Unbound => {}
            }
        }
    }

    /// Execute queued operations and then at most one step. Called on a pool worker.
    pub fn run(&self, taken: Taken, worker: &Worker) {
        let previous = self.last_worker_id.swap(worker.id, Ordering::Relaxed);
        if previous != usize::MAX && previous != worker.id {
            self.worker_moves.fetch_add(1, Ordering::Relaxed);
        }
        self.worker_slot.store(worker.slot, Ordering::Relaxed);
        let mut configured = false;
        loop {
            let op = lock(&self.ops).pop_front();
            let Some(op) = op else { break };
            let is_configure = matches!(op.kind, OpKind::Configure(_));
            *lock(&self.inflight) = Some(op.done.clone());
            let result = self.execute(op.kind, worker);
            if worker.abandoned() {
                return;
            }
            lock(&self.inflight).take();
            configured |= is_configure && result.is_ok();
            op.done.finish(result);
        }
        let step_reasons = taken.reasons & (REASON_INPUT | REASON_TIMER | REASON_WAKE);
        if (step_reasons != 0 || configured) && self.state() == State::Running {
            self.step(taken, step_reasons, worker);
        }
        if self.state() == State::Failed {
            // A failed instance must not hold event writers back or keep slots pinned.
            self.detach_inputs();
        }
    }

    fn execute(&self, kind: OpKind, worker: &Worker) -> Result<(), String> {
        let state = self.state();
        let limit = self.lifecycle_limit();
        let vtable = self.module.vtable;
        let handle = self.handle.load(Ordering::Acquire);
        let call_status = |status: Status, what: &str| -> Result<(), String> {
            if status == abi::OK {
                Ok(())
            } else {
                Err(format!("{what} returned status {status}"))
            }
        };
        if handle.is_null() && !matches!(kind, OpKind::Create | OpKind::Stop | OpKind::Destroy) {
            return Err(format!("the module instance does not exist (state {})", state.as_str()));
        }
        match kind {
            OpKind::Create => {
                if state != State::New {
                    return Err(format!("cannot create an instance in state {}", state.as_str()));
                }
                let json = CString::new(lock(&self.config).clone()).map_err(|_| "configuration contains a NUL byte".to_owned())?;
                let config = abi::Config { json: json.as_ptr(), length: json.as_bytes().len() };
                let mut created: *mut abi::Instance = std::ptr::null_mut();
                let ctx = self.host_ctx();
                let status = worker.guard(self.idx, limit, || {
                    // SAFETY: the vtable was validated at load; the host table and context
                    // outlive the instance; `created` is a valid out pointer.
                    unsafe { (vtable.create)(api::host_api() as *const HostApi, ctx, &config, &mut created) }
                });
                if worker.abandoned() {
                    return Err("create did not return".into());
                }
                if status != abi::OK || created.is_null() {
                    self.set_state(State::Failed);
                    let message = format!("create returned status {status}");
                    self.fail_with(message.clone());
                    return Err(message);
                }
                self.handle.store(created, Ordering::Release);
                self.set_state(State::Created);
                Ok(())
            }
            OpKind::Configure(json) => {
                if !matches!(state, State::Created | State::Running | State::Stopped) {
                    return Err(format!("cannot configure an instance in state {}", state.as_str()));
                }
                let text = CString::new(json.as_str()).map_err(|_| "configuration contains a NUL byte".to_owned())?;
                let config = abi::Config { json: text.as_ptr(), length: text.as_bytes().len() };
                // SAFETY: handle was returned by create; calls are serial on this task.
                let status = worker.guard(self.idx, limit, || unsafe { (vtable.configure)(handle, &config) });
                call_status(status, "configure")?;
                *lock(&self.config) = json;
                self.config_applied.store(true, Ordering::Release);
                Ok(())
            }
            OpKind::Start => {
                match state {
                    State::Created | State::Stopped => {}
                    State::Running => return Ok(()),
                    State::Failed => return Err("the instance failed; stop it before starting it again".into()),
                    other => return Err(format!("cannot start an instance in state {}", other.as_str())),
                }
                self.over_budget.store(false, Ordering::Release);
                self.reported_health.store(abi::HEALTH_OK, Ordering::Release);
                *lock(&self.last_error) = None;
                self.set_state(State::Starting);
                if let Err(message) = self.attach_inputs() {
                    self.set_state(State::Failed);
                    self.fail_with(message.clone());
                    return Err(message);
                }
                // SAFETY: as above.
                let status = worker.guard(self.idx, limit, || unsafe { (vtable.start)(handle) });
                if worker.abandoned() {
                    return Err("start did not return".into());
                }
                if let Err(message) = call_status(status, "start") {
                    self.detach_inputs();
                    self.set_state(State::Failed);
                    self.fail_with(message.clone());
                    return Err(message);
                }
                self.started.store(true, Ordering::Release);
                if self.state() == State::Failed {
                    // The module reported failure from inside start().
                    return Err(self.last_error().unwrap_or_else(|| "the module reported failure while starting".into()));
                }
                self.set_state(State::Running);
                let period = self.timing.period_ns.load(Ordering::Relaxed);
                if period > 0 {
                    self.env.timers.arm(self.idx, self.generation, period);
                }
                self.env.sched.resume(self.idx);
                Ok(())
            }
            OpKind::Stop => {
                if handle.is_null() {
                    return Ok(());
                }
                match state {
                    State::Created | State::Stopped | State::New => return Ok(()),
                    State::Running | State::Failed | State::Starting => {}
                    other => return Err(format!("cannot stop an instance in state {}", other.as_str())),
                }
                self.set_state(State::Stopping);
                self.env.sched.pause(self.idx);
                self.env.timers.disarm(self.idx);
                // A module whose start failed was never started, so it is not stopped either.
                let status = if self.started.swap(false, Ordering::AcqRel) {
                    // SAFETY: as above.
                    worker.guard(self.idx, limit, || unsafe { (vtable.stop)(handle) })
                } else {
                    abi::OK
                };
                if worker.abandoned() {
                    return Err("stop did not return".into());
                }
                self.detach_inputs();
                self.mark_outputs_stale(true);
                match call_status(status, "stop") {
                    Ok(()) => {
                        self.set_state(State::Stopped);
                        Ok(())
                    }
                    Err(message) => {
                        self.set_state(State::Failed);
                        self.fail_with(message.clone());
                        Err(message)
                    }
                }
            }
            OpKind::Destroy => {
                if matches!(state, State::Running | State::Starting) {
                    return Err("stop the instance before destroying it".into());
                }
                if !handle.is_null() {
                    // SAFETY: handle was returned by create and is destroyed exactly once.
                    worker.guard(self.idx, limit, || unsafe { (vtable.destroy)(handle) });
                    if worker.abandoned() {
                        return Err("destroy did not return".into());
                    }
                    self.handle.store(std::ptr::null_mut(), Ordering::Release);
                }
                self.detach_inputs();
                self.set_state(State::Removed);
                Ok(())
            }
        }
    }

    fn step(&self, taken: Taken, step_reasons: u32, worker: &Worker) {
        // Keep only the dirty bits that still have something to read: a commit that
        // arrived after the previous step already consumed it must not cause another step.
        let mut changed = 0u64;
        if taken.dirty != 0 {
            for port in &self.ports {
                let Some(bit) = port.input_bit else { continue };
                if (taken.dirty >> bit) & 1 == 1 {
                    if let PortIo::In { channel, reader: Some(reader) } = &*lock(&port.io) {
                        if channel.has_unread(reader) {
                            changed |= 1 << bit;
                        }
                    }
                }
            }
        }
        let mut reasons = 0;
        if changed != 0 {
            reasons |= abi::STEP_INPUT;
        }
        if step_reasons & REASON_TIMER != 0 {
            reasons |= abi::STEP_TIMER;
        }
        if step_reasons & REASON_WAKE != 0 {
            reasons |= abi::STEP_WAKE;
        }
        if self.config_applied.swap(false, Ordering::AcqRel) {
            reasons |= abi::STEP_CONFIG;
        }
        if reasons == 0 {
            self.stats.spurious.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let (budget_ns, hang_ns) = (self.timing.step_budget_ns.load(Ordering::Relaxed), self.timing.hang_limit_ns.load(Ordering::Relaxed));
        let started = steady_ns();
        if changed != 0 && taken.first_dirty_ns != 0 {
            self.stats.handoff.record((started - taken.first_dirty_ns).max(0) as u64);
        }
        let index = self.step_index.fetch_add(1, Ordering::Relaxed);
        self.changed.store(changed, Ordering::Release);
        let ctx = StepCtx { now_ns: self.env.clock.now_ns(), step_index: index, changed_inputs: changed, reasons };
        let handle = self.handle.load(Ordering::Acquire);
        let step = self.module.vtable.step;
        // SAFETY: handle was returned by create; steps are serial on this task.
        let status = worker.guard(self.idx, hang_ns, || unsafe { step(handle, &ctx) });
        if worker.abandoned() {
            return;
        }
        let elapsed = (steady_ns() - started).max(0);
        self.end_step_io();
        self.stats.steps.fetch_add(1, Ordering::Relaxed);
        self.stats.step_time.record(elapsed as u64);
        if elapsed > budget_ns {
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
            if !self.over_budget.swap(true, Ordering::AcqRel) {
                log_at!(Level::Warn, &self.name, "step {index} took {} us, budget {} us: degraded", elapsed / 1000, budget_ns / 1000);
            }
        } else if self.over_budget.swap(false, Ordering::AcqRel) {
            log_at!(Level::Info, &self.name, "step time back within budget");
        }
        match status {
            abi::OK => {}
            abi::ERR_INTERNAL => self.enter_failed(format!("step {index} returned internal error")),
            other => {
                self.stats.step_errors.fetch_add(1, Ordering::Relaxed);
                log_at!(Level::Debug, &self.name, "step {index} returned status {other}");
            }
        }
    }
}
