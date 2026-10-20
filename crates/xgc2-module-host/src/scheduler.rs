//! Event-driven scheduler: a bounded worker pool running serial per-instance tasks.
//!
//! Every instance owns one [`Cell`] in a fixed, contiguous table. A cell packs everything the
//! hot path touches: one state word (queued / running / paused / dead plus pending reasons),
//! the input dirty mask, the time of the oldest unprocessed commit and the wake counters. Cells
//! are 64-byte aligned, so two instances never share a cache line.
//!
//! A task becomes runnable when a reason is raised (input commit, period timer, `wake()`,
//! lifecycle operation) and the cell is neither queued nor running. Reasons raised while the
//! task is queued or running only add to the pending set, which is how many commits coalesce
//! into one step. At the end of a run the worker re-checks the state word and requeues the task
//! if reasons arrived meanwhile, so nothing is lost and the instance is never run twice at
//! once.
//!
//! Sticky tasks use their home worker's lane and condition variable. A task made runnable
//! on its home worker, or one with any-worker affinity, can use that worker's run-next slot.
//! The watchdog only steals any-worker tasks, and workers serve the shared queue after at
//! most `LOCAL_BURST` (8) local tasks. Replacement workers inherit the abandoned worker's slot.
//!
//! The timer thread calls [`Scheduler::watchdog`] on every tick: a worker stuck in a module
//! call beyond its limit is abandoned, the runner isolates the instance and a replacement
//! worker is spawned, so the other instances keep their parallelism.

use crate::clock::steady_ns;
use crate::log::Level;
use crate::log_at;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

pub const MAX_INSTANCES: usize = 128;
pub const NO_TASK: u32 = u32::MAX;
/// Consecutive run-next tasks a worker takes before it serves the shared queue.
const LOCAL_BURST: u32 = 8;
/// A run-next task is stolen when its worker has been busy this long.
const STEAL_AFTER_NS: i64 = 2_000_000;

// State word.
const QUEUED: u32 = 1 << 0;
const RUNNING: u32 = 1 << 1;
const PAUSED: u32 = 1 << 2;
const DEAD: u32 = 1 << 3;
// Pending reasons.
pub const REASON_INPUT: u32 = 1 << 8;
pub const REASON_TIMER: u32 = 1 << 9;
pub const REASON_WAKE: u32 = 1 << 10;
pub const REASON_OPS: u32 = 1 << 11;
pub const REASON_DEADLINE: u32 = 1 << 12;
const REASONS: u32 = REASON_INPUT | REASON_TIMER | REASON_WAKE | REASON_OPS | REASON_DEADLINE;
const HOME_SHIFT: u32 = 16;
const HOME_ANY: u32 = 0xff;
const HOME_MASK: u32 = HOME_ANY << HOME_SHIFT;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Affinity {
    #[default]
    Sticky,
    Any,
}

impl Affinity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sticky => "sticky",
            Self::Any => "any",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Placement {
    Any,
    Balanced,
    Slot(usize),
}

fn home(state: u32) -> Option<usize> {
    let slot = (state & HOME_MASK) >> HOME_SHIFT;
    (slot != HOME_ANY).then_some(slot as usize)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a worker took from a cell when it started the task.
#[derive(Clone, Copy, Debug, Default)]
pub struct Taken {
    pub reasons: u32,
    pub dirty: u64,
    /// Steady time of the oldest commit behind `dirty` (0 when none).
    pub first_dirty_ns: i64,
}

#[repr(align(64))]
pub struct Cell {
    state: AtomicU32,
    generation: AtomicU32,
    dirty: AtomicU64,
    first_dirty_ns: AtomicI64,
    /// `wake()` calls.
    pub wakes: AtomicU64,
    /// Input commits delivered to this instance.
    pub dirty_commits: AtomicU64,
    /// Commits that found their dirty bit already set (they cost no extra step).
    pub coalesced: AtomicU64,
    /// Period timer expirations.
    pub timer_fires: AtomicU64,
    /// Period timer expirations that were merged into a later step.
    pub missed_periods: AtomicU64,
}

impl Cell {
    fn new() -> Self {
        Cell {
            state: AtomicU32::new(DEAD | HOME_MASK),
            generation: AtomicU32::new(0),
            dirty: AtomicU64::new(0),
            first_dirty_ns: AtomicI64::new(0),
            wakes: AtomicU64::new(0),
            dirty_commits: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            timer_fires: AtomicU64::new(0),
            missed_periods: AtomicU64::new(0),
        }
    }

    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn home(&self) -> Option<usize> {
        home(self.state.load(Ordering::Acquire))
    }
}

/// Executes the tasks the scheduler hands out. Implemented by the host core.
pub trait Runner: Send + Sync + 'static {
    /// Run the task of `idx` (lifecycle operations and/or one step).
    fn run(&self, idx: u32, taken: Taken, worker: &Worker);
    /// `worker` stayed inside a guarded call of instance `idx` longer than its limit.
    fn hung(&self, idx: u32);
}

/// One pool thread as the watchdog sees it.
pub struct Worker {
    pub id: usize,
    pub slot: usize,
    /// Steady time the current guarded call started (0 when idle).
    busy_since_ns: AtomicI64,
    limit_ns: AtomicI64,
    instance: AtomicU32,
    call_seq: AtomicU64,
    abandoned: AtomicBool,
    /// Run-next slot.
    next: AtomicU32,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Worker>>> = const { RefCell::new(None) };
}

impl Worker {
    fn new(id: usize, slot: usize) -> Arc<Worker> {
        Arc::new(Worker {
            id,
            slot,
            busy_since_ns: AtomicI64::new(0),
            limit_ns: AtomicI64::new(0),
            instance: AtomicU32::new(NO_TASK),
            call_seq: AtomicU64::new(0),
            abandoned: AtomicBool::new(false),
            next: AtomicU32::new(NO_TASK),
        })
    }

    pub fn abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }

    /// Run `call` (a call into module code) under the watchdog: if it takes longer than
    /// `limit_ns`, the instance is isolated and this worker is abandoned.
    pub fn guard<R>(&self, instance: u32, limit_ns: i64, call: impl FnOnce() -> R) -> R {
        self.instance.store(instance, Ordering::Relaxed);
        self.limit_ns.store(limit_ns, Ordering::Relaxed);
        self.call_seq.fetch_add(1, Ordering::AcqRel);
        self.busy_since_ns.store(steady_ns(), Ordering::Release);
        let result = call();
        self.busy_since_ns.store(0, Ordering::Release);
        self.call_seq.fetch_add(1, Ordering::AcqRel);
        result
    }
}

struct Quiesce {
    lock: Mutex<()>,
    changed: Condvar,
}

struct Allocation {
    used: Vec<bool>,
    sticky: Vec<usize>,
}

struct Queues {
    shared: VecDeque<u32>,
    lanes: Vec<VecDeque<u32>>,
    /// FIFO park tickets; selecting a worker consumes its ticket before notifying it.
    parked: VecDeque<usize>,
    /// Only a failed spawn lets other workers serve this slot's lane.
    orphan: Vec<bool>,
}

impl Queues {
    fn wake_slot(&mut self, slot: usize) -> Option<usize> {
        let ticket = self.parked.iter().position(|parked| *parked == slot)?;
        self.parked.remove(ticket)
    }
}

pub struct Scheduler {
    table: Box<[Cell]>,
    allocation: Mutex<Allocation>,
    queue: Mutex<Queues>,
    queue_changed: Vec<Condvar>,
    shutdown: AtomicBool,
    target_workers: usize,
    workers: Mutex<Vec<Arc<Worker>>>,
    next_worker_id: AtomicUsize,
    abandoned_workers: AtomicUsize,
    runner: Mutex<Option<Arc<dyn Runner>>>,
    quiesce: Quiesce,
}

impl Scheduler {
    pub fn new(workers: usize) -> Arc<Scheduler> {
        assert!(workers > 0 && workers < HOME_ANY as usize);
        Arc::new(Scheduler {
            table: (0..MAX_INSTANCES).map(|_| Cell::new()).collect(),
            allocation: Mutex::new(Allocation { used: vec![false; MAX_INSTANCES], sticky: vec![0; workers] }),
            queue: Mutex::new(Queues {
                shared: VecDeque::new(),
                lanes: (0..workers).map(|_| VecDeque::new()).collect(),
                parked: VecDeque::new(),
                orphan: vec![false; workers],
            }),
            queue_changed: (0..workers).map(|_| Condvar::new()).collect(),
            shutdown: AtomicBool::new(false),
            target_workers: workers,
            workers: Mutex::new(Vec::new()),
            next_worker_id: AtomicUsize::new(0),
            abandoned_workers: AtomicUsize::new(0),
            runner: Mutex::new(None),
            quiesce: Quiesce { lock: Mutex::new(()), changed: Condvar::new() },
        })
    }

    /// Start the pool. The runner is released again by [`Scheduler::shutdown`].
    pub fn start(self: &Arc<Self>, runner: Arc<dyn Runner>) {
        *lock(&self.runner) = Some(runner);
        for slot in 0..self.target_workers {
            self.spawn_worker(slot);
        }
    }

    pub fn cell(&self, idx: u32) -> &Cell {
        &self.table[idx as usize]
    }

    pub fn worker_counts(&self) -> (usize, usize, usize) {
        (self.target_workers, lock(&self.workers).len(), self.abandoned_workers.load(Ordering::Relaxed))
    }

    pub fn sticky_counts(&self) -> Vec<usize> {
        lock(&self.allocation).sticky.clone()
    }

    // ---- cell allocation -------------------------------------------------------------

    /// Reserve a cell for a new instance. It starts paused, so nothing but lifecycle
    /// operations runs until [`Scheduler::resume`].
    pub fn alloc(&self, placement: Placement) -> Option<(u32, u32)> {
        let mut allocation = lock(&self.allocation);
        let idx = allocation.used.iter().position(|taken| !taken)?;
        let slot = match placement {
            Placement::Any => None,
            Placement::Balanced => allocation.sticky.iter().enumerate().min_by_key(|(_, count)| **count).map(|(slot, _)| slot),
            Placement::Slot(slot) if slot < self.target_workers => Some(slot),
            Placement::Slot(_) => return None,
        };
        allocation.used[idx] = true;
        if let Some(slot) = slot {
            allocation.sticky[slot] += 1;
        }
        let cell = &self.table[idx];
        let generation = cell.generation.fetch_add(1, Ordering::AcqRel) + 1;
        cell.dirty.store(0, Ordering::Relaxed);
        cell.first_dirty_ns.store(0, Ordering::Relaxed);
        for counter in [&cell.wakes, &cell.dirty_commits, &cell.coalesced, &cell.timer_fires, &cell.missed_periods] {
            counter.store(0, Ordering::Relaxed);
        }
        cell.state.store(PAUSED | (slot.map_or(HOME_ANY, |slot| slot as u32) << HOME_SHIFT), Ordering::Release);
        Some((idx as u32, generation))
    }

    /// Release a cell. Raises on a stale reference are ignored from now on.
    pub fn free(&self, idx: u32) {
        let mut allocation = lock(&self.allocation);
        let cell = self.cell(idx);
        cell.generation.fetch_add(1, Ordering::AcqRel);
        let state = cell.state.swap(DEAD | HOME_MASK, Ordering::AcqRel);
        if allocation.used[idx as usize] {
            if let Some(slot) = home(state) {
                allocation.sticky[slot] -= 1;
            }
            allocation.used[idx as usize] = false;
        }
    }

    /// The instance is isolated: no further dispatch, not even lifecycle operations.
    pub fn mark_dead(&self, idx: u32) {
        self.cell(idx).state.fetch_or(DEAD, Ordering::AcqRel);
    }

    // ---- raising reasons -------------------------------------------------------------

    fn eligible(state: u32) -> u32 {
        let pending = state & REASONS;
        if state & PAUSED != 0 {
            pending & REASON_OPS
        } else {
            pending
        }
    }

    /// Add pending reasons and make the task runnable if it is idle.
    pub fn raise(&self, idx: u32, reasons: u32) {
        let cell = self.cell(idx);
        let mut state = cell.state.load(Ordering::Relaxed);
        loop {
            let mut next = state | reasons;
            let runnable = state & (QUEUED | RUNNING | DEAD) == 0 && Self::eligible(next) != 0;
            if runnable {
                next |= QUEUED;
            }
            match cell.state.compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    if runnable {
                        self.enqueue(idx, home(next));
                    }
                    return;
                }
                Err(current) => state = current,
            }
        }
    }

    /// Input commit: mark `bit` dirty and raise the input reason.
    pub fn mark_dirty(&self, idx: u32, generation: u32, bit: u64, commit_ns: i64) {
        let cell = self.cell(idx);
        if cell.generation.load(Ordering::Acquire) != generation {
            return;
        }
        let previous = cell.dirty.fetch_or(bit, Ordering::AcqRel);
        cell.dirty_commits.fetch_add(1, Ordering::Relaxed);
        if previous & bit != 0 {
            cell.coalesced.fetch_add(1, Ordering::Relaxed);
        }
        let _ = cell.first_dirty_ns.compare_exchange(0, commit_ns, Ordering::AcqRel, Ordering::Relaxed);
        self.raise(idx, REASON_INPUT);
    }

    /// Timer expiration: `merged` counts expirations folded into this one.
    pub fn fire_timer(&self, idx: u32, generation: u32, merged: u64) {
        let cell = self.cell(idx);
        if cell.generation.load(Ordering::Acquire) != generation {
            return;
        }
        cell.timer_fires.fetch_add(1, Ordering::Relaxed);
        let pending = cell.state.load(Ordering::Acquire) & REASON_TIMER != 0;
        cell.missed_periods.fetch_add(merged + u64::from(pending), Ordering::Relaxed);
        self.raise(idx, REASON_TIMER);
    }

    pub fn wake(&self, idx: u32) {
        self.cell(idx).wakes.fetch_add(1, Ordering::Relaxed);
        self.raise(idx, REASON_WAKE);
    }

    /// A one-shot deadline of the instance was reached.
    pub fn fire_deadline(&self, idx: u32, generation: u32) {
        if self.cell(idx).generation.load(Ordering::Acquire) != generation {
            return;
        }
        self.raise(idx, REASON_DEADLINE);
    }

    // ---- pause / resume --------------------------------------------------------------

    pub fn pause(&self, idx: u32) {
        self.cell(idx).state.fetch_or(PAUSED, Ordering::AcqRel);
    }

    pub fn is_paused(&self, idx: u32) -> bool {
        self.cell(idx).state.load(Ordering::Acquire) & PAUSED != 0
    }

    /// Lift the pause; reasons that accumulated meanwhile make the task runnable again.
    pub fn resume(&self, idx: u32) {
        let cell = self.cell(idx);
        let mut state = cell.state.load(Ordering::Relaxed);
        loop {
            let mut next = state & !PAUSED;
            let runnable = state & (QUEUED | RUNNING | DEAD) == 0 && Self::eligible(next) != 0;
            if runnable {
                next |= QUEUED;
            }
            match cell.state.compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    if runnable {
                        self.enqueue(idx, home(next));
                    }
                    return;
                }
                Err(current) => state = current,
            }
        }
    }

    /// Wait until no task of `idx` is running. With the cell paused no new step starts, so a
    /// `true` result is a quiescent point for the instance.
    pub fn wait_not_running(&self, idx: u32, timeout: Duration) -> bool {
        let cell = self.cell(idx);
        let deadline = Instant::now() + timeout;
        let mut guard = lock(&self.quiesce.lock);
        loop {
            if cell.state.load(Ordering::Acquire) & RUNNING == 0 {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            guard = self.quiesce.changed.wait_timeout(guard, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    // ---- queueing --------------------------------------------------------------------

    fn enqueue(&self, idx: u32, home: Option<usize>) {
        let local = CURRENT.with(|current| {
            current.borrow().as_ref().is_some_and(|worker| {
                if worker.abandoned() || home.is_some_and(|slot| slot != worker.slot) {
                    return false;
                }
                if worker.next.compare_exchange(NO_TASK, idx, Ordering::AcqRel, Ordering::Relaxed).is_err() {
                    return false;
                }
                // Abandonment may race the insertion. Either the watchdog already spilled
                // the task, or this caller must spill it before leaving the abandoned thread.
                !worker.abandoned() || worker.next.compare_exchange(idx, NO_TASK, Ordering::AcqRel, Ordering::Relaxed).is_err()
            })
        });
        if !local {
            self.enqueue_queued(idx, home);
        }
    }

    fn enqueue_queued(&self, idx: u32, home: Option<usize>) {
        // Whether a worker is parked is read under the lock that parking happens under, so the
        // decision is exact; the wake-up itself is sent after the unlock, because a worker that
        // wakes while this thread still holds the lock would only block on it again.
        let wake = {
            let mut queue = lock(&self.queue);
            match home {
                Some(slot) => {
                    queue.lanes[slot].push_back(idx);
                    if queue.orphan[slot] {
                        queue.parked.pop_front()
                    } else {
                        queue.wake_slot(slot)
                    }
                }
                None => {
                    queue.shared.push_back(idx);
                    queue.parked.pop_front()
                }
            }
        };
        if let Some(slot) = wake {
            self.queue_changed[slot].notify_one();
        }
    }

    fn pop_blocking(&self, worker: &Worker, local_streak: &mut u32) -> Option<u32> {
        let mut queue = lock(&self.queue);
        loop {
            if self.shutdown.load(Ordering::Acquire) || worker.abandoned() {
                return None;
            }
            if *local_streak >= LOCAL_BURST {
                if let Some(idx) = queue.shared.pop_front() {
                    *local_streak = 0;
                    return Some(idx);
                }
            }
            let next = worker.next.swap(NO_TASK, Ordering::AcqRel);
            if next != NO_TASK {
                *local_streak = local_streak.saturating_add(1);
                return Some(next);
            }
            if let Some(idx) = queue.lanes[worker.slot].pop_front() {
                *local_streak = local_streak.saturating_add(1);
                return Some(idx);
            }
            for slot in 0..self.target_workers {
                if queue.orphan[slot] {
                    if let Some(idx) = queue.lanes[slot].pop_front() {
                        *local_streak = local_streak.saturating_add(1);
                        return Some(idx);
                    }
                }
            }
            if let Some(idx) = queue.shared.pop_front() {
                *local_streak = 0;
                return Some(idx);
            }
            queue.parked.push_back(worker.slot);
            queue = self.queue_changed[worker.slot].wait(queue).unwrap_or_else(PoisonError::into_inner);
            queue.wake_slot(worker.slot);
        }
    }

    // ---- running tasks ---------------------------------------------------------------

    /// Claim the task: clear queued, take the eligible reasons and the dirty mask.
    fn begin_run(&self, idx: u32, worker: &Worker) -> Option<Taken> {
        let cell = self.cell(idx);
        let mut state = cell.state.load(Ordering::Acquire);
        let eligible = loop {
            if state & RUNNING != 0 || state & QUEUED == 0 {
                // A stale queue entry (the cell was recycled); the live entry runs the task.
                return None;
            }
            // A recycled cell can still have an entry in its former worker's lane.
            if let Some(slot) = home(state).filter(|slot| *slot != worker.slot) {
                if !lock(&self.queue).orphan[slot] {
                    self.enqueue_queued(idx, Some(slot));
                    return None;
                }
            }
            let eligible = if state & DEAD != 0 { 0 } else { Self::eligible(state) };
            let next = if eligible == 0 { state & !QUEUED } else { (state & !(QUEUED | eligible)) | RUNNING };
            match cell.state.compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break eligible,
                Err(current) => state = current,
            }
        };
        if eligible == 0 {
            return None;
        }
        // The input reason was cleared above; a commit that sets its dirty bit after this
        // swap raises the reason again, so no bit is ever lost.
        let (dirty, first_dirty_ns) = if eligible & REASON_INPUT != 0 {
            (cell.dirty.swap(0, Ordering::AcqRel), cell.first_dirty_ns.swap(0, Ordering::AcqRel))
        } else {
            (0, 0)
        };
        Some(Taken { reasons: eligible, dirty, first_dirty_ns })
    }

    /// Release the task; requeue it if reasons arrived while it ran.
    fn end_run(&self, idx: u32) {
        let cell = self.cell(idx);
        let mut state = cell.state.load(Ordering::Acquire);
        loop {
            let again = state & DEAD == 0 && Self::eligible(state) != 0;
            let next = (state & !RUNNING) | if again { QUEUED } else { 0 };
            match cell.state.compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    if again {
                        self.enqueue_queued(idx, home(next));
                    }
                    if state & PAUSED != 0 {
                        let _guard = lock(&self.quiesce.lock);
                        self.quiesce.changed.notify_all();
                    }
                    return;
                }
                Err(current) => state = current,
            }
        }
    }

    fn run_one(&self, idx: u32, runner: &Arc<dyn Runner>, worker: &Worker) {
        if let Some(taken) = self.begin_run(idx, worker) {
            runner.run(idx, taken, worker);
            if worker.abandoned() {
                // The instance was isolated while this call was stuck; its cell is dead.
                return;
            }
            self.end_run(idx);
        }
    }

    fn worker_main(self: Arc<Self>, runner: Arc<dyn Runner>, worker: Arc<Worker>) {
        CURRENT.with(|current| *current.borrow_mut() = Some(worker.clone()));
        let mut local_streak = 0;
        loop {
            if self.shutdown.load(Ordering::Acquire) || worker.abandoned() {
                break;
            }
            let local = if local_streak < LOCAL_BURST { worker.next.swap(NO_TASK, Ordering::AcqRel) } else { NO_TASK };
            let idx = if local != NO_TASK {
                local_streak += 1;
                local
            } else {
                match self.pop_blocking(&worker, &mut local_streak) {
                    Some(idx) => idx,
                    None => break,
                }
            };
            self.run_one(idx, &runner, &worker);
        }
        // Anything still in the run-next slot must not be lost.
        let leftover = worker.next.swap(NO_TASK, Ordering::AcqRel);
        if leftover != NO_TASK && !self.shutdown.load(Ordering::Acquire) {
            self.enqueue_queued(leftover, self.cell(leftover).home());
        }
        CURRENT.with(|current| *current.borrow_mut() = None);
    }

    fn spawn_worker(self: &Arc<Self>, slot: usize) {
        let Some(runner) = lock(&self.runner).clone() else { return };
        let id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);
        let worker = Worker::new(id, slot);
        lock(&self.workers).push(worker.clone());
        let scheduler = self.clone();
        let spawned = thread::Builder::new().name(format!("xgc2-worker-{id}")).spawn(move || scheduler.worker_main(runner, worker));
        if let Err(error) = spawned {
            log_at!(Level::Error, "scheduler", "cannot spawn worker {id}: {error}");
            lock(&self.workers).retain(|worker| worker.id != id);
            self.orphan_slot(slot);
        }
    }

    fn orphan_slot(&self, slot: usize) {
        lock(&self.queue).orphan[slot] = true;
        for changed in &self.queue_changed {
            changed.notify_one();
        }
    }

    // ---- watchdog --------------------------------------------------------------------

    /// Called by the timer thread on every tick. Abandons workers stuck in a module call
    /// beyond their limit and releases run-next tasks of busy workers.
    pub fn watchdog(self: &Arc<Self>) {
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        let now = steady_ns();
        let workers = lock(&self.workers).clone();
        for worker in workers {
            let sequence = worker.call_seq.load(Ordering::Acquire);
            let since = worker.busy_since_ns.load(Ordering::Acquire);
            let limit = worker.limit_ns.load(Ordering::Relaxed);
            let instance = worker.instance.load(Ordering::Relaxed);
            if since == 0 || sequence != worker.call_seq.load(Ordering::Acquire) {
                continue;
            }
            if now - since > limit {
                self.abandon(&worker, instance);
            } else if now - since > STEAL_AFTER_NS {
                let next = worker.next.load(Ordering::Acquire);
                if next != NO_TASK
                    && self.cell(next).home().is_none()
                    && worker.next.compare_exchange(next, NO_TASK, Ordering::AcqRel, Ordering::Relaxed).is_ok()
                {
                    self.enqueue_queued(next, None);
                }
            }
        }
    }

    fn abandon(self: &Arc<Self>, worker: &Arc<Worker>, instance: u32) {
        if worker.abandoned.swap(true, Ordering::AcqRel) {
            return;
        }
        log_at!(Level::Error, "scheduler", "worker {} is stuck in a module call of instance slot {instance}; replacing it", worker.id);
        lock(&self.workers).retain(|other| other.id != worker.id);
        self.abandoned_workers.fetch_add(1, Ordering::Relaxed);
        let stolen = worker.next.swap(NO_TASK, Ordering::AcqRel);
        if stolen != NO_TASK {
            self.enqueue_queued(stolen, self.cell(stolen).home());
        }
        if let Some(runner) = lock(&self.runner).clone() {
            runner.hung(instance);
        }
        self.spawn_worker(worker.slot);
    }

    // ---- shutdown --------------------------------------------------------------------

    /// Stop the pool: wake all workers and wait (bounded) for the live ones. Abandoned
    /// workers are left to finish on their own.
    pub fn shutdown(&self, timeout: Duration) {
        self.shutdown.store(true, Ordering::Release);
        {
            let _queue = lock(&self.queue);
            for changed in &self.queue_changed {
                changed.notify_all();
            }
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let running = lock(&self.workers).iter().any(|worker| worker.busy_since_ns.load(Ordering::Acquire) != 0);
            if !running {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        lock(&self.workers).clear();
        *lock(&self.runner) = None;
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct CountingRunner {
        runs: Vec<AtomicUsize>,
        taken: Mutex<Vec<(u32, Taken)>>,
        hung: Mutex<Vec<u32>>,
        sleep_ms: AtomicU64,
        in_flight: Vec<AtomicUsize>,
        overlap: AtomicBool,
        placements: Mutex<Vec<(u32, usize, usize)>>,
    }

    impl CountingRunner {
        fn new() -> Arc<Self> {
            Arc::new(CountingRunner {
                runs: (0..MAX_INSTANCES).map(|_| AtomicUsize::new(0)).collect(),
                taken: Mutex::new(Vec::new()),
                hung: Mutex::new(Vec::new()),
                sleep_ms: AtomicU64::new(0),
                in_flight: (0..MAX_INSTANCES).map(|_| AtomicUsize::new(0)).collect(),
                overlap: AtomicBool::new(false),
                placements: Mutex::new(Vec::new()),
            })
        }
    }

    impl Runner for CountingRunner {
        fn run(&self, idx: u32, taken: Taken, worker: &Worker) {
            lock(&self.placements).push((idx, worker.id, worker.slot));
            if self.in_flight[idx as usize].fetch_add(1, Ordering::SeqCst) != 0 {
                self.overlap.store(true, Ordering::SeqCst);
            }
            let sleep = self.sleep_ms.load(Ordering::SeqCst);
            worker.guard(idx, 50_000_000, || {
                if sleep > 0 {
                    thread::sleep(Duration::from_millis(sleep));
                }
            });
            self.runs[idx as usize].fetch_add(1, Ordering::SeqCst);
            lock(&self.taken).push((idx, taken));
            self.in_flight[idx as usize].fetch_sub(1, Ordering::SeqCst);
        }
        fn hung(&self, idx: u32) {
            lock(&self.hung).push(idx);
        }
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn paused_cells_only_run_lifecycle_operations() {
        let scheduler = Scheduler::new(2);
        let runner = CountingRunner::new();
        scheduler.start(runner.clone());
        let (idx, _) = scheduler.alloc(crate::scheduler::Placement::Any).unwrap();
        scheduler.raise(idx, REASON_TIMER | REASON_WAKE);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(runner.runs[idx as usize].load(Ordering::SeqCst), 0);
        scheduler.raise(idx, REASON_OPS);
        wait_until("ops run", || runner.runs[idx as usize].load(Ordering::SeqCst) == 1);
        assert_eq!(lock(&runner.taken)[0].1.reasons, REASON_OPS);
        // The step reasons stayed pending; resuming delivers them.
        scheduler.resume(idx);
        wait_until("resumed run", || runner.runs[idx as usize].load(Ordering::SeqCst) == 2);
        assert_eq!(lock(&runner.taken)[1].1.reasons, REASON_TIMER | REASON_WAKE);
        scheduler.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn commits_coalesce_while_the_task_runs_and_are_never_lost() {
        let scheduler = Scheduler::new(2);
        let runner = CountingRunner::new();
        runner.sleep_ms.store(40, Ordering::SeqCst);
        scheduler.start(runner.clone());
        let (idx, generation) = scheduler.alloc(crate::scheduler::Placement::Any).unwrap();
        scheduler.resume(idx);
        scheduler.mark_dirty(idx, generation, 1 << 0, 100);
        wait_until("first run starts", || runner.in_flight[idx as usize].load(Ordering::SeqCst) == 1);
        for _ in 0..10 {
            scheduler.mark_dirty(idx, generation, 1 << 2, 200);
        }
        wait_until("second run", || runner.runs[idx as usize].load(Ordering::SeqCst) == 2);
        thread::sleep(Duration::from_millis(100));
        let taken = lock(&runner.taken).clone();
        assert_eq!(taken.len(), 2, "ten commits during one run produce exactly one more run");
        assert_eq!((taken[0].1.dirty, taken[0].1.first_dirty_ns), (1, 100));
        assert_eq!((taken[1].1.dirty, taken[1].1.first_dirty_ns), (1 << 2, 200));
        let cell = scheduler.cell(idx);
        assert_eq!(cell.dirty_commits.load(Ordering::Relaxed), 11);
        assert_eq!(cell.coalesced.load(Ordering::Relaxed), 9);
        assert!(!runner.overlap.load(Ordering::SeqCst));
        scheduler.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn stale_generations_are_ignored_and_freed_cells_never_run() {
        let scheduler = Scheduler::new(1);
        let runner = CountingRunner::new();
        scheduler.start(runner.clone());
        let (idx, generation) = scheduler.alloc(crate::scheduler::Placement::Any).unwrap();
        scheduler.resume(idx);
        scheduler.free(idx);
        scheduler.mark_dirty(idx, generation, 1, 1);
        scheduler.fire_timer(idx, generation, 0);
        scheduler.raise(idx, REASON_WAKE);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(runner.runs[idx as usize].load(Ordering::SeqCst), 0);
        // The slot is reusable and starts clean.
        let (again, new_generation) = scheduler.alloc(crate::scheduler::Placement::Any).unwrap();
        assert_eq!(again, idx);
        assert!(new_generation > generation);
        assert_eq!(scheduler.cell(again).dirty_commits.load(Ordering::Relaxed), 0);
        scheduler.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn many_threads_raising_never_overlap_one_instance() {
        let scheduler = Scheduler::new(4);
        let runner = CountingRunner::new();
        scheduler.start(runner.clone());
        let (idx, generation) = scheduler.alloc(crate::scheduler::Placement::Any).unwrap();
        scheduler.resume(idx);
        let raisers: Vec<_> = (0..4)
            .map(|n| {
                let scheduler = scheduler.clone();
                thread::spawn(move || {
                    for i in 0..2000 {
                        if (i + n) % 2 == 0 {
                            scheduler.mark_dirty(idx, generation, 1 << n, i);
                        } else {
                            scheduler.wake(idx);
                        }
                    }
                })
            })
            .collect();
        for raiser in raisers {
            raiser.join().unwrap();
        }
        wait_until("drain", || scheduler.cell(idx).state.load(Ordering::SeqCst) & (QUEUED | RUNNING) == 0);
        assert!(!runner.overlap.load(Ordering::SeqCst), "an instance ran concurrently with itself");
        let dirty_seen = lock(&runner.taken).iter().fold(0u64, |acc, (_, taken)| acc | taken.dirty);
        assert_eq!(dirty_seen, 0b1111);
        scheduler.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn pause_waits_for_the_running_task() {
        let scheduler = Scheduler::new(2);
        let runner = CountingRunner::new();
        runner.sleep_ms.store(60, Ordering::SeqCst);
        scheduler.start(runner.clone());
        let (idx, _) = scheduler.alloc(crate::scheduler::Placement::Any).unwrap();
        scheduler.resume(idx);
        scheduler.raise(idx, REASON_WAKE);
        wait_until("running", || runner.in_flight[idx as usize].load(Ordering::SeqCst) == 1);
        scheduler.pause(idx);
        assert!(!scheduler.wait_not_running(idx, Duration::from_millis(5)));
        assert!(scheduler.wait_not_running(idx, Duration::from_secs(2)));
        // Raised while paused: stays pending until resume.
        scheduler.raise(idx, REASON_WAKE);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(runner.runs[idx as usize].load(Ordering::SeqCst), 1);
        scheduler.resume(idx);
        wait_until("resumed", || runner.runs[idx as usize].load(Ordering::SeqCst) == 2);
        scheduler.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn watchdog_abandons_a_stuck_worker_and_keeps_the_pool_size() {
        struct Stuck {
            hung: Mutex<Vec<u32>>,
            others: AtomicUsize,
        }
        impl Runner for Stuck {
            fn run(&self, idx: u32, _: Taken, worker: &Worker) {
                if idx == 0 {
                    worker.guard(idx, 20_000_000, || thread::sleep(Duration::from_millis(400)));
                } else {
                    self.others.fetch_add(1, Ordering::SeqCst);
                }
            }
            fn hung(&self, idx: u32) {
                lock(&self.hung).push(idx);
            }
        }
        let scheduler = Scheduler::new(1);
        let runner = Arc::new(Stuck { hung: Mutex::new(Vec::new()), others: AtomicUsize::new(0) });
        scheduler.start(runner.clone());
        let (stuck, _) = scheduler.alloc(crate::scheduler::Placement::Any).unwrap();
        let (other, _) = scheduler.alloc(crate::scheduler::Placement::Any).unwrap();
        assert_eq!((stuck, other), (0, 1));
        scheduler.resume(stuck);
        scheduler.resume(other);
        scheduler.raise(stuck, REASON_WAKE);
        thread::sleep(Duration::from_millis(60));
        // The only worker is stuck, so the second instance cannot run yet.
        scheduler.raise(other, REASON_WAKE);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(runner.others.load(Ordering::SeqCst), 0);
        scheduler.watchdog();
        wait_until("replacement worker serves the queue", || runner.others.load(Ordering::SeqCst) == 1);
        assert_eq!(*lock(&runner.hung), [stuck]);
        assert_eq!(scheduler.worker_counts(), (1, 1, 1));
        scheduler.shutdown(Duration::from_secs(1));
    }

    fn chained_tasks(placement: Placement) {
        struct Chain {
            order: Mutex<Vec<(u32, String)>>,
            scheduler: Mutex<Option<Arc<Scheduler>>>,
        }
        impl Runner for Chain {
            fn run(&self, idx: u32, _: Taken, _: &Worker) {
                lock(&self.order).push((idx, thread::current().name().unwrap_or("?").to_owned()));
                let scheduler = lock(&self.scheduler).clone().unwrap();
                if idx == 0 {
                    scheduler.raise(1, REASON_WAKE);
                } else if idx == 1 {
                    scheduler.raise(2, REASON_WAKE);
                }
            }
            fn hung(&self, _: u32) {}
        }
        let scheduler = Scheduler::new(4);
        let runner = Arc::new(Chain { order: Mutex::new(Vec::new()), scheduler: Mutex::new(Some(scheduler.clone())) });
        scheduler.start(runner.clone());
        for _ in 0..3 {
            let (idx, _) = scheduler.alloc(placement).unwrap();
            scheduler.resume(idx);
        }
        scheduler.raise(0, REASON_WAKE);
        wait_until("chain", || lock(&runner.order).len() == 3);
        let order = lock(&runner.order).clone();
        assert_eq!(order.iter().map(|(idx, _)| *idx).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(order.iter().all(|(_, thread)| *thread == order[0].1), "{order:?}");
        *lock(&runner.scheduler) = None;
        scheduler.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn chained_tasks_run_back_to_back_on_one_worker() {
        chained_tasks(Placement::Any);
    }

    #[test]
    fn sticky_chain_runs_back_to_back_on_its_home_worker() {
        chained_tasks(Placement::Slot(2));
    }

    #[test]
    fn home_bits_survive_dispatch_and_allocations_balance_and_release() {
        let scheduler = Scheduler::new(4);
        let cells: Vec<_> = (0..8).map(|_| scheduler.alloc(Placement::Balanced).unwrap().0).collect();
        for (index, idx) in cells.iter().enumerate() {
            assert_eq!(scheduler.cell(*idx).home(), Some(index % 4));
        }
        assert_eq!(scheduler.sticky_counts(), [2, 2, 2, 2]);
        let (any, _) = scheduler.alloc(Placement::Any).unwrap();
        assert_eq!(scheduler.cell(any).home(), None);
        scheduler.raise(cells[2], REASON_OPS | REASON_WAKE);
        let worker = Worker::new(2, 2);
        assert!(scheduler.begin_run(cells[2], &worker).is_some());
        scheduler.resume(cells[2]);
        scheduler.end_run(cells[2]);
        assert_eq!(scheduler.cell(cells[2]).home(), Some(2));
        let (replacement, _) = scheduler.alloc(Placement::Slot(2)).unwrap();
        scheduler.free(cells[2]);
        assert_eq!(scheduler.sticky_counts(), [2, 2, 2, 2]);
        scheduler.free(replacement);
        assert_eq!(scheduler.sticky_counts(), [2, 2, 1, 2]);
        let (fresh, _) = scheduler.alloc(Placement::Balanced).unwrap();
        assert_eq!(scheduler.cell(fresh).home(), Some(2));
        assert!(scheduler.alloc(Placement::Slot(4)).is_none());
    }

    #[test]
    fn only_the_home_worker_serves_a_sticky_lane_when_other_workers_are_idle() {
        let scheduler = Scheduler::new(4);
        let runner = CountingRunner::new();
        scheduler.start(runner.clone());
        let (idx, _) = scheduler.alloc(Placement::Slot(2)).unwrap();
        scheduler.resume(idx);
        for runs in 1..=12 {
            scheduler.wake(idx);
            wait_until("sticky lane run", || runner.runs[idx as usize].load(Ordering::SeqCst) == runs);
            wait_until("idle cell", || scheduler.cell(idx).state.load(Ordering::Acquire) & RUNNING == 0);
        }
        assert!(lock(&runner.placements).iter().all(|(_, _, slot)| *slot == 2));
        scheduler.shutdown(Duration::from_secs(1));
    }

    struct PendingRunner {
        scheduler: Arc<Scheduler>,
        entered: AtomicBool,
        release: AtomicBool,
        returned: AtomicBool,
        limit_ns: i64,
        runs: Mutex<Vec<(u32, usize, usize)>>,
    }

    impl Runner for PendingRunner {
        fn run(&self, idx: u32, _: Taken, worker: &Worker) {
            lock(&self.runs).push((idx, worker.id, worker.slot));
            if idx == 0 {
                worker.guard(idx, self.limit_ns, || {
                    self.scheduler.raise(1, REASON_WAKE);
                    self.entered.store(true, Ordering::Release);
                    while !self.release.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(1));
                    }
                });
                self.returned.store(true, Ordering::Release);
            }
        }

        fn hung(&self, idx: u32) {
            self.scheduler.mark_dead(idx);
        }
    }

    fn pending_runner(scheduler: &Arc<Scheduler>, limit_ns: i64) -> Arc<PendingRunner> {
        Arc::new(PendingRunner {
            scheduler: scheduler.clone(),
            entered: AtomicBool::new(false),
            release: AtomicBool::new(false),
            returned: AtomicBool::new(false),
            limit_ns,
            runs: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn watchdog_steals_any_run_next_tasks_and_leaves_sticky_tasks_in_place() {
        for placement in [Placement::Any, Placement::Slot(0)] {
            let scheduler = Scheduler::new(2);
            let runner = pending_runner(&scheduler, 5_000_000_000);
            for placement in [Placement::Slot(0), placement] {
                let (idx, _) = scheduler.alloc(placement).unwrap();
                scheduler.resume(idx);
            }
            scheduler.start(runner.clone());
            scheduler.wake(0);
            wait_until("run-next published", || runner.entered.load(Ordering::Acquire));
            thread::sleep(Duration::from_millis(20));
            scheduler.watchdog();
            if matches!(placement, Placement::Any) {
                wait_until("stolen task", || lock(&runner.runs).len() == 2);
                assert_eq!(lock(&runner.runs)[1].2, 1);
            } else {
                thread::sleep(Duration::from_millis(20));
                assert_eq!(lock(&runner.runs).len(), 1);
            }
            runner.release.store(true, Ordering::Release);
            wait_until("both tasks finished", || runner.returned.load(Ordering::Acquire) && lock(&runner.runs).len() == 2);
            if matches!(placement, Placement::Slot(_)) {
                let runs = lock(&runner.runs);
                assert_eq!(runs[0].1, runs[1].1);
            }
            scheduler.shutdown(Duration::from_secs(1));
        }
    }

    #[test]
    fn hang_replacement_inherits_the_slot_lane_and_sticky_run_next() {
        let scheduler = Scheduler::new(2);
        let runner = pending_runner(&scheduler, 20_000_000);
        for _ in 0..3 {
            let (idx, _) = scheduler.alloc(Placement::Slot(0)).unwrap();
            scheduler.resume(idx);
        }
        scheduler.start(runner.clone());
        scheduler.wake(0);
        wait_until("stuck with run-next", || runner.entered.load(Ordering::Acquire));
        scheduler.wake(2); // Pending in the same home lane, rather than the run-next slot.
        thread::sleep(Duration::from_millis(50));
        scheduler.watchdog();
        wait_until("replacement drains both", || lock(&runner.runs).len() == 3);
        let runs = lock(&runner.runs).clone();
        assert!(runs.iter().all(|(_, _, slot)| *slot == 0));
        assert!(runs[1..].iter().all(|(_, id, _)| *id != runs[0].1));
        assert_eq!(scheduler.worker_counts(), (2, 2, 1));
        runner.release.store(true, Ordering::Release);
        wait_until("abandoned call returns", || runner.returned.load(Ordering::Acquire));
        scheduler.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn surviving_workers_serve_an_orphan_lane() {
        let scheduler = Scheduler::new(2);
        let runner = CountingRunner::new();
        *lock(&scheduler.runner) = Some(runner.clone());
        scheduler.spawn_worker(0);
        scheduler.orphan_slot(1);
        let (idx, _) = scheduler.alloc(Placement::Slot(1)).unwrap();
        scheduler.resume(idx);
        scheduler.wake(idx);
        wait_until("orphan task", || runner.runs[idx as usize].load(Ordering::SeqCst) == 1);
        assert_eq!(lock(&runner.placements)[0].2, 0);
        assert_eq!(scheduler.cell(idx).home(), Some(1));
        scheduler.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn a_recycled_cell_cannot_run_on_its_former_home_worker() {
        let scheduler = Scheduler::new(2);
        let (idx, _) = scheduler.alloc(Placement::Slot(0)).unwrap();
        scheduler.raise(idx, REASON_OPS);
        scheduler.free(idx); // Its old lane still has a queue entry.
        let (fresh, _) = scheduler.alloc(Placement::Slot(1)).unwrap();
        assert_eq!(fresh, idx);
        scheduler.raise(fresh, REASON_OPS);
        let mut streak = 0;
        let old_worker = Worker::new(0, 0);
        let new_worker = Worker::new(1, 1);
        assert_eq!(scheduler.pop_blocking(&old_worker, &mut streak), Some(idx));
        assert!(scheduler.begin_run(idx, &old_worker).is_none());
        assert_eq!(scheduler.pop_blocking(&new_worker, &mut streak), Some(idx));
        assert!(scheduler.begin_run(idx, &new_worker).is_some());
        assert!(scheduler.begin_run(idx, &new_worker).is_none());
        scheduler.end_run(idx);
        assert_eq!(scheduler.cell(idx).home(), Some(1));
    }

    #[test]
    fn local_bursts_serve_shared_work_before_more_local_tasks() {
        let scheduler = Scheduler::new(1);
        let worker = Worker::new(0, 0);
        let (sticky, _) = scheduler.alloc(Placement::Slot(0)).unwrap();
        let (any, _) = scheduler.alloc(Placement::Any).unwrap();
        scheduler.raise(sticky, REASON_OPS);
        scheduler.raise(any, REASON_OPS);
        let mut streak = LOCAL_BURST;
        assert_eq!(scheduler.pop_blocking(&worker, &mut streak), Some(any));
        assert_eq!(streak, 0);
        assert_eq!(scheduler.pop_blocking(&worker, &mut streak), Some(sticky));
        assert_eq!(streak, 1);
    }
}
