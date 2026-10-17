//! A simulated [`Platform`]: in memory, deterministic from a seed, and
//! hostile on request.
//!
//! Everything a test needs to make reality worse lives on [`SimFs`] (faults
//! and crashes) and [`SimClock`] (time moves only when told to).  Given the
//! same seed and the same sequence of calls, a simulation does exactly the
//! same thing every time — so any failure is reproducible from its seed (see
//! [`crate::harness`]).
//!
//! Determinism holds while one thread drives the simulation.  Several threads
//! remain *correct* but their interleaving is up to the OS scheduler; a
//! deterministic scheduler is future work (needed by PLAN.md §M19).

mod fs;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

pub use fs::{CrashMode, Faults, SECTOR_SIZE, SimFault, SimFs, sim_fault};

use crate::rng::mix;
use crate::{Clock, FileSystem, Platform, Rng, SplitMix64};

/// A simulated environment.
#[derive(Debug)]
pub struct SimPlatform {
    seed: u64,
    fs: SimFs,
    clock: SimClock,
    rng: SplitMix64,
}

impl SimPlatform {
    /// A fresh simulation: an empty file system, the clock at zero, faults
    /// off.  Everything random in it derives from `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            fs: SimFs::new(mix(seed ^ 0x0000_6673)),
            clock: SimClock::default(),
            rng: SplitMix64::new(mix(seed ^ 0x0072_6E67)),
        }
    }

    /// The seed this simulation was created with.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The simulated file system, with its fault and crash controls.
    pub fn sim_fs(&self) -> &SimFs {
        &self.fs
    }

    /// The simulated clock, with its controls.
    pub fn sim_clock(&self) -> &SimClock {
        &self.clock
    }
}

impl Platform for SimPlatform {
    fn fs(&self) -> &dyn FileSystem {
        &self.fs
    }
    fn clock(&self) -> &dyn Clock {
        &self.clock
    }
    fn rng(&self) -> &dyn Rng {
        &self.rng
    }
}

/// Wall-clock time at simulated monotonic time zero: 2026-01-01T00:00:00Z.
pub const SIM_EPOCH_SECS: u64 = 1_767_225_600;

/// A clock that moves only when [`SimClock::advance`] is called.
#[derive(Debug, Default)]
pub struct SimClock {
    nanos: AtomicU64,
}

impl SimClock {
    /// Moves time forward by `by`.
    pub fn advance(&self, by: Duration) {
        let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        self.nanos
            .update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.saturating_add(by)
            });
    }
}

impl Clock for SimClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::Relaxed))
    }
    fn wall(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(SIM_EPOCH_SECS) + self.now()
    }
    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
}
