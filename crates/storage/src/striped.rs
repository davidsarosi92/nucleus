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
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

/// Stripes: more than the threads that usually read at once.
const STRIPES: usize = 16;

/// Tries (a yield each) a writer keeps the stripes it took while it waits
/// for the others before it lets them all go.
const HOLD_TRIES: u32 = 256;

/// A stripe, a cache line of its own (128 bytes on Apple's cores).
#[repr(align(128))]
struct Stripe<T>(RwLock<Option<Arc<T>>>);

/// The lock.
pub struct Striped<T> {
    stripes: Box<[Stripe<T>]>,
    /// Writers one at a time (a writer waiting holds no stripe).
    writer: Mutex<()>,
    /// A writer is taking the stripes: a reader holding none waits for it.
    pending: AtomicBool,
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
        }
    }

    /// Locked for reading: this thread's stripe — once a writer taking
    /// the stripes is done, unless this thread holds a read guard already.
    pub fn read(&self) -> ReadGuard<'_, T> {
        if HOLDING.with(Cell::get) == 0 {
            while self.pending.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
        let g = self.stripes[stripe()]
            .0
            .read()
            .unwrap_or_else(|e| e.into_inner());
        HOLDING.with(|h| h.set(h.get() + 1));
        ReadGuard(g)
    }

    /// Locked for writing: every stripe.
    pub fn write(&self) -> WriteGuard<'_, T> {
        let writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let n = self.stripes.len();
        let mut held: Vec<Option<RwLockWriteGuard<'_, Option<Arc<T>>>>> =
            (0..n).map(|_| None).collect();
        let (mut count, mut tries, mut rounds) = (0, 0u32, 0u32);
        self.pending.store(true, Ordering::Release);
        while count < n {
            for (s, g) in self.stripes.iter().zip(&mut held) {
                if g.is_some() {
                    continue;
                }
                match s.0.try_write() {
                    Ok(w) => *g = Some(w),
                    Err(TryLockError::Poisoned(e)) => *g = Some(e.into_inner()),
                    Err(TryLockError::WouldBlock) => continue,
                }
                count += 1;
            }
            if count == n {
                break;
            }
            // A stripe held by a reader: the stripes taken kept meanwhile,
            // and readers holding none wait (so readers that come back at
            // once do not starve the writer) — for a while: a reader may
            // be waiting for a thread that waits for the writer.  Then all
            // let go, and taken again after a pause growing with the
            // rounds.
            tries += 1;
            if tries < HOLD_TRIES {
                std::thread::yield_now();
                continue;
            }
            self.pending.store(false, Ordering::Release);
            held.iter_mut().for_each(|g| *g = None);
            (count, tries) = (0, 0);
            rounds += 1;
            let us = u64::from(rounds.min(20)) * 50;
            std::thread::sleep(std::time::Duration::from_micros(us));
            self.pending.store(true, Ordering::Release);
        }
        // Every stripe held: readers wait on them now.
        self.pending.store(false, Ordering::Release);
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
pub struct ReadGuard<'a, T>(RwLockReadGuard<'a, Option<Arc<T>>>);

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        HOLDING.with(|h| h.set(h.get() - 1));
    }
}

impl<T> std::ops::Deref for ReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.0
            .as_deref()
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
    /// within a deadline while they read.
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
            let start = std::time::Instant::now();
            while done.load(Ordering::Relaxed) < 1000
                && start.elapsed() < std::time::Duration::from_secs(10)
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            stop.store(true, Ordering::Relaxed);
            assert_eq!(done.load(Ordering::Relaxed), 1000, "writes done in 10 s");
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
