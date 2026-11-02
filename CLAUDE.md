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
