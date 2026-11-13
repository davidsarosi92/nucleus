//! Decoded objects of index methods, kept in memory across transactions
//! (PLAN.md §M14.12 V4: an HNSW hop is a memory access; §M14.21: a text
//! index's segments).  Objects are keyed by a hash of their contents, so
//! they are the same in every snapshot and every index that wrote the
//! same bytes; the cache holds them up to a budget, the least recently
//! used dropped past it.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use nucleus_kernels::{MixState, mix};

/// An object's id: a hash of its kind and bytes (the column store's
/// `segment_id`), never 0.
pub(crate) fn object_id(kind: u64, b: &[u8]) -> u64 {
    let mut h = mix(kind ^ 0x6e75_636c_6575_7321 ^ b.len() as u64);
    for w in b.chunks(8) {
        let mut x = [0u8; 8];
        x[..w.len()].copy_from_slice(w);
        h = mix(h ^ u64::from_le_bytes(x));
    }
    h | 1
}

/// The bytes the cache holds at most, once known (0: not yet).  Index
/// objects past it are decoded again each time they are read — a text
/// index's segments over it, every search.
static BUDGET: AtomicUsize = AtomicUsize::new(0);

/// Sets the bytes the cache holds at most (the server's `--index-cache`).
pub fn set_budget(bytes: usize) {
    BUDGET.store(bytes.max(1 << 20), Relaxed);
}

/// The budget: as set, else a quarter of the memory the process may use
/// (`nucleus_platform::real::physical_memory`; PostgreSQL's advice for
/// `shared_buffers`, DuckDB's `memory_limit` sized by the machine too) —
/// a bound, not memory taken — else 1 GiB.
pub fn budget() -> usize {
    match BUDGET.load(Relaxed) {
        0 => {
            let b = nucleus_platform::real::physical_memory()
                .and_then(|m| usize::try_from(m / 4).ok())
                .unwrap_or(1 << 30)
                .max(1 << 20);
            // Another thread may have set it meanwhile: theirs stands.
            let _ = BUDGET.compare_exchange(0, b, Relaxed, Relaxed);
            BUDGET.load(Relaxed)
        }
        b => b,
    }
}

/// What kind of decoded object (part of its key).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Kind {
    Dir,
    Part,
    Vectors,
    Lists,
    Level,
    Dead,
    Sealed,
    /// A text index's segment.
    TextSegment,
    /// A text index segment's deleted documents.
    TextDeleted,
}

struct Entry {
    value: Arc<dyn Any + Send + Sync>,
    bytes: usize,
    used: AtomicU64,
}

#[derive(Default)]
struct Cache {
    map: HashMap<(Kind, u64), Entry, MixState>,
    bytes: usize,
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn tick() -> u64 {
    static T: AtomicU64 = AtomicU64::new(0);
    T.fetch_add(1, Relaxed)
}

/// The object `(kind, key)`, built by `build` (it and its bytes) if not
/// held.
pub(crate) fn get<T: Send + Sync + 'static>(
    kind: Kind,
    key: u64,
    build: impl FnOnce() -> crate::error::Result<(T, usize)>,
) -> crate::error::Result<Arc<T>> {
    {
        let c = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = c.map.get(&(kind, key))
            && let Ok(v) = e.value.clone().downcast::<T>()
        {
            e.used.store(tick(), Relaxed);
            return Ok(v);
        }
    }
    let (value, bytes) = build()?;
    let value = Arc::new(value);
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    let entry = Entry {
        value: value.clone(),
        bytes,
        used: AtomicU64::new(tick()),
    };
    if let Some(old) = c.map.insert((kind, key), entry) {
        c.bytes -= old.bytes;
    }
    c.bytes += bytes;
    if c.bytes > budget() {
        evict(&mut c);
    }
    Ok(value)
}

/// Drops every object: an erasure's (PLAN.md §M14.24 R2b) — objects are
/// known by their contents' hash, not by the rows they came from, so which
/// held a deleted row's values cannot be told.  Readers decode again.
pub(crate) fn clear() {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    c.map.clear();
    c.bytes = 0;
}

/// Drops the least recently used objects until a quarter of the budget is
/// free.
fn evict(c: &mut Cache) {
    let mut by_use: Vec<(u64, (Kind, u64), usize)> = c
        .map
        .iter()
        .map(|(k, e)| (e.used.load(Relaxed), *k, e.bytes))
        .collect();
    by_use.sort_unstable_by_key(|e| e.0);
    for (_, k, bytes) in by_use {
        if c.bytes <= budget() / 4 * 3 {
            break;
        }
        c.map.remove(&k);
        c.bytes -= bytes;
    }
}
