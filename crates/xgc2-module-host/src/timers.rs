//! Period timers and one-shot deadlines: one thread, one deadline heap.
//!
//! The heap holds one period entry per armed instance. An entry fires at `anchor + n * period`
//! (drift-free); if the host fell behind, the periods that were skipped are counted as merged
//! and the next deadline lies in the future, so a slow step never causes a burst of catch-up
//! steps. The anchor is the first expiration after the timer is armed, or, when the clock has
//! a grid ([`Clock::grid_origin`]: `utc`, or any clock with a `phase_t0_ns`), the grid of the
//! shared T0, so that entities on different hosts expire at the same instants.
//!
//! Besides the period an instance may hold one one-shot deadline (`wake_at_ns`): an absolute
//! instant of the host clock, replaced by the next request and cancelled by a negative one.
//! A deadline that is already due fires at once. It is not touched by a re-anchoring.
//!
//! In `steady` and `utc` mode the timer thread sleeps until the earliest deadline. In
//! `external` mode time only advances when the clock channel is committed; the committing
//! thread then calls [`Timers::on_clock`], which fires whatever became due, so simulated time
//! can run faster or slower than the wall clock.
//!
//! The same thread ticks the scheduler's watchdog every [`WATCHDOG_TICK_NS`].

use crate::clock::{steady_ns, Clock, Mode, Update};
use crate::scheduler::Scheduler;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

pub const WATCHDOG_TICK_NS: i64 = 10_000_000;
/// Shortest period a module may request.
pub const MIN_PERIOD_NS: i64 = 100_000;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    deadline: i64,
    idx: u32,
    epoch: u64,
    /// A `wake_at_ns` deadline instead of a period expiration.
    oneshot: bool,
}

#[derive(Clone, Copy)]
struct Slot {
    generation: u32,
    /// 0 = no period timer (a slot can exist only for a one-shot deadline).
    period: i64,
    epoch: u64,
    /// The pending one-shot deadline and its epoch.
    deadline: Option<(i64, u64)>,
}

struct State {
    heap: BinaryHeap<Reverse<Entry>>,
    slots: Vec<Option<Slot>>,
    epoch: u64,
    stop: bool,
}

pub struct Timers {
    sched: Arc<Scheduler>,
    clock: Arc<Clock>,
    state: Mutex<State>,
    changed: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Timers {
    pub fn new(sched: Arc<Scheduler>, clock: Arc<Clock>, slots: usize) -> Arc<Timers> {
        Arc::new(Timers {
            sched,
            clock,
            state: Mutex::new(State { heap: BinaryHeap::new(), slots: vec![None; slots], epoch: 0, stop: false }),
            changed: Condvar::new(),
        })
    }

    /// Arm the timer of `idx`; the first expiration is one period from now. With an external
    /// clock that has not published yet, the timer starts with the first sample.
    pub fn arm(&self, idx: u32, generation: u32, period_ns: i64) {
        let mut state = lock(&self.state);
        state.epoch += 1;
        let epoch = state.epoch;
        let period = period_ns.max(MIN_PERIOD_NS);
        let deadline = state.slots[idx as usize].filter(|slot| slot.generation == generation).and_then(|slot| slot.deadline);
        state.slots[idx as usize] = Some(Slot { generation, period, epoch, deadline });
        if self.clock.valid() {
            let deadline = self.clock.next_expiration(self.clock.now_ns(), period);
            state.heap.push(Reverse(Entry { deadline, idx, epoch, oneshot: false }));
            self.changed.notify_one();
        }
    }

    /// Set (or, with a negative instant, cancel) the one-shot deadline of `idx`; it replaces
    /// the previous one. The period timer, if armed, is not affected.
    pub fn arm_deadline(&self, idx: u32, generation: u32, deadline_ns: i64) {
        let mut state = lock(&self.state);
        let slot = state.slots[idx as usize].filter(|slot| slot.generation == generation);
        if deadline_ns < 0 {
            if let Some(mut slot) = slot {
                slot.deadline = None;
                state.slots[idx as usize] = Some(slot).filter(|slot| slot.period > 0);
            }
            return;
        }
        state.epoch += 1;
        let epoch = state.epoch;
        let mut slot = slot.unwrap_or(Slot { generation, period: 0, epoch: 0, deadline: None });
        slot.deadline = Some((deadline_ns, epoch));
        state.slots[idx as usize] = Some(slot);
        state.heap.push(Reverse(Entry { deadline: deadline_ns, idx, epoch, oneshot: true }));
        self.changed.notify_one();
    }

    /// Stop the period timer; a pending one-shot deadline stays.
    pub fn disarm(&self, idx: u32) {
        let mut state = lock(&self.state);
        if let Some(mut slot) = state.slots[idx as usize] {
            slot.period = 0;
            state.slots[idx as usize] = Some(slot).filter(|slot| slot.deadline.is_some());
        }
    }

    /// Drop the period timer and the one-shot deadline.
    pub fn clear(&self, idx: u32) {
        lock(&self.state).slots[idx as usize] = None;
    }

    pub fn is_armed(&self, idx: u32) -> bool {
        lock(&self.state).slots[idx as usize].is_some_and(|slot| slot.period > 0)
    }

    /// Fire everything due at `now` and schedule the next expirations.
    fn fire_due(&self, now: i64) {
        let mut fired: Vec<(u32, u32, Option<u64>)> = Vec::new();
        {
            let mut state = lock(&self.state);
            while let Some(Reverse(top)) = state.heap.peek().copied() {
                if top.deadline > now {
                    break;
                }
                state.heap.pop();
                let Some(mut slot) = state.slots[top.idx as usize] else { continue };
                if top.oneshot {
                    if slot.deadline.map(|(_, epoch)| epoch) != Some(top.epoch) {
                        continue;
                    }
                    slot.deadline = None;
                    state.slots[top.idx as usize] = Some(slot).filter(|slot| slot.period > 0);
                    fired.push((top.idx, slot.generation, None));
                    continue;
                }
                if slot.epoch != top.epoch || slot.period == 0 {
                    continue;
                }
                let skipped = (now - top.deadline) / slot.period;
                let deadline = top.deadline + (skipped + 1) * slot.period;
                state.heap.push(Reverse(Entry { deadline, idx: top.idx, epoch: top.epoch, oneshot: false }));
                fired.push((top.idx, slot.generation, Some(skipped as u64)));
            }
        }
        for (idx, generation, merged) in fired {
            match merged {
                Some(merged) => self.sched.fire_timer(idx, generation, merged),
                None => self.sched.fire_deadline(idx, generation),
            }
        }
    }

    /// Restart every armed period timer one period after `now` (on the grid, if there is
    /// one). One-shot deadlines are absolute instants and stay as they are.
    fn reanchor(&self, now: i64) {
        let mut state = lock(&self.state);
        state.heap.clear();
        let mut epoch = state.epoch;
        for idx in 0..state.slots.len() {
            let Some(mut slot) = state.slots[idx] else { continue };
            if slot.period > 0 {
                epoch += 1;
                slot.epoch = epoch;
                let deadline = self.clock.next_expiration(now, slot.period);
                state.heap.push(Reverse(Entry { deadline, idx: idx as u32, epoch, oneshot: false }));
            }
            if let Some((deadline, deadline_epoch)) = slot.deadline {
                state.heap.push(Reverse(Entry { deadline, idx: idx as u32, epoch: deadline_epoch, oneshot: true }));
            }
            state.slots[idx] = Some(slot);
        }
        state.epoch = epoch;
    }

    /// External clock sample committed (called on the committing thread).
    pub fn on_clock(&self, update: Update, now: i64) {
        if update != Update::Forward {
            self.reanchor(now);
        }
        self.fire_due(now);
    }

    pub fn stop(&self) {
        lock(&self.state).stop = true;
        self.changed.notify_all();
    }

    /// Thread body: steady-clock deadlines and the watchdog tick.
    pub fn run(self: Arc<Self>) {
        // The default 50 us timer slack would delay every wake-up of this thread.
        // SAFETY: plain prctl call without pointers.
        unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1u64, 0u64, 0u64, 0u64) };
        let mut next_tick = steady_ns() + WATCHDOG_TICK_NS;
        let mut last_clock = self.clock.now_ns();
        loop {
            let now = steady_ns();
            // The deadlines are in the clock's domain; a steady clock is `now`.
            let internal = self.clock.mode() != Mode::External;
            let clock_now = if internal { self.clock.now_ns() } else { 0 };
            if internal {
                if clock_now < last_clock {
                    // The wall clock was stepped back: deadlines of the old reading are far in the
                    // future. Restart the period timers on the grid.
                    self.reanchor(clock_now);
                }
                last_clock = clock_now;
                self.fire_due(clock_now);
            }
            if now >= next_tick {
                self.sched.watchdog();
                next_tick = now + WATCHDOG_TICK_NS;
            }
            let mut wait = next_tick - now;
            let state = lock(&self.state);
            if state.stop {
                return;
            }
            if internal {
                if let Some(Reverse(top)) = state.heap.peek() {
                    wait = wait.min(top.deadline - clock_now);
                }
            }
            if wait > 0 {
                let _ = self.changed.wait_timeout(state, Duration::from_nanos(wait as u64));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{Runner, Taken, Worker, REASON_DEADLINE, REASON_TIMER};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::thread;
    use std::time::Instant;

    struct Recorder {
        stamps: Mutex<Vec<(u32, Instant)>>,
        reasons: Mutex<Vec<u32>>,
        slow_ms: AtomicU64,
        runs: AtomicUsize,
    }

    impl Runner for Recorder {
        fn run(&self, idx: u32, taken: Taken, _: &Worker) {
            assert!(taken.reasons & (REASON_TIMER | REASON_DEADLINE) != 0);
            lock(&self.reasons).push(taken.reasons & (REASON_TIMER | REASON_DEADLINE));
            lock(&self.stamps).push((idx, Instant::now()));
            self.runs.fetch_add(1, Ordering::SeqCst);
            let slow = self.slow_ms.load(Ordering::SeqCst);
            if slow > 0 {
                thread::sleep(Duration::from_millis(slow));
            }
        }
        fn hung(&self, _: u32) {}
    }

    struct Fixture {
        sched: Arc<Scheduler>,
        timers: Arc<Timers>,
        clock: Arc<Clock>,
        recorder: Arc<Recorder>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Fixture {
        fn new(mode: Mode) -> Fixture {
            Self::with_phase(mode, None)
        }

        fn with_phase(mode: Mode, phase_t0_ns: Option<i64>) -> Fixture {
            let sched = Scheduler::new(2);
            let clock = Arc::new(Clock::with_phase(mode, phase_t0_ns));
            let timers = Timers::new(sched.clone(), clock.clone(), crate::scheduler::MAX_INSTANCES);
            let recorder = Arc::new(Recorder { stamps: Mutex::new(Vec::new()), reasons: Mutex::new(Vec::new()), slow_ms: AtomicU64::new(0), runs: AtomicUsize::new(0) });
            sched.start(recorder.clone());
            let runner = timers.clone();
            let thread = Some(thread::spawn(move || runner.run()));
            Fixture { sched, timers, clock, recorder, thread }
        }

        fn instance(&self, period_ns: i64) -> (u32, u32) {
            let (idx, generation) = self.bare();
            self.timers.arm(idx, generation, period_ns);
            (idx, generation)
        }

        /// An instance with no timer.
        fn bare(&self) -> (u32, u32) {
            let (idx, generation) = self.sched.alloc(crate::scheduler::Placement::Any).unwrap();
            self.sched.resume(idx);
            (idx, generation)
        }

        /// Publish simulated time `ns` as the external clock does.
        fn publish(&self, ns: i64) {
            let update = self.clock.set_external(ns);
            self.timers.on_clock(update, ns);
        }

        fn reasons(&self) -> Vec<u32> {
            lock(&self.recorder.reasons).clone()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.timers.stop();
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
            self.sched.shutdown(Duration::from_secs(1));
        }
    }

    #[test]
    fn periodic_timer_fires_every_period_and_stops_when_disarmed() {
        let f = Fixture::new(Mode::Steady);
        let armed_at = Instant::now();
        let (idx, _) = f.instance(5_000_000);
        thread::sleep(Duration::from_millis(250));
        f.timers.disarm(idx);
        let stamps: Vec<_> = lock(&f.recorder.stamps).iter().map(|(_, at)| *at).collect();
        // 250 ms / 5 ms = 50 expirations at most. A loaded machine delays some of them, and the
        // periods it made the timer miss are merged into later expirations, so only a loose lower
        // bound holds; what holds always is that no expiration comes early: the k-th is due at
        // least k periods after the timer was armed.
        assert!((8..=51).contains(&stamps.len()), "{} expirations", stamps.len());
        for (k, at) in stamps.iter().enumerate() {
            let due = armed_at + Duration::from_millis(5 * (k as u64 + 1));
            assert!(*at >= due, "expiration {} came {:?} early", k + 1, due.saturating_duration_since(*at));
        }
        // An expiration raised just before the disarm may still be running.
        thread::sleep(Duration::from_millis(30));
        let count = f.recorder.runs.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), count, "disarmed timer kept firing");
    }

    #[test]
    fn late_expirations_do_not_shift_the_deadlines_after_them() {
        // On the external clock the test decides when each expiration is noticed, so this holds
        // on any machine: deadlines are anchor + n * period, not "one period after the last
        // expiration was handled".
        let f = Fixture::new(Mode::External);
        let (idx, _) = f.instance(100_000_000);
        let publish = |ns: i64| {
            let update = f.clock.set_external(ns);
            f.timers.on_clock(update, ns);
        };
        publish(1_000_000_000);
        // Odd expirations are noticed 90 ms late, even ones exactly on time. A timer that counted
        // its next period from the moment it fired would not be due again at the even sample.
        for n in 1..=12i64 {
            let late = if n % 2 == 1 { 90_000_000 } else { 0 };
            publish(1_000_000_000 + n * 100_000_000 + late);
            wait_runs(&f, n as usize);
        }
        assert_eq!(f.sched.cell(idx).missed_periods.load(Ordering::Relaxed), 0, "no period was skipped");
    }

    #[test]
    fn a_slow_task_merges_periods_instead_of_bursting() {
        let f = Fixture::new(Mode::Steady);
        f.recorder.slow_ms.store(30, Ordering::SeqCst);
        let (idx, _) = f.instance(5_000_000);
        thread::sleep(Duration::from_millis(200));
        f.timers.disarm(idx);
        let runs = f.recorder.runs.load(Ordering::SeqCst);
        assert!(runs <= 8, "{runs} runs; 30 ms steps cannot fit more than 7 into 200 ms");
        let merged = f.sched.cell(idx).missed_periods.load(Ordering::Relaxed);
        assert!(merged >= 20, "only {merged} periods were reported as missed");
    }

    #[test]
    fn set_period_replaces_the_old_schedule() {
        let f = Fixture::new(Mode::Steady);
        let (idx, generation) = f.instance(1_000_000_000);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 0);
        f.timers.arm(idx, generation, 5_000_000);
        thread::sleep(Duration::from_millis(100));
        assert!(f.recorder.runs.load(Ordering::SeqCst) >= 10);
    }

    #[test]
    fn external_clock_drives_the_timers() {
        let f = Fixture::new(Mode::External);
        let (idx, _) = f.instance(100_000_000);
        // No time has been published: nothing fires, however long we wait.
        thread::sleep(Duration::from_millis(60));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 0);
        let publish = |ns: i64| {
            let update = f.clock.set_external(ns);
            f.timers.on_clock(update, ns);
        };
        publish(1_000_000_000);
        // The first sample anchors the timer: the first expiration is one period later.
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 0);
        publish(1_099_000_000);
        publish(1_100_000_000);
        wait_runs(&f, 1);
        // A jump of 5 periods fires once and reports four merged periods.
        publish(1_600_000_000);
        wait_runs(&f, 2);
        assert_eq!(f.sched.cell(idx).missed_periods.load(Ordering::Relaxed), 4);
        // Time moving backwards (simulation restart) re-anchors instead of waiting for 1.7 s.
        publish(50_000_000);
        publish(150_000_000);
        wait_runs(&f, 3);
    }

    const MS: i64 = 1_000_000;

    #[test]
    fn the_timer_grid_follows_the_phase_not_the_moment_of_arming() {
        // T0 = 50 ms, period 100 ms: expirations at 50, 150, 250 ms of simulated time. Without a
        // phase the first sample (10 ms) would anchor the first expiration at 110 ms.
        let f = Fixture::with_phase(Mode::External, Some(50 * MS));
        f.instance(100 * MS);
        f.publish(10 * MS);
        f.publish(49 * MS);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 0);
        f.publish(50 * MS);
        wait_runs(&f, 1);
        f.publish(149 * MS);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 1);
        f.publish(150 * MS);
        wait_runs(&f, 2);
        // A restart of the simulated time re-anchors on the same grid.
        f.publish(20 * MS);
        f.publish(50 * MS);
        wait_runs(&f, 3);
    }

    #[test]
    fn a_timer_armed_between_grid_points_waits_for_the_next_one() {
        let f = Fixture::with_phase(Mode::External, Some(0));
        f.publish(1_030 * MS);
        f.instance(100 * MS);
        f.publish(1_099 * MS);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 0);
        f.publish(1_100 * MS);
        wait_runs(&f, 1);
    }

    #[test]
    fn a_one_shot_deadline_fires_once_with_its_own_reason() {
        let f = Fixture::new(Mode::External);
        let (idx, generation) = f.bare();
        f.publish(1_000 * MS);
        f.timers.arm_deadline(idx, generation, 1_500 * MS);
        f.publish(1_499 * MS);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 0);
        f.publish(1_500 * MS);
        wait_runs(&f, 1);
        assert_eq!(f.reasons(), vec![REASON_DEADLINE]);
        // One-shot: later samples do not fire it again, and it is not a timer expiration.
        f.publish(2_500 * MS);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 1);
        assert_eq!(f.sched.cell(idx).timer_fires.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_new_deadline_replaces_the_old_one_and_a_negative_one_cancels() {
        let f = Fixture::new(Mode::External);
        let (idx, generation) = f.bare();
        f.publish(1_000 * MS);
        f.timers.arm_deadline(idx, generation, 1_500 * MS);
        f.timers.arm_deadline(idx, generation, 1_300 * MS);
        f.publish(1_400 * MS);
        wait_runs(&f, 1);
        f.publish(1_600 * MS);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 1, "the replaced deadline must not fire");
        f.timers.arm_deadline(idx, generation, 1_700 * MS);
        f.timers.arm_deadline(idx, generation, -1);
        f.publish(1_800 * MS);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 1, "the cancelled deadline must not fire");
        assert!(!f.timers.is_armed(idx));
    }

    #[test]
    fn a_deadline_in_the_past_fires_at_the_next_clock_reading_and_survives_a_reanchor() {
        let f = Fixture::new(Mode::External);
        let (idx, generation) = f.bare();
        f.publish(1_000 * MS);
        f.timers.arm_deadline(idx, generation, 10 * MS);
        f.publish(1_001 * MS);
        wait_runs(&f, 1);
        // A re-anchoring (time moved backwards) keeps the absolute instant of a pending deadline.
        f.timers.arm_deadline(idx, generation, 5_000 * MS);
        f.publish(100 * MS);
        f.publish(4_999 * MS);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 1);
        f.publish(5_000 * MS);
        wait_runs(&f, 2);
    }

    #[test]
    fn the_period_and_the_deadline_are_independent() {
        let f = Fixture::new(Mode::External);
        let (idx, generation) = f.instance(100 * MS);
        f.publish(1_000 * MS);
        f.timers.arm_deadline(idx, generation, 1_050 * MS);
        f.publish(1_050 * MS);
        wait_runs(&f, 1);
        f.publish(1_100 * MS);
        wait_runs(&f, 2);
        assert_eq!(f.reasons(), vec![REASON_DEADLINE, REASON_TIMER]);
        // Stopping the period keeps a pending deadline; clearing drops both.
        f.timers.arm_deadline(idx, generation, 1_300 * MS);
        f.timers.disarm(idx);
        assert!(!f.timers.is_armed(idx));
        f.publish(1_300 * MS);
        wait_runs(&f, 3);
        f.timers.arm_deadline(idx, generation, 1_500 * MS);
        f.timers.clear(idx);
        f.publish(1_600 * MS);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn the_utc_clock_runs_timers_on_the_wall_clock_grid() {
        // Real time: a 20 ms period on the utc grid expires at multiples of 20 ms of the wall
        // clock, so another host with the same T0 expires at the same instants.
        let f = Fixture::new(Mode::Utc);
        f.instance(20 * MS);
        thread::sleep(Duration::from_millis(250));
        let runs = f.recorder.runs.load(Ordering::SeqCst);
        assert!((3..=13).contains(&runs), "{runs} expirations");
        // A deadline 30 ms ahead on the same clock.
        let (idx, generation) = f.bare();
        let before = f.recorder.runs.load(Ordering::SeqCst);
        f.timers.arm_deadline(idx, generation, f.clock.now_ns() + 30 * MS);
        let deadline_wait = Instant::now() + Duration::from_secs(2);
        while !f.reasons().iter().any(|r| r & REASON_DEADLINE != 0) {
            assert!(Instant::now() < deadline_wait, "deadline did not fire; {before} runs before");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_runs(f: &Fixture, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while f.recorder.runs.load(Ordering::SeqCst) < expected {
            assert!(Instant::now() < deadline, "expected {expected} runs, got {}", f.recorder.runs.load(Ordering::SeqCst));
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(20));
        assert_eq!(f.recorder.runs.load(Ordering::SeqCst), expected);
    }
}
