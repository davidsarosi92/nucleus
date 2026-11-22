# nucleus-signals

Stop requests for [nucleus](https://github.com/davidsarosi92/nucleus)
(issue #17): `SIGTERM` — what `docker stop`, systemd, launchd and
Kubernetes send —, `SIGINT` (Ctrl-C) and `SIGHUP` on Unix; Ctrl-C,
Ctrl-Break or a console closing on Windows.

```rust
nucleus_signals::on_stop(move || shut_down(&db))?;
```

- **`on_stop(f)`**: calls `f` on a thread of its own the first time the
  process is asked to stop; `f` ends the process itself.  `nucleus
  server` ends its sessions (`57P01`), takes a checkpoint and exits 0,
  so its next start replays nothing (DOCS.md §9, "Stopping").
- **A second request** ends the process at once, exit code 130.

The core has no signal handling — std has no signal API and `unsafe` is
forbidden there — so this module brings it, through the
[`ctrlc`](https://crates.io/crates/ctrlc) crate, whose unsafe code is its
own.

Licence: AGPL-3.0-only (see `LICENSE` and the repository's `COPYRIGHT`).
