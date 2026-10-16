# nucleus

A layered, multi-model database written in Rust from scratch.  Its core is a
transactional, ordered key-value engine; SQL and document models are built on
top of it, and a distributed implementation of the same KV contract comes
later — without changing the layers above.

**Status:** the core exists (PLAN.md §F0): a crash-safe copy-on-write
B+tree with a write-ahead log, MVCC transactions with snapshot isolation, the
KV contract and a CLI.  SQL is next.  See [PLAN.md](PLAN.md) for the roadmap
and [DOCS.md](DOCS.md) for what exists today.

## Using it

```sh
nucleus kv ./mydb put greeting "hello world"
nucleus kv ./mydb get greeting
nucleus kv ./mydb            # interactive shell: begin, put, get, scan, commit, …
```

As a Rust library: `nucleus_kv::Database::open(dir, Options::default())`,
then `begin` / `get` / `scan` / `put` / `commit` (see DOCS.md §6).

## Building

```sh
cargo test --workspace
```

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it on
first use.  nucleus builds for Linux, macOS, Windows and
[d-os](https://github.com/davidsarosi92/d-os) (PLAN.md, "Build targets").

## License

MIT — see [LICENSE](LICENSE).
