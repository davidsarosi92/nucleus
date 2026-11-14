-- The nucleus tutorial (PLAN.md §M14.17 E5): a guided tour, step by step.
--
-- The playground (playground/) and the console show it one step at a
-- time; `nucleus sql :memory: < docs/tutorial.sql` runs it whole, and a
-- test runs it on every change (modules/console/tests/tutorial.rs), so it
-- stays true.  A step begins with `-- ## Title`; the comment lines after
-- it are its text; the rest is its SQL.  Time travel needs a history
-- (the server's --history, the playground's hour).

-- ## Hello
-- Every statement runs in the database in front of you — in the
-- playground, a database inside this page, compiled to WebAssembly.
--
-- Press "Run this step", or put your own SQL in the editor and press
-- Ctrl+Enter (⌘+Enter on a Mac).
SELECT 'hello, nucleus' AS greeting, version();

-- ## A table
-- Tables, types and constraints are PostgreSQL's. A primary key, a
-- NOT NULL column, a CHECK, a default.
CREATE TABLE products (
  id int PRIMARY KEY,
  name text NOT NULL,
  category text,
  price numeric(10, 2) CHECK (price >= 0),
  added date DEFAULT current_date
);
INSERT INTO products (id, name, category, price) VALUES
  (1, 'Espresso machine', 'kitchen', 249.00),
  (2, 'Grinder', 'kitchen', 89.50),
  (3, 'Desk lamp', 'office', 34.99),
  (4, 'Notebook', 'office', 4.20),
  (5, 'Kettle', 'kitchen', NULL);
SELECT * FROM products ORDER BY id;

-- ## Rows by the thousand
-- generate_series makes rows; INSERT … SELECT writes them in one
-- statement. Here, 50,000 orders of the products above.
CREATE TABLE orders (
  id int PRIMARY KEY,
  product_id int REFERENCES products (id),
  quantity int NOT NULL,
  placed timestamptz NOT NULL
);
INSERT INTO orders
SELECT i, i % 5 + 1, i % 7 + 1, timestamptz '2026-01-01' + i * interval '9 minutes'
FROM generate_series(1, 50000) i;
SELECT count(*) AS orders, min(placed) AS first, max(placed) AS last FROM orders;

-- ## Joins and aggregates
-- What each category sold, and for how much. A product without a price
-- counts its orders but adds nothing to the revenue.
SELECT p.category,
       count(*) AS orders,
       sum(o.quantity) AS items,
       sum(o.quantity * p.price) AS revenue
FROM orders o
JOIN products p ON p.id = o.product_id
GROUP BY p.category
ORDER BY revenue DESC NULLS LAST;

-- ## Plans
-- EXPLAIN ANALYZE runs a query and shows what each step did. In the
-- console the plan is drawn: each node's own time is a bar, and an
-- estimate far from what happened is marked.
--
-- Statistics make estimates right: ANALYZE gathers them.
ANALYZE;
EXPLAIN ANALYZE
SELECT p.name, sum(o.quantity)
FROM orders o JOIN products p ON p.id = o.product_id
WHERE o.placed >= timestamptz '2026-03-01'
GROUP BY p.name;

-- ## Indexes, found for you
-- ANALYZE QUERY reads a statement without running it and says what would
-- make it faster ("Lint" in the console). Here: a full scan of orders to
-- find one day's, and a foreign key with no index.
ANALYZE QUERY
SELECT * FROM orders WHERE placed >= timestamptz '2026-02-01' AND placed < timestamptz '2026-02-02';

-- ## An index, and the plan again
-- The index the linter asked for. The same query now reads only the
-- day's rows: compare the plan with the step before.
CREATE INDEX orders_placed ON orders (placed);
EXPLAIN ANALYZE
SELECT * FROM orders WHERE placed >= timestamptz '2026-02-01' AND placed < timestamptz '2026-02-02';

-- ## JSON
-- A json column keeps documents (as PostgreSQL's jsonb): -> and ->> read
-- into them, @> asks whether one contains another.
CREATE TABLE events (id int PRIMARY KEY, doc json NOT NULL);
INSERT INTO events VALUES
  (1, '{"kind": "view", "product": 1, "tags": ["new"]}'),
  (2, '{"kind": "cart", "product": 2, "qty": 2}'),
  (3, '{"kind": "view", "product": 3, "tags": ["sale", "new"]}');
SELECT id, doc ->> 'kind' AS kind, doc -> 'tags' AS tags
FROM events
WHERE doc @> '{"kind": "view"}'
ORDER BY id;

-- ## Vectors
-- Embeddings are vectors: the nearest by cosine distance (<=>). Real ones
-- have hundreds of dimensions and come from a model (embed(), on a
-- server); three are enough to see the idea. An hnsw index finds the
-- nearest among millions.
CREATE TABLE notes (id int PRIMARY KEY, body text, v vector(3));
CREATE INDEX notes_v ON notes USING hnsw (v);
INSERT INTO notes VALUES
  (1, 'grinding coffee beans fresh every morning', '[0.9, 0.1, 0.0]'),
  (2, 'a lamp that does not tire the eyes', '[0.0, 0.2, 0.9]'),
  (3, 'descaling the espresso machine', '[0.8, 0.3, 0.1]'),
  (4, 'paper notebooks for meetings', '[0.1, 0.9, 0.3]');
SELECT id, body, round((v <=> '[1, 0.2, 0]')::numeric, 3) AS distance
FROM notes
ORDER BY v <=> '[1, 0.2, 0]'
LIMIT 3;

-- ## Text search
-- @@ matches words, phrases and prefixes (Lucene's syntax); score() ranks
-- by BM25. A text index makes it fast on large tables.
CREATE INDEX notes_body ON notes USING text (body);
SELECT id, body, round(score(body @@ 'espresso OR coffee')::numeric, 3) AS score
FROM notes
WHERE body @@ 'espresso OR coffee'
ORDER BY score DESC;

-- ## Transactions
-- BEGIN … COMMIT, snapshot isolation. A failed statement fails the
-- block; ROLLBACK undoes it. The console's header shows the state.
BEGIN;
UPDATE products SET price = price * 0.9 WHERE category = 'kitchen';
SELECT name, price FROM products WHERE category = 'kitchen' ORDER BY id;
ROLLBACK;
SELECT name, price FROM products WHERE category = 'kitchen' ORDER BY id;

-- ## Time travel
-- With a history kept, the database as it was is a query away: AS OF a
-- time or a commit. Here, the products before a change.
SELECT newest_commit AS before_the_change FROM nc_catalog.history;
UPDATE products SET price = 0 WHERE id = 4;
SELECT p.name, p.price AS now, was.price AS before
FROM products p
JOIN (SELECT id, price FROM products AS OF COMMIT
        (SELECT newest_commit - 1 FROM nc_catalog.history)) was ON was.id = p.id
WHERE p.id = 4;

-- ## Branches
-- A branch is a copy of the whole database made in constant time and
-- space — for an experiment, a migration tried, an agent's work. A client
-- names it as its database; nc_catalog.branches lists them.
CREATE BRANCH experiment;
SELECT name, parent FROM nc_catalog.branches ORDER BY name;
DROP BRANCH experiment;

-- ## Where next
-- The catalog describes everything: nc_catalog.tables, columns, indexes,
-- statements (what ran, how often, how long). DOCS.md has the whole
-- reference; docs/llm.md the dialect in brief.
SELECT name, columns, indexes FROM nc_catalog.tables ORDER BY name;
