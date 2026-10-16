# nucleus on d-os

[d-os](https://github.com/davidsarosi92/d-os) runs the static musl builds of
nucleus through its Linux ABI.  This directory holds what is specific to it:

- `nucleus.dos-app` — the manifest d-os `app install` reads from the top
  directory of a release archive (`program=`, `link=`, `dir=`, `service=`
  lines; see d-os `kernel/core/apps.c`).

`scripts/package.sh <arch>-unknown-linux-musl` builds
`nucleus-<version>-d-os-<arch>.tar.gz` (`<arch>`: `x86_64`, `aarch64`,
`i386`), with a single top directory `nucleus-<version>/` — d-os reads the
version from that name — holding `bin/nucleus` and this manifest.

Install on d-os:

```
app install nucleus-<version>-d-os-<arch>.tar.gz
nucleus --version
```

PLAN.md §M6.5 (the d-os settings store) builds on this.
