# A sample nucleus module: `geo`

This crate adds a **type**, a **function** and a **catalog view** to
nucleus without touching the core — through the extension API alone,
`nucleus_sql::ext` (nucleus PLAN.md §M14.19, principle 12).  It is also
the guide for writing a module.

```sh
cargo run -p nucleus-example-extension --bin geo-demo -- ./geodb        # embedded
cargo run -p nucleus-example-extension --bin geo-demo -- serve ./geodb  # a server with it
cargo test -p nucleus-example-extension
```

## Writing a module

A module is a value implementing `nucleus_sql::ext::Module`: a name and a
`register(&mut Registry)` that adds what the module brings.  A session runs
against one registry, so whoever builds the program decides which modules
it has:

```rust
let mut registry = Registry::builtin();      // the built-in module
registry.add(&nucleus_example_extension::Geo)?;
let session = Session::with_registry(kv, Arc::new(registry));
// a server: Config::default().with_registry(registry), then
// nucleus_server::serve_front_end(Arc::new(Ncp), listener, kv, config)
```

What a module can register (`Registry::add_*`):

| | Interface | Here |
|-|-----------|------|
| a type | `Type`: names, text input/output, and each ability it has — `Equality`, `Hashing`, `TotalOrder`, `KeyEncoding`, `Distance` — an encoding version and `upgrade`, `examples()` | `geopoint` |
| a scalar function | `ScalarFunction`: arity, bind-time `resolve` (argument handling, result type), strictness, `volatility`, `call` | `geohash` |
| an aggregate | `Aggregate` + `Accumulator` | — |
| a table function | `TableFunction` | — |
| a schema and catalog views | `add_schema`, `CatalogView` | `geo.cities` |
| an operator outside the grammar's own | `add_operator(symbol, left, right, function)` | (`<->` comes from the distance) |
| a front end | `nucleus_server::FrontEnd` (in the server) | — |

Rules the engine holds a module to:

- **Values of a registered type are canonical bytes** (`Value::ext(type,
  bytes)`): equal values have equal bytes, because equality and hashing
  are on them (`CanonicalBytes`).  `geopoint` stores `-0` as `0`.
- **Abilities are promises, checked.**  Registration runs the laws on the
  type's `examples()` and refuses a type that breaks one; test with
  `nucleus_testkit::laws::check` on generated values (as `tests/geo.rs`
  does).  The engine refuses, at bind or DDL time and naming the ability,
  what a type lacks: `geopoint` has no order, so `ORDER BY at` and a
  primary key on it are errors, and `ORDER BY at <-> '(47.5,19.04)'`
  works through its distance.
- **A function that may define an ordering or an index must be
  `Volatility::Immutable`** (the default is `Volatile`).
- **The stored form is versioned**: raise `encoding_version()` and say how
  to read the older one in `upgrade`; nucleus reads old values as they
  come and rewrites them on `ALTER TYPE … UPGRADE`.
- **Public API only**: `nucleus_sql::ext` (it re-exports values,
  sessions, statements and the read-only catalog), `nucleus-kv`,
  `nucleus-server`.  `scripts/check-boundaries.py` checks it, as for the
  modules under `modules/`.

What SQL can then teach the type, with no code: an ordering by a mapping
(`ALTER TYPE geopoint ADD ORDERING by_geohash BY geohash(value)`, then
`ORDER BY at USING by_geohash` and `CREATE INDEX ON shops (at USING
by_geohash)`), operators and casts (`ADD OPERATOR`, `ADD CAST`) — see
nucleus DOCS.md §8.11.

## Licence

MIT — see [LICENSE](LICENSE).
