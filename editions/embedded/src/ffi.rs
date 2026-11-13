//! The C API (`include/nucleus_embedded.h`): a database opened, sessions
//! connected to it, and the nucleus protocol's bytes sent and received —
//! what the Python, JavaScript, Swift and Kotlin packages build their
//! drivers on.
//!
//! Every function takes pointers from C; each says what they must be.  A
//! connection's functions are not to be called on it from two threads at
//! once (a driver's connection is one thread's at a time) — except
//! [`nce_end`], which stops another thread's [`nce_recv`]; different
//! connections, and the database, from any threads.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::time::Duration;

use crate::{Connection, Database};

/// A database (`nce_database *`).
pub struct NcDatabase(Database);

/// A connection (`nce_connection *`).
pub struct NcConnection(Connection);

/// Writes `message` where `error` points (when not null), for
/// [`nce_free_error`].
unsafe fn set_error(error: *mut *mut c_char, message: &str) {
    if !error.is_null() {
        let c = CString::new(message.replace('\0', " ")).unwrap_or_default();
        // SAFETY: the caller passes a writable `char **` or null.
        unsafe { *error = c.into_raw() };
    }
}

/// A C string as UTF-8; null as "".
unsafe fn text<'a>(s: *const c_char) -> Result<&'a str, String> {
    if s.is_null() {
        return Ok("");
    }
    // SAFETY: the caller passes a NUL-terminated string.
    unsafe { CStr::from_ptr(s) }
        .to_str()
        .map_err(|_| "not UTF-8".to_string())
}

fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|p| {
        Err(p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "the engine failed".into()))
    })
}

/// This library's version (a static string).
#[unsafe(no_mangle)]
pub extern "C" fn nce_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Opens the database in directory `path` (made if missing), or a new one
/// in memory for `":memory:"`; `options` (null for none) as
/// `"cache=64MB; sync=full"`.  Null on failure, with `*error` set (when
/// `error` is not null) for [`nce_free_error`].
///
/// # Safety
/// `path` and `options` are NUL-terminated strings or null; `error` is
/// null or points to a writable `char *`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_open(
    path: *const c_char,
    options: *const c_char,
    error: *mut *mut c_char,
) -> *mut NcDatabase {
    // SAFETY: as the caller promises.
    let r = guarded(|| {
        let path = unsafe { text(path) }.map_err(|e| format!("the path: {e}"))?;
        let options = unsafe { text(options) }.map_err(|e| format!("the options: {e}"))?;
        Database::open(path, options)
    });
    match r {
        Ok(db) => Box::into_raw(Box::new(NcDatabase(db))),
        Err(e) => {
            // SAFETY: as the caller promises.
            unsafe { set_error(error, &e) };
            ptr::null_mut()
        }
    }
}

/// A new session on `db`.  Null on failure (`*error` set as by
/// [`nce_open`]).
///
/// # Safety
/// `db` came from [`nce_open`] and is not closed; `error` as for
/// [`nce_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_connect(
    db: *mut NcDatabase,
    error: *mut *mut c_char,
) -> *mut NcConnection {
    if db.is_null() {
        // SAFETY: as the caller promises.
        unsafe { set_error(error, "no database") };
        return ptr::null_mut();
    }
    // SAFETY: a live database from `nce_open`.
    let db = unsafe { &*db };
    match guarded(|| db.0.connect()) {
        Ok(c) => Box::into_raw(Box::new(NcConnection(c))),
        Err(e) => {
            // SAFETY: as the caller promises.
            unsafe { set_error(error, &e) };
            ptr::null_mut()
        }
    }
}

/// Sends `len` bytes to the session: 0, or -1 once it has ended.
///
/// # Safety
/// `c` came from [`nce_connect`] and is not disconnected; `buf` holds `len`
/// readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_send(c: *mut NcConnection, buf: *const u8, len: usize) -> i32 {
    if c.is_null() || (buf.is_null() && len > 0) {
        return -1;
    }
    // SAFETY: as the caller promises.
    let (c, bytes) = unsafe {
        (
            &*c,
            if len == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(buf, len)
            },
        )
    };
    match c.0.send(bytes) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Receives what the session sent, up to `cap` bytes into `buf`, waiting
/// for at least one up to `timeout_ms` (negative: no limit).  The bytes
/// received; 0 once the session has ended; -1 when the time ran out.
///
/// # Safety
/// `c` came from [`nce_connect`] and is not disconnected; `buf` holds `cap`
/// writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_recv(
    c: *mut NcConnection,
    buf: *mut u8,
    cap: usize,
    timeout_ms: i64,
) -> i64 {
    if c.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    // SAFETY: as the caller promises.
    let (c, out) = unsafe { (&*c, std::slice::from_raw_parts_mut(buf, cap)) };
    let timeout = u64::try_from(timeout_ms).ok().map(Duration::from_millis);
    match c.0.recv(out, timeout) {
        Ok(n) => n as i64,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => -1,
        Err(_) => 0,
    }
}

/// Ends the session without freeing `c`: a thread waiting in [`nce_recv`]
/// on it returns (what is left, then 0) — so one thread can stop
/// another's reading before [`nce_disconnect`].
///
/// # Safety
/// `c` came from [`nce_connect`] and is not disconnected.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_end(c: *mut NcConnection) {
    if !c.is_null() {
        // SAFETY: as the caller promises.
        unsafe { &*c }.0.close();
    }
}

/// Ends the session and frees `c`.
///
/// # Safety
/// `c` came from [`nce_connect`] (or is null) and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_disconnect(c: *mut NcConnection) {
    if !c.is_null() {
        // SAFETY: as the caller promises.
        drop(unsafe { Box::from_raw(c) });
    }
}

/// Closes `db` and frees it: its sessions end (their connections then
/// receive the end), it checkpoints, and its directory is free (unless
/// another handle in the process has it open).  0, or -1 with `*error`
/// set.
///
/// # Safety
/// `db` came from [`nce_open`] (or is null) and is not used again; `error`
/// as for [`nce_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_close(db: *mut NcDatabase, error: *mut *mut c_char) -> i32 {
    if db.is_null() {
        return 0;
    }
    // SAFETY: as the caller promises.
    let db = unsafe { Box::from_raw(db) };
    match guarded(|| db.0.close()) {
        Ok(()) => 0,
        Err(e) => {
            // SAFETY: as the caller promises.
            unsafe { set_error(error, &e) };
            -1
        }
    }
}

/// Frees an error message from this library.
///
/// # Safety
/// `error` came from this library (or is null) and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_free_error(error: *mut c_char) {
    if !error.is_null() {
        // SAFETY: made by `CString::into_raw` in `set_error`.
        drop(unsafe { CString::from_raw(error) });
    }
}

// ------------------------------------------------- sessions without threads

/// A session driven by its bytes (`nce_stepped *`): its server's state
/// and its last reply.
pub struct NcExchange {
    exchange: nucleus_server::Exchange,
    reply: Vec<u8>,
}

/// A new session on `db` run on the caller's thread, for a platform
/// without threads (WebAssembly): [`nce_feed`] gives it bytes and returns
/// its reply.  Null on failure (`*error` set as by [`nce_open`]).
///
/// # Safety
/// As for [`nce_connect`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_exchange(
    db: *mut NcDatabase,
    error: *mut *mut c_char,
) -> *mut NcExchange {
    if db.is_null() {
        // SAFETY: as the caller promises.
        unsafe { set_error(error, "no database") };
        return ptr::null_mut();
    }
    // SAFETY: a live database from `nce_open`.
    let db = unsafe { &*db };
    match guarded(|| db.0.exchange()) {
        Ok(exchange) => Box::into_raw(Box::new(NcExchange {
            exchange,
            reply: Vec::new(),
        })),
        Err(e) => {
            // SAFETY: as the caller promises.
            unsafe { set_error(error, &e) };
            ptr::null_mut()
        }
    }
}

/// Gives the session `len` bytes and runs the whole messages among what
/// it has; returns its reply, `*reply_len` bytes, valid until the next
/// call on `x`.  A statement that would read past what was given ends
/// the session (`08P01`): send `COPY` data with its `COPY`, and fetch
/// results whole.
///
/// # Safety
/// `x` came from [`nce_exchange`] and is not freed; `buf` holds `len`
/// readable bytes; `reply_len` points to a writable `size_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_feed(
    x: *mut NcExchange,
    buf: *const u8,
    len: usize,
    reply_len: *mut usize,
) -> *const u8 {
    if x.is_null() || reply_len.is_null() || (buf.is_null() && len > 0) {
        return ptr::null();
    }
    // SAFETY: as the caller promises.
    let (x, bytes) = unsafe {
        (
            &mut *x,
            if len == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(buf, len)
            },
        )
    };
    x.reply = guarded(|| Ok(x.exchange.feed(bytes))).unwrap_or_default();
    // SAFETY: as the caller promises.
    unsafe { *reply_len = x.reply.len() };
    x.reply.as_ptr()
}

/// 1 once the session has ended (a Bye, a failed login, a protocol
/// error), else 0.
///
/// # Safety
/// `x` came from [`nce_exchange`] and is not freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_exchange_ended(x: *mut NcExchange) -> i32 {
    // SAFETY: as the caller promises.
    i32::from(x.is_null() || unsafe { &*x }.exchange.ended())
}

/// Frees the session `x`.
///
/// # Safety
/// `x` came from [`nce_exchange`] (or is null) and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_exchange_free(x: *mut NcExchange) {
    if !x.is_null() {
        // SAFETY: as the caller promises.
        drop(unsafe { Box::from_raw(x) });
    }
}

/// `len` bytes of this library's memory, for a host that must write into
/// it (WebAssembly's): null when there is not that much.
#[unsafe(no_mangle)]
pub extern "C" fn nce_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::new();
    if v.try_reserve_exact(len.max(1)).is_err() {
        return ptr::null_mut();
    }
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// Frees what [`nce_alloc`] gave.
///
/// # Safety
/// `p` came from `nce_alloc(len)` (or is null) and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nce_dealloc(p: *mut u8, len: usize) {
    if !p.is_null() {
        // SAFETY: made by `nce_alloc` with this capacity.
        drop(unsafe { Vec::from_raw_parts(p, 0, len.max(1)) });
    }
}

/// The slots of the browser's pool not yet taken (the host adds more when
/// they run low), or -1 where there is no pool (not WebAssembly, or none
/// opened yet).
#[unsafe(no_mangle)]
pub extern "C" fn nce_pool_free() -> i64 {
    crate::pool_free().map_or(-1, |n| n as i64)
}
