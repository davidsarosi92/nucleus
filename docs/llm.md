# nucleus SQL — a reference for models

Compact and exact: what nucleus's SQL accepts today, what it does not, and
what to use instead.  For people the full reference is DOCS.md §8.

## Connecting

- **MCP:** `nucleus mcp <dir>` (or `nucleus mcp -h host:port -U user`) —
  tools `list_tables`, `describe_table`, `sample_rows`, `query` (read-only,
  `$1`… in `params`), `explain`, `lint`; `execute` with `--allow-writes`.
- **Shell:** `ncsql <dir>` or `ncsql -h 127.0.0.1:20035` (user and password
  `nucleus` by default); `-c "<sql>"` runs one command.
- **Drivers:** Rust, Python, JavaScript, Java, C, C++ over nucleus's protocol
  `ncp` (port 20035); PostgreSQL clients through the pg-compat front end
  (`nucleus server --pg-compat`, port 5432).  Large results: a cursor
  (`conn.stream(sql)` in Python and JavaScript, `cursor` elsewhere) reads
  them a batch at a time; for analysis read columns, not rows — Python
  `conn.pandas(sql)`, `conn.polars(sql)`, `conn.arrow(sql)`,
  `conn.numpy(sql)`; `batches` in every driver.

## Dialect

PostgreSQL-like.  Identifiers fold to lower case unless `"quoted"`; strings
in `'single quotes'` (`''` for a quote, `E'\n'` for escapes; no `$$`).
Parameters `$1`, `$2` … .  Statements end with `;`.

**Types:** `boolean`, `smallint`, `integer` (`int`), `bigint`, `real`,
`double precision` (`float`), `numeric(p, s)` (exact, ≤ 38 digits), `text`,
`varchar(n)`, `char(n)`, `bytea`, `date`, `time`, `timestamp`,
`timestamptz`, `interval`, `uuid`, `inet`, `cidr`, arrays `type[]` (one
dimension), enums (`CREATE TYPE mood AS ENUM ('sad', 'ok')`).  An integer
literal is `integer` if it fits, `1.5` is `numeric`.  Sessions are in UTC.

**Tables:** `CREATE TABLE [IF NOT EXISTS] t (id bigint PRIMARY KEY, name
text NOT NULL DEFAULT '', email text UNIQUE, score int CHECK (score >= 0),
owner_id bigint REFERENCES owners (id) ON DELETE CASCADE)`; table
constraints `PRIMARY KEY (a, b)`, `UNIQUE (…)`, `CHECK (…)`, `FOREIGN KEY
(…) REFERENCES t (…)`.  `CREATE [UNIQUE] INDEX [name] ON t (a, b)`, `DROP
INDEX`, `DROP TABLE [IF EXISTS] t [CASCADE]`.  `ALTER TABLE t ADD [COLUMN]
…`, `ADD CONSTRAINT …`, `DROP CONSTRAINT …`, `ALTER COLUMN c TYPE …`.
`COMMENT ON TABLE t IS '…'`, `COMMENT ON COLUMN t.c IS '…'` — read them
(`describe_table`, `nc_catalog.tables.comment`, `nc_catalog.columns.comment`)
to learn what the data means.

**Writes:** `INSERT INTO t [(cols)] VALUES (…), (…) | SELECT … | DEFAULT
VALUES [RETURNING …]`; `UPDATE t SET a = … [WHERE …] [RETURNING …]`;
`DELETE FROM t [WHERE …] [RETURNING …]`; bulk: `COPY t [(cols)] FROM
STDIN [WITH (FORMAT csv, HEADER)]` with the data sent by the client.

**Queries:** `WITH [RECURSIVE] …`, `SELECT [DISTINCT]`, `FROM` with comma
joins and `[INNER|LEFT|RIGHT|FULL|CROSS] JOIN … ON | USING`, subqueries in
`FROM`, `generate_series(a, b [, step])` and `unnest(array) [WITH
ORDINALITY]` in `FROM`, `WHERE`, `GROUP BY`, `HAVING`, `UNION | EXCEPT |
INTERSECT [ALL]`, `ORDER BY … [ASC|DESC] [NULLS FIRST|LAST]`, `LIMIT`,
`OFFSET`.  Expressions: `IS [NOT] DISTINCT FROM`, `BETWEEN`, `IN (list |
subquery)`, `EXISTS`, `= ANY (array | subquery)`, `LIKE`, `ILIKE`, `~`
(regular expressions), `CASE`, `CAST(x AS t)` / `x::t`, `ARRAY[…]`,
`a[i]` (1-based).

**Functions:** `lower upper length trim substr replace strpos concat
regexp_replace coalesce nullif greatest least abs round trunc floor ceil
sign mod sqrt power now current_date clock_timestamp extract date_part
date_trunc make_date make_time age to_timestamp justify_days
array_length array_append array_to_string cardinality gen_random_uuid
uuidv7 encode decode sha256 md5 host network masklen current_user
current_setting`; aggregates `count sum avg min max string_agg array_agg
bool_and bool_or` (with `DISTINCT`, and `ORDER BY` inside the call:
`string_agg(x, ',' ORDER BY x)`).

**Memory:** `SET work_mem = '256MB'` gives each sort and materialised
result more memory before it spills to disk (default 64MB).  Scans,
filters and aggregates run in batches (`batch_size`, default 1024; 0 runs
row by row) and on all CPUs (`parallel_workers`, default 0 = automatic; 1
= one thread) — same results either way.

**Transactions:** every statement is its own transaction unless in `BEGIN …
COMMIT | ROLLBACK`.  Snapshot isolation (`SERIALIZABLE` on request).  On
`40001` (a conflict with another transaction) run the transaction again;
after an error in a block everything is `25P02` until `ROLLBACK`.

## Not available — use instead

| Not there | Instead |
|-----------|---------|
| `SERIAL`, `GENERATED … AS IDENTITY`, sequences | `id uuid DEFAULT uuidv7() PRIMARY KEY` (increasing), or ids from the application |
| `INSERT … ON CONFLICT` (upsert) | `UPDATE … WHERE key = …`, then `INSERT` if it changed 0 rows, in one transaction |
| `CREATE VIEW` | a `WITH` query |
| window functions (`OVER (…)`) | a self-join or a correlated subquery; `GROUP BY` |
| `count(*) FILTER (WHERE …)` | `sum(CASE WHEN … THEN 1 ELSE 0 END)` |
| `LATERAL` | a correlated subquery in the select list |
| `unnest(…)` in the select list | `SELECT … FROM unnest(…) AS u(x)` |
| `json` / `jsonb` | text columns, parsed by the application |
| `TRUNCATE` | `DELETE FROM t` |
| `ALTER TABLE … DROP COLUMN`, `RENAME`, `ALTER COLUMN … SET DEFAULT` | a new table and `INSERT … SELECT` |
| `SELECT … FOR UPDATE` | a `SERIALIZABLE` transaction |
| `COPY … TO`, `COPY … FROM 'file'` | a query (`batches` in the drivers); `COPY t FROM STDIN` with the data from the client (a driver's copy, `ncsql \copy`) |
| `$$` strings, user-defined functions, `TABLESAMPLE`, time zones other than UTC | — |

## Finding your way

- `nc_catalog.tables` (name, comment, analyzed_rows, owner, …),
  `nc_catalog.columns` (table_name, name, type, nullable, default_expr,
  comment), `nc_catalog.indexes`, `nc_catalog.foreign_keys`,
  `nc_catalog.constraints`, `nc_catalog.types`, `nc_catalog.enums`,
  `nc_catalog.roles`, `nc_catalog.grants`, `nc_catalog.statistics`.
- **Errors carry a hint:** `column "emial" does not exist (42703)` comes
  with `HINT: Perhaps you meant column "email".`; unknown tables, functions
  and wrong argument counts likewise; a type mismatch names the cast.  Read
  the hint before retrying.
- `EXPLAIN <query>` shows the plan; `EXPLAIN ANALYZE <query>` runs it and
  shows each step's actual rows beside the estimate (run `ANALYZE` when they
  differ a lot).
- `ANALYZE QUERY <query | UPDATE | DELETE>` lints a statement without
  running it: full scans an index would serve (with the `CREATE INDEX`),
  indexed columns hidden in functions, foreign keys without an index,
  cartesian products, `NOT IN` over a nullable column.
- `nc_catalog.statements`: the statements run, normalized (constants as
  `$n`), with calls, times and rows; `nc_catalog.index_advice`: the
  indexes they would have used.

## Common error codes

`42703` unknown column · `42P01` unknown table · `42883` unknown function
or no such operator for these types (cast) · `42601` syntax · `42804` type
mismatch · `22P02` bad input for a type · `22003` out of range · `22012`
division by zero · `23502` NULL in NOT NULL · `23505` duplicate key ·
`23503` foreign key · `23514` CHECK · `42501` no privilege · `25P02` failed
transaction (ROLLBACK) · `25006` read-only · `40001` conflict (retry) ·
`57014` statement timeout · `0A000` not supported.
