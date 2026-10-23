# nucleus-mcp

A [Model Context Protocol](https://modelcontextprotocol.io) server for
[nucleus](https://github.com/davidsarosi92/nucleus): an AI model, through
any MCP client (an assistant, an IDE), picks up a database in seconds —
lists and describes its tables (with their comments, `COMMENT ON`), samples
rows, runs read-only queries, explains and lints them, and, when allowed,
writes (nucleus PLAN.md §M14.10).

The protocol only — JSON-RPC 2.0 over a line-delimited stream (MCP's stdio
transport; protocol versions 2025-06-18, 2025-03-26 and 2024-11-05) — with
the database behind a trait, `Database`, which runs SQL text.  The `nucleus`
edition gives it a local directory or a server: `nucleus mcp`.

| Tool | Does |
|------|------|
| `list_tables` | the tables: name, comment, analyzed row count |
| `describe_table` | columns (type, nullable, default, comment), primary key, indexes, foreign keys, referencing tables |
| `sample_rows` | a few rows of a table |
| `query` | read-only SQL, `$1`… from `params`; rows (at most `max_rows`, the rest counted) |
| `explain` | the plan; with `analyze`, run (read-only) with actual rows and times |
| `lint` | what in a statement is likely slow or wrong, with the fix (`ANALYZE QUERY`) |
| `execute` | SQL that writes — only with `Options::allow_writes` |

Reads are safe: their SQL goes to the database marked read-only — the
`Database` refuses anything but reads before it runs (`nucleus mcp` checks
every statement with nucleus's parser), so `COMMIT; DELETE …` cannot slip
out — and runs in a `BEGIN READ ONLY` transaction rolled back after it,
each statement within `statement_timeout`.  Errors come back as the tool's
(`isError`), with nucleus's hint: `HINT: Perhaps you meant column
"email".`  Results are JSON (`structuredContent`, and the same as text):
booleans and integers as JSON's, `numeric` as a string.

```rust
let mut db = MyDatabase::new();   // impl nucleus_mcp::Database
let mut server = nucleus_mcp::Server::new(&mut db, nucleus_mcp::Options::default());
server.serve(std::io::stdin().lock(), std::io::stdout().lock())?;
```

A client's configuration, for a local database:

```json
{ "mcpServers": { "nucleus": { "command": "nucleus", "args": ["mcp", "/path/to/db"] } } }
```

Tests: `tests/server.rs` against a database of its own; the edition's
`tests/cli.rs` runs `nucleus mcp` end to end.
