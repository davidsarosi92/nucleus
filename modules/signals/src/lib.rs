//! Stop requests for nucleus (issue #17): the process asked to stop —
//! `SIGTERM` (what `docker stop`, systemd, launchd and Kubernetes send),
//! `SIGINT` (Ctrl-C) or `SIGHUP` on Unix; Ctrl-C, Ctrl-Break or the
//! console closing on Windows — becomes a call on a thread of its own,
//! where the server can end its sessions and take a checkpoint, so the
//! next start replays nothing.
//!
//! The core has no signal handling (std has no signal API, and `unsafe`
//! is forbidden there): this module brings it, through the `ctrlc` crate,
//! whose unsafe code is its own.
//!
//! A second request while the first is being handled ends the process at
//! once (exit code 130, a shell's for an interrupt): an operator who asks
//! twice does not wait for the checkpoint.

use std::sync::atomic::{AtomicBool, Ordering};

/// The exit code of a process stopped by a second request.
pub const FORCED_EXIT: i32 = 130;

/// Calls `stop` on a thread of its own the first time the process is asked
/// to stop; a second request ends the process at once ([`FORCED_EXIT`]).
/// `stop` is expected to end the process itself (it is not called again).
/// Once per process: a second call is an error.
pub fn on_stop(stop: impl FnOnce() + Send + 'static) -> Result<(), String> {
    static ASKED: AtomicBool = AtomicBool::new(false);
    let mut stop = Some(stop);
    ctrlc::set_handler(move || {
        if ASKED.swap(true, Ordering::AcqRel) {
            std::process::exit(FORCED_EXIT);
        }
        // Off the handler's thread, so a second request is seen while
        // this one runs.
        if let Some(f) = stop.take() {
            let spawned = std::thread::Builder::new()
                .name("nucleus stop".into())
                .spawn(f);
            if spawned.is_err() {
                std::process::exit(FORCED_EXIT);
            }
        }
    })
    .map_err(|e| format!("stop requests: {e}"))
}
