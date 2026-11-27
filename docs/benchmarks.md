# Benchmarks

nucleus against PostgreSQL on the same machine and data (PLAN.md §M14.11
L8).  The suite is `bench/run.py`: one workload — a 4-column table loaded
by `INSERT … SELECT`, counts, a filtered scan, a sum, a 1000-group
`GROUP BY`, a sort, a self-join, 1000 reads by key, an update of a quarter
of the rows and a delete of a tenth — run statement by statement through
each system's own client with `\timing`, both as client and server:
nucleus as `nucleus server` driven by `nucleus sql -h` over its own
protocol, `ncp`; PostgreSQL, the baseline, through `psql`.  nucleus's
PostgreSQL protocol (`--pg-compat`) is not used.

Every number goes through one method (PLAN.md §M14.31 P1,
`bench/method.py`): the systems run in turn on the same machine, each
read cold (its server just restarted) and warm, five rounds or more
(three for a step whose round takes over ten minutes); a number is the
median of its rounds with a 95 % interval and the spread (`±` the
coefficient of variation), a measurement spreading past 10 % is run
again (unless its rounds took over ten minutes) and still past it
marked ⚠; a ratio is claimed only where
the two intervals do not overlap and the medians are more than 10 %
apart (`≈` otherwise: a number moves by that much between sessions on
one machine — Kalibera and Jones, 2013); a round starts only when the
machine's other work is under 2.5 cores (what it waited is kept in the
results); a second run reproduces a number when it is not apart from
the first by the same test, and on TPC-H and ClickBench each system's
plan of each query is kept beside its times, so a time that moved shows
whether its plan moved with it; every answer is checked against the
other system's.  PostgreSQL runs natively in a
cluster of its own, tuned for the machine (`bench/pg.py`), what it runs
with recorded beside the results.

CI runs the suite on every push to `main` with a million rows against the
`postgres:18` image (job **benchmarks against PostgreSQL**; the table is
in the job summary, the numbers in the `bench` artifact).  It fails only
when a run fails or an answer differs: shared runners are too noisy for
thresholds.

## Reproduce

Every number below comes from `bench/all.sh`: each step writes
`bench/results/<step>.json` — every round's numbers, their medians and
intervals, the environment (machine, load, systems, commit), the
command — and each table here is that file through `method.py show`;
the line under a table is the environment it was measured in.

```sh
brew install postgresql@18 pgvector   # the native baseline (bench/pg.py's cluster)
bash bench/all.sh                     # build, then every step (about a day)
bash bench/all.sh list                # the steps
bash bench/all.sh build tpch knn      # some of them
python3 bench/method.py show bench/results/tpch.json        # a results file as a table
RESULTS=bench/results-2 bash bench/all.sh                   # a second run elsewhere …
python3 bench/method.py reproduce bench/results/tpch.json bench/results-2/tpch.json  # … reproduces?
```

The data a step needs (TPC-H's generator, ClickBench's hits, SIFT,
BEIR, MS MARCO, LongMemEval) is fetched by the command its error
names; ParadeDB runs in Docker (the container `nucleus-paradedb`,
started and stopped by the script).  A single suite run, without the
method's rounds:

```sh
python3 bench/run.py --nucleus target/release/nucleus --rows 1000000 --out /tmp/run.json
python3 bench/run.py --nucleus target/release/nucleus --no-pg   # nucleus alone
```

## Local runs

Every step of `bench/all.sh` writes `bench/results/<step>.json` — each
round's numbers, the environment, the command — and every table below is
`python3 bench/method.py show bench/results/<step>.json`: a median, its
95 % interval in brackets, `±` the spread, ⚠ where it stayed past 10 %,
`≈` where two intervals overlap.  The machine: an Apple M2 Pro (6
performance and 4 efficiency cores, 32 GiB, SSD), nucleus release builds;
PostgreSQL 18.7 native (Homebrew), a cluster of its own tuned for the
machine by `bench/pg.py` (shared_buffers 8 GB, parallel workers on the
cores) — a stronger baseline than the Docker image of the runs before
§M14.31, whose numbers are in PLAN.md's step notes.  The nucleus server
gets PostgreSQL's `shared_buffers` as its page cache (`--cache`), and
both systems `VACUUM ANALYZE` after a load (PLAN.md §M14.33 R3).

**The suite** (`bash bench/all.sh suite`; `bench/run.py --engines
btree,lsm`, 1,000,000 rows, each statement cold — the server just
restarted — and warm; the table a B+tree's, and an LSM tree's, PLAN.md
§M14.28 L5):

| run | nucleus (ms) | nucleus (lsm) (ms) | postgresql (ms) | nucleus / postgresql | nucleus (lsm) / postgresql |
|---|---:|---:|---:|---:|---:|
| load (INSERT … SELECT) | 961 [955–966] ±0% | 998 [985–1,009] ±1% | 1,741 [1,692–1,771] ±2% | 0.55× | 0.57× |
| count(*) (cold) | 6.63 [6.34–6.86] ±3% | 8.72 [7.78–11.3] ±15% ⚠ | 27.0 [25.9–28.2] ±4% | 0.25× | 0.32× |
| count(*) | 6.38 [5.93–6.72] ±5% | 7.62 [7.04–22.1] ±63% ⚠ | 15.6 [15.3–15.9] ±2% | 0.41× | ≈ (0.49×) |
| filtered count (cold) | 20.3 [19.9–20.7] ±2% | 34.5 [33.1–35.7] ±3% | 22.8 [20.3–24.0] ±7% | ≈ (0.89×) | 1.51× |
| filtered count | 8.66 [8.40–9.32] ±4% | 9.46 [8.88–10.3] ±6% | 16.3 [15.4–16.6] ±3% | 0.53× | 0.58× |
| sum (cold) | 13.2 [12.7–17.8] ±15% ⚠ | 25.7 [25.5–28.1] ±4% | 21.8 [21.2–23.4] ±4% | 0.61× | 1.18× |
| sum | 7.79 [7.58–9.27] ±9% | 8.45 [8.14–9.16] ±5% | 15.7 [15.2–21.2] ±15% ⚠ | 0.50× | 0.54× |
| group by (1000 groups) (cold) | 17.7 [17.3–21.3] ±9% | 30.3 [29.5–45.6] ±21% ⚠ | 39.7 [37.1–42.2] ±5% | 0.45× | ≈ (0.76×) |
| group by (1000 groups) | 13.1 [12.6–15.7] ±9% | 13.0 [12.7–13.9] ±4% | 33.0 [30.9–33.2] ±3% | 0.40× | 0.39× |
| sort (cold) | 71.9 [69.9–73.5] ±2% | 86.3 [82.6–90.4] ±3% | 142 [142–143] ±0% | 0.51× | 0.61× |
| sort | 65.7 [63.9–68.3] ±2% | 66.5 [64.6–68.3] ±2% | 127 [126–129] ±1% | 0.52× | 0.52× |
| self-join (cold) | 72.8 [71.5–134] ±33% ⚠ | 92.2 [86.0–92.6] ±4% | 124 [118–129] ±4% | ≈ (0.59×) | 0.74× |
| self-join | 61.6 [61.1–63.0] ±1% | 62.3 [62.0–63.0] ±1% | 113 [111–121] ±3% | 0.55× | 0.55× |
| point reads (1000 by key) (cold) | 26.2 [24.9–32.0] ±11% ⚠ | 26.9 [26.1–28.2] ±3% | 30.1 [28.7–30.4] ±2% | ≈ (0.87×) | 0.89× |
| point reads (1000 by key) | 23.8 [23.4–29.7] ±12% ⚠ | 24.3 [24.1–24.9] ±1% | 23.0 [22.5–23.2] ±1% | ≈ (1.04×) | ≈ (1.06×) |
| update 25% | 526 [521–547] ±2% | 621 [618–636] ±1% | 1,555 [1,458–1,598] ±4% | 0.34× | 0.40× |
| delete 10% | 95.2 [92.9–97.0] ±2% | 154 [151–158] ±2% | 66.2 [64.9–66.8] ±1% | 1.44× | 2.33× |
| count after writes | 10.3 [9.96–11.4] ±5% | 28.5 [27.8–46.9] ±25% ⚠ | 15.2 [15.1–15.7] ±1% | 0.68× | 1.88× |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 0.1.0, nucleus (lsm) 0.1.0, postgresql 18.7 (Homebrew) — nucleus 18041cf93 — 2026-11-20T09:23:31+00:00

Against the native, tuned PostgreSQL, reads by key are even with its
time (1.04×, the intervals overlapping; cold 0.87×; nucleus reached by
its Unix socket, as `psql` reaches PostgreSQL — `nucleus server
--socket`), the scans, groupings, sorts and joins 0.25–0.61× (the cold
filtered count even, 0.89×), and a `DELETE` of a tenth of the rows the
one lag, 1.44× (issue #7; Targets, below).  An LSM table reads warm as
the B+tree's or near it (count(*) 7.6 against 6.4 ms, the other reads
within 1.09×, updates 1.18×); cold, after its blocks left the cache,
1.2–1.9× (each block decompressed, issue #13), after the writes 2.8×
(several sources merged an entry at a time, issue #11), and its
`DELETE` 2.33× PostgreSQL's (issue #12).

**Where a read by key goes** (`bash bench/all.sh phases`;
`crates/sql/tests/statement_phases.rs`, PLAN.md §M14.33 R1): the
suite's `SELECT s FROM b WHERE id = <n>` on its million rows, in the
process, phase by phase, and the operating system's round trip on
loopback TCP (how `nucleus sql -h` reaches the server) and on a Unix
socket (how `psql` reaches PostgreSQL here):

| phases | nucleus (µs) |
|---|---:|
| a read by key, literal, in the process | 8.38 [8.34–8.41] ±1% |
| parsing it | 0.992 [0.989–0.997] ±1% |
| a read by key, a parameter, in the process | 18.3 [18.1–18.4] ±2% |
| a read by key, the same text again, in the process | 5.73 [5.68–5.77] ±3% |
| EXPLAIN of it (binding and planning), in the process | 15.7 [15.5–15.9] ±4% |
| SELECT 1, in the process | 4.23 [4.21–4.28] ±4% |
| a round trip, loopback TCP | 15.7 [15.6–15.8] ±10% |
| a round trip, Unix socket | 4.69 [4.07–4.79] ±10% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T09:27:25+00:00

A read by key with its key inline is run as its shape, the key a
parameter (PLAN.md §M14.33 R2a), so every key shares one plan: binding
and planning (15.6 µs) are paid once, not by every literal.  A
parameter through `Session::run` without its text (the in-process API)
still plans each call.  The transport is the other part: loopback TCP's
round trip costs 11 µs more than a Unix socket's, which the suite now
uses (R2b).

**Results into numpy** (`bash bench/all.sh python`; `bench/embedded/python.py`,
the embedded edition in the process and the driver over loopback TCP):

| python | nucleus |
|---|---:|
| SELECT 1, in the process, µs | 9.50 [9.50–9.75] ±1% |
| SELECT 1, loopback TCP to nucleus server, µs | 27.9 [27.8–29.1] ±2% |
| 1,000,000 rows of two columns into numpy, in the process, ms | 21.6 [21.3–21.7] ±1% |
| 1,000,000 rows of two columns into numpy, in the process, a column table, ms | 10.8 [10.6–10.8] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus — nucleus 18041cf93+changes — 2026-11-20T22:05:37+00:00

## TPC-H

`bench/tpch.py` (§M14.20 V10): TPC-H's schema, its data generated from a
fixed seed with the specification's sizes, keys and distributions
(`bench/tpch_data.py`; not `dbgen`'s text), primary keys and indexes on
the foreign keys' columns on both systems, the 22 queries with their
validation parameters (`bench/tpch_queries.py`), each nucleus run's
results compared with PostgreSQL's (numbers to cents).  nucleus runs as
its own server, driven by `nucleus sql -h` (loads by its `\copy`) and
read back by the Python driver, with every table in rows, and with all
but `region` and `nation` in columns; PostgreSQL natively, as above.
`bash bench/all.sh tpch` (SF 1):

| tpch | nucleus (row) | nucleus (column) | postgresql | nucleus (row) / postgresql | nucleus (column) / postgresql |
|---|---:|---:|---:|---:|---:|
| Q1 (cold), ms | 279 [272–304] ±5% | 154 [148–161] ±4% | 512 [505–513] ±1% | 0.54× | 0.30× |
| Q1, ms | 177 [175–185] ±2% | 103 [101–106] ±2% | 473 [471–479] ±1% | 0.37× | 0.22× |
| Q2 (cold), ms | 35.7 [33.5–39.4] ±7% | 39.7 [37.7–40.9] ±4% | 149 [137–153] ±4% | 0.24× | 0.27× |
| Q2, ms | 22.9 [21.9–31.3] ±16% ⚠ | 23.6 [22.8–26.2] ±5% | 84.3 [80.6–88.1] ±3% | 0.27× | 0.28× |
| Q3 (cold), ms | 267 [257–300] ±7% | 94.7 [91.7–102] ±4% | 426 [414–433] ±2% | 0.63× | 0.22× |
| Q3, ms | 157 [156–166] ±3% | 40.2 [39.6–46.2] ±7% | 261 [260–263] ±0% | 0.60× | 0.15× |
| Q4 (cold), ms | 139 [138–141] ±1% | 76.9 [76.7–78.2] ±1% | 155 [154–160] ±2% | 0.89× | 0.49× |
| Q4, ms | 73.4 [71.1–79.6] ±5% | 37.4 [36.6–37.9] ±1% | 75.4 [74.8–78.1] ±2% | ≈ (0.97×) | 0.50× |
| Q5 (cold), ms | 229 [211–288] ±15% ⚠ | 94.9 [92.0–100] ±3% | 265 [264–339] ±12% ⚠ | ≈ (0.87×) | 0.36× |
| Q5, ms | 129 [125–142] ±5% | 42.5 [41.8–46.7] ±5% | 149 [147–157] ±3% | 0.87× | 0.29× |
| Q6 (cold), ms | 211 [161–224] ±12% ⚠ | 61.0 [59.4–68.8] ±6% | 175 [170–187] ±4% | ≈ (1.20×) | 0.35× |
| Q6, ms | 97.7 [90.1–103] ±5% | 19.8 [19.1–20.6] ±3% | 91.9 [91.8–92.1] ±0% | ≈ (1.06×) | 0.22× |
| Q7 (cold), ms | 249 [247–253] ±1% | 108 [107–113] ±2% | 1,039 [1,005–1,097] ±3% | 0.24× | 0.10× |
| Q7, ms | 153 [148–163] ±4% | 53.3 [51.8–53.6] ±1% | 386 [380–391] ±1% | 0.40× | 0.14× |
| Q8 (cold), ms | 129 [128–157] ±10% | 109 [108–113] ±2% | 171 [163–194] ±7% | 0.76× | 0.64× |
| Q8, ms | 70.3 [69.4–71.4] ±1% | 41.2 [40.0–42.3] ±2% | 69.8 [69.4–74.8] ±3% | ≈ (1.01×) | 0.59× |
| Q9 (cold), ms | 399 [392–455] ±8% | 250 [248–265] ±3% | 881 [847–893] ±2% | 0.45× | 0.28× |
| Q9, ms | 307 [303–315] ±1% | 176 [173–178] ±1% | 651 [647–657] ±1% | 0.47× | 0.27× |
| Q10 (cold), ms | 223 [215–232] ±3% | 149 [145–149] ±1% | 213 [211–216] ±1% | ≈ (1.05×) | 0.70× |
| Q10, ms | 134 [134–138] ±2% | 84.4 [83.6–85.4] ±1% | 127 [126–129] ±1% | ≈ (1.05×) | 0.66× |
| Q11 (cold), ms | 46.9 [43.9–49.3] ±4% | 20.1 [19.8–21.4] ±3% | 75.9 [74.5–82.5] ±4% | 0.62× | 0.27× |
| Q11, ms | 28.4 [27.8–31.8] ±6% | 14.2 [13.6–14.5] ±3% | 31.5 [29.2–40.7] ±14% ⚠ | ≈ (0.90×) | 0.45× |
| Q12 (cold), ms | 295 [262–308] ±6% | 78.9 [75.8–82.4] ±3% | 240 [239–246] ±2% | 1.23× | 0.33× |
| Q12, ms | 162 [157–166] ±2% | 32.1 [31.0–36.2] ±6% | 141 [140–148] ±2% | 1.15× | 0.23× |
| Q13 (cold), ms | 129 [125–132] ±2% | 94.8 [93.3–109] ±7% | 516 [495–542] ±4% | 0.25× | 0.18× |
| Q13, ms | 96.8 [95.0–104] ±4% | 62.4 [59.9–72.0] ±7% | 428 [418–454] ±4% | 0.23× | 0.15× |
| Q14 (cold), ms | 189 [171–213] ±10% | 54.7 [54.2–55.5] ±1% | 185 [184–186] ±0% | ≈ (1.02×) | 0.30× |
| Q14, ms | 100 [95.7–105] ±4% | 21.9 [21.6–22.6] ±2% | 97.5 [96.8–98.6] ±1% | ≈ (1.03×) | 0.22× |
| Q15 (cold), ms | 214 [189–227] ±8% | 63.1 [61.9–66.2] ±3% | 202 [201–206] ±1% | ≈ (1.06×) | 0.31× |
| Q15, ms | 103 [101–120] ±7% | 23.5 [23.3–24.5] ±2% | 114 [113–114] ±0% | ≈ (0.91×) | 0.21× |
| Q16 (cold), ms | 103 [99.3–108] ±4% | 85.8 [85.1–89.0] ±2% | 101 [99.2–102] ±1% | ≈ (1.02×) | 0.85× |
| Q16, ms | 78.2 [77.0–81.1] ±2% | 69.4 [68.6–70.3] ±1% | 91.9 [91.3–93.1] ±1% | 0.85× | 0.76× |
| Q17 (cold), ms | 19.8 [19.7–23.2] ±7% | 59.1 [57.0–60.0] ±2% | 482 [477–488] ±1% | 0.04× | 0.12× |
| Q17, ms | 10.9 [10.7–11.2] ±2% | 30.6 [29.8–32.8] ±4% | 370 [351–400] ±5% | 0.03× | 0.08× |
| Q18 (cold), ms | 310 [302–366] ±10% | 183 [177–187] ±2% | 2,656 [2,579–2,729] ±2% | 0.12× | 0.07× |
| Q18, ms | 228 [220–238] ±3% | 143 [142–156] ±4% | 2,528 [2,523–2,558] ±1% | 0.09× | 0.06× |
| Q19 (cold), ms | 39.2 [38.2–47.3] ±9% | 75.3 [74.5–80.6] ±3% | 44.7 [41.2–47.9] ±6% | ≈ (0.88×) | 1.69× |
| Q19, ms | 15.1 [14.9–16.4] ±5% | 34.8 [33.5–35.0] ±2% | 18.2 [17.4–19.6] ±5% | 0.83× | 1.91× |
| Q20 (cold), ms | 124 [124–126] ±1% | 108 [106–112] ±2% | 200 [196–212] ±3% | 0.62× | 0.54× |
| Q20, ms | 56.1 [55.1–59.3] ±3% | 36.9 [36.7–38.0] ±2% | 60.4 [58.6–64.8] ±4% | ≈ (0.93×) | 0.61× |
| Q21 (cold), ms | 336 [315–355] ±6% | 160 [157–174] ±4% | 973 [943–996] ±2% | 0.35× | 0.16× |
| Q21, ms | 211 [209–224] ±3% | 95.6 [89.7–104] ±6% | 273 [265–275] ±1% | 0.77× | 0.35× |
| Q22 (cold), ms | 55.1 [54.8–60.5] ±4% | 27.2 [27.1–28.5] ±2% | 44.8 [43.3–45.5] ±2% | 1.23× | 0.61× |
| Q22, ms | 31.5 [30.4–32.7] ±3% | 14.6 [14.3–15.9] ±4% | 35.2 [34.8–35.8] ±1% | 0.90× | 0.41× |
| load, s | 26.9 [26.3–27.9] ±3% | 22.9 [22.5–24.1] ±3% | 22.2 [21.4–26.0] ±8% | 1.21× | ≈ (1.03×) |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus (row) 0.1.0, nucleus (column) 0.1.0, postgresql 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T09:27:53+00:00

Every result agrees with PostgreSQL's.  Column tables 0.29× PostgreSQL's
time in the geometric mean of the warm runs (cold 0.32×), faster on 21
of the 22 queries; row tables 0.56× (cold 0.54×), faster on 17.  The
loads — `COPY`, the indexes and `VACUUM ANALYZE` — 1.21× PostgreSQL's in
rows (issue #4), 1.03× in columns (PLAN.md §M14.33 R4: what is left is `COPY`'s parsing of the
text, the index builds and a load into a table made before its
transaction, whose rows are logged twice).

Where the time goes against the tuned PostgreSQL:

- **Column tables**: Q19 1.9× the one query behind (issue #5): its `OR`
  of three conjunctions over `part` and `lineitem` fetches ~14,000
  `lineitem` rows by key from the column table, where PostgreSQL reads a
  heap tuple each; the next, Q16, 0.76×.
- **Row tables**: Q12 1.15×, Q6 1.06×, Q10 1.05×, Q14 1.03×, Q8 1.01× —
  `lineitem`'s rows read for a filter that keeps few of them, against a
  PostgreSQL whose 8 GB of buffers hold the whole database and whose
  workers scan it in parallel; cold, Q12, Q22 1.23× and Q6 1.20× (a cold
  scan's pages copied out of the read buffer, issue #6).

How a step is measured: `bench/phases.py` — the build, the build before
it (`--base`, the commit before built in a worktree into `target/base`)
and with `--pg` PostgreSQL, in turn, round by round, by the method above.
`load` times `COPY`, the indexes and `ANALYZE` on an empty database each
round; `reads` loads, then times each of the 22 queries cold and warm
(and with `--vacuum` again after a `VACUUM`).  `clickbench` loads `hits`
and times its 43 queries the same way, from `bench/clickbench.py`'s
cache (`create.sql`, `queries.sql`, `hits-N.tsv`).  The data is
`bench/tpch.py --data DIR`'s generated `*.tbl` files (SF 1).  The same
build against itself shows no difference (every interval overlapping, 12
of 12 at SF 0.01) — with the systems' order reversed every other round:
in a fixed order a query came out 3–5 % apart from itself.  The script's
header has the commands.

## ClickBench

`bench/clickbench.py` (§M14.20 V10): ClickBench's `hits` table (its
PostgreSQL schema) and 43 queries, from the ClickBench repository at a
fixed commit, on the first million rows of its data, each system's
results checked against the other's (a query whose `LIMIT` may cut ties
differently is reported, not failed).  nucleus and PostgreSQL are driven
as for TPC-H.  `bash bench/all.sh clickbench`:

| clickbench | nucleus (row) | nucleus (column) | postgresql | nucleus (row) / postgresql | nucleus (column) / postgresql |
|---|---:|---:|---:|---:|---:|
| Q0 (cold), ms | 83.0 [79.3–92.9] ±8% | 5.00 [4.89–5.15] ±2% | 119 [116–126] ±3% | 0.70× | 0.04× |
| Q0, ms | 25.1 [24.8–25.9] ±2% | 3.15 [3.02–3.35] ±4% | 25.4 [25.0–25.6] ±1% | ≈ (0.99×) | 0.12× |
| Q1 (cold), ms | 91.8 [83.7–98.6] ±6% | 5.92 [5.52–6.11] ±4% | 130 [128–135] ±2% | 0.71× | 0.05× |
| Q1, ms | 33.2 [32.4–37.1] ±6% | 3.22 [3.06–3.31] ±4% | 58.4 [56.7–62.4] ±4% | 0.57× | 0.06× |
| Q2 (cold), ms | 114 [85.6–139] ±19% ⚠ | 6.31 [6.24–6.55] ±2% | 132 [130–137] ±2% | ≈ (0.86×) | 0.05× |
| Q2, ms | 35.0 [33.5–37.7] ±5% | 3.47 [3.29–3.74] ±5% | 58.9 [58.2–67.5] ±6% | 0.59× | 0.06× |
| Q3 (cold), ms | 86.9 [80.8–97.1] ±8% | 5.84 [5.76–6.57] ±6% | 124 [122–125] ±1% | 0.70× | 0.05× |
| Q3, ms | 32.0 [31.1–38.5] ±9% | 3.17 [3.09–3.45] ±5% | 51.5 [51.2–53.4] ±2% | 0.62× | 0.06× |
| Q4 (cold), ms | 101 [93.2–114] ±10% | 14.7 [14.2–14.8] ±2% | 244 [241–254] ±2% | 0.42× | 0.06× |
| Q4, ms | 40.1 [39.7–44.7] ±5% | 11.1 [11.0–11.8] ±3% | 241 [240–267] ±5% | 0.17× | 0.05× |
| Q5 (cold), ms | 109 [96.5–120] ±9% | 14.3 [14.1–14.8] ±2% | 308 [306–315] ±1% | 0.36× | 0.05× |
| Q5, ms | 40.4 [39.9–41.0] ±1% | 10.7 [10.6–11.0] ±2% | 240 [238–244] ±1% | 0.17× | 0.04× |
| Q6 (cold), ms | 96.4 [85.7–112] ±10% | 8.64 [8.39–8.96] ±3% | 131 [127–140] ±4% | 0.74× | 0.07× |
| Q6, ms | 34.0 [33.9–36.1] ±3% | 6.12 [6.02–6.39] ±2% | 55.3 [53.6–60.4] ±5% | 0.62× | 0.11× |
| Q7 (cold), ms | 93.1 [89.5–100] ±4% | 6.10 [5.83–6.26] ±3% | 125 [121–129] ±3% | 0.74× | 0.05× |
| Q7, ms | 34.5 [33.5–36.1] ±3% | 3.40 [3.27–3.49] ±2% | 57.1 [55.6–60.6] ±4% | 0.60× | 0.06× |
| Q8 (cold), ms | 106 [98.0–118] ±8% | 22.7 [22.5–25.9] ±6% | 378 [374–402] ±3% | 0.28× | 0.06× |
| Q8, ms | 48.7 [47.7–49.4] ±1% | 19.6 [19.0–20.0] ±2% | 383 [376–405] ±3% | 0.13× | 0.05× |
| Q9 (cold), ms | 150 [139–185] ±13% ⚠ | 60.1 [59.9–64.2] ±3% | 464 [460–466] ±1% | 0.32× | 0.13× |
| Q9, ms | 91.1 [88.0–91.5] ±2% | 52.9 [51.8–53.4] ±1% | 407 [399–414] ±2% | 0.22× | 0.13× |
| Q10 (cold), ms | 117 [91.8–147] ±19% ⚠ | 8.90 [8.49–11.3] ±12% ⚠ | 130 [129–132] ±1% | ≈ (0.90×) | 0.07× |
| Q10, ms | 37.8 [37.0–52.5] ±16% ⚠ | 5.58 [5.41–6.46] ±7% | 58.4 [57.9–58.6] ±1% | 0.65× | 0.10× |
| Q11 (cold), ms | 104 [89.4–135] ±17% ⚠ | 9.30 [9.24–10.7] ±8% | 134 [132–136] ±1% | ≈ (0.78×) | 0.07× |
| Q11, ms | 38.3 [37.7–39.9] ±2% | 5.75 [5.61–5.98] ±3% | 61.2 [59.1–63.2] ±3% | 0.63× | 0.09× |
| Q12 (cold), ms | 137 [130–148] ±6% | 13.7 [13.7–15.1] ±4% | 139 [136–143] ±2% | ≈ (0.98×) | 0.10× |
| Q12, ms | 40.9 [40.7–44.7] ±4% | 10.1 [9.91–11.1] ±5% | 64.9 [64.5–66.0] ±1% | 0.63× | 0.16× |
| Q13 (cold), ms | 154 [111–161] ±14% ⚠ | 26.2 [23.3–26.8] ±6% | 142 [141–145] ±1% | ≈ (1.09×) | 0.18× |
| Q13, ms | 52.6 [50.9–64.9] ±10% ⚠ | 20.0 [19.7–20.9] ±2% | 73.5 [73.3–74.8] ±1% | 0.72× | 0.27× |
| Q14 (cold), ms | 119 [91.1–147] ±23% ⚠ | 15.8 [14.9–17.4] ±6% | 128 [124–139] ±4% | ≈ (0.93×) | 0.12× |
| Q14, ms | 42.3 [39.8–44.7] ±4% | 11.1 [10.9–12.3] ±5% | 60.3 [59.4–64.8] ±4% | 0.70× | 0.18× |
| Q15 (cold), ms | 107 [103–125] ±8% | 21.3 [20.9–21.3] ±1% | 253 [248–254] ±1% | 0.42× | 0.08× |
| Q15, ms | 46.8 [44.3–47.4] ±3% | 17.1 [16.9–17.6] ±2% | 249 [248–257] ±2% | 0.19× | 0.07× |
| Q16 (cold), ms | 146 [124–160] ±11% ⚠ | 31.5 [30.6–33.2] ±3% | 329 [324–347] ±3% | 0.44× | 0.10× |
| Q16, ms | 63.5 [61.6–66.1] ±3% | 26.5 [25.8–26.8] ±1% | 251 [229–277] ±8% | 0.25× | 0.11× |
| Q17 (cold), ms | 156 [152–161] ±2% | 26.7 [26.5–27.2] ±1% | 144 [141–155] ±4% | ≈ (1.09×) | 0.19× |
| Q17, ms | 57.6 [56.8–65.9] ±6% | 21.1 [20.7–21.6] ±2% | 74.6 [73.0–75.7] ±1% | 0.77× | 0.28× |
| Q18 (cold), ms | 237 [235–244] ±2% | 134 [131–137] ±2% | 509 [496–520] ±2% | 0.47× | 0.26× |
| Q18, ms | 160 [155–190] ±10% | 124 [120–127] ±2% | 432 [411–436] ±2% | 0.37× | 0.29× |
| Q19 (cold), ms | 101 [92.6–106] ±5% | 3.91 [3.85–4.04] ±2% | 133 [130–151] ±6% | 0.76× | 0.03× |
| Q19, ms | 38.1 [37.6–40.4] ±3% | 3.00 [2.91–3.01] ±2% | 52.5 [52.0–61.9] ±8% | 0.73× | 0.06× |
| Q20 (cold), ms | 109 [97.1–119] ±8% | 26.7 [25.9–28.9] ±4% | 135 [134–145] ±3% | 0.81× | 0.20× |
| Q20, ms | 40.3 [39.3–42.3] ±3% | 6.90 [6.72–8.02] ±7% | 65.5 [65.1–68.1] ±2% | 0.62× | 0.11× |
| Q21 (cold), ms | 118 [99.9–154] ±16% ⚠ | 26.8 [26.2–27.3] ±2% | 138 [137–141] ±1% | ≈ (0.86×) | 0.19× |
| Q21, ms | 47.1 [45.6–50.7] ±4% | 6.69 [6.40–6.95] ±4% | 66.1 [65.4–66.9] ±1% | 0.71× | 0.10× |
| Q22 (cold), ms | 123 [108–152] ±16% ⚠ | 36.4 [35.8–38.0] ±2% | 138 [135–141] ±1% | ≈ (0.89×) | 0.26× |
| Q22, ms | 49.7 [49.3–51.2] ±2% | 7.27 [7.16–7.32] ±1% | 70.3 [69.9–72.3] ±1% | 0.71× | 0.10× |
| Q23 (cold), ms | 128 [112–136] ±8% | 34.3 [33.7–35.3] ±2% | 134 [132–138] ±2% | ≈ (0.96×) | 0.26× |
| Q23, ms | 50.6 [49.5–52.6] ±2% | 7.85 [7.77–8.05] ±2% | 65.5 [65.0–68.6] ±2% | 0.77× | 0.12× |
| Q24 (cold), ms | 96.1 [92.6–105] ±5% | 11.9 [11.4–12.2] ±3% | 131 [130–136] ±2% | 0.73× | 0.09× |
| Q24, ms | 37.9 [37.5–40.2] ±3% | 5.54 [5.32–5.85] ±4% | 57.7 [57.6–58.4] ±1% | 0.66× | 0.10× |
| Q25 (cold), ms | 128 [96.0–163] ±22% ⚠ | 12.7 [12.2–13.0] ±3% | 137 [134–141] ±2% | ≈ (0.94×) | 0.09× |
| Q25, ms | 38.3 [35.9–54.1] ±18% ⚠ | 6.75 [6.63–7.26] ±4% | 58.1 [57.6–60.3] ±2% | 0.66× | 0.12× |
| Q26 (cold), ms | 107 [89.7–136] ±18% ⚠ | 14.2 [14.1–14.8] ±2% | 132 [131–151] ±6% | ≈ (0.81×) | 0.11× |
| Q26, ms | 38.7 [38.4–42.5] ±4% | 6.88 [6.76–7.46] ±5% | 57.6 [57.2–58.4] ±1% | 0.67× | 0.12× |
| Q27 (cold), ms | 157 [154–157] ±1% | 54.4 [53.7–59.3] ±4% | 144 [138–158] ±5% | ≈ (1.09×) | 0.38× |
| Q27, ms | 73.1 [71.5–78.6] ±4% | 39.3 [38.2–41.2] ±3% | 78.0 [77.2–83.0] ±3% | ≈ (0.94×) | 0.50× |
| Q28 (cold), ms | 310 [290–319] ±4% | 245 [228–271] ±7% | 626 [623–636] ±1% | 0.49× | 0.39× |
| Q28, ms | 256 [249–294] ±7% | 220 [218–228] ±2% | 600 [597–626] ±2% | 0.43× | 0.37× |
| Q29 (cold), ms | 157 [155–162] ±2% | 43.7 [42.1–45.4] ±3% | 177 [175–180] ±1% | 0.89× | 0.25× |
| Q29, ms | 73.2 [72.7–77.0] ±2% | 40.7 [39.3–42.4] ±3% | 125 [124–130] ±2% | 0.58× | 0.32× |
| Q30 (cold), ms | 155 [154–167] ±3% | 20.7 [20.3–21.5] ±3% | 139 [136–143] ±2% | 1.12× | 0.15× |
| Q30, ms | 56.8 [53.5–67.0] ±9% | 15.6 [15.2–16.7] ±4% | 64.8 [63.8–65.2] ±1% | ≈ (0.88×) | 0.24× |
| Q31 (cold), ms | 162 [150–164] ±4% | 33.6 [32.7–34.4] ±2% | 149 [145–152] ±2% | ≈ (1.09×) | 0.23× |
| Q31, ms | 66.6 [65.7–70.5] ±3% | 27.3 [27.1–27.8] ±1% | 77.7 [75.5–81.9] ±3% | 0.86× | 0.35× |
| Q32 (cold), ms | 399 [366–441] ±7% | 295 [285–301] ±2% | 850 [838–864] ±1% | 0.47× | 0.35× |
| Q32, ms | 311 [308–317] ±1% | 284 [272–293] ±3% | 796 [772–816] ±2% | 0.39× | 0.36× |
| Q33 (cold), ms | 233 [201–240] ±8% | 128 [121–142] ±6% | 372 [365–375] ±1% | 0.63× | 0.35× |
| Q33, ms | 146 [144–152] ±2% | 108 [105–127] ±8% | 348 [341–393] ±6% | 0.42× | 0.31× |
| Q34 (cold), ms | 245 [241–248] ±1% | 135 [133–145] ±4% | 369 [361–372] ±1% | 0.66× | 0.37× |
| Q34, ms | 157 [155–161] ±2% | 121 [116–138] ±7% | 342 [334–373] ±4% | 0.46× | 0.35× |
| Q35 (cold), ms | 158 [110–163] ±17% ⚠ | 30.9 [30.3–33.1] ±4% | 152 [150–164] ±4% | ≈ (1.04×) | 0.20× |
| Q35, ms | 59.4 [56.4–68.0] ±7% | 27.8 [27.0–29.9] ±4% | 87.1 [86.7–88.4] ±1% | 0.68× | 0.32× |
| Q36 (cold), ms | 168 [156–195] ±9% | 64.7 [63.1–66.9] ±3% | 206 [205–206] ±0% | 0.81× | 0.31× |
| Q36, ms | 97.1 [96.0–99.5] ±2% | 53.1 [51.1–54.0] ±2% | 138 [136–147] ±3% | 0.70× | 0.38× |
| Q37 (cold), ms | 130 [108–157] ±17% ⚠ | 19.0 [18.4–19.8] ±3% | 147 [142–147] ±1% | ≈ (0.88×) | 0.13× |
| Q37, ms | 57.3 [55.7–58.6] ±2% | 15.7 [15.3–16.4] ±3% | 75.2 [74.7–76.7] ±1% | 0.76× | 0.21× |
| Q38 (cold), ms | 152 [109–155] ±17% ⚠ | 18.0 [17.8–18.6] ±2% | 143 [141–144] ±1% | ≈ (1.06×) | 0.13× |
| Q38, ms | 52.4 [50.2–54.1] ±3% | 9.31 [9.29–9.58] ±1% | 65.9 [64.4–66.3] ±1% | 0.79× | 0.14× |
| Q39 (cold), ms | 275 [268–278] ±1% | 192 [189–194] ±1% | 401 [392–411] ±2% | 0.68× | 0.48× |
| Q39, ms | 212 [207–214] ±1% | 165 [165–185] ±6% | 353 [349–358] ±1% | 0.60× | 0.47× |
| Q40 (cold), ms | 112 [103–174] ±25% ⚠ | 14.3 [13.7–15.5] ±4% | 152 [151–154] ±1% | ≈ (0.74×) | 0.09× |
| Q40, ms | 53.1 [51.2–57.0] ±5% | 9.98 [9.86–10.0] ±1% | 87.0 [86.6–87.2] ±0% | 0.61× | 0.11× |
| Q41 (cold), ms | 154 [150–159] ±2% | 12.1 [11.7–12.4] ±2% | 152 [150–166] ±4% | ≈ (1.02×) | 0.08× |
| Q41, ms | 58.3 [57.1–59.9] ±2% | 8.33 [8.16–8.69] ±3% | 86.1 [84.7–88.1] ±2% | 0.68× | 0.10× |
| Q42 (cold), ms | 119 [109–134] ±8% | 21.8 [20.6–23.5] ±5% | 151 [146–159] ±3% | 0.79× | 0.15× |
| Q42, ms | 58.4 [58.1–60.3] ±2% | 18.7 [18.5–19.0] ±1% | 85.8 [84.0–90.1] ±3% | 0.68× | 0.22× |
| load, s | 8.23 [7.91–8.29] ±2% | 7.14 [7.13–7.26] ±1% | 7.10 [7.00–7.18] ±1% | 1.16× | ≈ (1.00×) |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus (row) 0.1.0, nucleus (column) 0.1.0, postgresql 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T09:42:00+00:00

Every result agrees, but for ties at a `LIMIT`.  A column table 0.14× of
PostgreSQL's time in the geometric mean of the warm runs (cold 0.13×),
faster on all 43 queries (Q23, `SELECT *` … `ORDER BY` a large text,
now 0.07× of its time before the sweep's late materialization, PLAN.md §M14.33 S6); a row
table 0.54× (cold 0.73×), faster on all 43 but Q0, a `count(*)`, even
(0.99×), and cold behind on eight by up to 1.12× (issue #6).  The loads
1.16× in rows (issue #4), even in columns (1.00×).

## Vectors

Against pgvector 0.8 in the native PostgreSQL 18.7 above (its HNSW
builds on the cores: the table's `parallel_workers` set for the build,
PLAN.md §M14.31 P1d), on the same machine.

**Exact nearest neighbours** (`bash bench/all.sh knn`; PLAN.md §M14.12,
`bench/vectors/knn.py`): 100,000 random vectors of 768 dimensions (293
MiB), the ten nearest of 20 queries; nucleus with a 1 GB page cache as
pgvector has `shared_buffers`; numpy's brute force on one core beside
them, every answer checked against numpy's:

| knn | numpy | nucleus | pgvector |
|---|---:|---:|---:|
| search, ms | 41.4 [41.2–41.4] ±0% | 36.0 [36.0–37.5] ±2% | 212 [210–213] ±0% |
| load, s | — | 0.861 [0.838–0.890] ±2% | 34.2 [34.2–34.3] ±0% |
| answers differing from numpy's, count | — | 0.000 [0.000–0.000] ±0% | 0.000 [0.000–0.000] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — numpy 2.5.4, nucleus, pgvector 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T10:05:43+00:00

**The HNSW index** (`bash bench/all.sh ann-sift`; PLAN.md §M14.12 V4b,
`bench/ann/sql.py` — the server on a fresh database, the Python driver's
clients each in a process — and `bench/ann/pgvector_ref.py`):
ann-benchmarks' SIFT-1M, `m` 16, `ef_construction` 64, 900,000 rows
indexed at once, then 100,000 inserted by 10 writers (a row a
transaction) while one client searches; 2,000 queries, `SELECT id FROM t
ORDER BY v <-> $1 LIMIT 10`, recall@10, µs a search on one client,
searches a second on 10 (the scheduler's noise: ⚠ is common there).
Both include SQL — parsing, planning, the protocol on a local socket;
"server µs" is `EXPLAIN ANALYZE`'s.  pgvector reaches a recall at a lower
`ef_search`; nucleus needs about 1.5× the `ef` for the same recall, in
less time.

| ann-sift | nucleus | pgvector |
|---|---:|---:|
| load, s | 1.43 [1.42–1.49] ±3% | 1.78 [1.72–1.89] ±4% |
| index build, s | 55.0 [54.0–57.0] ±3% | 72.5 [69.6–74.0] ±5% |
| index, resident, MB | 1,873 [1,868–1,953] ±4% | — |
| index, on disk, MB | 587 [587–587] ±0% | 702 [702–702] ±0% |
| ef 10: recall@10, recall | 0.654 [0.654–0.654] ±0% | 0.731 [0.730–0.733] ±0% |
| ef 10: µs a search, µs | 550 [525–562] ±11% ⚠ | 366 [343–402] ±10% ⚠ |
| ef 10: QPS, 10 clients, QPS | 16,510 [10,617–19,473] ±24% ⚠ | 11,844 [10,887–12,263] ±12% ⚠ |
| ef 20: recall@10, recall | 0.785 [0.785–0.785] ±0% | — |
| ef 20: µs a search, µs | 244 [240–259] ±5% | — |
| ef 20: QPS, 10 clients, QPS | 17,829 [16,994–18,557] ±4% | — |
| ef 40: recall@10, recall | 0.891 [0.891–0.891] ±0% | 0.928 [0.927–0.928] ±0% |
| ef 40: µs a search, µs | 334 [325–352] ±4% | 728 [686–816] ±10% |
| ef 40: QPS, 10 clients, QPS | 13,666 [11,067–14,712] ±14% ⚠ | 6,721 [6,428–6,895] ±3% |
| ef 60: recall@10, recall | 0.933 [0.933–0.933] ±0% | — |
| ef 60: µs a search, µs | 417 [403–427] ±4% | — |
| ef 60: QPS, 10 clients, QPS | 12,646 [11,192–12,829] ±6% | — |
| ef 80: recall@10, recall | 0.953 [0.953–0.953] ±0% | — |
| ef 80: µs a search, µs | 494 [482–500] ±3% | — |
| ef 80: QPS, 10 clients, QPS | 10,909 [9,626–11,095] ±7% | — |
| ef 120: recall@10, recall | 0.974 [0.974–0.974] ±0% | — |
| ef 120: µs a search, µs | 632 [619–641] ±2% | — |
| ef 120: QPS, 10 clients, QPS | 8,576 [8,101–8,750] ±5% | — |
| ef 200: recall@10, recall | 0.989 [0.989–0.989] ±0% | 0.993 [0.993–0.993] ±0% |
| ef 200: µs a search, µs | 881 [874–901] ±1% | 2,357 [2,338–2,624] ±9% |
| ef 200: QPS, 10 clients, QPS | 6,422 [5,874–6,676] ±6% | 2,499 [2,449–2,520] ±6% |
| ef 40: server µs a search, µs | 263 [256–269] ±3% | 949 [919–986] ±5% |
| inserts, rows/s, rows/s | 5,126 [4,759–5,204] ±4% | 2,685 [2,623–2,708] ±3% |
| ef 40 during inserts: µs a search, µs | 860 [827–928] ±6% | 1,910 [1,772–2,063] ±9% |
| ef 10 after inserts: recall@10, recall | 0.645 [0.645–0.648] ±0% | 0.726 [0.723–0.730] ±0% |
| ef 10 after inserts: µs a search, µs | 510 [395–649] ±21% ⚠ | 368 [339–384] ±8% |
| ef 10 after inserts: QPS, 10 clients, QPS | 13,212 [11,811–15,865] ±15% ⚠ | 10,751 [10,443–11,104] ±5% |
| ef 20 after inserts: recall@10, recall | 0.781 [0.780–0.781] ±0% | — |
| ef 20 after inserts: µs a search, µs | 328 [284–401] ±16% ⚠ | — |
| ef 20 after inserts: QPS, 10 clients, QPS | 13,127 [7,377–15,811] ±27% ⚠ | — |
| ef 40 after inserts: recall@10, recall | 0.886 [0.885–0.887] ±0% | 0.925 [0.924–0.926] ±0% |
| ef 40 after inserts: µs a search, µs | 380 [363–429] ±8% | 757 [750–840] ±6% |
| ef 40 after inserts: QPS, 10 clients, QPS | 14,345 [13,557–14,820] ±5% | 6,327 [5,927–6,524] ±4% |
| ef 60 after inserts: recall@10, recall | 0.930 [0.930–0.931] ±0% | — |
| ef 60 after inserts: µs a search, µs | 440 [409–455] ±5% | — |
| ef 60 after inserts: QPS, 10 clients, QPS | 11,781 [11,345–12,574] ±9% | — |
| ef 80 after inserts: recall@10, recall | 0.952 [0.952–0.953] ±0% | — |
| ef 80 after inserts: µs a search, µs | 506 [498–540] ±4% | — |
| ef 80 after inserts: QPS, 10 clients, QPS | 10,414 [9,672–10,983] ±6% | — |
| ef 120 after inserts: recall@10, recall | 0.972 [0.972–0.972] ±0% | — |
| ef 120 after inserts: µs a search, µs | 655 [634–684] ±4% | — |
| ef 120 after inserts: QPS, 10 clients, QPS | 8,224 [7,253–8,869] ±8% | — |
| ef 200 after inserts: recall@10, recall | 0.988 [0.988–0.988] ±0% | 0.993 [0.993–0.993] ±0% |
| ef 200 after inserts: µs a search, µs | 913 [890–942] ±2% | 2,499 [2,447–2,713] ±6% |
| ef 200 after inserts: QPS, 10 clients, QPS | 6,214 [5,861–6,528] ±5% | 2,364 [2,243–2,410] ±5% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus, pgvector 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-21T07:02:03+00:00

**Quantized graphs** (`bash bench/all.sh ann-sift-int8`; PLAN.md
§M14.12 V6a, `WITH (quantize = int8)`, the rows read again for the exact
distances), SIFT-1M as above:

| ann-sift-int8 | nucleus |
|---|---:|
| load, s | 1.45 [1.43–1.54] ±12% ⚠ |
| index build, s | 29.1 [28.5–29.4] ±2% |
| index, resident, MB | 1,123 [1,114–1,130] ±4% |
| index, on disk, MB | 455 [455–455] ±0% |
| ef 10: recall@10, recall | 0.787 [0.787–0.787] ±0% |
| ef 10: µs a search, µs | 315 [289–329] ±6% |
| ef 10: QPS, 10 clients, QPS | 17,882 [16,858–18,685] ±10% ⚠ |
| ef 20: recall@10, recall | 0.787 [0.787–0.787] ±0% |
| ef 20: µs a search, µs | 221 [210–223] ±4% |
| ef 20: QPS, 10 clients, QPS | 19,631 [19,433–20,508] ±3% |
| ef 40: recall@10, recall | 0.892 [0.892–0.892] ±0% |
| ef 40: µs a search, µs | 259 [257–261] ±1% |
| ef 40: QPS, 10 clients, QPS | 17,511 [17,425–18,101] ±2% |
| ef 60: recall@10, recall | 0.933 [0.933–0.933] ±0% |
| ef 60: µs a search, µs | 302 [289–302] ±2% |
| ef 60: QPS, 10 clients, QPS | 15,908 [15,608–15,954] ±3% |
| ef 80: recall@10, recall | 0.953 [0.953–0.953] ±0% |
| ef 80: µs a search, µs | 339 [334–345] ±2% |
| ef 80: QPS, 10 clients, QPS | 14,156 [13,618–14,614] ±3% |
| ef 120: recall@10, recall | 0.973 [0.973–0.973] ±0% |
| ef 120: µs a search, µs | 421 [413–435] ±2% |
| ef 120: QPS, 10 clients, QPS | 12,154 [11,703–12,391] ±3% |
| ef 200: recall@10, recall | 0.988 [0.988–0.988] ±0% |
| ef 200: µs a search, µs | 571 [563–588] ±2% |
| ef 200: QPS, 10 clients, QPS | 9,564 [9,461–9,951] ±8% |
| ef 40: server µs a search, µs | 216 [212–227] ±4% |
| inserts, rows/s, rows/s | 6,390 [6,320–6,459] ±2% |
| ef 40 during inserts: µs a search, µs | 615 [600–637] ±3% |
| ef 10 after inserts: recall@10, recall | 0.782 [0.780–0.782] ±0% |
| ef 10 after inserts: µs a search, µs | 366 [326–415] ±12% ⚠ |
| ef 10 after inserts: QPS, 10 clients, QPS | 13,852 [13,175–15,203] ±8% |
| ef 20 after inserts: recall@10, recall | 0.782 [0.780–0.782] ±0% |
| ef 20 after inserts: µs a search, µs | 269 [253–306] ±13% ⚠ |
| ef 20 after inserts: QPS, 10 clients, QPS | 17,103 [14,744–17,843] ±10% |
| ef 40 after inserts: recall@10, recall | 0.887 [0.886–0.887] ±0% |
| ef 40 after inserts: µs a search, µs | 329 [291–361] ±14% ⚠ |
| ef 40 after inserts: QPS, 10 clients, QPS | 15,269 [14,562–17,297] ±18% ⚠ |
| ef 60 after inserts: recall@10, recall | 0.930 [0.930–0.931] ±0% |
| ef 60 after inserts: µs a search, µs | 340 [312–353] ±6% |
| ef 60 after inserts: QPS, 10 clients, QPS | 15,294 [14,738–15,846] ±4% |
| ef 80 after inserts: recall@10, recall | 0.952 [0.952–0.953] ±0% |
| ef 80 after inserts: µs a search, µs | 352 [344–392] ±6% |
| ef 80 after inserts: QPS, 10 clients, QPS | 14,328 [13,381–14,617] ±4% |
| ef 120 after inserts: recall@10, recall | 0.972 [0.972–0.972] ±0% |
| ef 120 after inserts: µs a search, µs | 430 [418–468] ±5% |
| ef 120 after inserts: QPS, 10 clients, QPS | 12,174 [11,576–12,437] ±4% |
| ef 200 after inserts: recall@10, recall | 0.988 [0.987–0.988] ±0% |
| ef 200 after inserts: µs a search, µs | 581 [574–591] ±3% |
| ef 200 after inserts: QPS, 10 clients, QPS | 9,455 [9,041–9,699] ±3% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus — nucleus 18041cf93+changes — 2026-11-20T10:15:01+00:00

**Filtered searches** (`bash bench/all.sh ann-filtered`; PLAN.md §M14.12
V5, `bench/ann/filtered.py`): the same 1,000,000 rows indexed at once, a
column `a` (0–999, from a hash of the row's id: independent of its
vector) with a B-tree index, `ANALYZE`d; 500 queries, `SELECT id FROM t
WHERE … ORDER BY v <-> $1 LIMIT 10` at `ef_search` 40, recall@10 against
the exact answer among the rows that pass, µs a search on one client;
pgvector with and without `hnsw.iterative_scan = relaxed_order`.  nucleus
counts `hnsw.ef_search` among the rows that pass (recall holds at every
share) and searches a filter's B-tree range exactly, from the vectors in
the index's memory, when it holds up to ~33,000 rows at `ef` 40;
pgvector searches the graph and filters, or uses the B-tree and sorts.

| ann-filtered | nucleus | pgvector | pgvector, iterative_scan off (pgvector) | pgvector, iterative_scan relaxed_order (pgvector) | pgvector, iterative_scan strict_order (pgvector) |
|---|---:|---:|---:|---:|---:|
| index build, s | 61.7 [61.5–62.8] ±1% | 78.8 [78.4–78.9] ±0% | — | — | — |
| filter 50%: recall@10, recall | 0.924 [0.924–0.926] ±0% | — | 0.890 [0.888–0.892] ±0% | 0.890 [0.888–0.892] ±0% | 0.890 [0.888–0.892] ±0% |
| filter 50%: rows returned, rows | 10.0 [10.0–10.0] ±0% | — | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% |
| filter 50%: µs a search, µs | 541 [530–553] ±2% | — | 911 [818–937] ±7% | 828 [801–936] ±8% | 827 [825–872] ±3% |
| filter 10%: recall@10, recall | 0.983 [0.982–0.985] ±0% | — | 0.416 [0.414–0.416] ±0% | 0.932 [0.931–0.933] ±0% | 0.858 [0.857–0.861] ±0% |
| filter 10%: rows returned, rows | 10.0 [10.0–10.0] ±0% | — | 4.18 [4.16–4.18] ±0% | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% |
| filter 10%: µs a search, µs | 1,731 [1,728–1,733] ±0% | — | 859 [848–880] ±2% | 1,811 [1,808–1,986] ±5% | 1,907 [1,876–2,032] ±4% |
| filter 1%: recall@10, recall | 1.00 [1.00–1.00] ±0% | — | 0.046 [0.045–0.046] ±0% | 0.987 [0.986–0.987] ±0% | 0.943 [0.943–0.944] ±0% |
| filter 1%: rows returned, rows | 10.0 [10.0–10.0] ±0% | — | 0.456 [0.454–0.456] ±0% | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% |
| filter 1%: µs a search, µs | 1,051 [1,047–1,054] ±0% | — | 834 [817–855] ±2% | 10,272 [10,123–10,842] ±4% | 10,883 [10,670–11,350] ±3% |
| filter 0.1%: recall@10, recall | 1.00 [1.00–1.00] ±0% | — | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% |
| filter 0.1%: rows returned, rows | 10.0 [10.0–10.0] ±0% | — | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% |
| filter 0.1%: µs a search, µs | 266 [265–271] ±1% | — | 354 [350–360] ±1% | 356 [350–364] ±2% | 353 [350–354] ±1% |
| filter 0.01%: recall@10, recall | 1.00 [1.00–1.00] ±0% | — | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% |
| filter 0.01%: rows returned, rows | 10.0 [10.0–10.0] ±0% | — | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% |
| filter 0.01%: µs a search, µs | 273 [270–286] ±3% | — | 227 [216–230] ±3% | 228 [217–230] ±3% | 226 [213–227] ±4% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — pgvector 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T10:28:35+00:00

**OpenAI embeddings** (`bash bench/all.sh ann-dbpedia`):
`text-embedding-3-large`, 99,000 × 1536, cosine (`bench/ann/data.py
dbpedia-openai3-1536-100k`: Qdrant's set), indexed at once, `m` 16,
`ef_construction` 64, 1,000 queries; nucleus in `f32`, `int8` and
`binary` (rescored), pgvector's `vector`, `halfvec` and `bit` re-ranked
(its documented ways to the same codes).  "server µs" is `EXPLAIN
ANALYZE`'s over 200 queries, the query vector taken by a subquery on
pgvector's side (the clients differ: nucleus's Python client sends the
6 KB vector).

| ann-dbpedia | nucleus | pgvector | pgvector (half) | pgvector (binary) |
|---|---:|---:|---:|---:|
| load, s | 3.48 [3.41–3.48] ±1% | 6.08 [4.14–7.11] ±24% ⚠ | 5.85 [3.58–7.18] ±26% ⚠ | 3.53 [3.51–3.58] ±1% |
| index build, s | 8.01 [7.94–8.06] ±1% | 59.5 [58.9–59.8] ±1% | 56.7 [55.2–57.4] ±1% | 3.40 [3.38–3.49] ±2% |
| index, resident, MB | 680 [678–701] ±2% | — | — | — |
| index, on disk, MB | 394 [394–394] ±0% | 773 [773–773] ±0% | 386 [386–386] ±0% | 47.0 [47.0–47.0] ±0% |
| ef 10: recall@10, recall | 0.852 [0.852–0.852] ±0% | 0.812 [0.806–0.819] ±1% | 0.809 [0.807–0.816] ±1% | 0.615 [0.609–0.615] ±1% |
| ef 10: µs a search, µs | 593 [553–598] ±4% | 1,113 [1,105–1,196] ±4% | 1,076 [1,050–1,112] ±3% | 290 [282–301] ±3% |
| ef 10: QPS, 10 clients, QPS | 9,585 [9,375–10,212] ±4% | 5,171 [5,132–5,324] ±3% | 5,641 [5,558–5,693] ±2% | 15,763 [15,060–16,114] ±3% |
| ef 20: recall@10, recall | 0.852 [0.852–0.852] ±0% | 0.893 [0.888–0.894] ±0% | 0.893 [0.891–0.894] ±0% | 0.818 [0.813–0.819] ±0% |
| ef 20: µs a search, µs | 501 [496–519] ±2% | 1,542 [1,524–1,592] ±2% | 1,509 [1,481–1,526] ±2% | 472 [465–473] ±1% |
| ef 20: QPS, 10 clients, QPS | 10,710 [9,710–11,011] ±7% | 3,833 [3,736–3,958] ±3% | 4,177 [4,108–4,256] ±8% | 10,846 [10,296–10,928] ±3% |
| ef 40: recall@10, recall | 0.929 [0.929–0.929] ±0% | 0.947 [0.946–0.947] ±0% | 0.947 [0.946–0.949] ±0% | 0.917 [0.910–0.917] ±0% |
| ef 40: µs a search, µs | 603 [595–631] ±3% | 2,403 [2,358–2,441] ±1% | 2,294 [2,252–2,335] ±2% | 755 [744–769] ±2% |
| ef 40: QPS, 10 clients, QPS | 9,369 [9,306–9,626] ±2% | 2,574 [2,474–2,701] ±4% | 2,892 [2,816–2,936] ±2% | 6,562 [6,256–6,652] ±3% |
| ef 80: recall@10, recall | 0.964 [0.964–0.964] ±0% | 0.974 [0.973–0.975] ±0% | 0.974 [0.973–0.974] ±0% | 0.952 [0.952–0.953] ±0% |
| ef 80: µs a search, µs | 773 [762–774] ±1% | 3,879 [3,841–3,919] ±1% | 3,751 [3,667–3,815] ±2% | 1,071 [1,068–1,132] ±3% |
| ef 80: QPS, 10 clients, QPS | 7,399 [6,366–7,533] ±9% | 1,669 [1,532–1,710] ±5% | 1,871 [1,794–1,914] ±3% | 4,882 [4,876–5,037] ±2% |
| ef 200: recall@10, recall | 0.989 [0.989–0.989] ±0% | 0.991 [0.991–0.991] ±0% | 0.991 [0.991–0.991] ±0% | 0.969 [0.969–0.970] ±0% |
| ef 200: µs a search, µs | 1,208 [1,194–1,212] ±1% | 7,984 [7,872–8,047] ±1% | 7,581 [7,515–7,669] ±1% | 1,562 [1,538–1,613] ±2% |
| ef 200: QPS, 10 clients, QPS | 4,876 [4,577–4,917] ±4% | 884 [864–901] ±2% | 955 [870–988] ±7% | 3,222 [3,187–3,320] ±2% |
| ef 40: server µs a search, µs | 512 [505–525] ±2% | 2,524 [2,468–2,622] ±3% | 2,369 [2,343–2,482] ±3% | 838 [789–841] ±4% |
| ef 60: server µs a search, µs | 571 [558–580] ±2% | 3,327 [3,262–3,395] ±2% | 3,047 [3,029–3,233] ±3% | 1,013 [974–1,031] ±3% |
| ef 80: server µs a search, µs | 655 [628–672] ±3% | 4,009 [3,947–4,119] ±2% | 3,854 [3,799–3,928] ±2% | 1,085 [1,041–1,095] ±3% |
| ef 120: server µs a search, µs | 814 [802–816] ±1% | — | — | — |
| ef 200: server µs a search, µs | 1,074 [1,056–1,107] ±2% | 8,301 [7,987–8,546] ±3% | 7,809 [7,656–7,870] ±1% | 1,604 [1,538–1,636] ±3% |
| ef 60: recall@10, recall | — | 0.965 [0.964–0.966] ±0% | 0.966 [0.964–0.966] ±0% | 0.943 [0.941–0.945] ±0% |
| ef 60: µs a search, µs | — | 3,110 [3,072–3,149] ±1% | 3,005 [2,967–3,035] ±1% | 1,005 [986–1,007] ±1% |
| ef 60: QPS, 10 clients, QPS | — | 2,006 [1,753–2,072] ±7% | 2,248 [2,102–2,297] ±6% | 5,039 [4,801–5,267] ±5% |
| ef 20: server µs a search, µs | — | 1,733 [1,659–1,864] ±5% | 1,642 [1,576–1,679] ±3% | 498 [480–509] ±3% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus, pgvector 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T10:38:01+00:00

**By a stored row's vector** (`bash bench/all.sh ann-related`;
`bench/ann/related.py`, the same 99,000 rows, `ef_search` 40, both
clients sending an id): "more like this", `ORDER BY v <=> (SELECT v FROM t
WHERE id = $1) LIMIT 10` with the row left out, and each of 1,000 rows'
ten nearest, `FROM t a, LATERAL (SELECT … ORDER BY b.v <=> a.v LIMIT
10)` — nucleus runs the lateral side's searches on its workers, a row
each; PostgreSQL's nested loop runs its inner index scans on one process:

| ann-related | nucleus | nucleus, one thread | pgvector |
|---|---:|---:|---:|
| index build, s | 35.7 [35.6–35.9] ±0% | — | 59.8 [59.7–59.9] ±0% |
| more like (ef 40): recall@10, recall | 0.965 [0.965–0.965] ±0% | — | 0.975 [0.973–0.979] ±0% |
| more like (ef 40): µs a search, µs | 1,028 [1,003–1,097] ±5% | — | 2,345 [2,313–2,375] ±1% |
| more like (ef 40): server µs, µs | 879 [843–889] ±3% | — | 2,608 [2,572–2,640] ±1% |
| lateral, 1000 rows' nearest, ms | 149 [147–149] ±1% | 856 [854–859] ±0% | 2,325 [2,290–2,331] ±1% |
| lateral: recall@10, recall | 0.982 [0.982–0.982] ±0% | 0.982 [0.982–0.982] ±0% | 0.988 [0.986–0.988] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — pgvector 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T11:15:09+00:00

**Sparse vectors searched exactly** (`bash bench/all.sh ann-sparse`;
PLAN.md §M14.12 V6b, `bench/ann/sparse.py`): 100,000 SPLADE-like rows — a
vocabulary of 30,522, ~120 terms a document and ~20 a query by a Zipf
law, log-normal weights — as `sparsevec(30522)`; 200 queries, `SELECT id
FROM t ORDER BY v <#> q LIMIT 10`, one client; both read every row on
several workers:

| ann-sparse | nucleus | nucleus, one thread | pgvector |
|---|---:|---:|---:|
| load, s | 6.04 [6.01–6.08] ±0% | — | 1.31 [1.31–1.32] ±0% |
| recall@10, recall | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% |
| µs a search, µs | 19,813 [19,655–19,969] ±1% | 74,112 [73,609–74,392] ±0% | 20,567 [20,365–21,099] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — pgvector 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T11:22:23+00:00

The storage models behind the index (`bench/ann/`, calls in a process,
PLAN.md §M14.12 V4) are recorded in the plan.

## Statistics

Aggregates, window functions, exact quantiles, sketches, sampling and
drift tests over doubles and integers (`bash bench/all.sh stats`; PLAN.md
§M14.27, `bench/stats/aggs.py`): a table of 10 million rows — `x` on a
large offset (1e9 + a fraction), `y` spread over 30 decades, `id` the
integers, `g` a thousand groups — beside PostgreSQL (its parallel
workers), numpy (one core, the same data in an array: not a table scan)
and, for the tests PostgreSQL has not, scipy and scikit-learn on the same
values; nucleus also with a `work_mem` of 1 GB (64 MB per worker by
default, PostgreSQL's 4 MB):

| stats | nucleus (ms) | postgresql (ms) | numpy (ms) | python (ms) | nucleus, work_mem 1GB (work_mem 1GB) (ms) |
|---|---:|---:|---:|---:|---:|
| sum | 102 [102–104] ±1% | 85.1 [84.5–86.6] ±1% | 1.95 [1.88–2.13] ±5% | — | 103 [102–105] ±1% |
| avg spread | 105 [101–109] ±3% | 104 [103–105] ±1% | — | — | 106 [104–108] ±1% |
| sum 1000 groups | 119 [117–125] ±3% | 171 [171–173] ±1% | — | — | 120 [115–128] ±5% |
| avg 1M groups | 1,496 [1,467–1,530] ±2% | 1,634 [1,597–1,658] ±1% | — | — | 1,649 [1,566–1,780] ±5% |
| variance | 106 [106–111] ±2% | 93.0 [92.7–94.1] ±1% | 7.31 [7.16–8.79] ±9% | — | 108 [105–114] ±3% |
| stddev 1000 groups | 134 [127–139] ±4% | 182 [182–184] ±0% | — | — | 129 [125–131] ±2% |
| variance of integers | 93.7 [92.0–96.5] ±2% | 149 [148–150] ±1% | — | — | 92.2 [90.2–97.9] ±3% |
| skewness, kurtosis | 193 [187–202] ±3% | — | — | — | 191 [190–200] ±2% |
| corr | 143 [142–148] ±2% | 100 [99.5–102] ±1% | 34.7 [34.6–35.0] ±0% | — | 142 [137–152] ±4% |
| regression 1000 groups | 188 [180–192] ±3% | 197 [197–198] ±0% | — | — | 187 [179–190] ±2% |
| row_number by groups | 2,138 [2,128–2,197] ±1% | 4,597 [4,579–4,603] ±0% | — | — | 1,952 [1,939–1,965] ±1% |
| top 3 by groups | 1,962 [1,954–1,972] ±0% | 3,701 [3,688–3,720] ±0% | — | — | 1,762 [1,749–1,776] ±1% |
| running sum | 1,987 [1,980–2,010] ±1% | 2,311 [2,309–2,323] ±0% | — | — | 2,018 [1,983–2,061] ±2% |
| moving avg 10 rows | 4,423 [4,374–4,502] ±1% | 8,544 [8,518–8,607] ±0% | — | — | 4,496 [4,467–4,518] ±0% |
| lag | 1,913 [1,880–1,942] ±1% | 2,134 [2,128–2,147] ±0% | — | — | 1,918 [1,899–1,943] ±1% |
| median | 513 [497–523] ±2% | 1,947 [1,941–1,958] ±0% | 25.4 [25.3–26.7] ±2% | — | 184 [184–189] ±1% |
| p50, p95, p99 | 515 [511–528] ±1% | 2,186 [2,164–2,195] ±1% | — | — | 190 [187–203] ±3% |
| percentile_disc | 484 [470–492] ±2% | 2,046 [2,032–2,053] ±0% | 43.4 [43.4–44.9] ±1% | — | 179 [177–186] ±2% |
| median 1000 groups | 464 [458–492] ±3% | 463 [456–473] ±2% | — | — | 193 [188–200] ±2% |
| mode | 563 [553–622] ±5% | 1,094 [1,087–1,109] ±1% | — | — | 215 [206–224] ±3% |
| histogram | 97.2 [95.5–117] ±9% | — | — | — | 96.0 [94.8–97.4] ±1% |
| count distinct | 86.8 [79.0–88.7] ±5% | 401 [393–415] ±2% | — | — | 79.8 [78.5–83.5] ±2% |
| approx_count_distinct | 92.7 [91.1–97.0] ±3% | — | — | — | 92.1 [91.2–100] ±4% |
| approx_count_distinct 1000 groups | 276 [273–280] ±1% | — | — | — | 280 [272–284] ±2% |
| count distinct 1000 groups | 101 [98.2–105] ±3% | 789 [779–812] ±2% | — | — | 98.8 [96.4–107] ±4% |
| approx_percentile | 111 [104–115] ±4% | — | — | — | 106 [102–112] ±4% |
| approx_top_k | 175 [172–188] ±4% | — | — | — | 175 [172–181] ±2% |
| tablesample | 112 [110–119] ±3% | 191 [188–193] ±1% | — | — | 110 [110–116] ±2% |
| tablesample repeatable | 157 [154–167] ±4% | 194 [191–199] ±2% | — | — | 157 [147–161] ±4% |
| hash_unit split | 293 [283–301] ±2% | — | — | — | 299 [289–336] ±6% |
| reservoir_sample | 101 [99.3–103] ±1% | — | — | — | 99.4 [98.4–106] ±3% |
| order by random() limit | 159 [158–161] ±1% | 599 [598–611] ±1% | — | — | 156 [153–161] ±2% |
| ks_test | 635 [620–646] ±1% | — | — | 396 [389–399] ±1% | 264 [251–265] ±2% |
| auc | 712 [702–716] ±1% | — | — | 1,284 [1,275–1,289] ±0% | 345 [341–348] ±1% |
| psi | 633 [631–640] ±1% | — | — | — | 258 [256–277] ±3% |
| chisq_test | 182 [181–186] ±1% | — | — | — | 186 [181–194] ±3% |
| js_divergence | 184 [181–194] ±3% | — | — | — | 185 [183–188] ±1% |
| summarize | 436 [433–449] ±2% | — | — | — | 443 [432–453] ±2% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew), python scipy 1.18.1, scikit-learn 1.9.1 — nucleus 18041cf93+changes — 2026-11-21T07:47:43+00:00

What the times do not show: nucleus gives the same bits every run, the
exact value rounded once — the variance of `x`, `corr(x, id)` and the
running sums are exact where PostgreSQL's double accumulators differ from
run to run past the ninth digit and lose the fractions past 10¹⁶; the
quantiles are PostgreSQL's and numpy's bit for bit; the sketches' bytes
are the same in every executor and run; a `REPEATABLE` sample is the
same rows on every machine and in row and column storage (PostgreSQL's
depends on the pages).  Ordered by `id` a window sorts nothing: a
table's scan gives the primary key's order.  Against the native, tuned
PostgreSQL a few plain aggregates are behind (`sum` 1.2× its time,
`variance` 1.14×, `corr` 1.43×): its parallel workers read 8 GB of
buffers; PLAN.md §M14.27's notes keep the
comparison with PostgreSQL in Docker.

## Text analysis

**Analyzers and text queries** (`bash bench/all.sh analyzers`; PLAN.md
§M14.21 T1, T2, T4, T5, `bench/text/analyzers.py`): BEIR FiQA's 57,638
documents (title and text, 44 MB) in a table; each analyzer's terms
counted over every document through SQL — `sum(cardinality(tokenize(body,
a)))` — against PostgreSQL's `to_tsvector` (which counts distinct terms
a document); text queries without an index — `count(*) WHERE body @@ q`
against `to_tsvector('simple', body) @@ q::tsquery` — and with one,
`CREATE INDEX … USING text (body)` against a GIN on `to_tsvector('simple',
body)`; both also on one thread:

| analyzers | nucleus (ms) | nucleus, one thread (ms) | postgresql (ms) | postgresql, one thread (postgresql) (ms) |
|---|---:|---:|---:|---:|
| analyzer simple | 180 [179–195] ±4% | 1,040 [1,026–1,073] ±2% | 927 [925–933] ±0% | 2,720 [2,698–2,729] ±0% |
| analyzer porter | 317 [314–328] ±2% | 2,021 [1,993–2,063] ±1% | — | — |
| analyzer english | 319 [307–337] ±4% | 2,047 [2,024–2,088] ±1% | 1,137 [1,132–1,139] ±0% | 3,319 [3,317–3,352] ±0% |
| analyzer hungarian | 312 [308–336] ±4% | 1,968 [1,966–2,003] ±1% | 1,340 [1,326–1,348] ±1% | 3,904 [3,893–3,955] ±1% |
| analyzer trigram | 465 [459–480] ±2% | 2,737 [2,714–2,744] ±0% | — | — |
| @@ market, no index | 124 [122–126] ±1% | 750 [748–757] ±0% | 927 [925–927] ±0% | 2,721 [2,704–2,740] ±0% |
| @@ stock market, no index | 127 [124–132] ±3% | 764 [760–773] ±1% | 928 [926–930] ±0% | 2,725 [2,700–2,740] ±1% |
| @@ stock AND market, no index | 124 [123–137] ±4% | 760 [759–768] ±1% | 928 [926–932] ±0% | 2,704 [2,699–2,719] ±0% |
| @@ "stock market", no index | 127 [122–143] ±6% | 762 [761–763] ±0% | 928 [927–931] ±0% | 2,712 [2,695–2,725] ±0% |
| @@ invest*, no index | 124 [122–129] ±2% | 755 [753–771] ±1% | 928 [927–938] ±0% | 2,720 [2,703–2,750] ±1% |
| @@ markte~1, no index | 144 [137–148] ±4% | 863 [857–867] ±1% | — | — |
| CREATE INDEX | 520 [512–540] ±2% | — | 1,654 [1,643–1,736] ±2% | — |
| @@ market, indexed | 0.537 [0.523–0.562] ±3% | — | 3.43 [3.28–3.71] ±5% | — |
| @@ stock market, indexed | 0.769 [0.749–0.780] ±2% | — | 3.32 [3.30–3.84] ±7% | — |
| @@ stock AND market, indexed | 0.325 [0.318–0.342] ±3% | — | 0.945 [0.909–1.07] ±7% | — |
| @@ "stock market", indexed | 0.683 [0.681–0.688] ±0% | — | 231 [231–240] ±2% | — |
| @@ invest*, indexed | 0.992 [0.987–1.01] ±1% | — | 4.11 [4.06–4.90] ±8% | — |
| @@ markte~1, indexed | 4.00 [3.98–4.02] ±0% | — | — | — |
| @@ market, rows read, indexed | 8.53 [7.91–8.93] ±5% | — | 10.2 [10.2–11.3] ±5% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T11:38:47+00:00

### BM25 ranking

`bench/text/beir.py` (PLAN.md §M14.21 T5, T5b, T5c, T6): BEIR's SciFact,
NFCorpus, ArguAna and FiQA (title and text), a text index, and each
judged test query — its query-syntax characters taken out, a bag of
words — as `SELECT id FROM docs WHERE body @@ q ORDER BY score(body @@ q)
DESC, id LIMIT 11`; nDCG@10 as Anserini's trec_eval computes it (every
judged query; ArguAna's query document left out); the mean and median ms
of a query through the protocol.  Against ParadeDB's `pg_search` 0.26.6
(Tantivy's BM25 in PostgreSQL 18, in Docker on the same machine; `body
||| q ORDER BY pdb.score(id) DESC LIMIT 11`, timed by `\timing` inside
the container) — both with Snowball's English stemmer and stop words
(`english`), k1 1.2, b 0.75 (`bash bench/all.sh bm25`):

| bm25 | nucleus | paradedb |
|---|---:|---:|
| scifact: nDCG@10, nDCG@10 | 0.689 [0.689–0.689] ±0% | 0.684 [0.684–0.684] ±0% |
| scifact: CREATE INDEX, ms | 121 [119–128] ±3% | 287 [283–340] ±8% |
| scifact: mean, ms | 0.170 [0.169–0.198] ±8% | 0.855 [0.831–0.897] ±3% |
| scifact: p50, ms | 0.166 [0.164–0.187] ±7% | 0.810 [0.795–0.830] ±2% |
| nfcorpus: nDCG@10, nDCG@10 | 0.329 [0.329–0.329] ±0% | 0.322 [0.322–0.322] ±0% |
| nfcorpus: CREATE INDEX, ms | 99.8 [97.1–102] ±2% | 208 [207–233] ±5% |
| nfcorpus: mean, ms | 0.122 [0.120–0.125] ±1% | 0.433 [0.422–0.447] ±2% |
| nfcorpus: p50, ms | 0.125 [0.123–0.127] ±1% | 0.406 [0.391–0.422] ±3% |
| arguana: nDCG@10, nDCG@10 | 0.496 [0.496–0.496] ±0% | 0.481 [0.481–0.481] ±0% |
| arguana: CREATE INDEX, ms | 154 [151–163] ±3% | 337 [329–366] ±5% |
| arguana: mean, ms | 2.23 [2.19–2.24] ±1% | 5.45 [5.43–5.47] ±0% |
| arguana: p50, ms | 0.815 [0.804–0.817] ±1% | 4.93 [4.93–4.95] ±0% |
| fiqa: nDCG@10, nDCG@10 | 0.253 [0.253–0.253] ±0% | 0.250 [0.250–0.250] ±0% |
| fiqa: CREATE INDEX, ms | 596 [592–608] ±1% | 677 [670–681] ±1% |
| fiqa: mean, ms | 0.350 [0.342–0.370] ±3% | 1.66 [1.63–1.67] ±1% |
| fiqa: p50, ms | 0.322 [0.313–0.342] ±4% | 1.62 [1.59–1.64] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — paradedb pg_search 0.26.6 — nucleus 18041cf93+changes — 2026-11-20T11:51:37+00:00

With a second condition (`bash bench/all.sh bm25-filters`; PLAN.md
§M14.21 T5a, T5b: `AND n % 100 < p`, `n` a row's number): the index
reads its matches best first until 10 pass, against the same order by
`score(…) + 0`, which the index does not rank by (every match read — by a
merge with the table's scan — and scored on its text) — the same rows:

| bm25-filters | nucleus | nucleus, ranked by the index | nucleus, every match scored |
|---|---:|---:|---:|
| scifact: nDCG@10, nDCG@10 | 0.689 [0.689–0.689] ±0% | — | — |
| scifact: CREATE INDEX, ms | 125 [124–136] ±5% | — | — |
| scifact: mean, ms | 0.184 [0.168–0.204] ±10% | — | — |
| scifact: p50, ms | 0.178 [0.163–0.197] ±10% | — | — |
| scifact: filter 50%, mean, ms | — | 0.206 [0.204–0.214] ±3% | 83.5 [83.3–83.9] ±0% |
| scifact: filter 50%, p50, ms | — | 0.196 [0.195–0.204] ±2% | 87.4 [87.4–87.9] ±0% |
| scifact: filter 10%, mean, ms | — | 0.450 [0.426–0.450] ±3% | 18.4 [18.3–18.4] ±0% |
| scifact: filter 10%, p50, ms | — | 0.434 [0.416–0.437] ±3% | 19.0 [18.9–19.2] ±1% |
| scifact: filter 1%, mean, ms | — | 2.08 [2.07–2.19] ±3% | 4.31 [4.29–4.39] ±1% |
| scifact: filter 1%, p50, ms | — | 2.05 [2.04–2.09] ±1% | 4.54 [4.49–4.58] ±1% |
| nfcorpus: nDCG@10, nDCG@10 | 0.329 [0.329–0.329] ±0% | — | — |
| nfcorpus: CREATE INDEX, ms | 103 [100–107] ±3% | — | — |
| nfcorpus: mean, ms | 0.119 [0.118–0.139] ±9% | — | — |
| nfcorpus: p50, ms | 0.123 [0.123–0.138] ±7% | — | — |
| nfcorpus: filter 50%, mean, ms | — | 0.142 [0.141–0.164] ±9% | 17.4 [17.4–17.6] ±1% |
| nfcorpus: filter 50%, p50, ms | — | 0.145 [0.144–0.156] ±5% | 7.37 [7.27–7.74] ±3% |
| nfcorpus: filter 10%, mean, ms | — | 0.344 [0.275–0.348] ±13% ⚠ | 4.58 [4.58–4.66] ±1% |
| nfcorpus: filter 10%, p50, ms | — | 0.352 [0.293–0.353] ±10% ⚠ | 2.18 [2.16–2.21] ±1% |
| nfcorpus: filter 1%, mean, ms | — | 0.872 [0.860–0.885] ±1% | 1.28 [1.25–1.31] ±2% |
| nfcorpus: filter 1%, p50, ms | — | 0.553 [0.545–0.554] ±1% | 0.700 [0.677–0.738] ±4% |
| arguana: nDCG@10, nDCG@10 | 0.496 [0.496–0.496] ±0% | — | — |
| arguana: CREATE INDEX, ms | 157 [151–157] ±2% | — | — |
| arguana: mean, ms | 2.23 [2.21–2.24] ±1% | — | — |
| arguana: p50, ms | 0.813 [0.809–0.815] ±0% | — | — |
| arguana: filter 50%, mean, ms | — | 2.28 [2.25–2.29] ±1% | 222 [221–224] ±1% |
| arguana: filter 50%, p50, ms | — | 0.846 [0.842–0.854] ±1% | 221 [219–222] ±1% |
| arguana: filter 10%, mean, ms | — | 2.47 [2.46–2.54] ±2% | 51.4 [51.1–51.5] ±0% |
| arguana: filter 10%, p50, ms | — | 1.05 [1.04–1.06] ±1% | 50.9 [50.7–51.2] ±0% |
| arguana: filter 1%, mean, ms | — | 4.41 [4.36–4.46] ±1% | 10.2 [10.2–10.2] ±0% |
| arguana: filter 1%, p50, ms | — | 3.03 [3.02–3.07] ±1% | 10.1 [10.1–10.1] ±0% |
| fiqa: nDCG@10, nDCG@10 | 0.253 [0.253–0.253] ±0% | — | — |
| fiqa: CREATE INDEX, ms | 583 [581–589] ±1% | — | — |
| fiqa: mean, ms | 0.362 [0.346–0.366] ±3% | — | — |
| fiqa: p50, ms | 0.333 [0.319–0.340] ±3% | — | — |
| fiqa: filter 50%, mean, ms | — | 0.479 [0.478–0.490] ±1% | 419 [416–420] ±0% |
| fiqa: filter 50%, p50, ms | — | 0.447 [0.442–0.458] ±2% | 412 [412–415] ±0% |
| fiqa: filter 10%, mean, ms | — | 0.693 [0.690–0.714] ±2% | 93.4 [93.1–93.7] ±0% |
| fiqa: filter 10%, p50, ms | — | 0.624 [0.619–0.628] ±1% | 92.5 [91.5–92.7] ±1% |
| fiqa: filter 1%, mean, ms | — | 2.85 [2.83–2.87] ±1% | 18.5 [18.4–18.6] ±0% |
| fiqa: filter 1%, p50, ms | — | 2.81 [2.80–2.81] ±0% | 17.9 [17.9–18.0] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T11:54:23+00:00

Ranked by the index, a query costs about what it does unfiltered while
half or a tenth of the rows pass; at one in a hundred the index reads
further down its list before ten pass, and is still the faster: 2–6×
where queries match many documents, 1.5× on NFCorpus, whose queries
match few.

With each row's snippet (`bash bench/all.sh bm25-snippets`; PLAN.md
§M14.21 T6: `SELECT id, snippet(body @@ q) … LIMIT 11`, 35 words;
ParadeDB `pdb.snippet(body)`, Tantivy's 150 characters):

| bm25-snippets | nucleus | nucleus, snippets | paradedb, snippets (paradedb) |
|---|---:|---:|---:|
| scifact: nDCG@10, nDCG@10 | 0.689 [0.689–0.689] ±0% | — | 0.684 [0.684–0.684] ±0% |
| scifact: CREATE INDEX, ms | 120 [119–123] ±2% | — | 306 [281–312] ±5% |
| scifact: mean, ms | 0.170 [0.168–0.190] ±21% ⚠ | 1.12 [1.10–1.13] ±7% | 1.45 [1.44–1.50] ±2% |
| scifact: p50, ms | 0.167 [0.164–0.187] ±9% | 1.08 [1.07–1.09] ±2% | 1.41 [1.40–1.45] ±2% |
| nfcorpus: nDCG@10, nDCG@10 | 0.329 [0.329–0.329] ±0% | — | 0.322 [0.322–0.322] ±0% |
| nfcorpus: CREATE INDEX, ms | 104 [101–108] ±7% | — | 212 [207–216] ±2% |
| nfcorpus: mean, ms | 0.120 [0.119–0.125] ±12% ⚠ | 0.893 [0.889–0.906] ±1% | 0.916 [0.902–0.981] ±4% |
| nfcorpus: p50, ms | 0.124 [0.123–0.125] ±8% | 1.00 [0.999–1.01] ±1% | 0.981 [0.967–0.989] ±1% |
| arguana: nDCG@10, nDCG@10 | 0.496 [0.496–0.496] ±0% | — | 0.481 [0.481–0.481] ±0% |
| arguana: CREATE INDEX, ms | 154 [154–157] ±8% | — | 335 [329–341] ±2% |
| arguana: mean, ms | 2.22 [2.22–2.23] ±1% | 3.12 [3.11–3.13] ±1% | 7.09 [7.05–7.12] ±0% |
| arguana: p50, ms | 0.810 [0.808–0.814] ±1% | 1.70 [1.70–1.71] ±1% | 6.46 [6.43–6.48] ±0% |
| fiqa: nDCG@10, nDCG@10 | 0.253 [0.253–0.253] ±0% | — | 0.250 [0.250–0.250] ±0% |
| fiqa: CREATE INDEX, ms | 582 [580–592] ±3% | — | 665 [655–682] ±2% |
| fiqa: mean, ms | 0.344 [0.341–0.348] ±3% | 0.975 [0.969–0.982] ±2% | 2.16 [2.16–2.17] ±0% |
| fiqa: p50, ms | 0.319 [0.316–0.323] ±3% | 0.934 [0.927–0.938] ±2% | 2.12 [2.11–2.13] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — paradedb pg_search 0.26.6 — nucleus 18041cf93+changes — 2026-11-20T13:14:58+00:00

**MS MARCO passage** (`bash bench/all.sh msmarco`; PLAN.md §M14.21 T5b,
T5c): 8,841,823 passages, BEIR's dev split, its first 1,000 queries,
`english`, k1 1.2, b 0.75 — the same query, the same nDCG@10 measure;
the server's index cache a quarter of the machine's memory by default;
with 50, 10 and 1 % filters (a 1 % filter's time is its rows: about
1,100 found rows read a lookup each before 11 pass):

| msmarco | nucleus | nucleus, ranked by the index | paradedb |
|---|---:|---:|---:|
| msmarco: nDCG@10, nDCG@10 | 0.260 [0.260–0.260] ±0% | — | 0.258 [0.258–0.258] ±0% |
| msmarco: CREATE INDEX, ms | 41,567 [41,054–42,125] ±1% | — | 72,987 [72,041–73,299] ±1% |
| msmarco: mean, ms | 1.96 [1.91–2.08] ±4% | — | 6.28 [6.08–6.30] ±2% |
| msmarco: p50, ms | 1.30 [1.27–1.37] ±4% | — | 6.68 [6.48–6.68] ±2% |
| msmarco: filter 50%, mean, ms | — | 3.22 [3.17–3.25] ±2% | — |
| msmarco: filter 50%, p50, ms | — | 2.54 [2.52–2.58] ±2% | — |
| msmarco: filter 10%, mean, ms | — | 5.85 [5.11–6.09] ±9% | — |
| msmarco: filter 10%, p50, ms | — | 4.84 [4.08–5.17] ±11% ⚠ | — |
| msmarco: filter 1%, mean, ms | — | 20.9 [16.3–22.7] ±15% ⚠ | — |
| msmarco: filter 1%, p50, ms | — | 20.3 [15.5–22.1] ±16% ⚠ | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — paradedb pg_search 0.26.6 — nucleus 18041cf93+changes — 2026-11-20T13:22:47+00:00

ParadeDB here with `shared_buffers` 1 GB, `maintenance_work_mem` 512 MB
and two build workers: with its image's settings (2.4 GB, five workers
of 620 MB) the index build outgrew Docker's 4 GB VM and a backend was
killed, twice.  The 1 % filter's time spreads between runs (16–24 ms
here, 26–37 in the whole run before): its ~1,100
lookups a query go where the rows are, so it follows what the machine's
memory holds at the time.

### Hybrid search

`bench/text/hybrid.py` (PLAN.md §M14.21 T7a; `bash bench/all.sh
hybrid`): the same corpora and queries with BAAI/bge-small-en-v1.5's
vectors (`bench/text/embed.py`, fastembed, 384 dimensions) in a
`vector(384)` column with an HNSW index (cosine, pgvector's defaults)
beside the text index (`english`, 1.2 / 0.75): BM25 alone, the vectors
alone, `ORDER BY rrf(score(body @@ q), emb <=> $1) DESC, id LIMIT 11` and
`convex(…, 0.5)`; ParadeDB 0.26.6 with pgvector 0.8.6 the same, its RRF
as its documentation writes it (a CTE a ranking's first 100, `RANK()`, a
`FULL OUTER JOIN`; `hnsw.ef_search` 100):

| hybrid | nucleus | paradedb |
|---|---:|---:|
| scifact text: nDCG@10, nDCG@10 | 0.689 [0.689–0.689] ±0% | 0.684 [0.684–0.684] ±0% |
| scifact text: mean, ms | 0.250 [0.249–0.254] ±1% | 0.822 [0.814–0.829] ±1% |
| scifact text: p50, ms | 0.246 [0.244–0.250] ±1% | 0.797 [0.783–0.797] ±1% |
| scifact vector: nDCG@10, nDCG@10 | 0.713 [0.713–0.713] ±0% | 0.716 [0.716–0.716] ±0% |
| scifact vector: mean, ms | 0.271 [0.265–0.284] ±3% | 0.455 [0.431–0.470] ±4% |
| scifact vector: p50, ms | 0.270 [0.264–0.280] ±2% | 0.454 [0.434–0.469] ±3% |
| scifact rrf: nDCG@10, nDCG@10 | 0.733 [0.733–0.733] ±0% | 0.733 [0.733–0.733] ±0% |
| scifact rrf: mean, ms | 1.31 [1.27–1.36] ±2% | 7.18 [7.08–7.45] ±2% |
| scifact rrf: p50, ms | 1.29 [1.27–1.30] ±1% | 7.05 [6.93–7.14] ±1% |
| scifact convex: nDCG@10, nDCG@10 | 0.744 [0.744–0.744] ±0% | — |
| scifact convex: mean, ms | 1.44 [1.44–1.53] ±3% | — |
| scifact convex: p50, ms | 1.44 [1.43–1.49] ±1% | — |
| nfcorpus text: nDCG@10, nDCG@10 | 0.329 [0.329–0.329] ±0% | 0.322 [0.322–0.322] ±0% |
| nfcorpus text: mean, ms | 0.163 [0.162–0.167] ±1% | 0.436 [0.427–0.472] ±4% |
| nfcorpus text: p50, ms | 0.166 [0.164–0.168] ±1% | 0.414 [0.406–0.462] ±5% |
| nfcorpus vector: nDCG@10, nDCG@10 | 0.316 [0.316–0.316] ±0% | 0.330 [0.330–0.332] ±0% |
| nfcorpus vector: mean, ms | 0.209 [0.207–0.217] ±2% | 0.412 [0.406–0.428] ±2% |
| nfcorpus vector: p50, ms | 0.209 [0.207–0.214] ±1% | 0.408 [0.403–0.425] ±2% |
| nfcorpus rrf: nDCG@10, nDCG@10 | 0.360 [0.360–0.360] ±0% | 0.364 [0.364–0.364] ±0% |
| nfcorpus rrf: mean, ms | 1.13 [1.10–1.32] ±8% | 4.94 [4.92–4.98] ±0% |
| nfcorpus rrf: p50, ms | 1.17 [1.16–1.19] ±1% | 4.85 [4.80–4.87] ±1% |
| nfcorpus convex: nDCG@10, nDCG@10 | 0.363 [0.363–0.363] ±0% | — |
| nfcorpus convex: mean, ms | 1.27 [1.26–1.37] ±4% | — |
| nfcorpus convex: p50, ms | 1.34 [1.33–1.40] ±2% | — |
| arguana text: nDCG@10, nDCG@10 | 0.496 [0.496–0.496] ±0% | 0.481 [0.481–0.481] ±0% |
| arguana text: mean, ms | 2.28 [2.27–2.29] ±0% | 5.42 [5.41–5.47] ±0% |
| arguana text: p50, ms | 0.863 [0.856–0.878] ±1% | 4.93 [4.93–4.98] ±0% |
| arguana vector: nDCG@10, nDCG@10 | 0.597 [0.597–0.597] ±0% | 0.595 [0.595–0.595] ±0% |
| arguana vector: mean, ms | 0.222 [0.217–0.248] ±5% | 0.383 [0.375–0.386] ±1% |
| arguana vector: p50, ms | 0.216 [0.215–0.238] ±5% | 0.377 [0.371–0.381] ±1% |
| arguana rrf: nDCG@10, nDCG@10 | 0.598 [0.598–0.598] ±0% | 0.589 [0.589–0.589] ±0% |
| arguana rrf: mean, ms | 3.39 [3.38–3.41] ±0% | 6.58 [6.50–6.63] ±1% |
| arguana rrf: p50, ms | 2.00 [1.98–2.01] ±1% | 6.09 [5.99–6.11] ±1% |
| arguana convex: nDCG@10, nDCG@10 | 0.617 [0.617–0.617] ±0% | — |
| arguana convex: mean, ms | 3.56 [3.56–3.59] ±0% | — |
| arguana convex: p50, ms | 2.16 [2.16–2.18] ±0% | — |
| fiqa text: nDCG@10, nDCG@10 | 0.253 [0.253–0.253] ±0% | 0.250 [0.250–0.250] ±0% |
| fiqa text: mean, ms | 0.395 [0.380–0.402] ±2% | 1.62 [1.60–1.63] ±1% |
| fiqa text: p50, ms | 0.367 [0.355–0.375] ±2% | 1.59 [1.57–1.60] ±1% |
| fiqa vector: nDCG@10, nDCG@10 | 0.367 [0.367–0.367] ±0% | 0.373 [0.372–0.374] ±0% |
| fiqa vector: mean, ms | 0.343 [0.336–0.349] ±1% | 0.658 [0.643–0.710] ±4% |
| fiqa vector: p50, ms | 0.339 [0.333–0.345] ±1% | 0.648 [0.634–0.687] ±3% |
| fiqa rrf: nDCG@10, nDCG@10 | 0.357 [0.357–0.357] ±0% | 0.360 [0.359–0.360] ±0% |
| fiqa rrf: mean, ms | 1.97 [1.92–2.07] ±3% | 3.50 [3.46–3.60] ±2% |
| fiqa rrf: p50, ms | 1.94 [1.90–1.97] ±2% | 3.46 [3.41–3.58] ±2% |
| fiqa convex: nDCG@10, nDCG@10 | 0.383 [0.383–0.383] ±0% | — |
| fiqa convex: mean, ms | 2.12 [2.11–2.17] ±1% | — |
| fiqa convex: p50, ms | 2.10 [2.08–2.13] ±1% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — paradedb pg_search 0.26.6, pgvector 0.8.6 — nucleus 18041cf93+changes — 2026-11-20T13:53:35+00:00

With learned sparse vectors (`bash bench/all.sh hybrid-sparse`; PLAN.md
§M14.21 T7b): SPLADE++ (`prithivida/Splade_PP_en_v1`) in a
`sparsevec(30522)` column, nucleus's `USING sparse` index (exact) against
pgvector's HNSW on `sparsevec_ip_ops` (`ef_search` 40); `ORDER BY sv <#>
$1, id LIMIT 11`, and nucleus's `rrf` of all three rankings:

| hybrid-sparse | nucleus | paradedb |
|---|---:|---:|
| scifact: sparse index, ms | 79.5 [75.2–80.8] ±3% | — |
| scifact text: nDCG@10, nDCG@10 | 0.689 [0.689–0.689] ±0% | 0.684 [0.684–0.684] ±0% |
| scifact text: mean, ms | 0.271 [0.267–0.287] ±3% | 0.838 [0.827–0.848] ±1% |
| scifact text: p50, ms | 0.266 [0.262–0.274] ±2% | 0.810 [0.804–0.821] ±1% |
| scifact vector: nDCG@10, nDCG@10 | 0.713 [0.713–0.713] ±0% | 0.719 [0.719–0.719] ±0% |
| scifact vector: mean, ms | 0.270 [0.268–0.282] ±2% | 0.409 [0.406–0.419] ±2% |
| scifact vector: p50, ms | 0.270 [0.267–0.275] ±1% | 0.409 [0.407–0.419] ±1% |
| scifact rrf: nDCG@10, nDCG@10 | 0.733 [0.733–0.733] ±0% | 0.733 [0.733–0.733] ±0% |
| scifact rrf: mean, ms | 1.32 [1.29–1.39] ±3% | 7.96 [7.72–8.00] ±2% |
| scifact rrf: p50, ms | 1.30 [1.28–1.33] ±1% | 7.69 [7.58–7.91] ±2% |
| scifact convex: nDCG@10, nDCG@10 | 0.744 [0.744–0.744] ±0% | — |
| scifact convex: mean, ms | 1.49 [1.45–1.50] ±1% | — |
| scifact convex: p50, ms | 1.47 [1.46–1.48] ±1% | — |
| scifact splade: nDCG@10, nDCG@10 | 0.709 [0.709–0.709] ±0% | 0.699 [0.698–0.701] ±0% |
| scifact splade: mean, ms | 1.09 [1.07–1.11] ±1% | 0.814 [0.792–0.825] ±1% |
| scifact splade: p50, ms | 1.04 [1.03–1.05] ±1% | 0.807 [0.786–0.808] ±1% |
| scifact rrf3: nDCG@10, nDCG@10 | 0.737 [0.737–0.737] ±0% | — |
| scifact rrf3: mean, ms | 2.74 [2.71–2.87] ±2% | — |
| scifact rrf3: p50, ms | 2.69 [2.66–2.83] ±3% | — |
| nfcorpus: sparse index, ms | 58.4 [54.4–59.1] ±4% | — |
| nfcorpus text: nDCG@10, nDCG@10 | 0.329 [0.329–0.329] ±0% | 0.322 [0.322–0.322] ±0% |
| nfcorpus text: mean, ms | 0.172 [0.171–0.184] ±3% | 0.436 [0.434–0.441] ±1% |
| nfcorpus text: p50, ms | 0.177 [0.174–0.179] ±1% | 0.417 [0.405–0.426] ±2% |
| nfcorpus vector: nDCG@10, nDCG@10 | 0.316 [0.316–0.316] ±0% | 0.330 [0.330–0.333] ±0% |
| nfcorpus vector: mean, ms | 0.208 [0.206–0.230] ±5% | 0.386 [0.385–0.393] ±1% |
| nfcorpus vector: p50, ms | 0.205 [0.205–0.224] ±4% | 0.385 [0.381–0.391] ±1% |
| nfcorpus rrf: nDCG@10, nDCG@10 | 0.360 [0.360–0.360] ±0% | 0.364 [0.364–0.364] ±0% |
| nfcorpus rrf: mean, ms | 1.13 [1.11–1.22] ±4% | 5.42 [5.20–5.45] ±2% |
| nfcorpus rrf: p50, ms | 1.17 [1.16–1.19] ±1% | 5.31 [5.11–5.34] ±2% |
| nfcorpus convex: nDCG@10, nDCG@10 | 0.363 [0.363–0.363] ±0% | — |
| nfcorpus convex: mean, ms | 1.26 [1.25–1.27] ±1% | — |
| nfcorpus convex: p50, ms | 1.33 [1.32–1.34] ±0% | — |
| nfcorpus splade: nDCG@10, nDCG@10 | 0.343 [0.343–0.343] ±0% | 0.323 [0.322–0.324] ±0% |
| nfcorpus splade: mean, ms | 0.364 [0.362–0.429] ±8% | 0.541 [0.536–0.632] ±7% |
| nfcorpus splade: p50, ms | 0.327 [0.324–0.378] ±7% | 0.531 [0.524–0.619] ±7% |
| nfcorpus rrf3: nDCG@10, nDCG@10 | 0.359 [0.359–0.359] ±0% | — |
| nfcorpus rrf3: mean, ms | 1.75 [1.74–1.81] ±2% | — |
| nfcorpus rrf3: p50, ms | 1.76 [1.74–1.83] ±2% | — |
| arguana: sparse index, ms | 126 [125–130] ±1% | — |
| arguana text: nDCG@10, nDCG@10 | 0.496 [0.496–0.496] ±0% | 0.481 [0.481–0.481] ±0% |
| arguana text: mean, ms | 2.29 [2.27–2.30] ±1% | 5.42 [5.39–5.43] ±0% |
| arguana text: p50, ms | 0.868 [0.864–0.880] ±1% | 4.92 [4.90–4.93] ±0% |
| arguana vector: nDCG@10, nDCG@10 | 0.597 [0.597–0.597] ±0% | 0.595 [0.595–0.595] ±0% |
| arguana vector: mean, ms | 0.219 [0.214–0.251] ±7% | 0.363 [0.361–0.364] ±0% |
| arguana vector: p50, ms | 0.217 [0.212–0.235] ±4% | 0.360 [0.358–0.363] ±1% |
| arguana rrf: nDCG@10, nDCG@10 | 0.598 [0.598–0.598] ±0% | 0.589 [0.589–0.589] ±0% |
| arguana rrf: mean, ms | 3.45 [3.40–3.46] ±1% | 6.52 [6.48–6.53] ±0% |
| arguana rrf: p50, ms | 2.03 [2.02–2.05] ±0% | 6.01 [6.00–6.02] ±0% |
| arguana convex: nDCG@10, nDCG@10 | 0.617 [0.617–0.617] ±0% | — |
| arguana convex: mean, ms | 3.60 [3.58–3.63] ±1% | — |
| arguana convex: p50, ms | 2.20 [2.19–2.22] ±1% | — |
| arguana splade: nDCG@10, nDCG@10 | 0.474 [0.474–0.474] ±0% | 0.473 [0.472–0.473] ±0% |
| arguana splade: mean, ms | 3.89 [3.88–3.94] ±1% | 1.36 [1.36–1.38] ±1% |
| arguana splade: p50, ms | 3.89 [3.88–3.90] ±0% | 1.34 [1.34–1.38] ±1% |
| arguana rrf3: nDCG@10, nDCG@10 | 0.584 [0.584–0.584] ±0% | — |
| arguana rrf3: mean, ms | 8.15 [8.11–8.16] ±0% | — |
| arguana rrf3: p50, ms | 6.99 [6.95–7.01] ±0% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — paradedb pg_search 0.26.6, pgvector 0.8.6 — nucleus 18041cf93+changes — 2026-11-20T14:05:40+00:00

The vectors are made once, before: `KINDS=dense bash
target/bench-tools/embed-all.sh` and `KINDS=sparse SETS="scifact nfcorpus
arguana" bash target/bench-tools/embed-all.sh` (or `bench/text/embed.py`,
a set a process).

### PostgreSQL's text search through pg-compat

`bench/text/tsearch.py` (PLAN.md §M14.21 T8a; `bash bench/all.sh
tsearch`): what PostgreSQL clients write for a GIN index, `SELECT
count(*) FROM docs WHERE to_tsvector('english', body) @@
plainto_tsquery('english', q)` (all the query's words) or
`websearch_to_tsquery('english', 'w1 or w2 …')` (any), a BEIR query a
statement.  nucleus through PostgreSQL's protocol (psycopg, the time at
the client), the call rewritten to the text index's `@@` (`english`), and
over a stored `tsvector` column without an index ("values", computed as
PostgreSQL computes them); PostgreSQL 18 with `CREATE INDEX … USING gin
(to_tsvector('english', body))` (the server's `\timing`):

| tsearch | nucleus | nucleus, pg-compat | postgresql |
|---|---:|---:|---:|
| scifact: index built, ms | 120 [119–123] ±2% | — | 757 [745–771] ±1% |
| scifact all index: mean, ms | — | 0.160 [0.158–0.162] ±1% | — |
| scifact all index: p50, ms | — | 0.156 [0.155–0.158] ±1% | — |
| scifact all index: found, documents | — | 29.0 [29.0–29.0] ±0% | — |
| scifact all values: mean, ms | — | 17.0 [16.9–17.4] ±1% | — |
| scifact all values: p50, ms | — | 16.8 [16.7–16.8] ±0% | — |
| scifact all values: found, documents | — | 29.0 [29.0–29.0] ±0% | — |
| scifact any index: mean, ms | — | 0.390 [0.387–0.415] ±3% | — |
| scifact any index: p50, ms | — | 0.391 [0.388–0.402] ±2% | — |
| scifact any index: found, documents | — | 750,257 [750,257–750,257] ±0% | — |
| scifact any values: mean, ms | — | 17.7 [17.5–17.8] ±1% | — |
| scifact any values: p50, ms | — | 17.3 [17.2–17.3] ±0% | — |
| scifact any values: found, documents | — | 750,257 [750,257–750,257] ±0% | — |
| nfcorpus: index built, ms | 114 [113–120] ±2% | — | 554 [548–581] ±2% |
| nfcorpus all index: mean, ms | — | 0.125 [0.123–0.134] ±3% | — |
| nfcorpus all index: p50, ms | — | 0.122 [0.122–0.131] ±3% | — |
| nfcorpus all index: found, documents | — | 2,790 [2,790–2,790] ±0% | — |
| nfcorpus all values: mean, ms | — | 13.5 [13.2–13.9] ±2% | — |
| nfcorpus all values: p50, ms | — | 13.0 [13.0–13.4] ±1% | — |
| nfcorpus all values: found, documents | — | 2,790 [2,790–2,790] ±0% | — |
| nfcorpus any index: mean, ms | — | 0.190 [0.171–0.206] ±7% | — |
| nfcorpus any index: p50, ms | — | 0.171 [0.154–0.185] ±7% | — |
| nfcorpus any index: found, documents | — | 160,305 [160,305–160,305] ±0% | — |
| nfcorpus any values: mean, ms | — | 13.4 [13.2–14.0] ±2% | — |
| nfcorpus any values: p50, ms | — | 13.0 [12.9–13.6] ±2% | — |
| nfcorpus any values: found, documents | — | 160,305 [160,305–160,305] ±0% | — |
| fiqa: index built, ms | 592 [578–637] ±4% | — | 1,626 [1,611–1,647] ±1% |
| fiqa all index: mean, ms | — | 0.259 [0.257–0.262] ±1% | — |
| fiqa all index: p50, ms | — | 0.246 [0.244–0.250] ±1% | — |
| fiqa all index: found, documents | — | 13,878 [13,878–13,878] ±0% | — |
| fiqa all values: mean, ms | — | 80.2 [79.7–81.2] ±1% | — |
| fiqa all values: p50, ms | — | 79.4 [78.6–80.1] ±1% | — |
| fiqa all values: found, documents | — | 13,878 [13,878–13,878] ±0% | — |
| fiqa any index: mean, ms | — | 1.50 [1.50–1.53] ±1% | — |
| fiqa any index: p50, ms | — | 1.42 [1.41–1.43] ±1% | — |
| fiqa any index: found, documents | — | 11,473,938 [11,473,938–11,473,938] ±0% | — |
| fiqa any values: mean, ms | — | 82.8 [82.1–83.8] ±1% | — |
| fiqa any values: p50, ms | — | 81.6 [81.1–82.9] ±1% | — |
| fiqa any values: found, documents | — | 11,473,938 [11,473,938–11,473,938] ±0% | — |
| scifact all: mean, ms | — | — | 0.080 [0.080–0.082] ±1% |
| scifact all: p50, ms | — | — | 0.078 [0.077–0.079] ±1% |
| scifact all: found, documents | — | — | 29.0 [29.0–29.0] ±0% |
| scifact any: mean, ms | — | — | 16.8 [16.6–16.9] ±1% |
| scifact any: p50, ms | — | — | 0.673 [0.662–0.682] ±1% |
| scifact any: found, documents | — | — | 739,735 [739,735–739,735] ±0% |
| nfcorpus all: mean, ms | — | — | 0.059 [0.059–0.062] ±2% |
| nfcorpus all: p50, ms | — | — | 0.057 [0.057–0.058] ±1% |
| nfcorpus all: found, documents | — | — | 2,745 [2,745–2,745] ±0% |
| nfcorpus any: mean, ms | — | — | 0.152 [0.151–0.157] ±2% |
| nfcorpus any: p50, ms | — | — | 0.099 [0.098–0.106] ±3% |
| nfcorpus any: found, documents | — | — | 158,593 [158,593–158,593] ±0% |
| fiqa all: mean, ms | — | — | 0.234 [0.228–0.241] ±3% |
| fiqa all: p50, ms | — | — | 0.200 [0.194–0.204] ±2% |
| fiqa all: found, documents | — | — | 12,230 [12,230–12,230] ±0% |
| fiqa any: mean, ms | — | — | 7.60 [7.54–7.61] ±0% |
| fiqa any: p50, ms | — | — | 4.87 [4.82–4.90] ±1% |
| fiqa any: found, documents | — | — | 10,483,362 [10,483,362–10,483,362] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T14:20:02+00:00

A search page's query (`bash bench/all.sh tsearch-rank`; PLAN.md §M14.21
T8b): the ten best matches by `ts_rank(to_tsvector('english', body), q)`,
`id` breaking ties, and with each one's `ts_headline('english', body,
q)`; nDCG@10 and the ms of a query.  Both analyze every matching row's
text again for `ts_rank` — the "any" queries rank ~750,000 matches each;
nucleus's own BM25 ranking of the same corpora (above) is the fast way:

| tsearch-rank | nucleus | nucleus, pg-compat | postgresql |
|---|---:|---:|---:|
| scifact: index built, ms | 120 [118–127] ±3% | — | 748 [741–796] ±3% |
| scifact all index: mean, ms | — | 0.146 [0.144–0.174] ±8% | — |
| scifact all index: p50, ms | — | 0.142 [0.140–0.166] ±8% | — |
| scifact all index: found, documents | — | 29.0 [29.0–29.0] ±0% | — |
| scifact any index: mean, ms | — | 0.346 [0.333–0.374] ±5% | — |
| scifact any index: p50, ms | — | 0.336 [0.323–0.370] ±6% | — |
| scifact any index: found, documents | — | 750,257 [750,257–750,257] ±0% | — |
| scifact all rank: mean, ms | — | 0.166 [0.165–0.189] ±6% | 0.111 [0.106–0.114] ±3% |
| scifact all rank: p50, ms | — | 0.154 [0.153–0.169] ±5% | 0.096 [0.092–0.098] ±3% |
| scifact all rank: nDCG@10, nDCG@10 | — | 0.075 [0.075–0.075] ±0% | 0.075 [0.075–0.075] ±0% |
| scifact all headline: mean, ms | — | 0.190 [0.189–0.239] ±11% ⚠ | 0.132 [0.131–0.136] ±2% |
| scifact all headline: p50, ms | — | 0.163 [0.162–0.201] ±10% | 0.105 [0.105–0.107] ±1% |
| scifact all headline: nDCG@10, nDCG@10 | — | 0.075 [0.075–0.075] ±0% | 0.075 [0.075–0.075] ±0% |
| scifact any rank: mean, ms | — | 241 [239–242] ±1% | 308 [307–310] ±0% |
| scifact any rank: p50, ms | — | 251 [250–254] ±1% | 306 [303–309] ±1% |
| scifact any rank: nDCG@10, nDCG@10 | — | 0.572 [0.572–0.572] ±0% | 0.568 [0.568–0.568] ±0% |
| scifact any headline: mean, ms | — | 242 [241–243] ±0% | 310 [309–311] ±0% |
| scifact any headline: p50, ms | — | 252 [252–254] ±0% | 309 [308–311] ±0% |
| scifact any headline: nDCG@10, nDCG@10 | — | 0.572 [0.572–0.572] ±0% | 0.568 [0.568–0.568] ±0% |
| nfcorpus: index built, ms | 101 [101–109] ±3% | — | 550 [545–581] ±3% |
| nfcorpus all index: mean, ms | — | 0.109 [0.108–0.114] ±2% | — |
| nfcorpus all index: p50, ms | — | 0.105 [0.105–0.109] ±2% | — |
| nfcorpus all index: found, documents | — | 2,790 [2,790–2,790] ±0% | — |
| nfcorpus any index: mean, ms | — | 0.141 [0.140–0.144] ±1% | — |
| nfcorpus any index: p50, ms | — | 0.121 [0.120–0.124] ±1% | — |
| nfcorpus any index: found, documents | — | 160,305 [160,305–160,305] ±0% | — |
| nfcorpus all rank: mean, ms | — | 1.09 [1.08–1.10] ±1% | 1.17 [1.15–1.28] ±4% |
| nfcorpus all rank: p50, ms | — | 0.296 [0.294–0.310] ±2% | 0.213 [0.211–0.217] ±1% |
| nfcorpus all rank: nDCG@10, nDCG@10 | — | 0.207 [0.207–0.207] ±0% | 0.203 [0.203–0.203] ±0% |
| nfcorpus all headline: mean, ms | — | 1.44 [1.42–1.46] ±1% | 1.53 [1.50–1.56] ±1% |
| nfcorpus all headline: p50, ms | — | 0.422 [0.417–0.426] ±1% | 0.342 [0.340–0.358] ±2% |
| nfcorpus all headline: nDCG@10, nDCG@10 | — | 0.207 [0.207–0.207] ±0% | 0.203 [0.203–0.203] ±0% |
| nfcorpus any rank: mean, ms | — | 51.2 [50.9–51.3] ±0% | 62.3 [62.1–62.6] ±0% |
| nfcorpus any rank: p50, ms | — | 21.8 [21.3–22.8] ±3% | 26.9 [26.7–27.9] ±2% |
| nfcorpus any rank: nDCG@10, nDCG@10 | — | 0.304 [0.304–0.304] ±0% | 0.299 [0.299–0.299] ±0% |
| nfcorpus any headline: mean, ms | — | 52.1 [51.7–52.3] ±0% | 63.4 [63.2–63.7] ±0% |
| nfcorpus any headline: p50, ms | — | 23.1 [23.0–24.1] ±2% | 29.1 [27.8–29.4] ±2% |
| nfcorpus any headline: nDCG@10, nDCG@10 | — | 0.304 [0.304–0.304] ±0% | 0.299 [0.299–0.299] ±0% |
| scifact all: mean, ms | — | — | 0.080 [0.079–0.085] ±3% |
| scifact all: p50, ms | — | — | 0.077 [0.077–0.079] ±1% |
| scifact all: found, documents | — | — | 29.0 [29.0–29.0] ±0% |
| scifact any: mean, ms | — | — | 16.8 [16.7–16.8] ±0% |
| scifact any: p50, ms | — | — | 0.666 [0.658–0.673] ±1% |
| scifact any: found, documents | — | — | 739,735 [739,735–739,735] ±0% |
| nfcorpus all: mean, ms | — | — | 0.059 [0.058–0.073] ±10% ⚠ |
| nfcorpus all: p50, ms | — | — | 0.057 [0.056–0.069] ±9% |
| nfcorpus all: found, documents | — | — | 2,745 [2,745–2,745] ±0% |
| nfcorpus any: mean, ms | — | — | 0.152 [0.151–0.162] ±3% |
| nfcorpus any: p50, ms | — | — | 0.102 [0.096–0.109] ±6% |
| nfcorpus any: found, documents | — | — | 158,593 [158,593–158,593] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T14:44:00+00:00

## Embedding models

nucleus's local models (`bash bench/all.sh embed-speed`; §M14.22 E3: the
encoder on its own kernels, `bench/embed/speed.py`) against fastembed
0.9.1 (ONNX Runtime 1.31.1, batches of 32), the same 2,000 texts (BEIR
corpora and queries, twelve languages' word lists), tokens counted by
the models' tokenizers, cut to 512; on one thread and on all.  fastembed
runs the publisher's own f32 ONNX export — and, for bge, its default
(Qdrant's optimized export).  The vectors agree with ONNX Runtime's at
cosine ≥ 0.9999998 a text (`bench/embed/encoder.py`).  The models' files
come first: `target/ann-data/.venv/bin/python bench/embed/models.py`.

| embed-speed | nucleus | fastembed, publisher's f32 ONNX | fastembed, its default (Qdrant's) |
|---|---:|---:|---:|
| bge-small-en-v1.5, 1 thread: tokens/s, tokens/s | 1,613 [1,611–1,616] ±0% | 501 [501–503] ±0% | 507 [507–508] ±0% |
| bge-small-en-v1.5, 1 thread: texts/s, texts/s | 9.80 [9.79–9.81] ±0% | 3.04 [3.04–3.06] ±0% | 3.08 [3.08–3.09] ±0% |
| bge-small-en-v1.5, 10 threads: tokens/s, tokens/s | 10,403 [10,289–10,462] ±1% | 2,483 [2,417–2,485] ±2% | 2,362 [2,353–2,388] ±1% |
| bge-small-en-v1.5, 10 threads: texts/s, texts/s | 63.2 [62.5–63.6] ±1% | 15.1 [14.7–15.1] ±2% | 14.3 [14.3–14.5] ±1% |
| multilingual-e5-small, 1 thread: tokens/s, tokens/s | 1,600 [1,595–1,600] ±0% | 501 [500–502] ±0% | — |
| multilingual-e5-small, 1 thread: texts/s, texts/s | 9.52 [9.49–9.52] ±0% | 2.98 [2.97–2.99] ±0% | — |
| multilingual-e5-small, 10 threads: tokens/s, tokens/s | 10,339 [10,326–10,342] ±0% | 2,491 [2,488–2,493] ±0% | — |
| multilingual-e5-small, 10 threads: texts/s, texts/s | 61.5 [61.4–61.5] ±0% | 14.8 [14.8–14.8] ±0% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — fastembed 0.9.1 — nucleus 18041cf93+changes — 2026-11-20T15:52:57+00:00

End to end in a server (`bash bench/all.sh embed-beir`; §M14.22 E8b,
`bench/embed/beir.py`): BEIR's corpora loaded into a table with an
embedding column and a chunk table (`BY sentences SIZE 200 USING bge`)
with its own, HNSW on both, made empty first; bge-small-en-v1.5 by
`USING local`, the server's background worker, the server killed once
halfway, and a tenth of the documents rewritten and a twentieth deleted,
then put back (the chunks and vectors the same, bit for bit).  nDCG@10
of documents ranked directly and by their best chunk, beside exact
search over the same vectors and over fastembed's:

| embed-beir | nucleus |
|---|---:|
| scifact: documents, count | 5,183 [5,183–5,183] ±0% |
| scifact: chunks, count | 11,884 [11,884–11,884] ±0% |
| scifact: vectors, count | 17,067 [17,067–17,067] ±0% |
| scifact: vectors/s, vectors/s | 50.0 [49.8–50.2] ±0% |
| scifact: tokens/s, tokens/s | 10,029 [10,000–10,065] ±0% |
| nfcorpus: documents, count | 3,633 [3,633–3,633] ±0% |
| nfcorpus: chunks, count | 8,748 [8,748–8,748] ±0% |
| nfcorpus: vectors, count | 12,381 [12,381–12,381] ±0% |
| nfcorpus: vectors/s, vectors/s | 49.0 [48.8–49.1] ±0% |
| nfcorpus: tokens/s, tokens/s | 9,985 [9,962–10,013] ±0% |
| arguana: documents, count | 8,674 [8,674–8,674] ±0% |
| arguana: chunks, count | 13,215 [13,215–13,215] ±0% |
| arguana: vectors, count | 21,889 [21,889–21,889] ±0% |
| arguana: vectors/s, vectors/s | 65.3 [65.3–65.4] ±0% |
| arguana: tokens/s, tokens/s | 10,341 [10,339–10,349] ±0% |
| scifact: nDCG@10, documents, nDCG@10 | 0.720 [0.720–0.720] ±0% |
| scifact: nDCG@10, chunks, nDCG@10 | 0.709 [0.709–0.709] ±0% |
| scifact: nDCG@10, documents, exact, nDCG@10 | 0.720 [0.720–0.720] ±0% |
| scifact: nDCG@10, fastembed, exact, nDCG@10 | 0.720 [0.720–0.720] ±0% |
| scifact: nDCG@10, documents after, nDCG@10 | 0.720 [0.720–0.720] ±0% |
| scifact: nDCG@10, chunks after, nDCG@10 | 0.710 [0.710–0.710] ±0% |
| nfcorpus: nDCG@10, documents, nDCG@10 | 0.338 [0.338–0.338] ±0% |
| nfcorpus: nDCG@10, chunks, nDCG@10 | 0.340 [0.340–0.340] ±0% |
| nfcorpus: nDCG@10, documents, exact, nDCG@10 | 0.338 [0.338–0.338] ±0% |
| nfcorpus: nDCG@10, fastembed, exact, nDCG@10 | 0.338 [0.338–0.338] ±0% |
| nfcorpus: nDCG@10, documents after, nDCG@10 | 0.338 [0.338–0.338] ±0% |
| nfcorpus: nDCG@10, chunks after, nDCG@10 | 0.339 [0.339–0.339] ±0% |
| arguana: nDCG@10, documents, nDCG@10 | 0.597 [0.597–0.597] ±0% |
| arguana: nDCG@10, chunks, nDCG@10 | 0.577 [0.577–0.577] ±0% |
| arguana: nDCG@10, documents, exact, nDCG@10 | 0.597 [0.597–0.597] ±0% |
| arguana: nDCG@10, fastembed, exact, nDCG@10 | 0.597 [0.597–0.597] ±0% |
| arguana: nDCG@10, documents after, nDCG@10 | 0.598 [0.598–0.598] ±0% |
| arguana: nDCG@10, chunks after, nDCG@10 | 0.577 [0.577–0.577] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T18:20:14+00:00

## Memory stores

**LongMemEval** (`bash bench/all.sh longmemeval`; PLAN.md §M14.24 R1,
`bench/memory/longmemeval.py`): LongMemEval_S (cleaned; Wu et al., ICLR
2025) — 470 questions, each a subject of a memory store whose memories
are its history's user turns (114,902; bge-small-en-v1.5 by `USING
local`, made by the server's queue), the abstention questions left out
and 51 without evidence on the user side skipped, as the paper's code
does; its metrics (`recall_all@k`, `ndcg_any@k`), turns and sessions (the
first `k` distinct sessions of the ranked turns).  "paper BM25" is its
`flat-bm25` (rank_bm25's Okapi over tokens split on spaces, an index per
question), re-implemented in the script; times client to server over
loopback TCP.  The data: `curl -L -o
target/memory-data/longmemeval_s_cleaned.json
https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json`.

| longmemeval | recall | bm25 | vector | vector exact | paper bm25 | nucleus |
|---|---:|---:|---:|---:|---:|---:|
| T R@5, score | 0.680 [0.680–0.680] ±0% | 0.632 [0.632–0.632] ±0% | 0.613 [0.613–0.613] ±0% | 0.613 [0.613–0.613] ±0% | 0.489 [0.489–0.489] ±0% | — |
| T R@10, score | 0.819 [0.819–0.819] ±0% | 0.737 [0.737–0.737] ±0% | 0.780 [0.780–0.780] ±0% | 0.780 [0.780–0.780] ±0% | 0.592 [0.592–0.592] ±0% | — |
| T R@50, score | 0.959 [0.959–0.959] ±0% | 0.864 [0.864–0.864] ±0% | 0.955 [0.955–0.955] ±0% | 0.955 [0.955–0.955] ±0% | 0.768 [0.768–0.768] ±0% | — |
| T N@5, score | 0.687 [0.687–0.687] ±0% | 0.659 [0.659–0.659] ±0% | 0.630 [0.630–0.630] ±0% | 0.630 [0.630–0.630] ±0% | 0.525 [0.525–0.525] ±0% | — |
| T N@10, score | 0.728 [0.728–0.728] ±0% | 0.692 [0.692–0.692] ±0% | 0.677 [0.677–0.677] ±0% | 0.677 [0.677–0.677] ±0% | 0.561 [0.561–0.561] ±0% | — |
| T N@50, score | 0.757 [0.757–0.757] ±0% | 0.722 [0.722–0.722] ±0% | 0.712 [0.712–0.712] ±0% | 0.712 [0.712–0.712] ±0% | 0.606 [0.606–0.606] ±0% | — |
| S R@5, score | 0.897 [0.897–0.897] ±0% | 0.828 [0.828–0.828] ±0% | 0.912 [0.912–0.912] ±0% | 0.912 [0.912–0.912] ±0% | 0.685 [0.685–0.685] ±0% | — |
| S R@10, score | 0.962 [0.962–0.962] ±0% | 0.897 [0.897–0.897] ±0% | 0.969 [0.969–0.969] ±0% | 0.969 [0.969–0.969] ±0% | 0.785 [0.785–0.785] ±0% | — |
| S N@5, score | 0.908 [0.908–0.908] ±0% | 0.856 [0.856–0.856] ±0% | 0.905 [0.905–0.905] ±0% | 0.905 [0.905–0.905] ±0% | 0.700 [0.700–0.700] ±0% | — |
| S N@10, score | 0.923 [0.923–0.923] ±0% | 0.874 [0.874–0.874] ±0% | 0.917 [0.917–0.917] ±0% | 0.917 [0.917–0.917] ±0% | 0.733 [0.733–0.733] ±0% | — |
| recall, not analyzed: median, ms | — | — | — | — | — | 17.0 [16.9–18.2] ±4% |
| recall, not analyzed: p95, ms | — | — | — | — | — | 23.1 [23.1–25.3] ±5% |
| bm25, not analyzed: median, ms | — | — | — | — | — | 3.44 [3.42–3.44] ±0% |
| bm25, not analyzed: p95, ms | — | — | — | — | — | 4.24 [4.21–4.29] ±1% |
| vector, not analyzed: median, ms | — | — | — | — | — | 1.16 [1.16–1.17] ±1% |
| vector, not analyzed: p95, ms | — | — | — | — | — | 1.39 [1.36–1.40] ±1% |
| recall, analyzed: median, ms | — | — | — | — | — | 5.13 [5.13–5.17] ±0% |
| recall, analyzed: p95, ms | — | — | — | — | — | 6.73 [6.57–6.92] ±3% |
| bm25, analyzed: median, ms | — | — | — | — | — | 3.38 [3.36–3.38] ±0% |
| bm25, analyzed: p95, ms | — | — | — | — | — | 4.42 [4.35–4.43] ±1% |
| vector, analyzed: median, ms | — | — | — | — | — | 1.14 [1.14–1.15] ±1% |
| vector, analyzed: p95, ms | — | — | — | — | — | 1.33 [1.31–1.37] ±2% |
| recall, k=10, analyzed: median, ms | — | — | — | — | — | 3.98 [3.98–4.00] ±0% |
| recall, k=10, analyzed: p95, ms | — | — | — | — | — | 5.06 [5.06–5.08] ±0% |
| REMEMBER: median, ms | — | — | — | — | — | 29.8 [28.0–30.4] ±4% |
| REMEMBER: p95, ms | — | — | — | — | — | 59.0 [58.7–60.6] ±2% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T19:40:54+00:00

**In the process** (`bash bench/all.sh recall`; PLAN.md §M14.24 R4,
`tests/recall_bench.rs`): a store of 100,000 memories of 1,000 subjects
(8 made-up words each), real files, the hashing model (64 dimensions —
the database's time, not a model's), analyzed, `embedding.pending =
wait`; 1,000 questions of 3 words, a statement parsed once and run with
parameters, and again with a request id given (`REQUEST_ID=gen`).  The
target: `recall(…, 10)` under 1 ms at the median.

| recall | nucleus (ms) | request id (ms) |
|---|---:|---:|
| recall, a question a statement: p50 | 0.732 [0.727–0.749] ±1% | 1.34 [1.27–1.49] ±6% |
| recall, a question a statement: p95 | 0.785 [0.760–0.883] ±6% | 1.75 [1.60–1.78] ±5% |
| recall, 16 questions a statement: p50 | 0.560 [0.558–0.562] ±0% | 0.616 [0.614–0.617] ±0% |
| recall, 16 questions a statement: p95 | 0.583 [0.569–0.593] ±2% | 0.643 [0.627–0.650] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T19:42:35+00:00

## Erasure

`bash bench/all.sh erasure` (PLAN.md §M14.24 R2,
`crates/sql/tests/erasure_bench.rs`): a table of 100,000 rows of 1,000
subjects — a subject column, a text with a text index, a 4-dimensional
vector with an HNSW index — on real files; a subject's 100 texts are
planted, erased by `FORGET SUBJECT`, and every file searched for them
(`Kv::find_bytes`: the files raw and the data file's leaves decoded).

| erasure | nucleus |
|---|---:|
| loaded, MiB | 114 [114–114] ±0% |
| planted texts found, before, places | 100 [100–100] ±0% |
| planted texts found, after DELETE, places | 100 [100–100] ±0% |
| planted texts found, after the upkeep and a vacuum, places | 100 [100–100] ±0% |
| the search of the files, ms | 92.4 [91.6–96.0] ±2% |
| DELETE a subject's rows, ms | 3.85 [3.76–4.28] ±8% |
| FORGET SUBJECT, ms | 380 [373–400] ±4% |
| planted texts found, after FORGET SUBJECT, places | 0.000 [0.000–0.000] ±0% |
| after the erasures, MiB | 69.0 [64.0–69.0] ±4% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T19:44:57+00:00

A `DELETE` removes the rows from what queries see, not from the files:
the old versions, the WAL and the freed pages still hold every planted
text after the indexes' upkeep and a vacuum.  `FORGET SUBJECT` scrubs
them — the search finds none — and the files shrink with it.

## Backups

`bash bench/all.sh backup` (`bench/backup.py`; PLAN.md §M14.24 R6,
§M14.18 O3, O3c): `BACKUP TO` and `nucleus restore` on real files, each
with the `nucleus sql` process's start, every restore checked against
the database.  Three databases: 100,000 notes of 1,000 subjects (a
subject column, a B-tree index on it, a text index on the note) and
100,000 rows of a table of no one, analyzed — backed up with the notes'
rows sealed under their subjects' keys (finding each row's subjects, the
HMAC of its subject, ChaCha20-Poly1305) and without; a table of
1,000,000 rows of about 110 bytes with a B-tree index, backed up into
one repository one change after another — the chain of six restored,
and the full backup after it; and 1,000,000 rows of text of about 90
bytes, `BACKUP FULL` and its restore, compressed (LZ4 block) and
encrypted.

| backup | nucleus |
|---|---:|
| BACKUP TO, the notes' rows sealed under their subjects' keys, s | 0.546 [0.537–0.578] ±3% |
| BACKUP TO, the notes' rows sealed under their subjects' keys: stored, MiB | 16.5 [16.5–16.5] ±0% |
| BACKUP TO, the same with no subject column (nothing sealed), s | 0.164 [0.163–0.168] ±1% |
| BACKUP TO, the same with no subject column (nothing sealed): stored, MiB | 4.02 [4.02–4.02] ±0% |
| nucleus restore, sealed, s | 0.839 [0.835–0.856] ±1% |
| nucleus restore, unsealed, s | 0.694 [0.683–0.715] ±2% |
| BACKUP TO, the first: full, s | 0.688 [0.685–0.696] ±1% |
| BACKUP TO, the first: full: stored, MiB | 29.2 [29.2–29.2] ±0% |
| BACKUP TO, nothing changed, s | 0.020 [0.019–0.021] ±2% |
| BACKUP TO, nothing changed: stored, MiB | 0.000194 [0.000194–0.000194] ±0% |
| BACKUP TO, 10,000 rows updated, adjacent (1 %), s | 0.034 [0.032–0.038] ±7% |
| BACKUP TO, 10,000 rows updated, adjacent (1 %): stored, MiB | 0.284 [0.284–0.284] ±0% |
| BACKUP TO, 10,000 rows updated, every 100th (1 %: nearly every leaf), s | 0.479 [0.475–0.503] ±2% |
| BACKUP TO, 10,000 rows updated, every 100th (1 %: nearly every leaf): stored, MiB | 0.299 [0.299–0.299] ±0% |
| BACKUP TO, 100 rows updated, every 10,000th, s | 0.063 [0.062–0.064] ±2% |
| BACKUP TO, 100 rows updated, every 10,000th: stored, MiB | 0.004 [0.004–0.004] ±0% |
| BACKUP TO, 10,000 rows deleted, adjacent, s | 0.459 [0.445–0.473] ±2% |
| BACKUP TO, 10,000 rows deleted, adjacent: stored, MiB | 0.048 [0.048–0.048] ±0% |
| BACKUP FULL TO, s | 0.686 [0.685–0.698] ±1% |
| BACKUP FULL TO: stored, MiB | 29.0 [29.0–29.0] ±0% |
| nucleus restore, the chain of 6, s | 1.76 [1.58–1.97] ±8% |
| nucleus restore, the full one after it, s | 1.43 [1.42–1.54] ±4% |
| BACKUP FULL TO, compressed (LZ4 block), s | 0.691 [0.687–0.710] ±1% |
| BACKUP FULL TO, compressed (LZ4 block): stored, MiB | 29.2 [29.2–29.2] ±0% |
| nucleus restore, compressed (LZ4 block), s | 1.59 [1.51–1.74] ±6% |
| BACKUP FULL TO, compressed and encrypted (ChaCha20-Poly1305), s | 0.766 [0.758–0.772] ±1% |
| BACKUP FULL TO, compressed and encrypted (ChaCha20-Poly1305): stored, MiB | 29.2 [29.2–29.2] ±0% |
| nucleus restore, compressed and encrypted (ChaCha20-Poly1305), s | 1.53 [1.48–1.60] ±3% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus nucleus 0.1.0 — nucleus 18041cf93+changes — 2026-11-20T19:45:20+00:00

A backup costs what changed, not the database: the tree walk skips the
subtrees both branches share, and only the leaves left are compared —
when changes touch nearly every leaf ("every 100th"), that is nearly the
whole table read on both sides, still under the full backup's time at a
hundredth of its bytes.  After `FORGET SUBJECT` the restore of the same
backup leaves that subject's rows out.

## Changing a column's type

`bash bench/all.sh retype` (PLAN.md §M14.18 O5,
`crates/sql/tests/retype_bench.rs`): `ALTER TABLE t ALTER COLUMN v TYPE
… [CONCURRENTLY]` on real files, a table of 1,000,000 rows (`id` the
key, `g`, `v` with a B-tree index, `s` text of about 12 bytes), `v`
changed from `integer` to `bigint` and back.  Under writes, another
session updates single rows by key (`g` and `v`) as fast as it can,
retrying conflicts; the transactional form is tried three times a run.

| retype | nucleus |
|---|---:|
| idle, ALTER COLUMN … TYPE, s | 5.70 [3.87–6.88] ±21% ⚠ |
| idle, ALTER COLUMN … TYPE CONCURRENTLY, s | 11.5 [10.8–12.0] ±4% |
| a writer alone, updates/s | 4,070 [4,070–4,070] ±0% |
| a writer alone: p50, ms | 0.236 [0.236–0.236] ±0% |
| a writer alone: p99, ms | 0.416 [0.416–0.416] ±0% |
| under writes, ALTER COLUMN … TYPE, s | 153 [148–177] ±8% |
| under writes, TYPE: not done in 3 tries, runs | 5.00 [5.00–5.00] ±0% |
| under writes, TYPE: the writer, updates/s | 2,856 [1,976–2,934] ±15% ⚠ |
| under writes, TYPE: the writer's p50, ms | 0.308 [0.289–0.346] ±7% |
| under writes, TYPE: the writer's p99, ms | 0.705 [0.671–2.06] ±63% ⚠ |
| under writes, ALTER COLUMN … TYPE CONCURRENTLY, s | 25.3 [24.5–26.3] ±3% |
| under writes, TYPE CONCURRENTLY: not done in 3 tries, runs | 0.000 [0.000–0.000] ±0% |
| under writes, TYPE CONCURRENTLY: the writer, updates/s | 3,302 [3,114–3,331] ±3% |
| under writes, TYPE CONCURRENTLY: the writer's p50, ms | 0.239 [0.233–0.244] ±2% |
| under writes, TYPE CONCURRENTLY: the writer's p99, ms | 1.54 [1.02–1.89] ±23% ⚠ |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T19:47:20+00:00

The transactional form cannot finish while the table is written: it
reads every row in one transaction, and some row it read is written
before it commits.  `CONCURRENTLY` finishes in each run, the writer at
its rate alone; its p99 rises where it meets a part (up to 4,096 rows)
being written.  On an idle table it costs twice the transactional form's
time: each row is written twice (its hidden value, then the rewrite) and
the old index's entries are removed in parts, each a transaction of its
own.

## Sealing a page

What an 8 KiB page costs to seal on one core (`bash bench/all.sh
page-crypto`; PLAN.md §M14.30 K2a, `crates/storage/tests/page_crypto_speed.rs`,
the best of three passes over 32 MiB): the checksum pages carry in the
clear against the AEADs — RFC 8439's, the misuse-resistant ChaCha20 one
(`nucleus_crypto::siv`, RFC 8452's structure with ChaCha20) and
AES-256-GCM-SIV by the CPU's instructions (`nucleus_kernels::aes`), which
encrypted files use where the CPU has them; then in the store (K2b, K2c):
400,000 rows of 100-byte values written in batches of 1,000 (each a WAL
record, synced) and checkpointed (a 52 MB data file), then scanned whole
three times through an 8 MiB buffer pool — every page read from the file
and, encrypted, opened again (the file in the OS's cache: the cipher's
cost undiluted).

| page-crypto | nucleus |
|---|---:|
| crc32c: MB/s, MB/s | 22,726 [22,279–22,772] ±8% |
| crc32c: µs a page, µs | 0.360 [0.360–0.368] ±12% ⚠ |
| chacha20-poly1305 seal: MB/s, MB/s | 660 [652–661] ±1% |
| chacha20-poly1305 seal: µs a page, µs | 12.4 [12.4–12.6] ±1% |
| siv seal (in place): MB/s, MB/s | 667 [665–668] ±2% |
| siv seal (in place): µs a page, µs | 12.3 [12.3–12.3] ±2% |
| siv seal + open: MB/s, MB/s | 334 [333–334] ±0% |
| siv seal + open: µs a page, µs | 24.5 [24.5–24.6] ±0% |
| aes-gcm-siv seal (in place): MB/s, MB/s | 4,927 [4,911–4,935] ±1% |
| aes-gcm-siv seal (in place): µs a page, µs | 1.66 [1.66–1.67] ±1% |
| aes-gcm-siv seal + open: MB/s, MB/s | 2,476 [2,474–2,480] ±1% |
| aes-gcm-siv seal + open: µs a page, µs | 3.31 [3.30–3.31] ±1% |
| in the clear: scan, ms | 51.8 [51.4–52.3] ±2% |
| chacha20-siv: load + checkpoint, ms | 293 [291–296] ±2% |
| chacha20-siv: scan, ms | 134 [133–136] ±1% |
| aes-gcm-siv: load + checkpoint, ms | 157 [155–161] ±10% ⚠ |
| aes-gcm-siv: scan, ms | 61.0 [60.4–61.5] ±3% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T20:09:44+00:00

The AEADs' parts (`bash bench/all.sh aead`; `nucleus-crypto`'s speed
test): ChaCha20 runs four blocks side by side and Poly1305 in 64-bit
limbs, portable Rust the compiler vectorizes.

| aead | nucleus (MB/s) |
|---|---:|
| chacha20 | 997 [960–999] ±2% |
| chacha20, a block at a time | 621 [618–625] ±0% |
| poly1305 | 2,189 [2,180–2,198] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T20:10:45+00:00

AES-256-GCM-SIV by size (`bash bench/all.sh aes`; `nucleus-kernels`'
`aes_speed`): by ARMv8's AES and PMULL — POLYVAL eight blocks at a time,
reduced once, the counter blocks encrypted eight at a time, a fixed
~0.25 µs a message besides (its keys from the nonce, the powers of H) —
and the portable, constant-time AES for a file opened on a CPU without
the instructions.  A page is sealed when it is written and opened when
it is read from the file — never while it is cached.

| aes | nucleus (µs) |
|---|---:|
| instructions, 64 bytes: seal | 0.303 [0.298–0.306] ±1% |
| instructions, 64 bytes: open | 0.310 [0.308–0.313] ±1% |
| instructions, 4096 bytes: seal | 0.978 [0.973–1.04] ±3% |
| instructions, 4096 bytes: open | 0.951 [0.949–0.971] ±1% |
| instructions, 8168 bytes: seal | 1.76 [1.75–1.76] ±0% |
| instructions, 8168 bytes: open | 1.73 [1.60–1.73] ±3% |
| instructions, 65536 bytes: seal | 11.3 [11.1–11.3] ±0% |
| instructions, 65536 bytes: open | 11.3 [10.8–11.4] ±2% |
| portable, 8168 bytes: seal | 8,828 [8,823–8,902] ±0% |
| portable, 8168 bytes: open | 8,779 [8,608–8,908] ±2% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T20:10:47+00:00

Through SQL (`bash bench/all.sh encrypted`; K2e, `crates/sql/tests/encrypted.rs`,
real files): 300,000 rows (`id`, `g`, a 40-byte text) inserted and
checkpointed, an aggregate over them all through an 8 MiB page cache,
and an `ORDER BY` past a 1 MB `work_mem` (its runs in scratch files,
sealed in 4 KiB blocks when the database is); encrypted with
AES-256-GCM-SIV:

| encrypted | nucleus (ms) |
|---|---:|
| in the clear: load | 340 [328–361] ±4% |
| in the clear: scan | 7.12 [7.08–7.28] ±1% |
| in the clear: sort past work_mem | 94.9 [93.9–96.7] ±1% |
| encrypted: load | 322 [320–327] ±1% |
| encrypted: scan | 7.80 [7.55–8.33] ±4% |
| encrypted: sort past work_mem | 117 [116–118] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T20:10:22+00:00

A new data key (`bash bench/all.sh keys`; K3f, `bench/keys.py`, `nucleus
keys <dir> rotate data` on real files): a database of 1,000,000 rows of
~210 bytes sealed again under the new key in place, 256 pages a batch —
each batch's old bytes journaled and synced, then the pages written and
synced; the rows read back the same, none in the clear.  `ROTATE KEY
data` with the server running does the same in the background, paced
while transactions run.

| keys | nucleus |
|---|---:|
| rotate data, s | 1.35 [1.03–1.46] ±19% ⚠ |
| database, MB | 323 [323–323] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus nucleus 0.1.0 — nucleus 18041cf93+changes — 2026-11-20T20:11:33+00:00

Sealed rows and sealed memory stores (`bash bench/all.sh sealing`;
K4a, K4c, `crates/sql/tests/sealed.rs`, in memory): 200,000 rows of ~60
bytes over 1,000 subjects, stored `row` and `sealed`; memory stores of
1,000 subjects with a 384-dimension hashing model, `recall` of 10
averaged over 300 calls, indexed and sealed (at 100 memories a subject
the sealed one's exact ranking costs less than the shared index's
upkeep):

| sealing | nucleus |
|---|---:|
| 20 memories a subject, indexed: load, s | 2.82 [2.80–2.95] ±2% |
| 20 memories a subject, indexed: recall, ms | 0.247 [0.244–0.265] ±3% |
| 20 memories a subject, sealed: load, s | 0.545 [0.539–0.563] ±2% |
| 20 memories a subject, sealed: recall, ms | 0.355 [0.350–0.361] ±1% |
| 100 memories a subject, indexed: load, s | 23.6 [23.5–23.7] ±0% |
| 100 memories a subject, indexed: recall, ms | 18.2 [18.2–18.4] ±1% |
| 100 memories a subject, sealed: load, s | 14.1 [14.0–14.3] ±1% |
| 100 memories a subject, sealed: recall, ms | 15.1 [15.0–15.4] ±1% |
| row: load 200000 rows, ms | 209 [204–216] ±2% |
| row: scan, ms | 6.38 [6.19–6.60] ±3% |
| row: lookup, µs | 8.86 [8.72–9.29] ±2% |
| sealed: load 200000 rows, ms | 504 [490–510] ±2% |
| sealed: scan, ms | 38.9 [37.6–41.3] ±4% |
| sealed: lookup, µs | 9.09 [8.93–9.29] ±2% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T20:05:17+00:00

## JSON

PLAN.md §M14.15 J1–J5: the `json` type, its operators, SQL/JSON paths,
indexes and JSON Schema checks.  The usual corpora (`canada.json`:
numbers; `citm_catalog.json`: objects; `twitter.json`: strings and
Unicode), fetched by `bench/json/run.sh`.

In the process (`bash bench/all.sh json`; `bench/json`), against
`serde_json`'s `Value`; one whole file each: parse, print, read one
field of the parsed value (nucleus reads it from the stored form in
place, serde from the tree it built — the "field" row of serde is its
parse), and nucleus's stored size:

| json | nucleus | serde_json |
|---|---:|---:|
| parse (canada.json), ms | 3.83 [3.82–4.16] ±4% | 6.78 [6.77–6.84] ±0% |
| output (canada.json), ms | 4.32 [4.29–4.37] ±1% | 2.11 [2.10–2.12] ±1% |
| field (canada.json), µs | 0.017 [0.017–0.017] ±0% | 6,795 [6,784–7,087] ±2% |
| size (canada.json), KB | 1,667 [1,667–1,667] ±0% | — |
| parse (citm_catalog.json), ms | 1.74 [1.73–1.81] ±2% | 2.48 [2.46–2.72] ±4% |
| output (citm_catalog.json), ms | 0.967 [0.962–1.05] ±4% | 0.417 [0.415–0.448] ±3% |
| field (citm_catalog.json), µs | 0.024 [0.022–0.025] ±4% | 2,470 [2,462–2,512] ±1% |
| size (citm_catalog.json), KB | 407 [407–407] ±0% | — |
| parse (twitter.json), ms | 0.775 [0.772–0.787] ±1% | 1.28 [1.27–1.28] ±0% |
| output (twitter.json), ms | 0.522 [0.518–0.525] ±0% | 0.297 [0.295–0.301] ±1% |
| field (twitter.json), µs | 0.018 [0.018–0.018] ±0% | 1,278 [1,274–1,286] ±0% |
| size (twitter.json), KB | 431 [431–431] ±0% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T20:12:41+00:00

serde prints compact text from doubles it already holds; nucleus prints
PostgreSQL's layout from exact decimals.

Against PostgreSQL's `jsonb` (`bash bench/all.sh json-pg`;
`bench/json/pg.py`: PostgreSQL native, bench/pg.py's cluster; nucleus a
server; seconds for one statement over 50,000 rows a corpus — the
tweets, the performances, the features; the field sums, path counts and
value sums asserted equal), and the size of a whole file as one stored
value in PostgreSQL (nucleus's is in the table above):

| json-pg | nucleus | postgresql |
|---|---:|---:|
| parse (tweets), s | 0.299 [0.298–0.302] ±3% | 1.30 [1.30–1.31] ±1% |
| output (tweets), s | 0.135 [0.134–0.136] ±9% | 0.984 [0.982–0.988] ±1% |
| field (tweets), s | 0.066 [0.065–0.067] ±13% ⚠ | 0.248 [0.247–0.249] ±2% |
| path (tweets), s | 0.061 [0.061–0.062] ±3% | 0.246 [0.245–0.247] ±2% |
| value (tweets), s | 0.062 [0.062–0.063] ±3% | 0.249 [0.247–0.250] ±3% |
| table (tweets), s | 0.066 [0.065–0.066] ±4% | 0.262 [0.261–0.263] ±3% |
| parse (performances), s | 0.086 [0.086–0.087] ±6% | 0.707 [0.706–0.709] ±1% |
| output (performances), s | 0.044 [0.044–0.045] ±5% | 0.490 [0.489–0.491] ±1% |
| field (performances), s | 0.009 [0.009–0.009] ±3% | 0.035 [0.035–0.035] ±2% |
| path (performances), s | 0.017 [0.017–0.017] ±21% ⚠ | 0.050 [0.050–0.051] ±4% |
| value (performances), s | 0.008 [0.008–0.008] ±3% | 0.036 [0.036–0.036] ±2% |
| table (performances), s | 0.295 [0.294–0.296] ±2% | 0.318 [0.316–0.319] ±2% |
| parse (features), s | 0.025 [0.025–0.025] ±4% | 0.063 [0.063–0.063] ±2% |
| output (features), s | 0.026 [0.026–0.026] ±8% | 0.039 [0.039–0.039] ±3% |
| field (features), s | 0.004 [0.004–0.004] ±2% | 0.010 [0.010–0.010] ±2% |
| path (features), s | 0.004 [0.004–0.004] ±3% | 0.011 [0.010–0.011] ±1% |
| value (features), s | 0.004 [0.004–0.004] ±5% | 0.010 [0.010–0.010] ±3% |
| table (features), s | 0.103 [0.103–0.104] ±2% | 0.122 [0.121–0.122] ±3% |
| size (canada.json), KB | — | 2,599 [2,599–2,599] ±0% |
| size (citm_catalog.json), KB | — | 739 [739–739] ±0% |
| size (twitter.json), KB | — | 497 [497–497] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T20:12:46+00:00

Parse: `count(*) … WHERE doc::json IS NOT NULL` over a text column;
output: `sum(length(doc::text))`; a field: `sum(length(doc -> 'user' ->>
'screen_name'))` (tweets), `doc ->> 'start'` (performances), `doc ->
'properties' ->> 'name'` (features); a path: `count(*) … WHERE doc @?
'$.entities.hashtags[*] ? (@.text starts with "a")'` (tweets),
`'$.seatCategories[*].areas[*] ? (@.areaId > 205705999)'`
(performances), `'$.geometry.coordinates[*][*] ? (@[0] < -70)'`
(features); a value: `sum(JSON_VALUE(doc, path RETURNING bigint))` of
`'$.user.followers_count'` (tweets), `'$.start'` (performances),
`'$.geometry.coordinates.size()'` (features); a table: `count(*),
sum(<a column>)` over `j, JSON_TABLE(j.doc, <rows> COLUMNS (…))` (a
tweet's hashtags' lengths, a performance's seat areas' ids, a feature's
points west of 70°W).  `JSON_TABLE`'s rows are made on the workers, each
document's call beside the others' (PLAN.md §M14.15 J3c).

Indexes on JSON (`bash bench/all.sh json-index`; J4,
`bench/json/index.py`): 200,000 made-up events (`{"id", "user": {"name",
"tier"}, "kind", "tags": […], "score"[, "rare"]}`), `count(*)` of each
condition, a full scan against the index (each count asserted equal with
and without the index, and to PostgreSQL's).  PostgreSQL timed by
`psql`'s `\timing` over its Unix socket; nucleus a server, timed by the
client over TCP — for a lookup of one row that is mostly the round trip:

| json-index | nucleus (s) | postgresql (s) |
|---|---:|---:|
| id, no index | 0.008 [0.007–0.008] ±6% | 0.009 [0.009–0.009] ±7% |
| id, indexed | 0.00011 [0.000109–0.000111] ±21% ⚠ | 3.7e-05 [3.5e-05–3.9e-05] ±23% ⚠ |
| score, no index | 0.015 [0.015–0.015] ±4% | 0.014 [0.013–0.014] ±2% |
| score, indexed | 0.000219 [0.000215–0.000221] ±13% ⚠ | 0.000107 [0.000104–0.000113] ±27% ⚠ |
| user, no index | 0.008 [0.008–0.008] ±4% | 0.012 [0.012–0.012] ±1% |
| user, indexed | 0.000132 [0.00013–0.000137] ±13% ⚠ | 7e-05 [6.9e-05–7.3e-05] ±21% ⚠ |
| kind+tag, no index | 0.006 [0.006–0.007] ±2% | 0.010 [0.010–0.010] ±2% |
| kind+tag, indexed | 0.002 [0.002–0.002] ±3% | 0.001 [0.001–0.001] ±12% ⚠ |
| rare, no index | 0.006 [0.005–0.006] ±6% | 0.006 [0.006–0.006] ±15% ⚠ |
| rare, indexed | 0.000238 [0.000229–0.00024] ±9% | 0.006 [0.006–0.006] ±25% ⚠ |
| build id index | 0.089 [0.088–0.091] ±6% | 0.042 [0.041–0.043] ±5% |
| build score index | 0.088 [0.086–0.090] ±4% | 0.038 [0.037–0.040] ±4% |
| build containment index | 0.231 [0.229–0.233] ±2% | 0.269 [0.266–0.275] ±3% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 18041cf93+changes — 2026-11-20T20:28:04+00:00

`jsonb_path_ops` cannot serve `?` (PostgreSQL scans for "rare"); the
`json` index can.  The `json` index builds on the workers (§M14.33
S18), 231 ms against the GIN index's 269; PostgreSQL builds the B-tree
expression indexes 2.1–2.3× faster (issue #9).  A count over an index's
range reads no row (§M14.33 S17); what is left of the indexed lookups'
gap, and of score's count, is mostly the clients' (nucleus's Python
driver over TCP against psql; the statement itself 5 µs); kind+tag's
2,000 rows found by containment are 1.7 against 1.0 ms (issue #10).

JSON Schema checks (`bash bench/all.sh json-schema`; J5,
`bench/json/src/bin/schema.rs`): 100,000 made-up tool calls, one in ten
not fitting, against a schema with `$ref`, `enum`, `pattern`, `anyOf`,
`uniqueItems` and bounds; each document parsed already, against the
`jsonschema` crate 0.61 ("compiled, kept": the check by a schema
compiled once and held, as a `CHECK` uses it):

| json-schema | nucleus | jsonschema |
|---|---:|---:|
| fits (a document), ns | 272 [272–275] ±1% | 141 [141–149] ±3% |
| errors (a document), ns | 398 [396–402] ±1% | 252 [247–260] ±2% |
| compile (the schema), µs | 18.0 [17.4–18.3] ±2% | 22.6 [22.2–23.2] ±2% |
| compiled, kept, ns | 186 [184–188] ±1% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T20:30:12+00:00

In SQL (`bash bench/all.sh json-schema-pg`; `bench/json/schema.py`,
90,000 rows): `INSERT … SELECT` into a table with `CHECK
(json_matches_schema(args, '…'))` against one without, `count(*)`, and
`count(*) … WHERE json_matches_schema(…)` and the sum of
`json_schema_errors`' counts:

| json-schema-pg | nucleus (s) |
|---|---:|
| load without a check | 1.77 [1.76–1.78] ±1% |
| load with the CHECK | 1.89 [1.88–1.91] ±1% |
| count(*) | 0.000898 [0.000889–0.000911] ±6% |
| count matching | 0.010 [0.009–0.010] ±10% |
| sum of errors | 0.011 [0.010–0.012] ±8% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T20:30:38+00:00

**The form against BSON and `jsonb`** (`bash bench/all.sh json-forms
json-forms-pg`; `bench/json/src/bin/forms.rs`, `bench/json/forms.py`,
PLAN.md §M15 D1): a user's document as applications keep one — profile
fields, an address, preferences, orders, a text; the large kind 80 more
fields and a history — of ~1 KB and ~16 KB, fields in the order
written.  In the process, nucleus's `json` form (keys sorted, a key
found by binary search) against BSON's raw document (the `bson` crate:
fields in the order written, each found by walking those before it) —
a field of the top object, the second written, the middle one and the
last; a path three deep; the whole document out as JSON text (BSON's
through the crate's relaxed Extended JSON):

| json-forms | text | nucleus | bson |
|---|---:|---:|---:|
| size (1 KB), B | 860 [860–860] ±0% | 787 [787–787] ±0% | 917 [917–917] ±0% |
| second field (1 KB), ns | — | 26.7 [26.5–27.2] ±1% | 38.2 [38.2–40.0] ±2% |
| middle field (1 KB), ns | — | 33.9 [33.7–34.4] ±1% | 108 [107–110] ±1% |
| last field (1 KB), ns | — | 27.6 [27.1–29.4] ±3% | 214 [214–221] ±1% |
| path address.geo.lat (1 KB), ns | — | 98.9 [98.4–101] ±1% | 278 [277–285] ±1% |
| whole document as text (1 KB), ns | — | 1,338 [1,331–1,374] ±1% | 9,148 [9,117–9,232] ±1% |
| size (16 KB), B | 16,017 [16,017–16,017] ±0% | 13,898 [13,898–13,898] ±0% | 18,592 [18,592–18,592] ±0% |
| second field (16 KB), ns | — | 43.8 [43.7–46.6] ±3% | 39.7 [39.6–41.1] ±2% |
| middle field (16 KB), ns | — | 44.1 [43.6–45.1] ±1% | 804 [799–823] ±1% |
| last field (16 KB), ns | — | 36.3 [36.2–37.6] ±2% | 1,620 [1,609–1,658] ±1% |
| path address.geo.lat (16 KB), ns | — | 101 [101–102] ±0% | 274 [273–287] ±2% |
| whole document as text (16 KB), ns | — | 32,459 [32,348–33,311] ±1% | 206,928 [202,412–209,471] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus b9ec7c50b+changes — 2026-11-21T12:13:57+00:00

A key costs the same wherever it was written; BSON's walk grows with
the position — its best case, the second field of 80-odd, 1.10× faster
than the binary search.  The form is the smallest of the three.

In SQL, against PostgreSQL 18's `jsonb` on the same documents, 100,000
of 1 KB and 10,000 of 16 KB: `sum(length(doc ->> k))` for each field,
`sum(length(doc -> 'address' -> 'geo' ->> 'lat'))`, and
`sum(length(doc::text))`; `jsonb`'s size is `pg_column_size`, TOAST's
compression past ~2 KB included (nucleus's form is stored as it is):

| json-forms-pg | nucleus | postgresql |
|---|---:|---:|
| second field (1 KB), ms | 11.2 [11.0–11.3] ±6% | 11.9 [11.8–12.0] ±9% |
| middle field (1 KB), ms | 10.8 [10.7–10.9] ±7% | 12.1 [12.0–12.1] ±3% |
| last field (1 KB), ms | 10.4 [10.3–10.5] ±31% ⚠ | 11.6 [11.5–11.7] ±4% |
| path address.geo.lat (1 KB), ms | 12.2 [12.0–12.4] ±8% | 13.7 [13.5–13.9] ±4% |
| whole document as text (1 KB), ms | 40.2 [39.8–40.5] ±6% | 88.1 [88.0–88.3] ±3% |
| size a document (1 KB), B | — | 1,085 [1,085–1,085] ±0% |
| second field (16 KB), ms | 107 [105–108] ±4% | 202 [201–203] ±2% |
| middle field (16 KB), ms | 109 [107–110] ±6% | 204 [203–205] ±2% |
| last field (16 KB), ms | 108 [107–109] ±8% | 201 [201–202] ±1% |
| path address.geo.lat (16 KB), ms | 109 [107–110] ±3% | 205 [203–206] ±2% |
| whole document as text (16 KB), ms | 557 [555–560] ±2% | 950 [948–957] ±1% |
| size a document (16 KB), B | — | 8,923 [8,923–8,923] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus b9ec7c50b+changes — 2026-11-21T12:24:48+00:00

Fields and paths 0.89–0.94× `jsonb`'s time on 1 KB documents and about
half on 16 KB ones (`jsonb` decompresses them first); the whole text
0.46× and 0.59×.  The chain of `->` is one walk (DOCS.md §8.31,
operators); as three calls copying the values between, it was 1.11×
PostgreSQL's.  `jsonb` stores the 16 KB documents in 8.9 KB, compressed;
nucleus's form takes 13.9 KB.

## Upserts

PLAN.md §M15 D2: `INSERT … ON CONFLICT` (`bash bench/all.sh upsert`;
`bench/upsert.py`).  A table `t (id int PRIMARY KEY, n int, tag text
UNIQUE)` of 200,000 rows; each statement offers 200,000 rows, half of
them there already — `DO UPDATE SET n = t.n + excluded.n` by the key,
`DO NOTHING`, `DO UPDATE` by the unique `tag` — against a plain `INSERT
… SELECT` of as many new rows; each on a table loaded afresh.  nucleus as
a server (its Python client's clock), PostgreSQL 18 native and tuned
(`\timing`):

| upsert | nucleus (s) | postgresql (s) |
|---|---:|---:|
| insert | 0.384 [0.382–0.390] ±2% | 0.547 [0.519–0.553] ±9% |
| do update | 0.511 [0.507–0.518] ±2% | 0.703 [0.678–0.730] ±8% |
| do nothing | 0.333 [0.331–0.336] ±2% | 0.325 [0.318–0.334] ±5% |
| by tag | 0.580 [0.574–0.585] ±9% | 0.735 [0.727–0.746] ±4% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-27.2.1-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 6a9ca6605+changes — 2026-11-27T03:02:18+00:00

`DO UPDATE` 0.73× PostgreSQL's time, by the secondary key 0.79×; `DO
NOTHING` 1.02× (the intervals overlap) — PostgreSQL skips a taken row
without writing, as nucleus does, and both pay a lookup a row; nucleus's
is a descent of the tree for each (half the conflict path's time).

## Multi-valued indexes

PLAN.md §M15 D5: an index entry per array element (`bash bench/all.sh
multikey`; `bench/multikey.py`).  200,000 documents `{"_id", "tags": [3
of 1,000], "items": [3 × {"sku": one of 5,000, "qty": 0–999}]}`, loaded
with the indexes there (`load`); then 100 lookups a case, a different
value each: `tag` — `doc @> '{"tags": ["t…"]}'`, about 600 documents;
`sku` — `doc @> '{"items": [{"sku": "s…"}]}'`, about 120; `qty range` —
`doc @? 'strict $.items[*] ? (@.qty > n)'`, about 1,800.  nucleus a
collection with `tags[*]`, `items[*].sku`, `items[*].qty` indexes, as a
server (its Python client, a round trip a lookup); PostgreSQL 18 native
and tuned, one GIN index (`jsonb_path_ops`; it serves no range, so
`qty range` is its full scan), `\timing`:

| multikey | nucleus (s) | postgresql (s) |
|---|---:|---:|
| load | 3.00 [2.98–3.09] ±11% ⚠ | 2.22 [2.08–2.25] ±6% |
| tag | 0.131 [0.127–0.151] ±11% ⚠ | 0.038 [0.037–0.040] ±15% ⚠ |
| sku | 0.033 [0.033–0.039] ±43% ⚠ | 0.017 [0.016–0.017] ±10% |
| qty range | 2.29 [2.26–2.34] ±6% | 2.93 [2.93–2.95] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-27.2.1-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 00fa28eff+changes — 2026-11-27T08:33:41+00:00

A lookup 3.4× PostgreSQL's time by a tag, 1.9× by a sku; a range 0.78×
(PostgreSQL has no index for it).  MongoDB 8 (`mongo:8` in Docker, its
multikey indexes, mongosh inside the container), one run of the script
the same day — not through the method: Docker's VM could not hold it
beside other containers for a second — load 20.7 s, tag 0.052 s, sku
0.068 s, qty range 0.124 s (its index answers the range; nucleus keeps
the condition and checks it again on each row).  Where nucleus's time
goes: the row behind each entry fetched by its primary key, about 0.9
µs a row (the cost of any secondary index), and a strict path's
re-check, about 1.5 µs a row — gh #22.

## Write amplification

Bytes written to disk per byte of rows inserted (`bash bench/all.sh
wamp`; PLAN.md §M14.28, `bench/wamp/wamp.py`): rows of a `bigint` key
and 100 bytes of text (108 bytes); a phase counts once its pages are
durable — the server killed after it and restarted, recovery replaying
the WAL and checkpointing —, the bytes the server processes wrote
(macOS's `proc_pid_rusage`), APFS; nucleus with its default 128 MiB page
cache and a checkpoint every GiB of WAL.  Ten million rows loaded a
million a transaction, keys in order and random; then 100,000 rows one
a transaction (keys after the table's, and random among its keys), and
100 a transaction; the steady state, 5,000,000 rows 100 a transaction
into the random table; then a million of its rows updated and a million
deleted, 100 a transaction.  The recovery is the restart's after the
kill (the WAL since the last checkpoint replayed):

| wamp | nucleus |
|---|---:|
| bulk, 1M rows a transaction, keys in order: write amplification, x | 4.11 [4.04–4.11] ±1% |
| bulk, 1M rows a transaction, keys in order: time, s | 8.44 [8.42–8.79] ±2% |
| bulk, 1M rows a transaction, keys in order: files, MiB | 1,624 [1,624–1,624] ±0% |
| bulk, 1M rows a transaction, keys in order: space amplification, x | 1.58 [1.58–1.58] ±0% |
| bulk, 1M rows a transaction, keys in order: recovery, s | 0.986 [0.943–1.04] ±5% |
| bulk, 1M rows a transaction, random keys: write amplification, x | 5.76 [4.83–5.77] ±10% |
| bulk, 1M rows a transaction, random keys: time, s | 71.9 [69.4–73.1] ±3% |
| bulk, 1M rows a transaction, random keys: files, MiB | 4,774 [4,774–4,774] ±0% |
| bulk, 1M rows a transaction, random keys: space amplification, x | 2.32 [2.32–2.32] ±0% |
| bulk, 1M rows a transaction, random keys: recovery, s | 19.7 [19.7–20.0] ±1% |
| a row a transaction, keys in order: write amplification, x | 153 [153–153] ±0% |
| a row a transaction, keys in order: time, s | 21.3 [19.7–21.5] ±5% |
| a row a transaction, keys in order: files, MiB | 4,717 [4,717–4,717] ±0% |
| a row a transaction, keys in order: space amplification, x | 2.28 [2.28–2.28] ±0% |
| a row a transaction, keys in order: recovery, s | 1.57 [1.52–1.61] ±3% |
| a row a transaction, random keys: write amplification, x | 216 [216–217] ±0% |
| a row a transaction, random keys: time, s | 19.7 [19.5–20.7] ±3% |
| a row a transaction, random keys: files, MiB | 4,719 [4,719–4,719] ±0% |
| a row a transaction, random keys: space amplification, x | 2.27 [2.27–2.27] ±0% |
| a row a transaction, random keys: recovery, s | 2.52 [2.47–2.52] ±1% |
| 100 rows a transaction, random keys: write amplification, x | 48.0 [47.5–76.4] ±29% ⚠ |
| 100 rows a transaction, random keys: time, s | 4.51 [4.47–4.52] ±1% |
| 100 rows a transaction, random keys: files, MiB | 4,718 [4,718–4,718] ±0% |
| 100 rows a transaction, random keys: space amplification, x | 2.26 [2.26–2.26] ±0% |
| 100 rows a transaction, random keys: recovery, s | 2.82 [2.73–2.84] ±2% |
| 100 rows a transaction, random keys, 5M rows: write amplification, x | 142 [77.0–144] ±32% ⚠ |
| 100 rows a transaction, random keys, 5M rows: time, s | 648 [642–665] ±2% |
| 100 rows a transaction, random keys, 5M rows: files, MiB | 7,607 [7,607–8,503] ±7% |
| 100 rows a transaction, random keys, 5M rows: space amplification, x | 2.92 [2.92–3.26] ±7% |
| 100 rows a transaction, random keys, 5M rows: recovery, s | 923 [59.9–989] ±79% ⚠ |
| 100 rows a transaction, updates: write amplification, x | 94.7 [86.0–122] ±19% ⚠ |
| 100 rows a transaction, updates: time, s | 127 [119–147] ±11% ⚠ |
| 100 rows a transaction, updates: files, MiB | 7,600 [7,600–8,496] ±7% |
| 100 rows a transaction, updates: space amplification, x | 2.92 [2.92–3.26] ±7% |
| 100 rows a transaction, updates: recovery, s | 93.6 [83.7–125] ±21% ⚠ |
| 100 rows a transaction, deletes: write amplification, x | 151 [124–154] ±11% ⚠ |
| 100 rows a transaction, deletes: time, s | 151 [138–170] ±11% ⚠ |
| 100 rows a transaction, deletes: files, MiB | 7,662 [7,662–8,494] ±6% |
| 100 rows a transaction, deletes: space amplification, x | 3.06 [3.06–3.39] ±6% |
| 100 rows a transaction, deletes: recovery, s | 130 [118–134] ±6% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 15a14f564+changes — 2026-11-21T09:24:25+00:00

A commit costs ~16 KiB on APFS whatever the WAL does — APFS writes a
changed block elsewhere, so an overwrite costs as much as an append
there (§M14.28 L0).  Random keys among ten million rows put each row in
a leaf the 128 MiB cache rarely holds, so nearly every row costs a page
written when it is evicted or checkpointed: 8 KiB for 108 bytes.  That
is the B+tree's cost an LSM tree's runs avoid (§M14.28 L2–L5).  The
comparison with PostgreSQL and the runs on ext4 have no step yet
(PLAN.md §M14.28's study).

**An LSM table beside the B+tree** (`bash bench/all.sh wamp-lsm`;
`wamp.py --engine=lsm`, the same phases, a database and a port of their
own).  The text is one letter repeated, which the LSM tree's LZ4 runs
shrink to almost nothing — hence space amplification below 1:

| wamp-lsm | nucleus lsm |
|---|---:|
| bulk, 1M rows a transaction, keys in order: write amplification, x | 2.71 [2.71–2.71] ±0% |
| bulk, 1M rows a transaction, keys in order: time, s | 8.27 [8.25–8.30] ±0% |
| bulk, 1M rows a transaction, keys in order: files, MiB | 198 [198–198] ±0% |
| bulk, 1M rows a transaction, keys in order: space amplification, x | 0.192 [0.192–0.192] ±0% |
| bulk, 1M rows a transaction, keys in order: recovery, s | 0.980 [0.977–0.985] ±0% |
| bulk, 1M rows a transaction, random keys: write amplification, x | 2.92 [2.92–2.92] ±0% |
| bulk, 1M rows a transaction, random keys: time, s | 49.0 [48.8–49.2] ±0% |
| bulk, 1M rows a transaction, random keys: files, MiB | 429 [429–429] ±0% |
| bulk, 1M rows a transaction, random keys: space amplification, x | 0.208 [0.208–0.208] ±0% |
| bulk, 1M rows a transaction, random keys: recovery, s | 2.39 [2.38–2.41] ±1% |
| a row a transaction, keys in order: write amplification, x | 149 [149–149] ±0% |
| a row a transaction, keys in order: time, s | 17.3 [17.1–18.1] ±3% |
| a row a transaction, keys in order: files, MiB | 373 [373–373] ±0% |
| a row a transaction, keys in order: space amplification, x | 0.180 [0.180–0.180] ±0% |
| a row a transaction, keys in order: recovery, s | 0.328 [0.327–0.333] ±1% |
| a row a transaction, random keys: write amplification, x | 150 [150–150] ±0% |
| a row a transaction, random keys: time, s | 17.3 [17.2–17.6] ±1% |
| a row a transaction, random keys: files, MiB | 378 [378–378] ±0% |
| a row a transaction, random keys: space amplification, x | 0.182 [0.182–0.182] ±0% |
| a row a transaction, random keys: recovery, s | 0.327 [0.322–0.333] ±2% |
| 100 rows a transaction, random keys: write amplification, x | 3.10 [3.10–3.10] ±0% |
| 100 rows a transaction, random keys: time, s | 3.21 [3.21–3.27] ±1% |
| 100 rows a transaction, random keys: files, MiB | 379 [379–379] ±0% |
| 100 rows a transaction, random keys: space amplification, x | 0.181 [0.181–0.181] ±0% |
| 100 rows a transaction, random keys: recovery, s | 0.223 [0.223–0.227] ±1% |
| 100 rows a transaction, random keys, 5M rows: write amplification, x | 3.06 [3.06–3.06] ±0% |
| 100 rows a transaction, random keys, 5M rows: time, s | 173 [172–173] ±0% |
| 100 rows a transaction, random keys, 5M rows: files, MiB | 479 [479–479] ±0% |
| 100 rows a transaction, random keys, 5M rows: space amplification, x | 0.184 [0.184–0.184] ±0% |
| 100 rows a transaction, random keys, 5M rows: recovery, s | 5.88 [5.84–5.88] ±0% |
| 100 rows a transaction, updates: write amplification, x | 3.10 [3.10–3.11] ±0% |
| 100 rows a transaction, updates: time, s | 47.3 [47.1–48.3] ±1% |
| 100 rows a transaction, updates: files, MiB | 508 [508–508] ±0% |
| 100 rows a transaction, updates: space amplification, x | 0.195 [0.195–0.195] ±0% |
| 100 rows a transaction, updates: recovery, s | 0.971 [0.971–0.989] ±1% |
| 100 rows a transaction, deletes: write amplification, x | 2.05 [2.05–2.05] ±0% |
| 100 rows a transaction, deletes: time, s | 41.1 [41.0–41.2] ±0% |
| 100 rows a transaction, deletes: files, MiB | 519 [519–519] ±0% |
| 100 rows a transaction, deletes: space amplification, x | 0.208 [0.208–0.208] ±0% |
| 100 rows a transaction, deletes: recovery, s | 0.868 [0.858–0.880] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus f2ba1d0ab+changes — 2026-11-21T11:28:42+00:00

With `--values=random` (100 random hex digits, which LZ4 cannot shrink)
one run each (PLAN.md §M14.28 L5):

| phase, random values | write amplification B+tree / LSM | time, s | restart, s | space amplification |
|---|---:|---:|---:|---:|
| bulk, keys in order | 3.35 / 3.65 | 54 / 54 | 0.95 / 2.52 | 1.51 / 1.14 |
| bulk, random keys | 5.71 / 3.87 | 117 / 97 | 19.2 / 4.1 | 2.26 / 1.16 |
| a row a transaction, keys in order | 153 / 151 | 23 / 22 | 1.5 / 0.3 | 2.22 / 1.13 |
| a row a transaction, random keys | 216 / 151 | 22 / 20 | 2.4 / 0.4 | 2.21 / 1.13 |
| 100 rows a transaction, random keys | 77 / 4.05 | 5.4 / 4.6 | 2.7 / 0.2 | 2.20 / 1.13 |
| steady state, 5M rows | 79 / 4.05 | 725 / 246 | 134 / 8.3 | 3.12 / 1.13 |
| 1M updates | 128 / 4.08 | 157 / 64 | 111 / 1.4 | 3.11 / 1.18 |
| 1M deletes | 126 / 2.05 | 151 / 48 | 115 / 0.8 | 3.24 / 1.24 |

The B+tree's restart after the steady state replays up to a GiB of WAL
of random keys through its 128 MiB cache, a page read and written back
for nearly every row: 923 s in the whole run, its recovery writing
~35 GiB (one of three runs 60 s, writing 1.5 GiB), against the LSM
table's 5.9 s (issue #14).

**YCSB** (`bash bench/all.sh ycsb`; `crates/sql/tests/ycsb.rs`, PLAN.md
§M14.28 L5): a million rows of a `bigint` key and 100 bytes of text,
then 200,000 operations of each workload, keys by YCSB's scrambled
Zipfian (θ = 0.99), each a statement in a transaction of its own, one
client, in the process:

| ycsb | nucleus |
|---|---:|
| btree: load 1000000 rows, s | 0.812 [0.810–0.817] ±1% |
| btree: workload A, ops/s | 9,929 [9,697–10,063] ±3% |
| btree: workload A, read p50, µs | 13.7 [13.7–13.8] ±1% |
| btree: workload A, read p99, µs | 23.7 [23.1–24.0] ±3% |
| btree: workload A, update p50, µs | 176 [174–180] ±2% |
| btree: workload A, update p99, µs | 313 [291–349] ±10% |
| btree: workload B, ops/s | 34,391 [32,196–34,727] ±9% |
| btree: workload B, read p50, µs | 11.8 [11.8–11.9] ±2% |
| btree: workload B, read p99, µs | 23.1 [21.8–23.5] ±10% |
| btree: workload B, update p50, µs | 359 [355–374] ±4% |
| btree: workload B, update p99, µs | 521 [503–842] ±33% ⚠ |
| btree: workload E, ops/s | 16,833 [16,157–17,973] ±7% |
| btree: workload E, scan p50, µs | 27.8 [27.0–28.3] ±4% |
| btree: workload E, scan p99, µs | 60.6 [57.8–61.7] ±5% |
| btree: workload E, insert p50, µs | 635 [435–657] ±21% ⚠ |
| btree: workload E, insert p99, µs | 858 [830–888] ±6% |
| lsm: load 1000000 rows, s | 0.845 [0.840–0.853] ±1% |
| lsm: workload A, ops/s | 10,172 [10,056–10,272] ±2% |
| lsm: workload A, read p50, µs | 15.8 [15.7–15.8] ±0% |
| lsm: workload A, read p99, µs | 24.4 [24.0–25.2] ±3% |
| lsm: workload A, update p50, µs | 168 [167–171] ±2% |
| lsm: workload A, update p99, µs | 322 [287–369] ±13% ⚠ |
| lsm: workload B, ops/s | 32,417 [30,944–32,906] ±7% |
| lsm: workload B, read p50, µs | 13.5 [13.3–13.6] ±3% |
| lsm: workload B, read p99, µs | 19.8 [19.4–20.9] ±11% ⚠ |
| lsm: workload B, update p50, µs | 369 [360–375] ±5% |
| lsm: workload B, update p99, µs | 547 [499–734] ±27% ⚠ |
| lsm: workload E, ops/s | 14,169 [13,972–14,533] ±3% |
| lsm: workload E, scan p50, µs | 34.6 [33.9–35.1] ±3% |
| lsm: workload E, scan p99, µs | 73.5 [70.9–75.3] ±4% |
| lsm: workload E, insert p50, µs | 668 [650–674] ±4% |
| lsm: workload E, insert p99, µs | 843 [821–899] ±6% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T21:41:38+00:00

A write there takes ~170–660 µs, the device's: a barrier `fsync` after
a millisecond idle costs ~640–770 µs on this SSD, back to back ~200 µs.

## Time travel

The cost of recording each commit's time (`bash bench/all.sh history`;
§M14.14, `crates/kv/examples/bench.rs` with `NUCLEUS_HISTORY=3600`
against none, and `bench/inserts.py` — one-row `INSERT`s from eight
sessions — against a server with `--history 1h`; `snapshot_at`: a time
to its commit, ~3,000 recorded):

| history | no history (/s) | history (/s) |
|---|---:|---:|
| single-row commits | 5,196 [5,085–5,503] ±11% ⚠ | 5,316 [5,096–5,492] ±9% |
| rows in 500-row commits | 1,204,947 [1,182,016–1,209,667] ±1% | 1,206,755 [1,157,752–1,210,344] ±6% |
| group commit, 8 threads | 14,886 [14,466–15,607] ±13% ⚠ | 14,556 [13,327–14,772] ±9% |
| point reads | 1,707,314 [1,561,807–1,719,886] ±7% | 1,699,065 [1,595,307–1,738,032] ±10% ⚠ |
| full scan | 8,573,510 [8,500,015–8,593,180] ±2% | 8,538,064 [8,368,286–8,600,355] ±3% |
| snapshot_at | — | 213,343 [211,199–216,219] ±3% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T22:01:59+00:00

| history-sql | no history (/s) | history (/s) |
|---|---:|---:|
| one-row INSERTs, 8 sessions | 13,944 [13,874–14,372] ±2% | 14,006 [13,481–14,164] ±2% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus — nucleus 18041cf93+changes — 2026-11-20T22:02:27+00:00

Within the runs' spread: the times are deferred puts, in the tree in
bulk (DOCS.md §4.5).

## Branches

`bash bench/all.sh branches` (PLAN.md §M14.13 B5,
`crates/kv/examples/branches.rs`): real files, rows of a 16-byte key and
a 100-byte value, a 256 MiB cache, an hour of history; "in use" the data
file's pages in use, the branches' together; "+space" what making the
branch took; "as of" a read of the branch as it was 10,000 rows'
changes ago; then a branch of the 4,000,000 rows with rows changed on it
— together (adjacent keys) or spread (one a leaf, up to every leaf) —
and dropped, "freed" what the drop gave back:

| branches | nucleus |
|---|---:|
| 100000 rows: in use, MiB | 14.1 [14.1–14.1] ±0% |
| 100000 rows: create, ms | 2.36 [1.87–2.84] ±23% ⚠ |
| 100000 rows: +space, MiB | 0.008 [0.008–0.008] ±0% |
| 100000 rows: drop, ms | 2.07 [1.74–2.73] ±27% ⚠ |
| 100000 rows: as of 10k commits before, ms | 37.0 [36.0–39.0] ±6% |
| 100000 rows: reset, ms | 8.09 [8.06–8.66] ±4% |
| 1000000 rows: in use, MiB | 141 [141–141] ±0% |
| 1000000 rows: create, ms | 5.92 [5.06–6.68] ±18% ⚠ |
| 1000000 rows: +space, MiB | 0.000 [0.000–0.000] ±0% |
| 1000000 rows: drop, ms | 3.36 [2.88–3.49] ±10% |
| 1000000 rows: as of 10k commits before, ms | 257 [254–263] ±2% |
| 1000000 rows: reset, ms | 41.8 [39.6–42.8] ±4% |
| 4000000 rows: in use, MiB | 562 [562–562] ±0% |
| 4000000 rows: create, ms | 5.54 [5.08–7.94] ±21% ⚠ |
| 4000000 rows: +space, MiB | 0.000 [0.000–0.000] ±0% |
| 4000000 rows: drop, ms | 2.23 [1.79–2.57] ±31% ⚠ |
| 4000000 rows: as of 10k commits before, ms | 912 [906–915] ±1% |
| 4000000 rows: reset, ms | 133 [128–143] ±6% |
| 1000 changed, together: +space, MiB | 0.312 [0.312–0.312] ±0% |
| 1000 changed, together: drop, ms | 2.73 [2.41–2.87] ±12% ⚠ |
| 1000 changed, together: freed, MiB | 0.312 [0.312–0.312] ±0% |
| 100000 changed, together: +space, MiB | 28.1 [28.1–28.1] ±0% |
| 100000 changed, together: drop, ms | 17.0 [16.9–17.5] ±3% |
| 100000 changed, together: freed, MiB | 28.1 [28.1–28.1] ±0% |
| 1000 changed, spread: +space, MiB | 11.3 [11.3–11.3] ±0% |
| 1000 changed, spread: drop, ms | 143 [141–148] ±3% |
| 1000 changed, spread: freed, MiB | 11.3 [11.3–11.3] ±0% |
| 10000 changed, spread: +space, MiB | 82.7 [82.7–82.7] ±0% |
| 10000 changed, spread: drop, ms | 174 [168–178] ±2% |
| 10000 changed, spread: freed, MiB | 82.7 [82.7–82.7] ±0% |
| 100000 changed, spread: +space, MiB | 565 [565–565] ±0% |
| 100000 changed, spread: drop, ms | 328 [322–332] ±3% |
| 100000 changed, spread: freed, MiB | 565 [565–565] ±0% |
| 1000000 changed, spread: +space, MiB | 1,124 [1,124–1,124] ±0% |
| 1000000 changed, spread: drop, ms | 528 [520–539] ±3% |
| 1000000 changed, spread: freed, MiB | 1,123 [1,123–1,123] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T22:04:24+00:00

Making a branch is a checkpoint, whatever the size, and takes no page.
`AS OF` is a pass over the branch's entries (DOCS.md §5.2): about
200 ns an entry.  The reset is of that branch, so it drops what its
roll-back changed.  The space a changed branch takes is the pages it
copied — a row's leaf and the internal nodes above it, the old versions
kept with the history — and the drop gives all of it back.  The drop's
time follows the internal nodes copied rather than the rows: it reads
each copied node's children for their births, the ones not copied too,
so changes spread over the table read up to its leaf level once.

## Embedded editions

PLAN.md §M14.23.  Python's in the process and over TCP are under *Local
runs* above.  WebAssembly in Node, in memory (`bash bench/all.sh wasm`;
`bench/embedded/wasm.mjs`: an `INSERT … SELECT` of 200,000 rows, a
`GROUP BY` over them, a point query — one round trip into the engine,
the median of 1,000):

| wasm | nucleus |
|---|---:|
| INSERT … SELECT of 200000 rows, ms | 220 [216–223] ±1% |
| GROUP BY over them, ms | 26.4 [26.0–26.9] ±1% |
| a point query, µs | 26.1 [25.5–26.6] ±2% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus — nucleus 18041cf93+changes — 2026-11-20T22:05:51+00:00

In a browser (`bash bench/all.sh browser`; `drivers/js/test/browser.mjs`,
headless Chrome): the first answer after the page starts — the engine
fetched and compiled, the database opened from OPFS, three statements —
on a new profile's first visit, after a reload, with the engine left
open by the page, and after the browser restarted:

| browser | nucleus (ms) |
|---|---:|
| first answer, a new profile's first visit (cold) | 165 [160–180] ±5% |
| first answer, after a reload | 113 [111–117] ±2% |
| first answer, left open | 62.0 [62.0–65.0] ±2% |
| first answer, after a reload of a page that left it open | 66.0 [62.0–69.0] ±4% |
| first answer, after the browser restarted (cold) | 127 [123–128] ±2% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 18041cf93+changes — 2026-11-20T22:05:54+00:00

Sizes (LTO, one codegen unit, no model modules; the budget for phones
is 8 MiB on arm64, checked by both build scripts):

| Edition | Size |
|---------|------|
| Browser (`drivers/js/wasm/build.sh`) | 7.1 MB; 1.65 MB brotli |
| iOS arm64: what the engine adds to an app, linked and stripped (`drivers/swift/build-xcframework.sh`) | 7.0 MiB (7,359,552 B); 2.4 MB xz |
| Android arm64-v8a: `libnucleus_android.so` (`drivers/android/build-native.sh`) | 7.45 MiB (7,813,912 B); 3.8 MB gzip, 2.6 MB xz |
| Android x86_64 (emulators) | 9.3 MB |

## Targets

PLAN.md §M14.11 asks for targets set from the first results, and
§M14.20 is done when analytical queries beat PostgreSQL by them and
transactional workloads stay within their margin.  First set from the
first `bench/run.py` run (`79506d4`, 2026-10-27) — within 2× of
PostgreSQL on loads and updates, within 3× on scans, sorts and joins, at
least even on reads by key —, checked here by the results above (the
warm medians' ratios; geometric means over the queries) against the
native, tuned PostgreSQL 18.7:

| target | now | met |
|---|---|---|
| TPC-H SF 1, column tables: geometric mean ≤ 0.8× PostgreSQL's time | 0.29× | yes |
| ClickBench 1M rows, column table: geometric mean ≤ 0.5× | 0.14× | yes |
| column tables: no TPC-H or ClickBench query above 3× | the highest TPC-H Q19 1.91×, ClickBench Q27 0.50× | yes |
| row tables: geometric mean ≤ 2× (TPC-H, ClickBench) | TPC-H 0.56×, ClickBench 0.54× | yes |
| loads ≤ 2× (`bench/run.py`, TPC-H, ClickBench) | 0.55×; TPC-H 1.21× row, 1.03× column; ClickBench 1.16×, 1.00× | yes |
| updates and deletes ≤ 2× (`bench/run.py`) | update 0.34×, delete 1.44× (an LSM table: 0.40×, 2.33×) | yes (the B+tree, the default) |
| scans, aggregates, sorts and joins ≤ 3× (`bench/run.py`) | 0.25–0.89× (count(*) 0.41×, the filtered count cold 0.89×) | yes |
| reads by key ≤ 1× (`bench/run.py`) | 1.04×, the intervals overlapping (23.8 ms against 23.0 for 1,000 reads; cold 0.87×) | yes |

Against PostgreSQL in Docker with its defaults (the runs before
§M14.31) every target was met, reads by key at 0.5×; natively, with
its buffers sized for the machine, PostgreSQL's 1,000 reads by key took
24 ms, nucleus's 59: nucleus's own per-statement cost — planning every
literal anew and loopback TCP — which §M14.33 R2 took away (a read by
key's plan kept by its shape, the server on a Unix socket, the
session's floor lowered).  §M14.33 R3–R4 and the sweep before F1's
whole run (S1–S20) brought row tables from 1.27× (TPC-H) and 1.10×
(ClickBench) to 0.56× and 0.54×; their worst queries now TPC-H Q12
1.15× and Q6 1.06×, ClickBench Q0 0.99×.  PostgreSQL's own suite load
and update vary between whole runs (load 1.03 s in the run before this
one, 1.74 s here), so those two ratios move with it.  The CI's shared
runners are too noisy to fail on a ratio, so a target is checked here,
by a local run.
