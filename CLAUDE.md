# nucleus — notes for Claude

The plan, its north-star principles and the milestones are in `PLAN.md`;
the design as built is in `DOCS.md`.  The principles come first: a
feature that conflicts with one gives way.

## Where speed comes from: look at prior art, then think

When something has to be faster, look beyond this repository before
inventing:

- **Mathematical and numerical libraries** (BLAS / LAPACK, Eigen, numpy,
  libdivide, SIMD libraries, hashing and compression libraries) — their
  kernels, data layouts and tricks are a source of technique.
- **Algorithms and mathematical models.**  Many engine problems are
  optimization problems in disguise — join ordering, cost models,
  scheduling, memory and cache use, partitioning, compression — and
  have known formulations and solvers (dynamic programming, linear and
  integer programming, graph algorithms, probabilistic data structures,
  cache-oblivious and external-memory algorithms).  Name the problem,
  find its model.
- **Programming models** — vectorization, morsel-driven parallelism,
  expression templates, code generation, lock-free structures.
- **Other database engines** (DuckDB, ClickHouse, HyPer / Umbra, Velox,
  PostgreSQL, SQLite, RocksDB, CockroachDB, …) and their papers.

These are for ideas, not copying: understand why a technique works,
check it against nucleus's principles and its own design, measure it
here, and make it better where we can.  Record what was taken, from
where, and what was measured — or why it was not taken — in `PLAN.md`
beside the step (as §M14.20 V13 does).

## Credit what is taken: ideas and code

**Ideas** — an algorithm, a data layout, a query form's meaning, a
paper's technique — are written fresh, in nucleus's own code.  Credit
them where the step is recorded (`PLAN.md`, as above), where the feature
is described (`DOCS.md`), in the commit message, and in
`ACKNOWLEDGEMENTS.md` (one line: the source, what was taken, where).

**Code** is not copied — not translated into Rust either: a translation
is a derived work of the original.  Where code must come from elsewhere
(agreed with the user first), it comes only under a licence compatible
with AGPL-3.0 (MIT, BSD, Apache-2.0, zlib, ISC — or MIT-compatible for
the drivers, client and the other MIT parts), and:

- the file keeps the original's copyright line and licence (an
  `SPDX-License-Identifier` header), and says where it came from (project,
  version or commit, path);
- `THIRD_PARTY.md` lists it — file, source, licence — and carries the
  licence text; Apache-2.0 sources' `NOTICE` text is kept there too;
  `COPYRIGHT`'s "Third-party material" names it as well;
- the commit message names it.

Reading another project's documentation and tests to learn what a
feature means is fine; checking answers against another system (as
against PostgreSQL) is fine; reading its source to write ours is the
grey zone — prefer its documentation and papers, and when its source
was read, say so in `PLAN.md`.

## Before a push

Run `scripts/precheck.sh` (about 5 minutes; `--quick` skips the wasm
tests): it checks what CI checks on the platforms this Mac does not run
(Windows, 32-bit Linux, WASI, Android, iOS) and runs the wasm job's
tests.  Most CI failures were caught by nothing else locally.
