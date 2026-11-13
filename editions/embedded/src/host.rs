//! The browser's pool of storage (PLAN.md §M14.23): files the host —
//! the edition's worker, `drivers/js/src/browser` — opened ahead in the
//! Origin Private File System, reached through its imports
//! (`nucleus_slots`), behind [`Slots`] for the platform's [`SlotFs`].
//!
//! [`SlotFs`]: nucleus_platform::slots::SlotFs

#![allow(unsafe_code)]

use std::io;

use nucleus_platform::slots::Slots;

#[link(wasm_import_module = "nucleus_slots")]
unsafe extern "C" {
    fn count() -> u32;
    /// Bytes read, or -1.
    fn read(slot: u32, buf: *mut u8, len: u32, offset: f64) -> i32;
    /// 0, or -1.
    fn write(slot: u32, buf: *const u8, len: u32, offset: f64) -> i32;
    /// The length, or -1.
    fn size(slot: u32) -> f64;
    fn truncate(slot: u32, len: f64) -> i32;
    fn flush(slot: u32) -> i32;
}

fn failed(what: &str, slot: usize) -> io::Error {
    io::Error::other(format!(
        "the browser's storage failed to {what} slot {slot}"
    ))
}

/// Offsets go as doubles (exact to 2^53, past any file here).
fn offset(o: u64) -> f64 {
    o as f64
}

/// The host's slots.
pub(crate) struct HostSlots;

impl Slots for HostSlots {
    fn count(&self) -> usize {
        // SAFETY: an import without arguments.
        unsafe { count() as usize }
    }

    fn read(&self, slot: usize, buf: &mut [u8], at: u64) -> io::Result<usize> {
        // In pieces a u32 holds (a wasm32 buffer always fits; for form).
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: `buf` is writable for `len` bytes.
        let n = unsafe { read(slot as u32, buf.as_mut_ptr(), len, offset(at)) };
        usize::try_from(n).map_err(|_| failed("read", slot))
    }

    fn write(&self, slot: usize, buf: &[u8], at: u64) -> io::Result<()> {
        let len = u32::try_from(buf.len()).map_err(|_| failed("write", slot))?;
        // SAFETY: `buf` is readable for `len` bytes.
        match unsafe { write(slot as u32, buf.as_ptr(), len, offset(at)) } {
            0 => Ok(()),
            _ => Err(failed("write", slot)),
        }
    }

    fn size(&self, slot: usize) -> io::Result<u64> {
        // SAFETY: an import of plain values.
        let n = unsafe { size(slot as u32) };
        if n < 0.0 {
            Err(failed("measure", slot))
        } else {
            Ok(n as u64)
        }
    }

    fn truncate(&self, slot: usize, len: u64) -> io::Result<()> {
        // SAFETY: an import of plain values.
        match unsafe { truncate(slot as u32, offset(len)) } {
            0 => Ok(()),
            _ => Err(failed("truncate", slot)),
        }
    }

    fn flush(&self, slot: usize) -> io::Result<()> {
        // SAFETY: an import of plain values.
        match unsafe { flush(slot as u32) } {
            0 => Ok(()),
            _ => Err(failed("flush", slot)),
        }
    }
}
