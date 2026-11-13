//! nucleus inside an application (PLAN.md §M14.23): a database in memory
//! or in a directory, opened by the process that uses it — no server.
//!
//! A [`Database`] is the engine, its maintenance and its embedding
//! columns' worker, as `nucleus server` runs them.  Each [`Connection`]
//! is one session speaking the nucleus protocol (`ncp`, DOCS.md §10) over
//! a pipe inside the process instead of a socket: the drivers' own
//! protocol code runs unchanged over it, so the Python and JavaScript
//! drivers have one API for a server and for a database in the process,
//! and a program moves between them by its connection string.
//!
//! There is no login: whoever can open the directory has the database
//! (its file permissions guard it, as SQLite's do); a session is the user
//! its Hello names.  One process opens a directory at once (its lock); in
//! the process, opening it again shares the open database.
//!
//! The C API ([`ffi`], `include/nucleus_embedded.h`) is what Python,
//! Node, Swift and Kotlin load.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nucleus_kv::{Kv, Options, SyncPolicy};
use nucleus_server::{Config, Stop, Trust};
use nucleus_sql::ext::Registry;

pub mod ffi;
#[cfg(target_os = "wasi")]
mod host;

/// What `open` takes for a database in memory.
pub const MEMORY: &str = ":memory:";

/// The bytes a pipe holds before its writer waits for the reader (a
/// socket's buffers, roughly; a result larger streams through).
const PIPE_BUFFER: usize = 4 << 20;

/// How long a reader spins for bytes before it sleeps.
const SPIN: Duration = Duration::from_micros(50);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The functions, types and models a session has: the engine's, and the
/// modules this build includes.
pub fn registry() -> Arc<Registry> {
    static R: OnceLock<Arc<Registry>> = OnceLock::new();
    R.get_or_init(|| {
        #[allow(unused_mut)]
        let mut r = Registry::builtin();
        #[cfg(feature = "embeddings")]
        r.add(&nucleus_embeddings::Embeddings)
            .expect("the embeddings module registers");
        #[cfg(feature = "local-models")]
        r.add(&nucleus_local_models::LocalModels)
            .expect("the local-models module registers");
        Arc::new(r)
    })
    .clone()
}

// ------------------------------------------------------------------ options

/// How a database opens: `key=value` pairs separated by `;` or `,`
/// (`cache=64MB; sync=full`).  Keys: `cache` (the page cache), `sync`
/// (`ordered` or `full`, PLAN.md D17), `wal` (`minimal` or `full`).
pub fn options(text: &str, memory: bool) -> Result<Options, String> {
    let defaults = Options::default();
    let mut o = if memory {
        // Files in memory: the WAL is memory too, so checkpoints come
        // sooner and segments are smaller.  The page cache stays as large
        // as a file's: a page it misses is copied from the memory files
        // and decoded again (a 32 MiB cache made a million-row scan twice
        // as slow).
        Options {
            checkpoint_wal_bytes: 16 << 20,
            wal_segment_bytes: 1 << 20,
            ..defaults
        }
    } else {
        defaults
    };
    for pair in text
        .split([';', ','])
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let (k, v) = pair
            .split_once('=')
            .map(|(k, v)| (k.trim(), v.trim()))
            .ok_or_else(|| format!("option {pair:?}: give key=value"))?;
        match k {
            "cache" => {
                o.cache_bytes = nucleus_sql::dbsettings::memory(v)
                    .map_err(|e| format!("cache: {}", e.message))?
            }
            "sync" => {
                o.sync = match v {
                    "ordered" => SyncPolicy::Ordered,
                    "full" => SyncPolicy::Full,
                    _ => return Err("sync takes ordered or full".into()),
                }
            }
            "wal" => {
                o.wal_minimal = match v {
                    "minimal" => true,
                    "full" => false,
                    _ => return Err("wal takes minimal or full".into()),
                }
            }
            other => {
                return Err(format!("no option {other:?} (there are: cache, sync, wal)"));
            }
        }
    }
    Ok(o)
}

/// The browser's pool of storage ([`host`]'s slots), opened once.
#[cfg(target_os = "wasi")]
fn slot_pool() -> Result<&'static nucleus_platform::slots::SlotPlatform, String> {
    use nucleus_platform::slots::SlotPlatform;
    static POOL: OnceLock<SlotPlatform> = OnceLock::new();
    if let Some(p) = POOL.get() {
        return Ok(p);
    }
    let p = SlotPlatform::new(Arc::new(host::HostSlots))
        .map_err(|e| format!("the browser's storage: {e}"))?;
    Ok(POOL.get_or_init(|| p))
}

/// What a directory is opened on: the browser's pool on WASI; `None`
/// elsewhere, where a directory is the OS's.
fn pool() -> Result<Option<Arc<dyn nucleus_platform::Platform>>, String> {
    #[cfg(target_os = "wasi")]
    {
        /// The pool, as the engine takes a platform.
        struct Pool(&'static nucleus_platform::slots::SlotPlatform);
        impl nucleus_platform::Platform for Pool {
            fn fs(&self) -> &dyn nucleus_platform::FileSystem {
                self.0.fs()
            }
            fn clock(&self) -> &dyn nucleus_platform::Clock {
                self.0.clock()
            }
            fn rng(&self) -> &dyn nucleus_platform::Rng {
                self.0.rng()
            }
            fn parallelism(&self) -> usize {
                1
            }
        }
        Ok(Some(Arc::new(Pool(slot_pool()?))))
    }
    #[cfg(not(target_os = "wasi"))]
    Ok(None)
}

/// The pool's slots not yet taken (`None`: no pool).
pub(crate) fn pool_free() -> Option<usize> {
    #[cfg(target_os = "wasi")]
    return slot_pool().ok().map(|p| p.slot_fs().free_slots());
    #[cfg(not(target_os = "wasi"))]
    None
}

/// Whether the platform has threads (WASI has none: no background
/// threads, sessions by [`Database::exchange`] only).
pub const THREADS: bool = !cfg!(target_os = "wasi");

// ------------------------------------------------------------------ database

struct Inner {
    /// What it was opened as (`:memory:` or the directory).
    name: String,
    /// `None` once closed.
    kv: Mutex<Option<Arc<nucleus_kv::Database>>>,
    config: Config,
    stop: Stop,
    background: Mutex<Vec<JoinHandle<()>>>,
    sessions: Mutex<Vec<Session>>,
    next_id: AtomicU32,
    /// Open [`Database`] handles.
    handles: AtomicUsize,
    /// Its key in [`opened`] (a directory).
    key: Option<PathBuf>,
}

struct Session {
    pipes: Arc<Pipes>,
    thread: JoinHandle<()>,
}

/// The directories open in this process.
fn opened() -> &'static Mutex<HashMap<PathBuf, Weak<Inner>>> {
    static O: OnceLock<Mutex<HashMap<PathBuf, Weak<Inner>>>> = OnceLock::new();
    O.get_or_init(Default::default)
}

/// A database open in this process (see the crate's documentation).
/// Dropping it closes it, as [`close`](Self::close) does.
pub struct Database {
    inner: Arc<Inner>,
    closed: bool,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("name", &self.inner.name)
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Opens the database in directory `path` (made if missing), or a new
    /// one in memory for [`MEMORY`]; `options` as [`options`] reads them.
    /// A directory this process has open already is shared (its options
    /// then are the first opening's).
    pub fn open(path: &str, options_text: &str) -> Result<Self, String> {
        let memory = path == MEMORY;
        let opts = options(options_text, memory)?;
        if memory {
            let platform = nucleus_platform::memory::MemoryPlatform::new()
                .map_err(|e| format!("the machine's clock: {e}"))?;
            let db = nucleus_kv::Database::open_on(Arc::new(platform), Path::new("/db"), opts)
                .map_err(|e| e.to_string())?;
            return Ok(Self::start(path.to_string(), db, None));
        }
        if path.is_empty() {
            return Err("give a directory, or :memory:".into());
        }
        let key = if THREADS {
            std::fs::create_dir_all(path).map_err(|e| format!("{path}: {e}"))?;
            std::fs::canonicalize(path).map_err(|e| format!("{path}: {e}"))?
        } else {
            // In the browser's pool, by its name.
            Path::new("/").join(path.trim_start_matches('/'))
        };
        let mut open = lock(opened());
        if let Some(inner) = open.get(&key).and_then(Weak::upgrade)
            && lock(&inner.kv).is_some()
        {
            inner.handles.fetch_add(1, Ordering::SeqCst);
            return Ok(Self {
                inner,
                closed: false,
            });
        }
        let db = match pool()? {
            Some(pool) => nucleus_kv::Database::open_on(pool, &key, opts),
            None => nucleus_kv::Database::open(&key, opts),
        }
        .map_err(|e| format!("{path}: {e}"))?;
        let d = Self::start(path.to_string(), db, Some(key.clone()));
        open.insert(key, Arc::downgrade(&d.inner));
        Ok(d)
    }

    fn start(name: String, db: nucleus_kv::Database, key: Option<PathBuf>) -> Self {
        let db = Arc::new(db);
        let kv: Arc<dyn Kv> = db.clone();
        let config = Config {
            auth: Arc::new(Trust),
            registry: registry(),
            ..Config::default()
        };
        let stop = Stop::new();
        // Without threads (WASI) the sessions do it all: the embedding
        // columns' jobs, checkpoints at commits.
        let background = if THREADS {
            vec![
                nucleus_server::maintain_until(Arc::clone(&kv), stop.clone()),
                // The embedding columns' jobs (§M14.22 E4b), off the sessions.
                nucleus_sql::embedding::serve(Arc::clone(&kv), Arc::clone(&config.registry)),
            ]
        } else {
            Vec::new()
        };
        Self {
            inner: Arc::new(Inner {
                name,
                kv: Mutex::new(Some(db)),
                config,
                stop,
                background: Mutex::new(background),
                sessions: Mutex::new(Vec::new()),
                next_id: AtomicU32::new(1),
                handles: AtomicUsize::new(1),
                key,
            }),
            closed: false,
        }
    }

    /// What it was opened as: `:memory:` or its directory.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// The engine (for Rust programs that use it directly).
    pub fn kv(&self) -> Option<Arc<dyn Kv>> {
        lock(&self.inner.kv).clone().map(|db| db as Arc<dyn Kv>)
    }

    /// A new session driven by the bytes given to it, on the caller's
    /// thread ([`nucleus_server::Exchange`]): for a platform without
    /// threads, the browser's WebAssembly.  Nothing waits mid-statement,
    /// so a client sends `COPY` data with its `COPY` and fetches results
    /// whole.  It keeps the engine until dropped (a [`close`](Self::close)
    /// leaves it be).
    pub fn exchange(&self) -> Result<nucleus_server::Exchange, String> {
        let kv = self.kv().ok_or("the database is closed")?;
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        Ok(nucleus_server::Exchange::new(
            kv,
            self.inner.config.clone(),
            id,
        ))
    }

    /// A new session, served on a thread of its own until the connection
    /// closes.
    pub fn connect(&self) -> Result<Connection, String> {
        // Under the sessions' lock: a close that took the engine first
        // finds this one's session (it took the engine after) or this one
        // finds none.
        let mut sessions = lock(&self.inner.sessions);
        let kv = self.kv().ok_or("the database is closed")?;
        let pipes = Arc::new(Pipes::default());
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let config = self.inner.config.clone();
        let server = ServerEnd(Arc::clone(&pipes));
        let thread = std::thread::Builder::new()
            .name(format!("nucleus session {id}"))
            .spawn(move || {
                let _ = nucleus_server::handle_native_stream(Box::new(server), kv, &config, id);
            })
            .map_err(|e| format!("a session's thread: {e}"))?;
        sessions.retain(|s| !s.thread.is_finished());
        sessions.push(Session {
            pipes: Arc::clone(&pipes),
            thread,
        });
        Ok(Connection { pipes })
    }

    /// Closes this handle; the last one open closes the database: its
    /// sessions end (each after its statement), its background threads
    /// stop, it checkpoints, and the directory is free again.
    pub fn close(mut self) -> Result<(), String> {
        self.close_handle()
    }

    fn close_handle(&mut self) -> Result<(), String> {
        if std::mem::replace(&mut self.closed, true) {
            return Ok(());
        }
        let inner = &self.inner;
        let mut open = self.inner.key.as_ref().map(|_| lock(opened()));
        if inner.handles.fetch_sub(1, Ordering::SeqCst) != 1 {
            return Ok(());
        }
        if let (Some(open), Some(key)) = (open.as_mut(), &inner.key) {
            open.remove(key);
        }
        let Some(kv) = lock(&inner.kv).take() else {
            return Ok(());
        };
        drop(open);
        let sessions = std::mem::take(&mut *lock(&inner.sessions));
        for s in &sessions {
            s.pipes.close();
        }
        for s in sessions {
            let _ = s.thread.join();
        }
        inner.stop.stop();
        nucleus_sql::embedding::stop(&*kv);
        for t in std::mem::take(&mut *lock(&inner.background)) {
            let _ = t.join();
        }
        // The last reference: dropping it releases the directory.
        let result = kv.checkpoint().map_err(|e| e.to_string());
        drop(kv);
        result
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = self.close_handle();
    }
}

// ------------------------------------------------------------------ pipes

/// One direction of bytes between a connection and its session.
#[derive(Default)]
struct Pipe {
    state: Mutex<PipeState>,
    changed: Condvar,
    /// Bytes waiting, or anything once closed: what a reader spins on.
    ready: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct PipeState {
    data: VecDeque<u8>,
    closed: bool,
}

impl Pipe {
    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut s = lock(&self.state);
        while !s.closed && s.data.len() >= PIPE_BUFFER {
            s = self.changed.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        if s.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the connection is closed",
            ));
        }
        let n = buf.len().min(PIPE_BUFFER - s.data.len());
        s.data.extend(&buf[..n]);
        self.ready.store(true, Ordering::Release);
        self.changed.notify_all();
        Ok(n)
    }

    /// Bytes into `buf`: 0 once closed and drained; `TimedOut` after
    /// `timeout` without any.
    fn read(&self, buf: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let deadline = timeout.map(|t| Instant::now() + t);
        let spin = Instant::now();
        while !self.ready.load(Ordering::Acquire) && spin.elapsed() < SPIN {
            std::hint::spin_loop();
        }
        let mut s = lock(&self.state);
        while s.data.is_empty() && !s.closed {
            s = match deadline {
                None => self.changed.wait(s).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "no reply in time"));
                    }
                    self.changed
                        .wait_timeout(s, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
        let n = buf.len().min(s.data.len());
        let (a, b) = s.data.as_slices();
        let from_a = n.min(a.len());
        buf[..from_a].copy_from_slice(&a[..from_a]);
        buf[from_a..n].copy_from_slice(&b[..n - from_a]);
        s.data.drain(..n);
        self.ready
            .store(!s.data.is_empty() || s.closed, Ordering::Release);
        self.changed.notify_all();
        Ok(n)
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.ready.store(true, Ordering::Release);
        self.changed.notify_all();
    }
}

/// A connection's two pipes: to its session and back.
#[derive(Default)]
struct Pipes {
    to_server: Pipe,
    to_client: Pipe,
}

impl Pipes {
    fn close(&self) {
        self.to_server.close();
        self.to_client.close();
    }
}

/// The session's end: what the protocol's server reads and writes.
struct ServerEnd(Arc<Pipes>);

impl Read for ServerEnd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.to_server.read(buf, None)
    }
}

impl Write for ServerEnd {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.to_client.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ServerEnd {
    fn drop(&mut self) {
        // The session ended: the client reads what is left, then the end.
        self.0.close();
    }
}

/// A client's end of one session: the bytes of the nucleus protocol, as
/// a socket would carry them.  Dropping it ends the session.
pub struct Connection {
    pipes: Arc<Pipes>,
}

impl Connection {
    /// Sends all of `buf` (waiting while the session has a full pipe
    /// unread).
    pub fn send(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = self.pipes.to_server.write(buf)?;
            buf = &buf[n..];
        }
        Ok(())
    }

    /// What the session sent, into `buf`: waits for at least a byte (up to
    /// `timeout`, else `TimedOut`); 0 once the session has ended.
    pub fn recv(&self, buf: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        self.pipes.to_client.read(buf, timeout)
    }

    /// Ends the session (it finishes the statement it runs first).
    pub fn close(&self) {
        self.pipes.close();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv(buf, None)
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pipes.to_server.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
