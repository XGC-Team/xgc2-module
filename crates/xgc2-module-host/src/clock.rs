//! The host clock that `now_ns()`, step contexts, producer stamps and period timers share.
//!
//! `steady` reads CLOCK_MONOTONIC (one process; nothing is comparable across hosts). `utc`
//! reads CLOCK_REALTIME, the domain hosts share when their clocks are disciplined (chrony or
//! PTP; the host reports nothing about the quality and gates nothing). `external` follows one
//! designated state channel whose payload is an `i64` nanosecond time (schema
//! [`CLOCK_SCHEMA`]); in simulation the entity's ROS edge module publishes `/clock` there.
//! Timers then run on that time, so a simulation that runs faster or slower than real time
//! drives the modules at the same ratio.
//!
//! A clock may carry a phase `T0` (`phase_t0_ns`): the origin of the timer grid. Period timers
//! then expire at `T0 + k * period` for whole `k`, so entities on different hosts that share
//! a T0 and a period expire at the same instants. Without a phase `utc` uses `T0 = 0` (a
//! grid of multiples of the period); `steady` has no grid.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// Schema id of the clock channel; the payload is one native-endian `int64_t`.
pub const CLOCK_SCHEMA: &str = "xgc2.clock.v1";
pub const CLOCK_PAYLOAD_SIZE: u32 = 8;
pub const CLOCK_PAYLOAD_ALIGN: u32 = 8;

/// CLOCK_REALTIME in nanoseconds.
pub fn utc_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out pointer; CLOCK_REALTIME always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    #[allow(clippy::unnecessary_cast)]
    let ns = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
    ns
}

/// CLOCK_MONOTONIC in nanoseconds.
pub fn steady_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out pointer; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    // time_t and the nanosecond field are narrower than i64 on some 32-bit targets.
    #[allow(clippy::unnecessary_cast)]
    let ns = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
    ns
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Steady,
    /// CLOCK_REALTIME in nanoseconds.
    Utc,
    External,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Steady => "steady",
            Mode::Utc => "utc",
            Mode::External => "external",
        }
    }
}

/// How an external sample relates to the previous one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Update {
    /// First sample: the clock became valid.
    First,
    Forward,
    /// Time moved backwards (simulation restart); timers must be re-anchored.
    Backward,
}

pub struct Clock {
    mode: Mode,
    /// Origin of the timer grid, in this clock's domain.
    phase_t0_ns: Option<i64>,
    external_ns: AtomicI64,
    valid: AtomicBool,
}

impl Clock {
    pub fn new(mode: Mode) -> Self {
        Self::with_phase(mode, None)
    }

    pub fn with_phase(mode: Mode, phase_t0_ns: Option<i64>) -> Self {
        Self { mode, phase_t0_ns, external_ns: AtomicI64::new(0), valid: AtomicBool::new(mode != Mode::External) }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The origin of the timer grid: the configured phase, 0 for `utc` without one, none for
    /// `steady` and for `external` without one (timers then count from the moment they start).
    pub fn grid_origin(&self) -> Option<i64> {
        match (self.phase_t0_ns, self.mode) {
            (Some(t0), _) => Some(t0),
            (None, Mode::Utc) => Some(0),
            (None, _) => None,
        }
    }

    /// The origin modules see through `phase_t0_ns()`: 0 when there is no grid.
    pub fn phase_t0_ns(&self) -> i64 {
        self.grid_origin().unwrap_or(0)
    }

    /// The first instant strictly after `now` on the grid of `period_ns`, or `now + period_ns`
    /// without a grid.
    pub fn next_expiration(&self, now: i64, period_ns: i64) -> i64 {
        match self.grid_origin() {
            Some(t0) => t0 + ((now - t0).div_euclid(period_ns) + 1) * period_ns,
            None => now + period_ns,
        }
    }

    /// False until an external clock received its first sample.
    pub fn valid(&self) -> bool {
        self.valid.load(Ordering::Acquire)
    }

    pub fn now_ns(&self) -> i64 {
        match self.mode {
            Mode::Steady => steady_ns(),
            Mode::Utc => utc_ns(),
            Mode::External => self.external_ns.load(Ordering::Acquire),
        }
    }

    /// Record an external sample. Only the clock channel's single writer calls this.
    pub fn set_external(&self, ns: i64) -> Update {
        let previous = self.external_ns.swap(ns, Ordering::AcqRel);
        if !self.valid.swap(true, Ordering::AcqRel) {
            Update::First
        } else if ns < previous {
            Update::Backward
        } else {
            Update::Forward
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_clock_is_monotonic_and_valid() {
        let clock = Clock::new(Mode::Steady);
        assert!(clock.valid());
        let a = clock.now_ns();
        let b = clock.now_ns();
        assert!(b >= a && a > 0);
    }

    #[test]
    fn utc_clock_follows_the_wall_clock_and_is_valid() {
        let clock = Clock::new(Mode::Utc);
        assert!(clock.valid());
        let (before, now, after) = (utc_ns(), clock.now_ns(), utc_ns());
        assert!(before <= now && now <= after);
        // 2020-01-01 in nanoseconds: the value is a real UTC time, not an uptime.
        assert!(now > 1_577_836_800_000_000_000);
    }

    #[test]
    fn the_grid_is_anchored_to_the_phase() {
        let utc = Clock::new(Mode::Utc);
        assert_eq!(utc.phase_t0_ns(), 0);
        assert_eq!(utc.next_expiration(1_234_000_000, 100_000_000), 1_300_000_000);
        // On the grid itself the next expiration is a whole period later, never "now".
        assert_eq!(utc.next_expiration(1_300_000_000, 100_000_000), 1_400_000_000);
        let phased = Clock::with_phase(Mode::Utc, Some(1_000_000_037));
        assert_eq!(phased.phase_t0_ns(), 1_000_000_037);
        assert_eq!(phased.next_expiration(1_234_000_000, 100_000_000), 1_300_000_037);
        // Before T0 the grid extends backwards: the next expiration is T0 itself or earlier.
        assert_eq!(phased.next_expiration(500_000_000, 100_000_000), 500_000_037);
        let steady = Clock::new(Mode::Steady);
        assert_eq!(steady.grid_origin(), None);
        assert_eq!(steady.next_expiration(1_234_000_000, 100_000_000), 1_334_000_000);
    }

    #[test]
    fn external_clock_reports_updates() {
        let clock = Clock::new(Mode::External);
        assert!(!clock.valid());
        assert_eq!(clock.now_ns(), 0);
        assert_eq!(clock.set_external(10), Update::First);
        assert!(clock.valid());
        assert_eq!(clock.set_external(20), Update::Forward);
        assert_eq!(clock.now_ns(), 20);
        assert_eq!(clock.set_external(5), Update::Backward);
        assert_eq!(clock.now_ns(), 5);
    }
}
