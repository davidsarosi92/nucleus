# nucleus on d-os

[d-os](https://github.com/davidsarosi92/d-os) runs the static musl builds of
nucleus through its Linux ABI.  This directory holds what is specific to it.

## Install

`scripts/package.sh <arch>-unknown-linux-musl` builds
`nucleus-<version>-d-os-<arch>.tar.gz` (`<arch>`: `x86_64`, `aarch64`,
`i386`): one top directory `nucleus-<version>/` — d-os reads the version from
that name — holding `bin/nucleus`, `bin/nucleus-settings` and the
`.dos-app` manifest (`nucleus.dos-app` here; keys as in d-os
`kernel/core/apps.c`).

```
app install nucleus-<version>-d-os-<arch>.tar.gz
nucleus --version
```

## The settings store (PLAN.md §M6.5)

`nucleus-settings <dir>` is the persistence behind the d-os kernel's
settings cache.  The kernel keeps its defaults, cache and watchers; the
service stores the values.  It speaks one command per line on stdin and
answers on stdout, so any d-os IPC that carries lines (a pipe will do) can
drive it.  It first prints `READY nucleus-settings <version>`.

A *layer* is `machine` or `user/<uid>`; keys are d-os setting names
(`gui.wallpaper`), values the rest of the line.

| Command | Reply |
|---------|-------|
| `GET <layer> <key>` | `OK <value>` or `NONE` (committed values only) |
| `SET <layer> <key> <value>` | `OK` — staged |
| `UNSET <layer> <key>` | `OK` — staged |
| `COMMIT` | `OK <ts>` — every staged change in one transaction |
| `DISCARD` | `OK` — drop staged changes |
| `SNAPSHOT` | `OK <ts>` — the current point in history |
| `DUMP <layer> [AT <ts>]` | `<key> = <value>` lines, then `END` |
| `LAYERS` | layer names, then `END` |
| `IMPORT <layer> <path>` | `OK <n>` — stage a `d-os.conf` file |
| `EXPORT <layer> <path>` | `OK` — write the layer as a `d-os.conf` file |
| `QUIT` | `BYE` |

Errors are `ERR <message>`.  Staged changes not committed when the service
exits are discarded.  A `COMMIT` is atomic and durable when `OK` arrives: a
crash leaves the complete old or the complete new set of a save (tested at
every I/O point in `tests/settings.rs`).  Old versions are kept for 10 000
commits, so `DUMP <layer> AT <ts>` reads earlier settings back.

Storage layout: `cfg/machine/<key>`, `cfg/user/<uid>/<key>`.

### Switching d-os over

On the nucleus side the service, its protocol and its packaging are done.
What remains is on the d-os side (tracked in the d-os PLAN): start the
service, overlay its values on the kernel cache where
`config_attach_persistent` overlays `d-os.conf` today, send `SET` … `COMMIT`
from `config_save`, run `IMPORT machine <volume>/d-os.conf` once, and verify
on i386, x86_64 and aarch64 (PLAN.md §M6.5 "done when").
