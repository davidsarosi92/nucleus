//! The C API (`include/nucleus_embedded.h`): a database opened, sessions
//! connected to it, and the nucleus protocol's bytes sent and received —
//! what the Python, JavaScript, Swift and Kotlin packages build their
//! drivers on.
//!
//! Every function takes pointers from C; each says what they must be.  A
//! connection's functions are not to be called on it from two threads at
//! once (a driver's connection is one thread's at a time) — except
//! [`nc_end`], which stops another thread's [`nc_recv`]; different
//! connections, and the database, from any threads.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::time::Duration;

use crate::{Connection, Database};

/// A database (`nc_database *`).
pub struct NcDatabase(Database);

/// A connection (`nc_connection *`).
pub struct NcConnection(Connection);

/// Writes `message` where `error` points (when not null), for
/// [`nc_free_error`].
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
pub extern "C" fn nc_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Opens the database in directory `path` (made if missing), or a new one
/// in memory for `":memory:"`; `options` (null for none) as
/// `"cache=64MB; sync=full"`.  Null on failure, with `*error` set (when
/// `error` is not null) for [`nc_free_error`].
///
/// # Safety
/// `path` and `options` are NUL-terminated strings or null; `error` is
/// null or points to a writable `char *`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nc_open(
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
/// [`nc_open`]).
///
/// # Safety
/// `db` came from [`nc_open`] and is not closed; `error` as for
/// [`nc_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nc_connect(
    db: *mut NcDatabase,
    error: *mut *mut c_char,
) -> *mut NcConnection {
    if db.is_null() {
        // SAFETY: as the caller promises.
        unsafe { set_error(error, "no database") };
        return ptr::null_mut();
    }
    // SAFETY: a live database from `nc_open`.
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
/// `c` came from [`nc_connect`] and is not disconnected; `buf` holds `len`
/// readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nc_send(c: *mut NcConnection, buf: *const u8, len: usize) -> i32 {
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
/// `c` came from [`nc_connect`] and is not disconnected; `buf` holds `cap`
/// writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nc_recv(
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

/// Ends the session without freeing `c`: a thread waiting in [`nc_recv`]
/// on it returns (what is left, then 0) — so one thread can stop
/// another's reading before [`nc_disconnect`].
///
/// # Safety
/// `c` came from [`nc_connect`] and is not disconnected.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nc_end(c: *mut NcConnection) {
    if !c.is_null() {
        // SAFETY: as the caller promises.
        unsafe { &*c }.0.close();
    }
}

/// Ends the session and frees `c`.
///
/// # Safety
/// `c` came from [`nc_connect`] (or is null) and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nc_disconnect(c: *mut NcConnection) {
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
/// `db` came from [`nc_open`] (or is null) and is not used again; `error`
/// as for [`nc_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nc_close(db: *mut NcDatabase, error: *mut *mut c_char) -> i32 {
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
pub unsafe extern "C" fn nc_free_error(error: *mut c_char) {
    if !error.is_null() {
        // SAFETY: made by `CString::into_raw` in `set_error`.
        drop(unsafe { CString::from_raw(error) });
    }
}
