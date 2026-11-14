# nucleus SQL — a reference for models

Compact and exact: what nucleus's SQL accepts today, what it does not, and
what to use instead.  For people the full reference is DOCS.md §8.

## Connecting

- **MCP:** `nucleus mcp <dir>` (or `nucleus mcp -h host:port -U user`) —
  tools `list_tables`, `describe_table`, `sample_rows`, `summarize_table`
  (each column's min, max, distinct count, mean, quartiles, NULLs), `query` (read-only,
  `$1`… in `params`), `explain`, `lint`; `execute` with `--allow-writes`;
  `remember`, `recall`, `forget`, `cite` (memory stores) with `--memory`;
  `forget_subject` with `--allow-erasure`.  Results are JSON: numbers and
  booleans as JSON's, a `json` column's value as the JSON itself (as text
  only where a number would lose digits).  A walk-through: `docs/memory.md`.
- **Shell:** `ncsql <dir>` or `ncsql -h 127.0.0.1:20035` (user and password
  `nucleus` by default); `-c "<sql>"` runs one command.
- **Drivers:** Rust, Python, JavaScript, Java, C, C++, Swift over nucleus's protocol
  `ncp` (port 20035); PostgreSQL clients through the pg-compat front end
  (`nucleus server --pg-compat`, port 5432).  Large results: a cursor
  (`conn.stream(sql)` in Python and JavaScript, `cursor` elsewhere) reads
  them a batch at a time; for analysis read columns, not rows — Python
  `conn.pandas(sql)`, `conn.polars(sql)`, `conn.arrow(sql)`,
  `conn.numpy(sql)`; `batches` in every driver.
- **Without a server** (Python, JavaScript): `nucleus.connect("file:app.db")`
  or `connect(":memory:")` opens a database inside the program with the same
  API; `connect("nucleus://user:password@host:20035/db")` reaches a server —
  only the string changes.  No login in the process.  In a browser the
  same `connect("file:app.db")` (package `nucleus-db`, its browser entry)
  runs the engine in WebAssembly, kept in OPFS across reloads; there a
  statement runs through (a stream's result is produced whole, at most
  256 MB) and there is no server to reach.  On iOS the Swift package's
  `NucleusEmbedded` (`Database.open(named: "app").connect()`), on Android
  the Kotlin library (`Database.open(context, "app").connect()`, the Java
  driver's `NucleusClient`): the same API in an app.

## Dialect

PostgreSQL-like.  Identifiers fold to lower case unless `"quoted"`; strings
in `'single quotes'` (`''` for a quote, `E'\n'` for escapes; no `$$`).
Parameters `$1`, `$2` … .  Statements end with `;`.

**Types:** `boolean`, `smallint`, `integer` (`int`), `bigint`, `real`,
`double precision` (`float`), `numeric(p, s)` (exact, ≤ 38 digits), `text`,
`varchar(n)`, `char(n)`, `bytea`, `date`, `time`, `timestamp`,
`timestamptz`, `interval`, `uuid`, `inet`, `cidr`, `json` (`jsonb` too:
one binary type, keys sorted as `jsonb` sorts them, `1.50` read back as
`1.5`), `vector(n)`, arrays
`type[]` (one dimension), enums (`CREATE TYPE mood AS ENUM ('sad', 'ok')`).
An integer literal is `integer` if it fits, `1.5` is `numeric`.  Sessions
are in UTC.

**JSON** (as PostgreSQL's `jsonb`): `doc -> 'k'` (json), `doc ->> 'k'`
(text), `doc -> 0`, `doc #> '{a,0}'`, `doc #>> '{a,b}'`, `doc @> '{"k":
1}'`, `doc ? 'k'`, `?|` / `?&` with `text[]`, `a || b`, `doc - 'k'`,
`doc #- '{a,b}'`; `json_build_object('k', v, …)`, `json_build_array`,
`to_json(x)`, `json_set(doc, '{a,b}', '2')`, `json_insert`,
`json_strip_nulls`, `json_typeof`, `json_array_length`, `json_pretty`,
aggregates `json_agg(x)` and `json_object_agg(k, v)`, and in `FROM`
`json_each(doc)` (`key`, `value`), `json_array_elements(doc)`,
`json_object_keys(doc)` (`_text` forms too).  Every `jsonb_` name works.
**SQL/JSON paths** (PostgreSQL's `jsonpath`): `doc @? '$.items[*] ?
(@.qty > 1)'` (exists), `doc @@ '$.age > 40'` (a predicate's value),
`json_path_query_array(doc, '$.items[*].sku')`,
`json_path_query_first(doc, path [, vars [, silent]])`,
`json_path_exists`, `json_path_match`, in `FROM` `json_path_query(doc,
path)`; `lax` (default) or `strict`, filters, `like_regex`, `starts
with $var`, `.size() .type() .double() .datetime() .keyvalue()` and the
other methods; `_tz` forms for zone-dependent datetime comparisons.
**SQL/JSON's functions** (the standard's, as PostgreSQL 17+):
`JSON_VALUE(doc, '$.user.age' RETURNING int DEFAULT 0 ON EMPTY NULL ON
ERROR)`, `JSON_QUERY(doc, '$.items[*].sku' WITH WRAPPER)`,
`JSON_EXISTS(doc, '$.tags[*] ? (@ == $t)' PASSING 'ai' AS t)`, `body IS
JSON OBJECT WITH UNIQUE KEYS`; constructors `JSON_OBJECT('id': id,
'name': name ABSENT ON NULL)`, `JSON_ARRAY(1, 'x')`, `JSON_ARRAY(SELECT
id FROM t)`, `JSON_OBJECTAGG(k: v)`, `JSON_ARRAYAGG(x ORDER BY x)`,
`JSON(text WITH UNIQUE KEYS)`, `JSON_SCALAR(x)`, `JSON_SERIALIZE(doc)`.
Rows from documents: `SELECT o.id, i.* FROM orders o, JSON_TABLE(o.doc,
'$.items[*]' COLUMNS (n FOR ORDINALITY, sku text, qty int PATH '$.qty'
DEFAULT 0 ON ERROR, tags jsonb PATH '$.tags', NESTED PATH '$.parts[*]'
COLUMNS (part text PATH '$.name'))) i`.
**JSON Schema** (2020-12, the schemas a model's tools and structured
outputs take): `json_matches_schema(doc, schema)` (true or false; in a
`CHECK`, a call that does not fit is refused, `23514`) and
`json_schema_errors(doc, schema)` — `text[]`, each problem with its place
(`$.days: 30 is greater than the maximum 14`), to hand back to the model.
`$ref` is `#…` pointers into the schema; an unknown keyword is refused
(`0A000`), not ignored; `pattern` is Rust's `regex`.

**Vectors** (embeddings, as pgvector): `embedding vector(768)`, literals
`'[0.1, 0.2, …]'` (or a driver's float array as a parameter); distances
`<->` (Euclidean), `<=>` (cosine), `<#>` (negative inner product), `<+>`
(taxicab); the nearest rows: `SELECT id FROM docs WHERE tenant = $2
ORDER BY embedding <=> $1 LIMIT 10` (exact, no index yet); `+ - *` and
`||` component-wise, `vector_dims`, `vector_norm`, `l2_normalize`,
`subvector`, `avg(embedding)` (a centroid); components by position as
arrays' elements: `embedding[1]` (a `real`), `embedding[1:256]` (a
vector), `UPDATE docs SET embedding[3] = 0.5`.  Over the PostgreSQL
protocol pgvector's clients work (`register_vector` with psycopg).

**Embeddings by the database:** `embed(text, model [, 'query' |
'document'])` returns a `vector` — `UPDATE docs SET embedding =
embed(body, 'small')`, `ORDER BY embedding <=> embed($1, 'small',
'query') LIMIT 10` (the query's vector computed once).  Models are made
with `CREATE MODEL small USING openai WITH (model =
'text-embedding-3-small', secret = openai_key)` (`openai` — any
OpenAI-compatible server, Ollama included —, `cohere`, `voyage`, and
`hashing`, a lexical vector without a network) and listed in
`nc_catalog.models`; keys are secrets (`CREATE SECRET openai_key VALUE
'…'`), never shown.  A model can also run inside the database, with no
network after it is made: `CREATE MODEL bge USING local WITH
(huggingface = 'BAAI/bge-small-en-v1.5')` downloads its files once and
keeps them in the database (BERT-family encoders: bge, e5 — its
`multilingual-e5-small` reads Hungarian —, MiniLM, GTE; give e5
`query_prefix = 'query: ', document_prefix = 'passage: '`).  Use the
model's own name, and `'query'` for what is searched for.

**Columns that keep themselves current:** `total numeric GENERATED
ALWAYS AS (price * qty) STORED` (computed on every write; never given),
and `v vector GENERATED BY EMBEDDING (title || ' ' || body) USING bge`
(the table needs a primary key): each write that changes the text queues
the row, and its vector is made in the background, retried when the
provider is down — no `UPDATE … SET v = embed(…)` to remember; search
it with `ORDER BY v <=> embed($1) LIMIT 10` (the column's own model, as
a query).  Until it
is made a row keeps its old vector (NULL at first); `SET
embedding.pending = skip` leaves such rows out of queries, `wait` waits
for them.  `nc_catalog.embedding_jobs` lists what is pending or failed.
To change the model: `ALTER TABLE t ALTER COLUMN v SET MODEL e5` — the
new vectors are made in the background while queries read the old, then
the column switches at once (its dimensions may change).

**Chunks:** `SELECT d.id, c.position, c.chunk, c.headings FROM docs d,
chunks(d.body, 'markdown', 200, 20, 'e5') c` cuts each document into
pieces of at most 200 of the model's tokens (`'sentences'`, `'tokens'`
or `'markdown'`; overlap 20; without a model, sizes are words);
`substr(d.body, c.start, c.length) = c.chunk`.  To keep them:
`CREATE TABLE parts CHUNKS OF docs (body) BY markdown SIZE 200 OVERLAP
20 USING e5`, then `ALTER TABLE parts ADD COLUMN v vector GENERATED BY
EMBEDDING (chunk) USING e5` — every write to `docs` updates `parts` in
the same transaction (unchanged chunks keep their vectors); search
`SELECT id, chunk FROM parts ORDER BY v <=> embed($1) LIMIT 10`.

**Memory for a model:** `CREATE MEMORY STORE notes USING e5` makes a
table `notes` (`id uuid`, `subject`, `kind`, `content`, `embedding`,
`confidence`, `seen`, `valid_from`, `valid_to`, `expires_at`, `source`,
`replaces`, `created`, `last_seen`).  `REMEMBER 'Ann prefers short
answers' FOR 'ann' IN notes [AS 'preference'] [CONFIDENCE 0.8] [EXPIRES
now() + interval '30 days'] [SOURCE 'chat 12']` returns `id, action`:
`merged` into the subject's current memory with the same or nearly the
same text, else `inserted`; when a fact changed, `REPLACING old_id`
closes the old one (`valid_to` set, kept) — `replaced`.  Recall:
`SELECT id, content, score FROM recall('notes', 'ann', 'how should I
answer', 5)` (the store a text literal; current, unexpired memories,
BM25 and vectors fused; under a millisecond on 100 000 memories in the
process).  Several questions at once, one statement: `SELECT n, r.id,
r.content FROM unnest($2::text[]) WITH ORDINALITY AS b(q, n), LATERAL
recall('notes', $1, q, 5) r ORDER BY n, r.score DESC`.  Forget one: `DELETE FROM notes WHERE id = $1`.  Any
table's rows can expire: `ALTER TABLE t SET EXPIRY (expires_at)` (a
timestamp or date column; deleted in the background).  Erase a person:
mark the columns that say whose a row is, `ALTER TABLE messages SET
SUBJECT (sender, recipient)` (a memory store's `subject` is marked), then
`FORGET SUBJECT 'ann'` (not in a transaction block) deletes the subject's rows from
every such table — foreign keys followed, all or nothing (`42501` if row
security hides any from you) — then scrubs the files (old versions,
index pages, free pages, the log, audit details) and searches them for
the rows' values: it returns `erasure, rows, scrubbed, found` (`found =
0` is the proof none is left); `nc_catalog.erasures` lists erasures
without the subject's value.  Backups: `BACKUP TO '/abs/dir'` (an
administrator, not in a block) writes a snapshot while writes go on —
each subject's rows sealed under the subject's key (the server's
`<dir>.subject-keys` file; keep it apart from the backups), which
`FORGET SUBJECT` destroys; `nucleus restore <backup> <newdir>
[--subject-keys FILE]` rebuilds indexes and statistics and leaves
forgotten subjects out.  Many tenants in one table: a policy
(`CREATE POLICY t ON docs USING (tenant = current_setting('app.tenant'))`)
keeps every search — vectors, `@@`, `score()`, `rrf` — to the tenant's
rows; partition the table by the tenant (`PARTITION BY LIST (tenant)`)
and BM25's scores are the tenant's own too, not moved by other tenants'
rows.

**Model limits and costs:** give a remote model your account's limits —
`CREATE MODEL oa USING openai WITH (model = 'text-embedding-3-small',
secret = k, requests_per_minute = 3000, tokens_per_minute = 1000000,
batch_tokens = 8000, parallel = 4)`; calls are paced by them (a query
waits within its `statement_timeout`, else `53400`; background
embedding retries later); a remote call itself ends within the
statement's `statement_timeout` too (at most five minutes).  `EXPLAIN ANALYZE` shows a `Models:` line
(calls, texts, cached, tokens, errors); `nc_catalog.statements` has
`model_calls`, `model_tokens`, `model_ms`; `nc_catalog.models` the
totals.

**Tables:** `CREATE TABLE [IF NOT EXISTS] t (id bigint PRIMARY KEY, name
text NOT NULL DEFAULT '', email text UNIQUE, score int CHECK (score >= 0),
owner_id bigint REFERENCES owners (id) ON DELETE CASCADE)`; table
constraints `PRIMARY KEY (a, b)`, `UNIQUE (…)`, `CHECK (…)`, `FOREIGN KEY
(…) REFERENCES t (…)`.  `CREATE [UNIQUE] INDEX [name] ON t (a, b)` — a
key part may be an expression, `((doc->>'kind'))` or `lower(email)`, used
for conditions on that very expression —; `CREATE INDEX ON t USING json
(doc)` serves `doc @> '{…}'`, `doc ? 'key'`, `?|`, `?&`, `DROP
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
`string_agg(x, ',' ORDER BY x)`); statistics `variance` / `var_samp`,
`var_pop`, `stddev` / `stddev_samp`, `stddev_pop` (`numeric` for integers
and decimals, as PostgreSQL's; `double precision` otherwise), `skewness`,
`kurtosis` (sample-corrected, as DuckDB's and Excel's), `corr(y, x)`,
`covar_pop`, `covar_samp`, `regr_slope`, `regr_intercept`, `regr_r2`,
`regr_count`, `regr_avgx`, `regr_avgy`, `regr_sxx`, `regr_syy`,
`regr_sxy` — over doubles the exact value rounded once, the same bits
however the rows are split.  Window functions as PostgreSQL's: `f(…) OVER
(PARTITION BY … ORDER BY … ROWS | RANGE | GROUPS BETWEEN … AND …
[EXCLUDE …])` and `WINDOW w AS (…)`, any aggregate above, `row_number
rank dense_rank percent_rank cume_dist ntile lag lead first_value
last_value nth_value` (top n per group: `SELECT … FROM (SELECT …,
row_number() OVER (PARTITION BY g ORDER BY x DESC) AS rn FROM t) q WHERE
rn <= 3`).  Exact quantiles as PostgreSQL's ordered-set aggregates:
`percentile_cont(0.95) WITHIN GROUP (ORDER BY latency)` (interpolated;
numbers and intervals), `percentile_disc(0.95) WITHIN GROUP (ORDER BY
x)` (an actual value, any ordered type), an array of fractions for
several at once (`percentile_cont(ARRAY[0.5, 0.95, 0.99]) WITHIN GROUP
(ORDER BY x)` gives an array), `mode() WITHIN GROUP (ORDER BY x)`, and
`median(x)`.  Buckets: `width_bucket(x, low, high, n)` (0 below, `n + 1`
from `high`), `width_bucket(x, ARRAY[bounds])`, and `histogram(x, low,
high, n)` — the `n + 2` bucket counts as a `bigint[]` in one pass.
Approximate, fixed memory, the same result every run:
`approx_count_distinct(x)` (±0.81 %; near exact below about 4000 distinct),
`approx_percentile(x, 0.95)` or `approx_percentile(x, ARRAY[0.5, 0.99])`
(within 1 % of the value — use `percentile_cont` for values far from
zero, such as timestamps), `approx_top_k(x, 10)` and
`approx_top_k_counts(x, 10)` (the most frequent values and their
counts).  Sketches to keep and merge later: `hll_agg(x)` →
`hll_merge(h)` → `hll_count(h)`; `ddsketch_agg(x)` →
`ddsketch_merge(s)` → `ddsketch_quantile(s, 0.99)` (e.g. one row per
day, merged for a month).
Samples: `FROM t TABLESAMPLE BERNOULLI (10) REPEATABLE (42)` — 10 % of
the rows, the same rows on every run and machine (without `REPEATABLE`,
new each time); a stable train/test split: `WHERE hash_unit(id) < 0.8`
and `WHERE hash_unit(id) >= 0.8` (a second argument as the seed);
`reservoir_sample(x, 100 [, seed])` — 100 values as an array, uniform,
the same every run — unlike `ORDER BY random() LIMIT n` (also there),
which differs each time.
Drift between two samples, `side` a boolean (false the reference, true
the new data): `SELECT ks_test(x, side) FROM t` → `{statistic, p-value}`
(scipy's `ks_2samp`); `auc(score, label)` (ROC AUC, label true =
positive); `psi(x, side)` (population stability index, 10 bins of the
reference; > 0.2 is usually called a shift); `js_divergence(category,
side)` (0 to 1); `chisq_test(category, side)` → `{statistic, p-value,
dof}`.  E.g. this week against last: `SELECT ks_test(latency, day >=
current_date - 7) FROM requests WHERE day >= current_date - 14`.
**First look at a table:** `SUMMARIZE t` (or `SUMMARIZE SELECT …`) — one
row per column: type, min, max, approx_unique, avg, std,
approx_q25/q50/q75, count, null_percentage, from one scan.

**Memory:** `SET work_mem = '256MB'` gives each sort and materialised
result more memory before it spills to disk (default 64MB).  Scans,
filters and aggregates run in batches (`batch_size`, default 1024; 0 runs
row by row) and on all CPUs (`parallel_workers`, default 0 = automatic; 1
= one thread) — same results either way.  For analytics tables (many
rows, queries reading a few columns) `CREATE TABLE … WITH (storage =
column)` stores them as compressed column segments; same SQL.  `ALTER TABLE
t SET STORAGE row, column` keeps both (fast key lookups and fast scans),
`SET STORAGE column (a, b)` a column copy of some columns; conversions run
online.  Large tables split by a key: `CREATE TABLE ev (at timestamp, …,
PRIMARY KEY (at, id)) PARTITION BY RANGE (at)` then `CREATE TABLE ev_2026
PARTITION OF ev FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')` (also
`LIST`, `HASH … WITH (MODULUS m, REMAINDER r)`, `DEFAULT`); queries skip
the partitions their filters rule out; `ATTACH` / `DETACH` / `DROP
PARTITION`.  `ALTER TABLE ev SET STORAGE POLICY (column AFTER interval
'30 days')` moves aged partitions to columns by itself.

**Transactions:** every statement is its own transaction unless in `BEGIN …
COMMIT | ROLLBACK`.  Snapshot isolation (`SERIALIZABLE` on request).  On
`40001` (a conflict with another transaction) run the transaction again;
after an error in a block everything is `25P02` until `ROLLBACK`.

**The past:** when the server keeps history (`nucleus server --history
7d`; `SELECT * FROM nc_catalog.history` shows how far back), read a
time: `BEGIN READ ONLY AS OF TIMESTAMP now() - interval '1 hour'` (the
whole block, the catalog as it was too), or one table beside the
present: `SELECT … FROM t AS OF TIMESTAMP '2026-11-13 10:00+00' AS old
JOIN t ON …` (it reads the whole table then; for a lookup by an index at
that time use the block form).  Undo a mistake: `INSERT INTO t SELECT * FROM t AS OF
TIMESTAMP … WHERE …`, or `CREATE TABLE t_restored AS SELECT * FROM t AS
OF TIMESTAMP …`.  `72000` means past the history kept.

**Branches — try things on a copy:** `CREATE BRANCH experiment` (an
administrator, not in a block) makes a copy of the database in constant
time, whatever its size; connect to it by naming it as the database
(`nucleus://user:pw@host:20035/experiment`, `connect(url,
database="experiment")`, `ncsql -d experiment`, PostgreSQL's `dbname`),
change anything there — the database it came from does not see it, and
it does not see the database's later writes — then `DROP BRANCH
experiment`, or `ALTER BRANCH experiment RESET` to start over from the
database as it is now.  `CREATE BRANCH b FROM a` branches a branch;
`SELECT * FROM nc_catalog.branches` lists them (`current`: yours).
Names: letters, digits, `-`, `_`, `.` (quote `"pr-1"`); `main` and
`nucleus` are the main database.  No merge: carry a change back by
running it on the main database.

**Citing memories:** `recall` rows carry `as_of`; cite a memory as its
`id` and `as_of` — `SELECT content FROM notes AS OF COMMIT <as_of> WHERE
id = '<id>'` shows exactly what you read.  `SET request_id = 'gen-42'`
before a generation's queries records what each returned
(`nc_catalog.reads`).

## Not available — use instead

| Not there | Instead |
|-----------|---------|
| `SERIAL`, `GENERATED … AS IDENTITY`, sequences | `id uuid DEFAULT uuidv7() PRIMARY KEY` (increasing), or ids from the application |
| `INSERT … ON CONFLICT` (upsert) | `UPDATE … WHERE key = …`, then `INSERT` if it changed 0 rows, in one transaction |
| `CREATE VIEW` | a `WITH` query |
| `count(*) FILTER (WHERE …)` | `sum(CASE WHEN … THEN 1 ELSE 0 END)` |
| `unnest(…)` in the select list | `SELECT … FROM unnest(…) AS u(x)` |
| `json[]`, `json_each(…)` in the select list | `json_agg` / `json_build_array`; `SELECT … FROM t, json_each(t.doc) e` |
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
  `nc_catalog.roles`, `nc_catalog.grants`, `nc_catalog.statistics`
  (and `.column_groups`, `.dependencies`: columns counted together —
  `CREATE STATISTICS s ON a, b FROM t` adds a group, then `ANALYZE t`).
- **Errors carry a hint:** `column "emial" does not exist (42703)` comes
  with `HINT: Perhaps you meant column "email".`; unknown tables, functions
  and wrong argument counts likewise; a type mismatch names the cast.  Read
  the hint before retrying.
- `EXPLAIN <query>` shows the plan; `EXPLAIN ANALYZE <query>` runs it and
  shows each step's actual rows beside the estimate (run `ANALYZE` when they
  differ a lot).  `LIKE`, a distance bound or a function of a column is
  estimated on up to 1,000 rows `ANALYZE` keeps, so analyze again after
  the data changes a lot.  `Hash Join (…) or lookups up to N rows` decides
  when it runs: N or fewer right rows look the left table up by its index.
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
`57014` statement timeout · `0A000` not supported · `3D000` no such
database (branch) · `42P04` the branch exists.
