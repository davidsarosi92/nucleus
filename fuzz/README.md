# Fuzzing nucleus

Fuzz targets for every input that crosses a trust boundary (PLAN.md
§M14.29 S2): what a client, an application or a file can hand nucleus.
An error is an answer; a panic, a hang, a crash, a memory error or an
allocation past the limit is a finding.

| Target | What it is given |
|---|---|
| `sql_parse` | SQL text: the lexer and the parser (and what parsed parses again the same) |
| `values` | a value as text, into each type (`'…'::type`), and back out |
| `json` | JSON text into the binary form and back (text, indented, key), and bytes read as the form |
| `text_search` | text-search queries (nucleus's `@@`, PostgreSQL's `tsquery`/`tsvector`), highlighting, the chunkers |
| `copy` | `COPY … FROM STDIN`'s data, text format and CSV with options, into a table with keys, checks and indexes |
| `ncp` | a client's bytes to the nucleus protocol: login (none, or SCRAM / OAUTHBEARER), queries, parameters, `COPY`, columnar loads |
| `pg_wire` | a client's bytes to PostgreSQL's protocol: startup, simple and extended queries, `COPY`, SASL |
| `scram` | SCRAM-SHA-256 messages, to the server and to the client; a real client's exchange accepted exactly when its password is right |
| `jwt` | OIDC identity tokens before they are trusted, the claims of signed ones, the issuer's key set |
| `disk` | a real database's files (pages, WAL) patched — checksums sealed again, or not — then opened, checked and read |
| `model_files` | a local model's `tokenizer.json`, `config.json` with `model.safetensors` (the encoder built and run), pooling |

## Running

The targets need a nightly toolchain and `cargo-fuzz`:

```sh
rustup toolchain install nightly --profile minimal
cargo +nightly install cargo-fuzz --locked
cd fuzz
cargo +nightly fuzz run -O ncp corpus/ncp seeds/ncp -- -max_total_time=300 -rss_limit_mb=2048 -timeout=10
```

`corpus/<target>` is where the fuzzer keeps what it found new (not in
the repository); `seeds/<target>` is the start, kept in the repository.
A finding is written to `artifacts/<target>/`; run it again with
`cargo +nightly fuzz run -O <target> artifacts/<target>/<file>`, and
shrink it with `cargo +nightly fuzz tmin -O <target> <file>`.

CI runs every target for three minutes on each change, from the seeds and
the corpus earlier runs grew (`.github/workflows/ci.yml`, job `fuzz`).

## Seeds

`seeds.py` makes them: the sqllogictest files' statements, well-formed
inputs for each statement or type a target picks by its first byte, a
SCRAM exchange with its proof, signed-token claims, tiny model files, and
— given `--server target/release/nucleus` — what nucleus's Python driver
and psycopg send, recorded through a proxy.  Run it again when a target's
statements or a protocol change:

```sh
target/ann-data/.venv/bin/python fuzz/seeds.py --server target/release/nucleus
```

## How the targets reach deep

- The server's front ends take any byte stream
  (`nucleus_server::handle_native_stream`, `nucleus_pg_compat::handle_stream`),
  so a target serves its bytes as one connection, with no socket.
- The database is in memory (the simulation's file system) under the
  machine's clock, with `statement_timeout` at 250 ms, so a statement
  that asks for a billion rows ends.
- `disk` seals each patched page and WAL record with its checksum again
  (its first byte's low bit off): a checksum stops random damage at the
  first read, so the decoders behind it see only damage it cannot catch
  — a bug that wrote a wrong page, a misdirected or stale write.
- `jwt` signs the fuzzer's header and claims with the issuer's key, so
  the claims are checked as a real issuer's would be.
