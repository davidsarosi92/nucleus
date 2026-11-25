//! A reader/writer lock whose readers write no line other threads' readers
//! write: a lock a stripe, a reader taking its thread's stripe's, a writer
//! every stripe's.
//!
//! A store's tree is read under its lock by every lookup — on ten
//! threads, the one lock word every read acquired and released was a
//! fifth of their time.  Readers on different stripes share nothing; a
//! writer pays a lock a stripe.  The value sits in an [`Arc`] in every
//! stripe: a reader reads it through its stripe's guard (no count
//! changed), a writer holding every stripe takes the copies out, so the
//! last is the only one and lends `&mut`, and puts them back.  All safe
//! Rust.
//!
//! A writer never blocks on a stripe: it tries them, keeping those it
//! takes, and tries the others again — meanwhile a reader holding no read
//! guard waits for it (so readers that come back at once do not starve
//! it); if a stripe stays held a while it lets go of all and starts again
//! later.  So a reader that reads again while holding its stripe never
//! waits for the writer, and one that waits for a thread that reads waits
//! for it a while at most.
//!
//! The idea is the Linux kernel's "big reader" lock and its per-CPU
//! reader/writer semaphore, and BRAVO's reader slots (Dice and Kogan,
//! "BRAVO — Biased Locking for Reader-Writer Locks", USENIX ATC 2019).

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{
    Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError,
};
use std::time::{Duration, Instant};

/// Stripes: more than the threads that usually read at once.
const STRIPES: usize = 16;

/// How long a writer keeps the stripes it took while it waits for the
/// others before it lets them all go: [`SPIN_TRIES`] tries, a short spin
/// between them — not a yield: on Windows a yield hands the processor to
/// a busy thread for its whole time slice (CI's runner, 48 busy readers
/// on four cores: ~390 ms a write, the stripes free at the second try) —
/// then asleep until a reader lets a stripe go (it wakes the writer) or
/// [`HOLD_WINDOW`] is over, doubled each time it lets go, up to 32 times
/// (readers holding their stripes longer than a window took them again
/// each time: on four cores 20 writes took 10–14 s, doubling 0.1–0.3 s).
/// Woken, not polling with sleeps: a sleep lasts a timer's tick on some
/// systems, and a timed wait on a condition 10–15 ms on Windows.
const SPIN_TRIES: u32 = 32;
const HOLD_WINDOW: Duration = Duration::from_millis(5);

/// A stripe, a cache line of its own (128 bytes on Apple's cores).
#[repr(align(128))]
struct Stripe<T>(RwLock<Option<Arc<T>>>);

/// The lock.
pub struct Striped<T> {
    stripes: Box<[Stripe<T>]>,
    /// Writers one at a time (a writer waiting holds no stripe).
    writer: Mutex<()>,
    /// A writer is taking the stripes: a reader holding none waits for it
    /// — asleep on `readers` (a reader spinning took the processor from
    /// the readers the writer waits for: on a few cores, CI's Windows and
    /// Android runners, writes stalled).
    pending: AtomicBool,
    /// Guards `pending`'s changes and the sleeps on it: readers on
    /// `readers` wait for it cleared, the writer on `writer_wait` for a
    /// stripe let go (a reader letting go while it is set wakes it).
    gate: Mutex<()>,
    readers: Condvar,
    writer_wait: Condvar,
    /// The shortest sleep: zero, but for a test making the system's timer
    /// coarse (as Windows's) everywhere.
    timer_floor: Duration,
    /// The first hold window: [`HOLD_WINDOW`], but for a test on a
    /// machine whose sleeps run longer than it.
    hold_window: Duration,
}

impl<T: std::fmt::Debug> std::fmt::Debug for Striped<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Striped").finish_non_exhaustive()
    }
}

thread_local! {
    /// The read guards this thread holds (of any lock): one that holds
    /// some does not wait for a pending writer — it may hold the stripe
    /// the writer waits for.
    static HOLDING: Cell<usize> = const { Cell::new(0) };
}

/// This thread's stripe.
fn stripe() -> usize {
    thread_local! {
        static STRIPE: usize = {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            NEXT.fetch_add(1, Ordering::Relaxed) % STRIPES
        };
    }
    STRIPE.with(|s| *s)
}

impl<T> Striped<T> {
    /// A lock over `value`.
    pub fn new(value: T) -> Self {
        let value = Arc::new(value);
        Self {
            stripes: (0..STRIPES)
                .map(|_| Stripe(RwLock::new(Some(Arc::clone(&value)))))
                .collect(),
            writer: Mutex::new(()),
            pending: AtomicBool::new(false),
            gate: Mutex::new(()),
            readers: Condvar::new(),
            writer_wait: Condvar::new(),
            timer_floor: Duration::ZERO,
            hold_window: HOLD_WINDOW,
        }
    }

    /// A lock whose every sleep and timed wait lasts `floor` at least (a
    /// coarse system timer) and whose writer's first hold window is
    /// `window`, for tests.
    #[cfg(test)]
    fn with_timer_floor(value: T, floor: Duration, window: Duration) -> Self {
        Self {
            timer_floor: floor,
            hold_window: window,
            ..Self::new(value)
        }
    }

    fn gate(&self) -> MutexGuard<'_, ()> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `pending` set or cleared; readers waiting for it woken when cleared.
    fn set_pending(&self, on: bool) {
        if on {
            self.pending.store(true, Ordering::Release);
        } else {
            let g = self.gate();
            self.pending.store(false, Ordering::Release);
            drop(g);
            self.readers.notify_all();
        }
    }

    /// Locked for reading: this thread's stripe — once a writer taking
    /// the stripes is done, unless this thread holds a read guard already.
    pub fn read(&self) -> ReadGuard<'_, T> {
        if HOLDING.with(Cell::get) == 0 && self.pending.load(Ordering::Acquire) {
            let mut g = self.gate();
            while self.pending.load(Ordering::Acquire) {
                g = self.readers.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        }
        let g = self.stripes[stripe()]
            .0
            .read()
            .unwrap_or_else(|e| e.into_inner());
        HOLDING.with(|h| h.set(h.get() + 1));
        ReadGuard {
            guard: Some(g),
            lock: self,
        }
    }

    /// Every stripe not yet in `held` tried once; how many are held now.
    fn try_stripes<'a>(
        &'a self,
        held: &mut [Option<RwLockWriteGuard<'a, Option<Arc<T>>>>],
    ) -> usize {
        for (s, g) in self.stripes.iter().zip(held.iter_mut()) {
            if g.is_some() {
                continue;
            }
            match s.0.try_write() {
                Ok(w) => *g = Some(w),
                Err(TryLockError::Poisoned(e)) => *g = Some(e.into_inner()),
                Err(TryLockError::WouldBlock) => {}
            }
        }
        held.iter().filter(|g| g.is_some()).count()
    }

    /// Locked for writing: every stripe.
    pub fn write(&self) -> WriteGuard<'_, T> {
        let writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let n = self.stripes.len();
        let mut held: Vec<Option<RwLockWriteGuard<'_, Option<Arc<T>>>>> =
            (0..n).map(|_| None).collect();
        let (mut tries, mut rounds) = (0u32, 0u32);
        let mut since: Option<Instant> = None;
        self.set_pending(true);
        while self.try_stripes(&mut held) < n {
            // A stripe held by a reader: the stripes taken kept meanwhile,
            // and readers holding none wait (so readers that come back at
            // once do not starve the writer) — for a while: a reader may
            // be waiting for a thread that waits for the writer.  Then all
            // let go, and taken again after a pause growing with the
            // rounds.
            tries += 1;
            if tries < SPIN_TRIES {
                for _ in 0..16 {
                    std::hint::spin_loop();
                }
                continue;
            }
            let window = self.hold_window * (1 << rounds.min(5));
            let left = window.checked_sub(since.get_or_insert_with(Instant::now).elapsed());
            if let Some(left) = left.filter(|l| !l.is_zero()) {
                // Tried again under the gate: a reader letting go after
                // this try wakes the writer only once it sleeps.
                let g = self.gate();
                if self.try_stripes(&mut held) < n {
                    let _ = self
                        .writer_wait
                        .wait_timeout(g, left.max(self.timer_floor))
                        .unwrap_or_else(|e| e.into_inner());
                }
                continue;
            }
            self.set_pending(false);
            held.iter_mut().for_each(|g| *g = None);
            (tries, since) = (0, None);
            rounds += 1;
            let pause = Duration::from_micros(u64::from(rounds.min(20)) * 50);
            std::thread::sleep(pause.max(self.timer_floor));
            self.set_pending(true);
        }
        // Every stripe held: readers wait on them now.
        self.set_pending(false);
        let mut guards: Vec<_> = held.into_iter().flatten().collect();
        // Every copy out: the last one is then the only one.
        let mut value = None;
        for g in &mut guards {
            value = g.take();
        }
        WriteGuard {
            guards,
            value: Some(value.expect("a value in every stripe between writers")),
            _writer: writer,
        }
    }
}

/// Read access, through a stripe.
pub struct ReadGuard<'a, T> {
    /// Taken out when dropped, so the stripe is let go before a waiting
    /// writer is woken.
    guard: Option<RwLockReadGuard<'a, Option<Arc<T>>>>,
    lock: &'a Striped<T>,
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        drop(self.guard.take());
        HOLDING.with(|h| h.set(h.get() - 1));
        // A writer taking the stripes may be asleep waiting for this one.
        if self.lock.pending.load(Ordering::Acquire) {
            let _g = self.lock.gate();
            self.lock.writer_wait.notify_one();
        }
    }
}

impl<T> std::ops::Deref for ReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.guard
            .as_ref()
            .and_then(|g| g.as_deref())
            .expect("a value in every stripe between writers")
    }
}

/// Write access: every stripe held, the value's only copy.
pub struct WriteGuard<'a, T> {
    guards: Vec<RwLockWriteGuard<'a, Option<Arc<T>>>>,
    value: Option<Arc<T>>,
    _writer: MutexGuard<'a, ()>,
}

impl<T> std::ops::Deref for WriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value.as_deref().expect("held until dropped")
    }
}

impl<T> std::ops::DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        Arc::get_mut(self.value.as_mut().expect("held until dropped"))
            .expect("the only copy while every stripe is held")
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        // The copies back, before the stripes are let go (the guards
        // drop after this).
        let value = self.value.take().expect("held until dropped");
        for g in &mut self.guards {
            **g = Some(Arc::clone(&value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Readers on many threads see a writer's whole change or none of it:
    /// two halves that a writer keeps equal.
    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
    fn readers_see_a_writers_change_whole() {
        let lock = Striped::new((0u64, 0u64));
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let mut seen = 0;
                    while !stop.load(Ordering::Relaxed) {
                        let g = lock.read();
                        let (a, b) = *g;
                        assert_eq!(a, b);
                        assert!(a >= seen);
                        seen = a;
                    }
                });
            }
            for i in 1..=2000 {
                let mut g = lock.write();
                g.0 = i;
                std::thread::yield_now();
                g.1 = i;
            }
            stop.store(true, Ordering::Relaxed);
        });
        assert_eq!(*lock.read(), (2000, 2000));
    }

    /// A writer is not starved by readers that hold their stripes nearly
    /// all the time and take them again at once — more readers than
    /// stripes and, as on a small machine, than cores: its writes are done
    /// within a deadline while they read.  The deadline grows on a few
    /// cores: each write waits for every reader holding a stripe to be run
    /// again (on two, 5–40 ms a write with the readers asleep while they
    /// wait; the writer never let go).
    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
    fn a_writer_is_not_starved_by_readers_that_come_back() {
        let lock = Striped::new(0u64);
        let stop = AtomicBool::new(false);
        let done = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..3 * STRIPES {
                s.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let g = lock.read();
                        // A little work under the guard: held nearly always.
                        let mut x = *g;
                        for i in 0..200u64 {
                            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(i));
                        }
                        std::hint::black_box(x);
                    }
                });
            }
            s.spawn(|| {
                for _ in 0..1000 {
                    *lock.write() += 1;
                    done.fetch_add(1, Ordering::Relaxed);
                }
            });
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            let deadline = Duration::from_secs(10 * (STRIPES / cores).max(1) as u64);
            let start = Instant::now();
            while done.load(Ordering::Relaxed) < 1000 && start.elapsed() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            stop.store(true, Ordering::Relaxed);
            assert_eq!(
                done.load(Ordering::Relaxed),
                1000,
                "writes done in {deadline:?}"
            );
        });
    }

    /// A writer is woken when the stripes it waits for are let go, not by
    /// a system timer: with every sleep of the lock made a coarse timer's
    /// (15 ms at least, as Windows's can be) and readers holding their
    /// stripes while asleep (as readers a scheduler took off a few cores
    /// do), a write waits about a reader's hold, not a tick.  A writer
    /// that slept between its tries did 52 writes in 40 s on CI's Windows
    /// runner.
    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
    fn a_writer_is_woken_when_its_stripes_are_let_go_not_by_a_timer() {
        // Longer than a writer's spinning, even on a loaded machine.
        const HOLD: Duration = Duration::from_millis(2);
        const WRITES: u32 = 100;
        // This system's sleep of HOLD: a reader's hold.
        let t = Instant::now();
        for _ in 0..10 {
            std::thread::sleep(HOLD);
        }
        let hold = t.elapsed() / 10;
        let floor = (hold * 12).max(Duration::from_millis(16));
        // The writer's window longer than a hold, wherever sleeps run
        // long (a loaded runner's 2 ms sleep took 9 ms: past the 5 ms
        // window, every write gave up and slept the timer out).
        let window = (hold * 4).max(HOLD_WINDOW);
        let lock = Striped::with_timer_floor(0u64, floor, window);
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            for _ in 0..3 * STRIPES {
                s.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let g = lock.read();
                        std::thread::sleep(HOLD);
                        drop(g);
                    }
                });
            }
            // Each write after a pause longer than a hold, so that every
            // reader holds its stripe again; the writes' own times.
            let mut took: Vec<Duration> = (0..WRITES)
                .map(|_| {
                    std::thread::sleep(hold * 2);
                    let start = Instant::now();
                    *lock.write() += 1;
                    start.elapsed()
                })
                .collect();
            stop.store(true, Ordering::Relaxed);
            // A tick a write would be `floor` each; the median, as a
            // loaded machine (other tests running) makes some writes wait
            // out a give-up's pause.
            took.sort();
            let median = took[took.len() / 2];
            assert!(
                median < floor / 3,
                "a write's median {median:?}, a reader's hold {hold:?}, the timer {floor:?}"
            );
        });
        assert_eq!(*lock.read(), u64::from(WRITES));
    }

    /// Readers holding their stripes longer than the writer's first hold
    /// window, coming back at once, do not starve it: its window grows
    /// each time it lets go.  (A window that did not grow: 20 writes in
    /// 10–14 s on four cores — the readers took their stripes again each
    /// time it let go — 0.1–0.3 s growing; ten cores hid it.)
    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
    fn a_writer_outwaits_readers_holding_longer_than_its_window() {
        let lock = Striped::new(0u64);
        let stop = AtomicBool::new(false);
        let done = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..3 * STRIPES {
                s.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let g = lock.read();
                        std::thread::sleep(HOLD_WINDOW + HOLD_WINDOW / 2);
                        drop(g);
                    }
                });
            }
            s.spawn(|| {
                for _ in 0..20 {
                    *lock.write() += 1;
                    done.fetch_add(1, Ordering::Relaxed);
                }
            });
            let start = Instant::now();
            while done.load(Ordering::Relaxed) < 20 && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(5));
            }
            stop.store(true, Ordering::Relaxed);
            assert_eq!(done.load(Ordering::Relaxed), 20, "writes done in 5 s");
        });
    }

    /// A reader holding its stripe reads again, and waits for another
    /// thread that reads, while a writer waits: neither waits for the
    /// writer (it holds no stripe while it waits).
    #[test]
    #[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
    fn a_reader_reading_again_does_not_wait_for_a_waiting_writer() {
        let lock = Striped::new(1u32);
        let outer = lock.read();
        std::thread::scope(|s| {
            let writer = s.spawn(|| *lock.write() += 1);
            // The writer is trying meanwhile.
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert_eq!(*lock.read(), 1);
            // Another thread's read, joined while this one holds its own.
            let other = s.spawn(|| *lock.read());
            assert_eq!(other.join().unwrap(), 1);
            drop(outer);
            writer.join().unwrap();
        });
        assert_eq!(*lock.read(), 2);
    }
}
