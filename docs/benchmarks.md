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
| load (INSERT … SELECT) | 967 [955–972] ±1% | 981 [978–992] ±1% | 1,029 [989–1,258] ±10% ⚠ | ≈ (0.94×) | ≈ (0.95×) |
| count(*) (cold) | 6.54 [6.12–6.77] ±4% | 9.21 [8.26–11.8] ±14% ⚠ | 27.5 [27.1–28.4] ±2% | 0.24× | 0.33× |
| count(*) | 6.04 [5.85–6.43] ±4% | 8.01 [7.82–8.96] ±6% | 16.3 [16.1–16.8] ±2% | 0.37× | 0.49× |
| filtered count (cold) | 20.9 [19.2–21.5] ±5% | 35.7 [34.9–36.8] ±2% | 21.1 [20.9–21.3] ±1% | ≈ (0.99×) | 1.70× |
| filtered count | 9.42 [9.02–9.95] ±4% | 11.3 [10.8–11.9] ±4% | 15.8 [15.5–15.9] ±1% | 0.60× | 0.72× |
| sum (cold) | 13.7 [13.3–14.1] ±2% | 26.8 [25.9–27.2] ±2% | 20.8 [20.4–21.9] ±3% | 0.66× | 1.29× |
| sum | 9.25 [8.31–9.44] ±5% | 9.46 [9.31–9.75] ±2% | 15.1 [14.8–17.2] ±7% | 0.61× | 0.63× |
| group by (1000 groups) (cold) | 18.9 [18.9–19.4] ±1% | 31.1 [30.7–35.8] ±7% | 36.9 [35.6–37.9] ±2% | 0.51× | ≈ (0.84×) |
| group by (1000 groups) | 13.9 [13.8–14.3] ±1% | 14.8 [14.5–15.4] ±2% | 31.8 [31.4–32.7] ±2% | 0.44× | 0.46× |
| sort (cold) | 79.7 [74.8–85.3] ±5% | 89.6 [86.3–91.2] ±2% | 141 [140–141] ±0% | 0.57× | 0.64× |
| sort | 72.2 [71.5–75.5] ±2% | 74.5 [71.3–77.0] ±3% | 131 [130–134] ±1% | 0.55× | 0.57× |
| self-join (cold) | 74.7 [73.5–75.4] ±1% | 89.5 [88.3–90.7] ±1% | 125 [124–136] ±4% | 0.60× | 0.71× |
| self-join | 64.6 [64.4–68.0] ±3% | 66.1 [65.2–66.2] ±1% | 117 [114–127] ±5% | 0.55× | 0.56× |
| point reads (1000 by key) (cold) | 26.4 [25.5–26.8] ±2% | 27.3 [26.7–27.9] ±2% | 31.1 [30.4–39.3] ±11% ⚠ | 0.85× | 0.88× |
| point reads (1000 by key) | 24.4 [24.1–26.8] ±5% | 25.1 [24.6–25.6] ±2% | 23.5 [23.2–23.7] ±1% | ≈ (1.04×) | ≈ (1.07×) |
| update 25% | 573 [569–581] ±1% | 666 [660–708] ±3% | 911 [871–1,351] ±20% ⚠ | 0.63× | 0.73× |
| delete 10% | 96.1 [94.5–96.9] ±1% | 155 [154–157] ±1% | 68.3 [67.5–69.8] ±1% | 1.41× | 2.27× |
| count after writes | 9.86 [9.71–9.95] ±1% | 28.9 [26.9–31.5] ±6% | 15.4 [14.5–16.0] ±4% | 0.64× | 1.88× |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 0.1.0, nucleus (lsm) 0.1.0, postgresql 18.7 (Homebrew) — nucleus c9dad1615+changes — 2026-11-18T09:18:53+00:00

Against the native, tuned PostgreSQL, reads by key are even with its
time (1.02×, the intervals overlapping; cold 0.85×; nucleus reached by
its Unix socket, as `psql` reaches PostgreSQL — `nucleus server
--socket`), the scans, groupings, sorts and joins 0.24–0.67×, and a
`DELETE` of a tenth of the rows the one lag, 1.41× (Targets, below).
An LSM table reads warm as the B+tree's or near it (count(*) 8.0 against
6.0 ms, the rest within 1.07×); cold, after its blocks left the cache,
1.3–1.9× (each block decompressed), after the writes 2.9× (several
sources merged an entry at a time), and its `DELETE` 2.27× PostgreSQL's
(PLAN.md §M14.28 L5's next steps).

**Where a read by key goes** (`bash bench/all.sh phases`;
`crates/sql/tests/statement_phases.rs`, PLAN.md §M14.33 R1): the
suite's `SELECT s FROM b WHERE id = <n>` on its million rows, in the
process, phase by phase, and the operating system's round trip on
loopback TCP (how `nucleus sql -h` reaches the server) and on a Unix
socket (how `psql` reaches PostgreSQL here):

| phases | nucleus (µs) |
|---|---:|
| a read by key, literal, in the process | 8.06 [8.02–8.07] ±1% |
| parsing it | 1.01 [1.00–1.01] ±1% |
| a read by key, a parameter, in the process | 17.8 [17.7–17.9] ±1% |
| a read by key, the same text again, in the process | 5.39 [5.35–5.43] ±1% |
| EXPLAIN of it (binding and planning), in the process | 15.6 [15.5–15.7] ±1% |
| SELECT 1, in the process | 4.22 [4.21–4.27] ±1% |
| a round trip, loopback TCP | 15.7 [15.6–15.9] ±2% |
| a round trip, Unix socket | 4.18 [3.90–4.71] ±14% ⚠ |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus eeb542857+changes — 2026-11-17T16:47:19+00:00

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
| SELECT 1, in the process, µs | 9.88 [9.83–9.88] ±0% |
| SELECT 1, loopback TCP to nucleus server, µs | 28.5 [28.2–30.1] ±3% |
| 1,000,000 rows of two columns into numpy, in the process, ms | 31.4 [30.6–33.3] ±4% |
| 1,000,000 rows of two columns into numpy, in the process, a column table, ms | 10.8 [10.4–11.1] ±3% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus — nucleus 7ff7b48e3+changes — 2026-11-17T09:17:51+00:00

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
| Q1 (cold), ms | 315 [310–340] ±4% | 163 [159–166] ±2% | 503 [497–505] ±1% | 0.63× | 0.32× |
| Q1, ms | 235 [222–247] ±4% | 146 [140–158] ±5% | 470 [468–477] ±1% | 0.50× | 0.31× |
| Q2 (cold), ms | 35.6 [35.2–36.1] ±1% | 40.0 [39.1–40.3] ±1% | 151 [150–156] ±2% | 0.24× | 0.26× |
| Q2, ms | 23.4 [22.6–24.0] ±2% | 23.9 [23.2–25.1] ±3% | 87.1 [86.2–101] ±7% | 0.27× | 0.27× |
| Q3 (cold), ms | 280 [270–332] ±10% | 92.0 [91.6–93.5] ±1% | 422 [417–440] ±2% | 0.66× | 0.22× |
| Q3, ms | 160 [155–168] ±3% | 68.1 [66.0–70.1] ±2% | 262 [260–265] ±1% | 0.61× | 0.26× |
| Q4 (cold), ms | 153 [145–164] ±5% | 83.3 [83.0–84.9] ±1% | 155 [153–159] ±2% | ≈ (0.99×) | 0.54× |
| Q4, ms | 80.0 [77.7–92.3] ±7% | 63.3 [62.8–64.8] ±1% | 75.2 [74.6–76.0] ±1% | ≈ (1.06×) | 0.84× |
| Q5 (cold), ms | 275 [269–291] ±3% | 95.3 [94.1–96.4] ±1% | 258 [257–272] ±2% | ≈ (1.06×) | 0.37× |
| Q5, ms | 165 [159–166] ±2% | 71.3 [68.2–74.5] ±4% | 146 [145–147] ±1% | 1.13× | 0.49× |
| Q6 (cold), ms | 247 [246–284] ±7% | 85.8 [83.9–89.4] ±2% | 178 [177–184] ±2% | 1.39× | 0.48× |
| Q6, ms | 158 [153–162] ±2% | 66.4 [65.0–68.1] ±2% | 93.9 [92.3–105] ±6% | 1.68× | 0.71× |
| Q7 (cold), ms | 257 [251–276] ±4% | 107 [104–107] ±1% | 997 [979–1,008] ±1% | 0.26× | 0.11× |
| Q7, ms | 154 [150–157] ±2% | 79.9 [79.4–81.5] ±1% | 375 [371–380] ±1% | 0.41× | 0.21× |
| Q8 (cold), ms | 151 [146–175] ±8% | 161 [160–166] ±2% | 165 [165–171] ±2% | ≈ (0.91×) | ≈ (0.97×) |
| Q8, ms | 82.4 [81.3–86.5] ±2% | 111 [109–113] ±1% | 69.1 [68.4–70.8] ±1% | 1.19× | 1.60× |
| Q9 (cold), ms | 482 [448–522] ±7% | 278 [269–283] ±2% | 866 [859–874] ±1% | 0.56× | 0.32× |
| Q9, ms | 344 [340–359] ±2% | 249 [248–291] ±7% | 656 [652–678] ±2% | 0.52× | 0.38× |
| Q10 (cold), ms | 240 [237–244] ±1% | 159 [157–161] ±1% | 213 [212–215] ±0% | 1.13× | 0.75× |
| Q10, ms | 147 [146–148] ±1% | 129 [128–138] ±3% | 129 [127–132] ±2% | 1.14× | ≈ (1.00×) |
| Q11 (cold), ms | 51.4 [45.3–55.2] ±8% | 19.0 [18.8–20.2] ±3% | 75.9 [71.3–78.7] ±4% | 0.68× | 0.25× |
| Q11, ms | 33.4 [32.6–34.9] ±3% | 13.8 [13.5–13.8] ±1% | 30.7 [26.5–33.6] ±9% | ≈ (1.09×) | 0.45× |
| Q12 (cold), ms | 359 [324–368] ±5% | 84.0 [81.8–89.9] ±4% | 243 [241–246] ±1% | 1.48× | 0.35× |
| Q12, ms | 192 [189–201] ±2% | 60.1 [59.5–60.8] ±1% | 142 [141–143] ±0% | 1.36× | 0.42× |
| Q13 (cold), ms | 126 [118–134] ±5% | 92.8 [90.5–93.1] ±1% | 505 [501–573] ±6% | 0.25× | 0.18× |
| Q13, ms | 92.3 [91.2–95.7] ±2% | 61.3 [60.5–62.5] ±2% | 416 [408–449] ±4% | 0.22× | 0.15× |
| Q14 (cold), ms | 238 [231–280] ±10% | 73.6 [73.1–76.3] ±2% | 225 [218–226] ±1% | ≈ (1.06×) | 0.33× |
| Q14, ms | 127 [124–143] ±6% | 52.0 [51.0–52.7] ±1% | 167 [166–169] ±1% | 0.76× | 0.31× |
| Q15 (cold), ms | 271 [226–277] ±8% | 79.2 [77.3–83.9] ±4% | 205 [200–219] ±4% | 1.32× | 0.39× |
| Q15, ms | 128 [125–131] ±2% | 57.8 [57.3–58.7] ±1% | 115 [113–119] ±2% | 1.12× | 0.50× |
| Q16 (cold), ms | 128 [126–131] ±2% | 118 [113–129] ±5% | 99.6 [98.9–102] ±1% | 1.29× | 1.18× |
| Q16, ms | 100 [99.5–103] ±2% | 78.5 [74.9–79.3] ±3% | 91.3 [91.0–92.4] ±1% | ≈ (1.10×) | 0.86× |
| Q17 (cold), ms | 24.3 [23.9–27.1] ±5% | 64.6 [63.9–72.5] ±6% | 473 [471–477] ±0% | 0.05× | 0.14× |
| Q17, ms | 13.0 [12.2–13.8] ±5% | 36.2 [35.2–37.6] ±3% | 338 [330–343] ±2% | 0.04× | 0.11× |
| Q18 (cold), ms | 353 [350–400] ±6% | 182 [178–194] ±4% | 2,585 [2,567–2,657] ±1% | 0.14× | 0.07× |
| Q18, ms | 259 [255–276] ±3% | 163 [160–170] ±2% | 2,552 [2,513–2,740] ±4% | 0.10× | 0.06× |
| Q19 (cold), ms | 43.7 [40.7–50.6] ±9% | 84.4 [80.2–92.7] ±5% | 41.9 [40.6–47.8] ±7% | ≈ (1.04×) | 2.02× |
| Q19, ms | 17.3 [16.5–17.7] ±3% | 39.6 [39.2–44.3] ±5% | 17.8 [17.3–18.3] ±2% | ≈ (0.97×) | 2.22× |
| Q20 (cold), ms | 182 [174–185] ±3% | 212 [207–226] ±3% | 202 [199–203] ±1% | 0.90× | ≈ (1.05×) |
| Q20, ms | 96.4 [91.1–105] ±6% | 169 [161–174] ±3% | 61.0 [60.4–62.4] ±1% | 1.58× | 2.78× |
| Q21 (cold), ms | 404 [356–440] ±8% | 183 [181–187] ±1% | 968 [943–979] ±2% | 0.42× | 0.19× |
| Q21, ms | 237 [232–251] ±3% | 144 [141–148] ±2% | 274 [266–282] ±2% | 0.86× | 0.53× |
| Q22 (cold), ms | 57.6 [54.9–58.4] ±3% | 34.0 [33.3–35.5] ±2% | 43.4 [41.8–44.7] ±3% | 1.33× | 0.78× |
| Q22, ms | 33.2 [32.2–35.3] ±4% | 15.5 [15.3–15.7] ±1% | 35.2 [34.9–37.4] ±3% | ≈ (0.94×) | 0.44× |
| load, s | 29.9 [29.4–31.0] ±2% | 23.6 [23.2–23.9] ±1% | 21.5 [21.0–22.8] ±3% | 1.40× | ≈ (1.10×) |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus (row) 0.1.0, nucleus (column) 0.1.0, postgresql 18.7 (Homebrew) — nucleus b7219825c+changes — 2026-11-18T01:33:15+00:00

Every result agrees with PostgreSQL's.  Column tables 0.46× PostgreSQL's
time in the geometric mean of the warm runs, faster on 18 of the 22
queries; row tables 0.65×, faster on 12.  The loads — `COPY`, the
indexes and `VACUUM ANALYZE` — 1.40× PostgreSQL's in rows, 1.10× in
columns (PLAN.md §M14.33 R4: what is left is `COPY`'s parsing of the
text, the index builds and a load into a table made before its
transaction, whose rows are logged twice).

Where the time goes against the tuned PostgreSQL:

- **Column tables**: Q20 2.8×, Q19 2.2×, Q8 1.6× the highest.  Q19's
  `OR` of three conjunctions over `part` and `lineitem` fetches ~14,000
  `lineitem` rows by key from the column table, where PostgreSQL reads a
  heap tuple each; Q8's `part`–`lineitem` join is a near tie between
  hashing `lineitem`'s matches and looking them up per `part` row,
  decided by `ANALYZE`'s sample (PLAN.md §M14.12).
- **Row tables**: Q6 1.7×, Q20 1.6×, Q12 1.4×, Q8 1.2×, Q10 and Q5
  1.1× — `lineitem`'s rows read for a filter that keeps few of them,
  against a PostgreSQL whose 8 GB of buffers hold the whole database and
  whose workers scan it in parallel.

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
| Q0 (cold), ms | 85.7 [76.3–88.4] ±7% | 5.35 [5.17–6.19] ±7% | 117 [116–118] ±1% | 0.73× | 0.05× |
| Q0, ms | 25.4 [24.5–25.9] ±2% | 2.62 [2.50–3.28] ±11% ⚠ | 25.6 [25.3–28.7] ±5% | ≈ (0.99×) | 0.10× |
| Q1 (cold), ms | 123 [91.3–136] ±18% ⚠ | 5.85 [5.67–6.26] ±4% | 127 [124–128] ±1% | ≈ (0.97×) | 0.05× |
| Q1, ms | 39.3 [36.7–40.4] ±4% | 3.11 [3.02–3.17] ±2% | 57.1 [56.2–60.1] ±3% | 0.69× | 0.05× |
| Q2 (cold), ms | 94.4 [91.3–137] ±19% ⚠ | 6.42 [6.27–6.77] ±3% | 129 [128–130] ±1% | ≈ (0.73×) | 0.05× |
| Q2, ms | 38.1 [37.9–40.5] ±3% | 3.11 [2.90–3.21] ±4% | 58.9 [58.4–60.7] ±2% | 0.65× | 0.05× |
| Q3 (cold), ms | 95.5 [94.1–97.2] ±1% | 6.05 [5.90–6.54] ±4% | 120 [119–123] ±1% | 0.79× | 0.05× |
| Q3, ms | 35.7 [34.4–37.4] ±3% | 2.80 [2.74–2.99] ±4% | 52.0 [51.5–52.8] ±1% | 0.69× | 0.05× |
| Q4 (cold), ms | 108 [104–118] ±5% | 16.6 [16.2–17.4] ±3% | 243 [241–256] ±2% | 0.44× | 0.07× |
| Q4, ms | 46.3 [45.1–48.5] ±3% | 13.3 [13.1–14.0] ±3% | 237 [235–238] ±1% | 0.20× | 0.06× |
| Q5 (cold), ms | 108 [97.6–146] ±19% ⚠ | 15.4 [14.9–15.8] ±2% | 308 [307–310] ±0% | 0.35× | 0.05× |
| Q5, ms | 45.7 [44.9–51.1] ±5% | 11.7 [11.5–11.8] ±1% | 224 [222–226] ±1% | 0.20× | 0.05× |
| Q6 (cold), ms | 109 [96.2–142] ±17% ⚠ | 9.08 [8.46–9.82] ±6% | 129 [126–136] ±3% | ≈ (0.85×) | 0.07× |
| Q6, ms | 38.4 [37.4–42.7] ±5% | 6.36 [5.92–6.75] ±5% | 54.3 [53.8–56.5] ±2% | 0.71× | 0.12× |
| Q7 (cold), ms | 102 [97.1–122] ±10% | 6.28 [6.11–6.48] ±2% | 123 [121–127] ±2% | ≈ (0.84×) | 0.05× |
| Q7, ms | 37.7 [36.5–40.2] ±4% | 3.21 [3.18–3.27] ±1% | 56.1 [55.6–56.3] ±0% | 0.67× | 0.06× |
| Q8 (cold), ms | 153 [124–154] ±9% | 25.8 [25.6–26.1] ±1% | 380 [377–388] ±1% | 0.40× | 0.07× |
| Q8, ms | 55.7 [54.9–57.3] ±2% | 22.0 [21.6–22.5] ±2% | 377 [375–391] ±2% | 0.15× | 0.06× |
| Q9 (cold), ms | 196 [194–202] ±2% | 69.7 [69.3–70.6] ±1% | 462 [459–468] ±1% | 0.43× | 0.15× |
| Q9, ms | 103 [101–118] ±7% | 63.0 [62.3–63.4] ±1% | 391 [387–398] ±1% | 0.26× | 0.16× |
| Q10 (cold), ms | 99.2 [94.7–142] ±21% ⚠ | 9.37 [9.26–9.90] ±3% | 126 [123–127] ±1% | ≈ (0.79×) | 0.07× |
| Q10, ms | 38.6 [38.4–42.4] ±4% | 5.53 [5.38–5.87] ±4% | 58.3 [57.5–60.6] ±3% | 0.66× | 0.09× |
| Q11 (cold), ms | 115 [101–140] ±16% ⚠ | 9.97 [9.58–10.6] ±4% | 126 [124–134] ±3% | ≈ (0.91×) | 0.08× |
| Q11, ms | 41.0 [39.2–44.1] ±5% | 6.14 [5.85–6.30] ±3% | 59.9 [58.7–64.0] ±4% | 0.68× | 0.10× |
| Q12 (cold), ms | 103 [92.9–149] ±20% ⚠ | 14.8 [14.4–15.6] ±3% | 132 [130–169] ±12% ⚠ | ≈ (0.78×) | 0.11× |
| Q12, ms | 42.6 [41.4–46.8] ±5% | 10.4 [10.3–10.5] ±1% | 64.6 [63.7–65.0] ±1% | 0.66× | 0.16× |
| Q13 (cold), ms | 127 [116–154] ±14% ⚠ | 26.0 [25.0–27.1] ±4% | 145 [143–148] ±1% | ≈ (0.88×) | 0.18× |
| Q13, ms | 55.9 [54.7–56.9] ±2% | 19.5 [19.1–20.5] ±3% | 73.4 [73.1–75.5] ±1% | 0.76× | 0.27× |
| Q14 (cold), ms | 142 [100–143] ±14% ⚠ | 16.0 [15.4–16.5] ±3% | 124 [124–130] ±2% | ≈ (1.14×) | 0.13× |
| Q14, ms | 45.3 [43.7–45.7] ±2% | 11.2 [11.0–11.3] ±1% | 60.0 [59.1–62.0] ±2% | 0.76× | 0.19× |
| Q15 (cold), ms | 114 [103–151] ±15% ⚠ | 21.7 [21.0–22.5] ±3% | 247 [244–263] ±3% | 0.46× | 0.09× |
| Q15, ms | 50.7 [49.3–52.4] ±2% | 17.4 [16.4–17.7] ±3% | 237 [237–244] ±1% | 0.21× | 0.07× |
| Q16 (cold), ms | 120 [119–125] ±2% | 32.0 [30.7–34.4] ±4% | 324 [323–325] ±0% | 0.37× | 0.10× |
| Q16, ms | 62.4 [62.1–62.8] ±0% | 26.3 [25.4–27.0] ±3% | 221 [217–233] ±3% | 0.28× | 0.12× |
| Q17 (cold), ms | 153 [113–163] ±13% ⚠ | 26.4 [26.0–27.8] ±3% | 135 [131–139] ±2% | ≈ (1.13×) | 0.20× |
| Q17, ms | 59.0 [57.0–59.5] ±2% | 20.9 [20.6–23.0] ±5% | 74.4 [73.0–76.5] ±2% | 0.79× | 0.28× |
| Q18 (cold), ms | 233 [205–250] ±7% | 132 [130–134] ±1% | 497 [494–498] ±0% | 0.47× | 0.27× |
| Q18, ms | 157 [154–162] ±2% | 121 [120–125] ±2% | 398 [384–406] ±2% | 0.40× | 0.31× |
| Q19 (cold), ms | 102 [93.9–110] ±7% | 4.17 [3.98–4.30] ±3% | 129 [125–140] ±4% | 0.79× | 0.03× |
| Q19, ms | 41.4 [40.4–42.4] ±2% | 2.99 [2.96–3.04] ±1% | 52.9 [51.9–55.6] ±3% | 0.78× | 0.06× |
| Q20 (cold), ms | 146 [106–148] ±16% ⚠ | 26.2 [24.8–28.9] ±6% | 130 [127–132] ±1% | ≈ (1.12×) | 0.20× |
| Q20, ms | 42.9 [40.6–50.4] ±9% | 7.02 [6.77–7.22] ±3% | 65.4 [65.1–66.2] ±1% | 0.66× | 0.11× |
| Q21 (cold), ms | 149 [118–153] ±10% | 26.5 [25.8–28.9] ±5% | 132 [131–133] ±1% | ≈ (1.13×) | 0.20× |
| Q21, ms | 48.0 [46.2–49.1] ±2% | 6.51 [6.37–6.96] ±4% | 66.8 [65.9–67.9] ±1% | 0.72× | 0.10× |
| Q22 (cold), ms | 133 [113–153] ±12% ⚠ | 36.1 [35.7–39.1] ±4% | 140 [138–165] ±8% | ≈ (0.96×) | 0.26× |
| Q22, ms | 50.8 [48.9–52.4] ±3% | 6.88 [6.83–6.96] ±1% | 71.0 [70.0–71.2] ±1% | 0.72× | 0.10× |
| Q23 (cold), ms | 116 [115–121] ±2% | 131 [128–134] ±2% | 133 [132–137] ±2% | 0.87× | ≈ (0.99×) |
| Q23, ms | 49.7 [48.0–53.2] ±4% | 112 [109–127] ±6% | 66.0 [65.6–68.0] ±2% | 0.75× | 1.70× |
| Q24 (cold), ms | 100 [93.9–141] ±18% ⚠ | 12.6 [10.7–12.9] ±8% | 128 [126–136] ±3% | ≈ (0.78×) | 0.10× |
| Q24, ms | 39.1 [38.4–42.4] ±4% | 6.04 [5.44–6.07] ±5% | 59.2 [58.1–61.2] ±2% | 0.66× | 0.10× |
| Q25 (cold), ms | 112 [87.4–135] ±17% ⚠ | 11.9 [11.4–12.6] ±4% | 126 [124–129] ±2% | ≈ (0.88×) | 0.09× |
| Q25, ms | 40.5 [38.5–40.9] ±2% | 6.78 [6.45–7.14] ±4% | 57.9 [57.8–61.8] ±3% | 0.70× | 0.12× |
| Q26 (cold), ms | 113 [94.5–146] ±19% ⚠ | 13.7 [13.6–14.0] ±1% | 133 [133–140] ±2% | ≈ (0.84×) | 0.10× |
| Q26, ms | 41.0 [39.2–52.1] ±12% ⚠ | 6.96 [6.85–7.36] ±3% | 59.0 [57.9–60.6] ±2% | 0.70× | 0.12× |
| Q27 (cold), ms | 152 [122–155] ±9% | 53.7 [52.5–54.6] ±2% | 139 [137–146] ±2% | ≈ (1.09×) | 0.39× |
| Q27, ms | 70.8 [69.3–72.4] ±2% | 37.4 [36.6–39.5] ±3% | 76.7 [76.7–77.5] ±0% | ≈ (0.92×) | 0.49× |
| Q28 (cold), ms | 293 [290–306] ±2% | 228 [224–229] ±1% | 629 [626–637] ±1% | 0.47× | 0.36× |
| Q28, ms | 251 [245–252] ±1% | 214 [211–218] ±1% | 600 [597–609] ±1% | 0.42× | 0.36× |
| Q29 (cold), ms | 150 [121–154] ±10% | 42.9 [40.6–45.5] ±4% | 176 [174–178] ±1% | 0.86× | 0.24× |
| Q29, ms | 72.5 [71.7–73.6] ±1% | 40.0 [37.8–40.8] ±3% | 126 [125–129] ±1% | 0.58× | 0.32× |
| Q30 (cold), ms | 152 [147–152] ±1% | 20.6 [20.0–21.2] ±2% | 130 [127–132] ±2% | 1.17× | 0.16× |
| Q30, ms | 54.9 [54.1–55.6] ±1% | 15.4 [14.7–16.2] ±4% | 63.2 [62.7–64.0] ±1% | 0.87× | 0.24× |
| Q31 (cold), ms | 161 [123–165] ±12% ⚠ | 33.1 [32.7–33.6] ±1% | 148 [147–150] ±1% | ≈ (1.09×) | 0.22× |
| Q31, ms | 67.2 [63.0–68.1] ±3% | 25.8 [24.9–26.4] ±3% | 78.4 [76.8–78.7] ±1% | 0.86× | 0.33× |
| Q32 (cold), ms | 399 [381–410] ±3% | 289 [280–296] ±2% | 849 [844–862] ±1% | 0.47× | 0.34× |
| Q32, ms | 310 [307–318] ±2% | 278 [271–285] ±2% | 781 [756–819] ±4% | 0.40× | 0.36× |
| Q33 (cold), ms | 233 [197–240] ±7% | 120 [119–124] ±2% | 361 [349–363] ±2% | 0.64× | 0.33× |
| Q33, ms | 143 [143–144] ±0% | 105 [104–116] ±5% | 335 [328–340] ±1% | 0.43× | 0.31× |
| Q34 (cold), ms | 213 [206–239] ±6% | 132 [132–138] ±2% | 364 [352–436] ±9% | 0.58× | 0.36× |
| Q34, ms | 150 [148–155] ±2% | 116 [115–119] ±1% | 334 [332–337] ±1% | 0.45× | 0.35× |
| Q35 (cold), ms | 154 [127–159] ±9% | 30.3 [29.7–30.8] ±1% | 149 [145–150] ±1% | ≈ (1.03×) | 0.20× |
| Q35, ms | 60.8 [59.2–68.2] ±6% | 27.5 [27.1–27.7] ±1% | 87.9 [87.0–92.8] ±3% | 0.69× | 0.31× |
| Q36 (cold), ms | 165 [153–194] ±9% | 63.6 [62.5–66.3] ±2% | 209 [205–227] ±4% | 0.79× | 0.30× |
| Q36, ms | 99.6 [97.0–104] ±3% | 53.6 [51.5–53.8] ±2% | 138 [136–140] ±1% | 0.72× | 0.39× |
| Q37 (cold), ms | 120 [112–122] ±3% | 20.7 [20.5–21.8] ±2% | 147 [144–149] ±1% | 0.81× | 0.14× |
| Q37, ms | 57.8 [57.1–64.5] ±5% | 17.4 [17.0–18.1] ±2% | 75.4 [74.1–76.5] ±1% | 0.77× | 0.23× |
| Q38 (cold), ms | 113 [111–150] ±16% ⚠ | 17.0 [16.4–18.4] ±5% | 133 [132–141] ±3% | ≈ (0.85×) | 0.13× |
| Q38, ms | 57.2 [55.3–76.6] ±15% ⚠ | 9.73 [9.50–10.2] ±3% | 63.8 [62.9–64.9] ±1% | ≈ (0.90×) | 0.15× |
| Q39 (cold), ms | 301 [282–309] ±4% | 191 [183–194] ±2% | 401 [399–414] ±1% | 0.75× | 0.48× |
| Q39, ms | 206 [204–218] ±3% | 166 [164–170] ±2% | 342 [340–356] ±2% | 0.60× | 0.48× |
| Q40 (cold), ms | 150 [113–153] ±12% ⚠ | 15.8 [14.7–16.3] ±4% | 151 [150–152] ±0% | ≈ (0.99×) | 0.10× |
| Q40, ms | 57.8 [56.7–60.2] ±3% | 10.6 [10.5–10.7] ±1% | 87.1 [85.8–87.7] ±1% | 0.66× | 0.12× |
| Q41 (cold), ms | 148 [114–153] ±15% ⚠ | 13.0 [12.7–14.0] ±5% | 139 [138–144] ±2% | ≈ (1.06×) | 0.09× |
| Q41, ms | 60.4 [58.7–61.7] ±2% | 8.96 [8.84–9.49] ±3% | 83.4 [83.1–85.9] ±1% | 0.72× | 0.11× |
| Q42 (cold), ms | 136 [119–155] ±12% ⚠ | 23.5 [22.9–24.0] ±2% | 152 [151–153] ±1% | ≈ (0.90×) | 0.15× |
| Q42, ms | 64.1 [60.1–64.2] ±3% | 18.6 [18.2–19.6] ±3% | 84.9 [84.4–87.3] ±2% | 0.76× | 0.22× |
| load, s | 8.08 [7.90–8.19] ±1% | 7.10 [7.07–7.12] ±0% | 7.26 [6.97–7.50] ±3% | 1.11× | ≈ (0.98×) |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus (row) 0.1.0, nucleus (column) 0.1.0, postgresql 18.7 (Homebrew) — nucleus b7219825c+changes — 2026-11-18T01:50:01+00:00

Every result agrees, but for ties at a `LIMIT`.  A column table 0.16× of
PostgreSQL's time in the geometric mean of the warm runs (cold 0.14×),
faster on 42 of the 43 queries (Q23, `SELECT *` … `ORDER BY` a large
text, 1.7×); a row table 0.57× (cold 0.76×), faster on all 43 but Q0, a
`count(*)`, even (0.99×).  The loads 1.11× in rows, even in columns
(0.98×).

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
| search, ms | 41.4 [41.2–41.5] ±0% | 37.9 [37.6–38.3] ±1% | 212 [211–214] ±0% |
| load, s | — | 20.2 [19.8–20.3] ±1% | 34.1 [34.0–34.1] ±0% |
| answers differing from numpy's, count | — | 0.000 [0.000–0.000] ±0% | 0.000 [0.000–0.000] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — numpy 2.5.4, nucleus, pgvector 18.7 (Homebrew) — nucleus dc9aefb04 — 2026-11-17T13:34:07+00:00

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
| load, s | 15.9 [15.6–15.9] ±1% | 1.80 [1.72–1.82] ±3% |
| index build, s | 56.5 [55.6–56.6] ±1% | 73.1 [72.7–74.7] ±1% |
| index, resident, MB | 1,961 [1,879–1,974] ±3% | — |
| index, on disk, MB | 587 [587–587] ±0% | 702 [702–702] ±0% |
| ef 10: recall@10, recall | 0.654 [0.654–0.654] ±0% | 0.731 [0.730–0.733] ±0% |
| ef 10: µs a search, µs | 663 [648–663] ±1% | 374 [368–394] ±4% |
| ef 10: QPS, 10 clients, QPS | 13,895 [9,330–14,238] ±22% ⚠ | 11,252 [9,945–11,739] ±8% |
| ef 20: recall@10, recall | 0.785 [0.785–0.785] ±0% | — |
| ef 20: µs a search, µs | 275 [271–282] ±2% | — |
| ef 20: QPS, 10 clients, QPS | 16,824 [16,502–16,985] ±1% | — |
| ef 40: recall@10, recall | 0.891 [0.891–0.891] ±0% | 0.927 [0.927–0.927] ±0% |
| ef 40: µs a search, µs | 357 [357–373] ±2% | 782 [770–784] ±1% |
| ef 40: QPS, 10 clients, QPS | 10,480 [9,314–11,340] ±10% | 5,834 [5,683–6,646] ±9% |
| ef 60: recall@10, recall | 0.933 [0.933–0.933] ±0% | — |
| ef 60: µs a search, µs | 447 [434–504] ±8% | — |
| ef 60: QPS, 10 clients, QPS | 11,798 [11,649–12,151] ±2% | — |
| ef 80: recall@10, recall | 0.953 [0.953–0.953] ±0% | — |
| ef 80: µs a search, µs | 527 [518–533] ±1% | — |
| ef 80: QPS, 10 clients, QPS | 9,301 [9,066–10,767] ±9% | — |
| ef 120: recall@10, recall | 0.974 [0.974–0.974] ±0% | — |
| ef 120: µs a search, µs | 657 [657–696] ±3% | — |
| ef 120: QPS, 10 clients, QPS | 7,116 [6,122–8,256] ±15% ⚠ | — |
| ef 200: recall@10, recall | 0.989 [0.989–0.989] ±0% | 0.993 [0.993–0.993] ±0% |
| ef 200: µs a search, µs | 934 [899–943] ±3% | 2,644 [2,429–2,691] ±5% |
| ef 200: QPS, 10 clients, QPS | 6,131 [4,773–6,527] ±16% ⚠ | 2,371 [2,192–2,446] ±6% |
| ef 40: server µs a search, µs | 264 [261–271] ±2% | 1,000 [986–1,047] ±3% |
| inserts, rows/s, rows/s | 4,881 [4,828–5,046] ±2% | 2,470 [2,429–2,552] ±3% |
| ef 40 during inserts: µs a search, µs | 1,004 [883–1,043] ±9% | 2,167 [1,852–2,238] ±10% |
| ef 10 after inserts: recall@10, recall | 0.646 [0.643–0.647] ±0% | 0.726 [0.725–0.727] ±0% |
| ef 10 after inserts: µs a search, µs | 582 [428–599] ±18% ⚠ | 361 [353–365] ±2% |
| ef 10 after inserts: QPS, 10 clients, QPS | 11,655 [10,856–13,792] ±13% ⚠ | 10,311 [7,417–10,384] ±18% ⚠ |
| ef 20 after inserts: recall@10, recall | 0.780 [0.779–0.780] ±0% | — |
| ef 20 after inserts: µs a search, µs | 374 [324–400] ±10% ⚠ | — |
| ef 20 after inserts: QPS, 10 clients, QPS | 1,027 [915–15,411] ±144% ⚠ | — |
| ef 40 after inserts: recall@10, recall | 0.886 [0.885–0.887] ±0% | 0.925 [0.925–0.926] ±0% |
| ef 40 after inserts: µs a search, µs | 442 [407–450] ±5% | 796 [778–846] ±4% |
| ef 40 after inserts: QPS, 10 clients, QPS | 11,480 [11,383–14,058] ±12% ⚠ | 6,164 [5,666–6,191] ±5% |
| ef 60 after inserts: recall@10, recall | 0.931 [0.930–0.931] ±0% | — |
| ef 60 after inserts: µs a search, µs | 497 [477–511] ±3% | — |
| ef 60 after inserts: QPS, 10 clients, QPS | 10,874 [9,699–11,826] ±10% | — |
| ef 80 after inserts: recall@10, recall | 0.952 [0.952–0.952] ±0% | — |
| ef 80 after inserts: µs a search, µs | 557 [532–590] ±5% | — |
| ef 80 after inserts: QPS, 10 clients, QPS | 8,650 [7,691–9,853] ±12% ⚠ | — |
| ef 120 after inserts: recall@10, recall | 0.972 [0.972–0.972] ±0% | — |
| ef 120 after inserts: µs a search, µs | 694 [675–726] ±4% | — |
| ef 120 after inserts: QPS, 10 clients, QPS | 7,959 [7,245–8,262] ±7% | — |
| ef 200 after inserts: recall@10, recall | 0.988 [0.988–0.988] ±0% | 0.993 [0.992–0.993] ±0% |
| ef 200 after inserts: µs a search, µs | 948 [929–951] ±1% | 2,633 [2,580–2,694] ±2% |
| ef 200 after inserts: QPS, 10 clients, QPS | 5,291 [5,263–6,091] ±8% | 1,928 [1,759–2,239] ±12% ⚠ |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus, pgvector 18.7 (Homebrew) — nucleus 15d104378+changes — 2026-11-16T14:00:34+00:00

**Quantized graphs** (`bash bench/all.sh ann-sift-int8`; PLAN.md
§M14.12 V6a, `WITH (quantize = int8)`, the rows read again for the exact
distances), SIFT-1M as above:

| ann-sift-int8 | nucleus |
|---|---:|
| load, s | 16.0 [15.9–16.1] ±1% |
| index build, s | 30.4 [29.8–31.1] ±2% |
| index, resident, MB | 1,156 [1,149–1,161] ±1% |
| index, on disk, MB | 455 [455–455] ±0% |
| ef 10: recall@10, recall | 0.787 [0.787–0.787] ±0% |
| ef 10: µs a search, µs | 514 [403–535] ±15% ⚠ |
| ef 10: QPS, 10 clients, QPS | 14,266 [12,790–16,361] ±12% ⚠ |
| ef 20: recall@10, recall | 0.787 [0.787–0.787] ±0% |
| ef 20: µs a search, µs | 250 [249–254] ±1% |
| ef 20: QPS, 10 clients, QPS | 16,850 [16,325–17,924] ±5% |
| ef 40: recall@10, recall | 0.892 [0.892–0.892] ±0% |
| ef 40: µs a search, µs | 301 [292–309] ±3% |
| ef 40: QPS, 10 clients, QPS | 15,657 [14,941–16,020] ±4% |
| ef 60: recall@10, recall | 0.933 [0.933–0.933] ±0% |
| ef 60: µs a search, µs | 363 [337–371] ±5% |
| ef 60: QPS, 10 clients, QPS | 14,270 [14,131–14,524] ±1% |
| ef 80: recall@10, recall | 0.953 [0.953–0.953] ±0% |
| ef 80: µs a search, µs | 417 [386–431] ±6% |
| ef 80: QPS, 10 clients, QPS | 13,061 [13,051–13,210] ±1% |
| ef 120: recall@10, recall | 0.973 [0.973–0.973] ±0% |
| ef 120: µs a search, µs | 479 [459–506] ±5% |
| ef 120: QPS, 10 clients, QPS | 11,156 [10,592–11,304] ±3% |
| ef 200: recall@10, recall | 0.988 [0.988–0.988] ±0% |
| ef 200: µs a search, µs | 616 [596–617] ±2% |
| ef 200: QPS, 10 clients, QPS | 8,884 [7,438–9,152] ±11% ⚠ |
| ef 40: server µs a search, µs | 222 [221–226] ±1% |
| inserts, rows/s, rows/s | 6,309 [6,218–6,761] ±5% |
| ef 40 during inserts: µs a search, µs | 735 [722–752] ±2% |
| ef 10 after inserts: recall@10, recall | 0.782 [0.782–0.784] ±0% |
| ef 10 after inserts: µs a search, µs | 511 [450–521] ±8% |
| ef 10 after inserts: QPS, 10 clients, QPS | 12,737 [9,333–12,996] ±17% ⚠ |
| ef 20 after inserts: recall@10, recall | 0.782 [0.782–0.784] ±0% |
| ef 20 after inserts: µs a search, µs | 430 [373–432] ±8% |
| ef 20 after inserts: QPS, 10 clients, QPS | 12,346 [11,715–13,221] ±6% |
| ef 40 after inserts: recall@10, recall | 0.887 [0.887–0.888] ±0% |
| ef 40 after inserts: µs a search, µs | 450 [393–843] ±44% ⚠ |
| ef 40 after inserts: QPS, 10 clients, QPS | 16,135 [16,128–16,216] ±0% |
| ef 60 after inserts: recall@10, recall | 0.931 [0.930–0.931] ±0% |
| ef 60 after inserts: µs a search, µs | 341 [330–348] ±3% |
| ef 60 after inserts: QPS, 10 clients, QPS | 14,650 [13,916–14,671] ±3% |
| ef 80 after inserts: recall@10, recall | 0.952 [0.952–0.953] ±0% |
| ef 80 after inserts: µs a search, µs | 384 [373–391] ±2% |
| ef 80 after inserts: QPS, 10 clients, QPS | 13,298 [13,166–13,306] ±1% |
| ef 120 after inserts: recall@10, recall | 0.973 [0.972–0.973] ±0% |
| ef 120 after inserts: µs a search, µs | 462 [462–471] ±1% |
| ef 120 after inserts: QPS, 10 clients, QPS | 11,048 [10,718–11,198] ±2% |
| ef 200 after inserts: recall@10, recall | 0.988 [0.987–0.988] ±0% |
| ef 200 after inserts: µs a search, µs | 620 [616–638] ±2% |
| ef 200 after inserts: QPS, 10 clients, QPS | 8,593 [7,312–8,932] ±10% ⚠ |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus — nucleus 15d104378+changes — 2026-11-16T14:49:59+00:00

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
| index build, s | 61.1 [60.9–61.6] ±1% | 77.3 [76.1–77.6] ±1% | — | — | — |
| filter 50%: recall@10, recall | 0.924 [0.924–0.926] ±0% | — | 0.894 [0.891–0.895] ±0% | 0.894 [0.891–0.895] ±0% | 0.894 [0.891–0.895] ±0% |
| filter 50%: rows returned, rows | 10.0 [10.0–10.0] ±0% | — | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% |
| filter 50%: µs a search, µs | 568 [561–609] ±4% | — | 811 [806–858] ±3% | 828 [825–893] ±5% | 831 [810–834] ±2% |
| filter 10%: recall@10, recall | 0.982 [0.982–0.982] ±0% | — | 0.415 [0.414–0.416] ±0% | 0.932 [0.931–0.933] ±0% | 0.858 [0.857–0.863] ±0% |
| filter 10%: rows returned, rows | 10.0 [10.0–10.0] ±0% | — | 4.17 [4.16–4.17] ±0% | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% |
| filter 10%: µs a search, µs | 1,712 [1,694–1,727] ±1% | — | 843 [834–886] ±3% | 1,872 [1,849–1,920] ±2% | 1,958 [1,896–2,004] ±3% |
| filter 1%: recall@10, recall | 1.00 [1.00–1.00] ±0% | — | 0.045 [0.045–0.047] ±2% | 0.987 [0.987–0.988] ±0% | 0.944 [0.943–0.946] ±0% |
| filter 1%: rows returned, rows | 10.0 [10.0–10.0] ±0% | — | 0.454 [0.448–0.466] ±2% | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% |
| filter 1%: µs a search, µs | 1,395 [1,390–1,427] ±1% | — | 872 [828–879] ±3% | 10,397 [10,352–10,603] ±1% | 10,943 [10,751–11,138] ±2% |
| filter 0.1%: recall@10, recall | 1.00 [1.00–1.00] ±0% | — | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% |
| filter 0.1%: rows returned, rows | 10.0 [10.0–10.0] ±0% | — | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% |
| filter 0.1%: µs a search, µs | 310 [310–311] ±0% | — | 353 [353–368] ±2% | 354 [352–358] ±1% | 355 [351–356] ±1% |
| filter 0.01%: recall@10, recall | 1.00 [1.00–1.00] ±0% | — | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% |
| filter 0.01%: rows returned, rows | 10.0 [10.0–10.0] ±0% | — | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% | 10.0 [10.0–10.0] ±0% |
| filter 0.01%: µs a search, µs | 473 [468–502] ±4% | — | 215 [215–216] ±0% | 228 [216–230] ±3% | 217 [217–218] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — pgvector 18.7 (Homebrew) — nucleus dc9aefb04 — 2026-11-17T13:42:39+00:00

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
| load, s | 42.7 [42.6–42.9] ±0% | 7.30 [4.90–7.93] ±21% ⚠ | 5.23 [4.63–5.56] ±9% | 5.64 [5.43–5.96] ±5% |
| index build, s | 8.62 [8.44–8.69] ±2% | 59.1 [58.8–59.9] ±1% | 55.5 [54.7–58.3] ±3% | 3.36 [3.35–3.46] ±2% |
| index, resident, MB | 725 [718–782] ±5% | — | — | — |
| index, on disk, MB | 394 [394–394] ±0% | 773 [773–773] ±0% | 386 [386–386] ±0% | 47.0 [47.0–47.0] ±0% |
| ef 10: recall@10, recall | 0.852 [0.852–0.852] ±0% | 0.812 [0.809–0.818] ±0% | 0.809 [0.803–0.811] ±1% | 0.615 [0.614–0.617] ±0% |
| ef 10: µs a search, µs | 994 [976–1,009] ±2% | 1,128 [1,102–1,142] ±2% | 1,080 [1,071–1,083] ±1% | 286 [282–310] ±5% |
| ef 10: QPS, 10 clients, QPS | 6,645 [6,319–6,671] ±9% | 5,080 [4,787–5,294] ±4% | 5,619 [5,515–5,682] ±2% | 15,987 [15,449–16,048] ±2% |
| ef 20: recall@10, recall | 0.852 [0.852–0.852] ±0% | 0.893 [0.890–0.896] ±0% | 0.893 [0.892–0.893] ±0% | 0.820 [0.818–0.821] ±0% |
| ef 20: µs a search, µs | 913 [907–919] ±1% | 1,565 [1,544–1,611] ±2% | 1,495 [1,489–1,576] ±3% | 480 [446–486] ±5% |
| ef 20: QPS, 10 clients, QPS | 7,031 [6,654–7,091] ±8% | 3,888 [3,793–3,983] ±3% | 4,239 [4,182–4,252] ±1% | 10,658 [9,954–10,948] ±5% |
| ef 40: recall@10, recall | 0.929 [0.929–0.929] ±0% | 0.947 [0.945–0.949] ±0% | 0.947 [0.945–0.948] ±0% | 0.916 [0.913–0.917] ±0% |
| ef 40: µs a search, µs | 1,001 [994–1,013] ±1% | 2,421 [2,387–2,429] ±1% | 2,284 [2,274–2,413] ±3% | 803 [779–812] ±2% |
| ef 40: QPS, 10 clients, QPS | 6,402 [6,135–6,457] ±6% | 2,659 [2,608–2,698] ±2% | 2,847 [2,780–2,959] ±3% | 6,400 [6,290–6,474] ±1% |
| ef 80: recall@10, recall | 0.964 [0.964–0.964] ±0% | 0.974 [0.973–0.975] ±0% | 0.974 [0.973–0.974] ±0% | 0.952 [0.952–0.953] ±0% |
| ef 80: µs a search, µs | 1,176 [1,158–1,202] ±2% | 3,935 [3,865–3,944] ±1% | 3,708 [3,685–3,712] ±0% | 1,083 [1,068–1,093] ±1% |
| ef 80: QPS, 10 clients, QPS | 5,568 [5,450–5,588] ±1% | 1,696 [1,585–1,742] ±4% | 1,756 [1,728–1,911] ±5% | 4,813 [4,685–4,886] ±2% |
| ef 200: recall@10, recall | 0.989 [0.989–0.989] ±0% | 0.991 [0.991–0.991] ±0% | 0.991 [0.991–0.991] ±0% | 0.970 [0.968–0.970] ±0% |
| ef 200: µs a search, µs | 1,604 [1,586–1,627] ±1% | 7,949 [7,897–8,126] ±2% | 7,662 [7,623–7,676] ±0% | 1,640 [1,627–1,654] ±1% |
| ef 200: QPS, 10 clients, QPS | 3,989 [3,936–4,027] ±1% | 890 [816–896] ±5% | 965 [964–982] ±1% | 3,312 [3,238–3,383] ±2% |
| ef 40: server µs a search, µs | 530 [507–531] ±2% | 2,601 [2,547–2,677] ±2% | 2,377 [2,364–2,400] ±1% | 788 [786–820] ±2% |
| ef 60: server µs a search, µs | 571 [567–590] ±2% | 3,294 [3,245–3,346] ±2% | 3,022 [3,008–3,156] ±3% | 1,050 [1,044–1,210] ±9% |
| ef 80: server µs a search, µs | 650 [641–681] ±3% | 4,101 [3,986–4,184] ±2% | 3,851 [3,838–3,992] ±2% | 1,113 [1,079–1,151] ±3% |
| ef 120: server µs a search, µs | 793 [789–803] ±2% | — | — | — |
| ef 200: server µs a search, µs | 1,107 [1,084–1,129] ±2% | 8,279 [8,105–8,389] ±2% | 7,832 [7,624–7,912] ±2% | 1,612 [1,584–1,700] ±4% |
| ef 60: recall@10, recall | — | 0.965 [0.964–0.966] ±0% | 0.965 [0.964–0.966] ±0% | 0.943 [0.941–0.944] ±0% |
| ef 60: µs a search, µs | — | 3,164 [3,097–3,228] ±2% | 3,054 [3,006–3,063] ±1% | 976 [973–986] ±1% |
| ef 60: QPS, 10 clients, QPS | — | 2,046 [1,964–2,087] ±3% | 2,306 [2,191–2,378] ±4% | 5,044 [4,655–5,173] ±5% |
| ef 20: server µs a search, µs | — | 1,783 [1,724–1,812] ±3% | 1,636 [1,608–1,691] ±3% | 507 [476–511] ±4% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus, pgvector 18.7 (Homebrew) — nucleus 7ff7b48e3+changes — 2026-11-17T09:18:21+00:00

**By a stored row's vector** (`bash bench/all.sh ann-related`;
`bench/ann/related.py`, the same 99,000 rows, `ef_search` 40, both
clients sending an id): "more like this", `ORDER BY v <=> (SELECT v FROM t
WHERE id = $1) LIMIT 10` with the row left out, and each of 1,000 rows'
ten nearest, `FROM t a, LATERAL (SELECT … ORDER BY b.v <=> a.v LIMIT
10)` — nucleus runs the lateral side's searches on its workers, a row
each; PostgreSQL's nested loop runs its inner index scans on one process:

| ann-related | nucleus | nucleus, one thread | pgvector |
|---|---:|---:|---:|
| index build, s | 37.8 [37.5–39.2] ±2% | — | 59.4 [58.8–60.9] ±2% |
| more like (ef 40): recall@10, recall | 0.965 [0.965–0.965] ±0% | — | 0.977 [0.977–0.977] ±0% |
| more like (ef 40): µs a search, µs | 1,084 [1,071–1,095] ±1% | — | 2,379 [2,329–2,425] ±2% |
| more like (ef 40): server µs, µs | 870 [860–881] ±1% | — | 2,490 [2,437–2,502] ±1% |
| lateral, 1000 rows' nearest, ms | 151 [151–170] ±7% | 872 [853–883] ±2% | 2,342 [2,302–2,360] ±1% |
| lateral: recall@10, recall | 0.982 [0.982–0.982] ±0% | 0.982 [0.982–0.982] ±0% | 0.988 [0.987–0.988] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — pgvector 18.7 (Homebrew) — nucleus 7ff7b48e3+changes — 2026-11-17T09:51:59+00:00

**Sparse vectors searched exactly** (`bash bench/all.sh ann-sparse`;
PLAN.md §M14.12 V6b, `bench/ann/sparse.py`): 100,000 SPLADE-like rows — a
vocabulary of 30,522, ~120 terms a document and ~20 a query by a Zipf
law, log-normal weights — as `sparsevec(30522)`; 200 queries, `SELECT id
FROM t ORDER BY v <#> q LIMIT 10`, one client; both read every row on
several workers:

| ann-sparse | nucleus | nucleus, one thread | pgvector |
|---|---:|---:|---:|
| load, s | 6.09 [6.05–6.21] ±1% | — | 1.40 [1.34–1.45] ±3% |
| recall@10, recall | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% | 1.00 [1.00–1.00] ±0% |
| µs a search, µs | 20,832 [20,595–21,199] ±1% | 73,794 [72,903–76,190] ±2% | 20,946 [20,701–22,236] ±3% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — pgvector 18.7 (Homebrew) — nucleus c6a8dfd7c+changes — 2026-11-16T19:25:57+00:00

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
| sum | 211 [195–227] ±6% | 85.7 [85.3–85.9] ±0% | 1.97 [1.89–2.05] ±3% | — | 222 [194–234] ±8% |
| avg spread | 211 [191–234] ±7% | 102 [101–104] ±1% | — | — | 210 [183–234] ±10% |
| sum 1000 groups | 207 [180–219] ±8% | 172 [171–178] ±2% | — | — | 205 [193–208] ±3% |
| avg 1M groups | 1,716 [1,703–1,884] ±4% | 1,734 [1,714–1,769] ±1% | — | — | 1,984 [1,891–2,062] ±4% |
| variance | 200 [189–219] ±6% | 93.4 [92.9–94.9] ±1% | 7.75 [7.48–8.22] ±4% | — | 190 [184–212] ±5% |
| stddev 1000 groups | 225 [212–228] ±3% | 183 [180–190] ±2% | — | — | 219 [205–233] ±5% |
| variance of integers | 203 [186–220] ±7% | 148 [145–149] ±1% | — | — | 181 [166–200] ±7% |
| skewness, kurtosis | 298 [292–310] ±2% | — | — | — | 286 [277–307] ±4% |
| corr | 242 [218–252] ±5% | 102 [101–104] ±1% | 34.7 [34.5–35.3] ±1% | — | 234 [215–237] ±5% |
| regression 1000 groups | 287 [283–295] ±2% | 199 [196–200] ±1% | — | — | 281 [264–295] ±4% |
| row_number by groups | 2,683 [2,640–2,707] ±1% | 4,634 [4,604–4,676] ±1% | — | — | 2,741 [2,686–2,758] ±1% |
| top 3 by groups | 2,116 [2,104–2,153] ±1% | 3,784 [3,698–3,806] ±1% | — | — | 1,857 [1,830–1,888] ±1% |
| running sum | 3,827 [3,726–3,953] ±2% | 2,300 [2,292–2,309] ±0% | — | — | 3,684 [3,647–3,704] ±1% |
| moving avg 10 rows | 5,301 [5,242–5,325] ±1% | 8,582 [8,577–8,593] ±0% | — | — | 5,315 [5,270–5,371] ±1% |
| lag | 2,815 [2,810–2,907] ±1% | 2,135 [2,126–2,144] ±0% | — | — | 2,835 [2,781–2,853] ±1% |
| median | 700 [661–784] ±7% | 1,968 [1,964–2,001] ±1% | 25.3 [25.3–25.4] ±0% | — | 257 [242–300] ±8% |
| p50, p95, p99 | 689 [654–727] ±4% | 2,216 [2,183–2,275] ±2% | — | — | 256 [247–301] ±8% |
| percentile_disc | 624 [602–662] ±4% | 2,071 [2,054–2,108] ±1% | 43.5 [43.4–44.0] ±1% | — | 251 [246–268] ±4% |
| median 1000 groups | 667 [642–702] ±4% | 512 [502–520] ±1% | — | — | 276 [271–305] ±5% |
| mode | 760 [724–798] ±4% | 1,109 [1,096–1,124] ±1% | — | — | 310 [288–315] ±4% |
| histogram | 176 [167–177] ±2% | — | — | — | 185 [164–193] ±7% |
| count distinct | 899 [871–939] ±3% | 401 [396–402] ±1% | — | — | 911 [858–948] ±4% |
| approx_count_distinct | 199 [188–210] ±5% | — | — | — | 199 [176–210] ±7% |
| approx_count_distinct 1000 groups | 361 [347–369] ±2% | — | — | — | 363 [351–371] ±2% |
| count distinct 1000 groups | 1,579 [1,558–1,639] ±2% | 821 [811–902] ±5% | — | — | 1,613 [1,555–1,657] ±2% |
| approx_percentile | 182 [174–194] ±4% | — | — | — | 189 [171–192] ±5% |
| approx_top_k | 255 [239–255] ±3% | — | — | — | 250 [244–281] ±6% |
| tablesample | 186 [180–209] ±6% | 191 [189–200] ±3% | — | — | 198 [181–211] ±6% |
| tablesample repeatable | 223 [217–271] ±10% | 194 [193–195] ±0% | — | — | 229 [216–247] ±5% |
| hash_unit split | 369 [358–375] ±2% | — | — | — | 380 [368–420] ±5% |
| reservoir_sample | 180 [166–193] ±6% | — | — | — | 181 [167–196] ±6% |
| order by random() limit | 235 [225–243] ±3% | 599 [596–599] ±0% | — | — | 225 [221–238] ±3% |
| ks_test | 813 [782–829] ±2% | — | — | 393 [390–399] ±1% | 335 [323–340] ±2% |
| auc | 880 [854–906] ±2% | — | — | 1,293 [1,285–1,305] ±1% | 424 [417–437] ±2% |
| psi | 797 [787–823] ±2% | — | — | — | 336 [322–341] ±2% |
| chisq_test | 261 [254–268] ±2% | — | — | — | 259 [255–276] ±3% |
| js_divergence | 261 [252–277] ±3% | — | — | — | 267 [254–271] ±3% |
| summarize | 523 [499–526] ±2% | — | — | — | 531 [517–553] ±3% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew), python scipy 1.18.1, scikit-learn 1.9.1 — nucleus c6a8dfd7c+changes — 2026-11-16T19:35:45+00:00

What the times do not show: nucleus gives the same bits every run, the
exact value rounded once — the variance of `x`, `corr(x, id)` and the
running sums are exact where PostgreSQL's double accumulators differ from
run to run past the ninth digit and lose the fractions past 10¹⁶; the
quantiles are PostgreSQL's and numpy's bit for bit; the sketches' bytes
are the same in every executor and run; a `REPEATABLE` sample is the
same rows on every machine and in row and column storage (PostgreSQL's
depends on the pages).  Ordered by `id` a window sorts nothing: a
table's scan gives the primary key's order.  Against the native, tuned
PostgreSQL the plain aggregates are behind (`sum` 2.5× its time): its
parallel workers read 8 GB of buffers; PLAN.md §M14.27's notes keep the
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
| analyzer simple | 180 [173–181] ±2% | 1,055 [1,031–1,071] ±2% | 927 [926–930] ±0% | 2,700 [2,693–2,721] ±1% |
| analyzer porter | 317 [304–323] ±2% | 2,009 [1,986–2,031] ±1% | — | — |
| analyzer english | 313 [306–323] ±2% | 2,069 [2,027–2,083] ±1% | 1,138 [1,136–1,141] ±0% | 3,329 [3,321–3,353] ±1% |
| analyzer hungarian | 308 [300–324] ±3% | 2,018 [1,972–2,025] ±1% | 1,332 [1,331–1,339] ±1% | 3,902 [3,897–3,920] ±0% |
| analyzer trigram | 454 [446–464] ±2% | 2,681 [2,616–2,712] ±2% | — | — |
| @@ market, no index | 125 [122–137] ±5% | 763 [754–766] ±1% | 928 [927–929] ±0% | 2,706 [2,702–2,724] ±0% |
| @@ stock market, no index | 124 [123–128] ±2% | 776 [763–781] ±1% | 929 [928–932] ±0% | 2,708 [2,702–2,736] ±1% |
| @@ stock AND market, no index | 124 [120–129] ±3% | 771 [762–780] ±1% | 925 [925–928] ±0% | 2,710 [2,701–2,728] ±1% |
| @@ "stock market", no index | 125 [123–127] ±1% | 771 [767–779] ±1% | 927 [927–928] ±0% | 2,707 [2,702–2,720] ±0% |
| @@ invest*, no index | 125 [124–131] ±2% | 765 [760–781] ±1% | 929 [928–932] ±0% | 2,706 [2,702–2,721] ±0% |
| @@ markte~1, no index | 145 [137–147] ±3% | 875 [867–884] ±1% | — | — |
| CREATE INDEX | 665 [652–672] ±1% | — | 1,658 [1,649–1,671] ±1% | — |
| @@ market, indexed | 0.634 [0.626–0.716] ±6% | — | 3.44 [3.38–3.48] ±3% | — |
| @@ stock market, indexed | 0.866 [0.860–0.977] ±6% | — | 3.33 [3.26–3.76] ±8% | — |
| @@ stock AND market, indexed | 0.351 [0.345–0.378] ±4% | — | 0.928 [0.916–0.939] ±7% | — |
| @@ "stock market", indexed | 0.702 [0.687–0.759] ±4% | — | 232 [231–234] ±1% | — |
| @@ invest*, indexed | 1.10 [1.07–1.21] ±5% | — | 4.18 [4.13–4.31] ±3% | — |
| @@ markte~1, indexed | 4.07 [4.05–4.46] ±4% | — | — | — |
| @@ market, rows read, indexed | 8.63 [8.19–9.49] ±6% | — | 10.4 [10.3–10.5] ±1% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 7e1f26937+changes — 2026-11-16T21:53:51+00:00

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
| scifact: CREATE INDEX, ms | 389 [387–393] ±1% | 297 [282–335] ±7% |
| scifact: mean, ms | 0.174 [0.174–0.175] ±4% | 0.837 [0.813–0.847] ±2% |
| scifact: p50, ms | 0.170 [0.170–0.171] ±2% | 0.806 [0.784–0.818] ±2% |
| nfcorpus: nDCG@10, nDCG@10 | 0.329 [0.329–0.329] ±0% | 0.322 [0.322–0.322] ±0% |
| nfcorpus: CREATE INDEX, ms | 342 [341–345] ±1% | 210 [208–217] ±2% |
| nfcorpus: mean, ms | 0.124 [0.123–0.126] ±12% ⚠ | 0.432 [0.422–0.447] ±2% |
| nfcorpus: p50, ms | 0.128 [0.127–0.130] ±6% | 0.409 [0.399–0.428] ±3% |
| arguana: nDCG@10, nDCG@10 | 0.496 [0.496–0.496] ±0% | 0.481 [0.481–0.481] ±0% |
| arguana: CREATE INDEX, ms | 307 [305–312] ±2% | 340 [333–376] ±5% |
| arguana: mean, ms | 2.21 [2.20–2.21] ±1% | 5.41 [5.40–5.43] ±0% |
| arguana: p50, ms | 0.809 [0.806–0.813] ±1% | 4.92 [4.91–4.94] ±0% |
| fiqa: nDCG@10, nDCG@10 | 0.253 [0.253–0.253] ±0% | 0.250 [0.250–0.250] ±0% |
| fiqa: CREATE INDEX, ms | 662 [655–681] ±4% | 674 [655–699] ±3% |
| fiqa: mean, ms | 0.368 [0.356–0.371] ±2% | 1.61 [1.59–1.65] ±1% |
| fiqa: p50, ms | 0.340 [0.330–0.342] ±3% | 1.59 [1.57–1.61] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — paradedb pg_search 0.26.6 — nucleus 7e1f26937+changes — 2026-11-16T22:15:04+00:00

With a second condition (`bash bench/all.sh bm25-filters`; PLAN.md
§M14.21 T5a, T5b: `AND n % 100 < p`, `n` a row's number): the index
reads its matches best first until 10 pass, against the same order by
`score(…) + 0`, which the index does not rank by (every match read — by a
merge with the table's scan — and scored on its text) — the same rows:

| bm25-filters | nucleus | nucleus, ranked by the index | nucleus, every match scored |
|---|---:|---:|---:|
| scifact: nDCG@10, nDCG@10 | 0.689 [0.689–0.689] ±0% | — | — |
| scifact: CREATE INDEX, ms | 393 [393–415] ±3% | — | — |
| scifact: mean, ms | 0.175 [0.172–0.241] ±20% ⚠ | — | — |
| scifact: p50, ms | 0.170 [0.167–0.240] ±22% ⚠ | — | — |
| scifact: filter 50%, mean, ms | — | 0.219 [0.211–0.227] ±3% | 83.9 [83.7–85.6] ±1% |
| scifact: filter 50%, p50, ms | — | 0.209 [0.204–0.215] ±3% | 88.1 [87.7–89.6] ±1% |
| scifact: filter 10%, mean, ms | — | 0.473 [0.464–0.479] ±2% | 18.4 [18.3–18.9] ±2% |
| scifact: filter 10%, p50, ms | — | 0.454 [0.450–0.454] ±0% | 19.1 [18.9–19.4] ±1% |
| scifact: filter 1%, mean, ms | — | 2.34 [2.29–2.35] ±1% | 4.37 [4.37–4.44] ±1% |
| scifact: filter 1%, p50, ms | — | 2.29 [2.27–2.30] ±1% | 4.48 [4.46–4.60] ±2% |
| nfcorpus: nDCG@10, nDCG@10 | 0.329 [0.329–0.329] ±0% | — | — |
| nfcorpus: CREATE INDEX, ms | 349 [348–359] ±2% | — | — |
| nfcorpus: mean, ms | 0.136 [0.127–0.137] ±4% | — | — |
| nfcorpus: p50, ms | 0.135 [0.130–0.136] ±3% | — | — |
| nfcorpus: filter 50%, mean, ms | — | 0.153 [0.145–0.157] ±4% | 17.6 [17.6–17.9] ±1% |
| nfcorpus: filter 50%, p50, ms | — | 0.155 [0.149–0.159] ±3% | 7.44 [7.38–7.45] ±1% |
| nfcorpus: filter 10%, mean, ms | — | 0.293 [0.291–0.305] ±2% | 4.63 [4.61–4.73] ±1% |
| nfcorpus: filter 10%, p50, ms | — | 0.314 [0.311–0.323] ±2% | 2.21 [2.21–2.27] ±2% |
| nfcorpus: filter 1%, mean, ms | — | 0.933 [0.929–0.966] ±2% | 1.27 [1.27–1.32] ±2% |
| nfcorpus: filter 1%, p50, ms | — | 0.591 [0.583–0.622] ±3% | 0.723 [0.703–0.768] ±5% |
| arguana: nDCG@10, nDCG@10 | 0.496 [0.496–0.496] ±0% | — | — |
| arguana: CREATE INDEX, ms | 311 [308–321] ±2% | — | — |
| arguana: mean, ms | 2.26 [2.21–2.26] ±1% | — | — |
| arguana: p50, ms | 0.813 [0.810–0.826] ±1% | — | — |
| arguana: filter 50%, mean, ms | — | 2.27 [2.26–2.33] ±2% | 225 [222–226] ±1% |
| arguana: filter 50%, p50, ms | — | 0.845 [0.843–0.868] ±2% | 224 [221–225] ±1% |
| arguana: filter 10%, mean, ms | — | 2.47 [2.46–2.54] ±2% | 52.1 [51.6–52.3] ±1% |
| arguana: filter 10%, p50, ms | — | 1.07 [1.06–1.09] ±2% | 51.7 [51.2–51.9] ±1% |
| arguana: filter 1%, mean, ms | — | 4.51 [4.50–4.58] ±1% | 10.3 [10.3–10.4] ±1% |
| arguana: filter 1%, p50, ms | — | 3.18 [3.16–3.20] ±1% | 10.3 [10.2–10.3] ±1% |
| fiqa: nDCG@10, nDCG@10 | 0.253 [0.253–0.253] ±0% | — | — |
| fiqa: CREATE INDEX, ms | 703 [700–714] ±1% | — | — |
| fiqa: mean, ms | 0.358 [0.354–0.381] ±4% | — | — |
| fiqa: p50, ms | 0.331 [0.329–0.336] ±1% | — | — |
| fiqa: filter 50%, mean, ms | — | 0.488 [0.481–0.495] ±1% | 425 [421–427] ±1% |
| fiqa: filter 50%, p50, ms | — | 0.457 [0.446–0.458] ±1% | 423 [420–423] ±0% |
| fiqa: filter 10%, mean, ms | — | 0.731 [0.723–0.759] ±3% | 94.2 [94.2–94.4] ±0% |
| fiqa: filter 10%, p50, ms | — | 0.657 [0.655–0.674] ±2% | 93.4 [93.3–94.2] ±1% |
| fiqa: filter 1%, mean, ms | — | 3.19 [3.18–3.20] ±0% | 18.8 [18.7–18.9] ±0% |
| fiqa: filter 1%, p50, ms | — | 3.15 [3.12–3.18] ±1% | 18.3 [18.1–18.3] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T10:10:22+00:00

Ranked by the index, a query costs about what it does unfiltered while
half or a tenth of the rows pass; at one in a hundred the index reads
further down its list before ten pass, and is still the faster: 2–6×
where queries match many documents, 1.4× on NFCorpus, whose queries
match few.

With each row's snippet (`bash bench/all.sh bm25-snippets`; PLAN.md
§M14.21 T6: `SELECT id, snippet(body @@ q) … LIMIT 11`, 35 words;
ParadeDB `pdb.snippet(body)`, Tantivy's 150 characters):

| bm25-snippets | nucleus | nucleus, snippets | paradedb, snippets (paradedb) |
|---|---:|---:|---:|
| scifact: nDCG@10, nDCG@10 | 0.689 [0.689–0.689] ±0% | — | 0.684 [0.684–0.684] ±0% |
| scifact: CREATE INDEX, ms | 394 [392–398] ±1% | — | 289 [287–317] ±4% |
| scifact: mean, ms | 0.174 [0.174–0.180] ±2% | 1.13 [1.11–1.16] ±2% | 1.48 [1.45–1.50] ±1% |
| scifact: p50, ms | 0.170 [0.169–0.176] ±2% | 1.09 [1.08–1.12] ±2% | 1.42 [1.40–1.45] ±1% |
| nfcorpus: nDCG@10, nDCG@10 | 0.329 [0.329–0.329] ±0% | — | 0.322 [0.322–0.322] ±0% |
| nfcorpus: CREATE INDEX, ms | 342 [341–351] ±1% | — | 224 [209–235] ±5% |
| nfcorpus: mean, ms | 0.124 [0.122–0.130] ±2% | 0.912 [0.897–0.941] ±2% | 0.936 [0.899–0.962] ±3% |
| nfcorpus: p50, ms | 0.128 [0.126–0.133] ±2% | 1.02 [1.01–1.04] ±1% | 0.982 [0.959–0.999] ±2% |
| arguana: nDCG@10, nDCG@10 | 0.496 [0.496–0.496] ±0% | — | 0.481 [0.481–0.481] ±0% |
| arguana: CREATE INDEX, ms | 310 [305–320] ±2% | — | 341 [335–372] ±4% |
| arguana: mean, ms | 2.22 [2.20–2.27] ±1% | 3.11 [3.10–3.17] ±1% | 7.14 [7.10–7.29] ±1% |
| arguana: p50, ms | 0.809 [0.802–0.829] ±1% | 1.70 [1.69–1.74] ±1% | 6.48 [6.43–6.60] ±1% |
| fiqa: nDCG@10, nDCG@10 | 0.253 [0.253–0.253] ±0% | — | 0.250 [0.250–0.250] ±0% |
| fiqa: CREATE INDEX, ms | 663 [662–695] ±2% | — | 650 [648–684] ±3% |
| fiqa: mean, ms | 0.358 [0.344–0.369] ±3% | 0.976 [0.975–0.983] ±0% | 2.18 [2.16–2.21] ±1% |
| fiqa: p50, ms | 0.335 [0.320–0.346] ±3% | 0.940 [0.937–0.945] ±0% | 2.13 [2.12–2.16] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — paradedb pg_search 0.26.6 — nucleus 7e1f26937+changes — 2026-11-17T00:51:38+00:00

**MS MARCO passage** (`bash bench/all.sh msmarco`; PLAN.md §M14.21 T5b,
T5c): 8,841,823 passages, BEIR's dev split, its first 1,000 queries,
`english`, k1 1.2, b 0.75 — the same query, the same nDCG@10 measure;
the server's index cache a quarter of the machine's memory by default;
with 50, 10 and 1 % filters (a 1 % filter's time is its rows: about
1,100 found rows read a lookup each before 11 pass):

| msmarco | nucleus | nucleus, ranked by the index | paradedb |
|---|---:|---:|---:|
| msmarco: nDCG@10, nDCG@10 | 0.260 [0.260–0.260] ±0% | — | 0.258 [0.257–0.258] ±0% |
| msmarco: CREATE INDEX, ms | 48,281 [46,221–51,603] ±7% | — | 70,958 [70,945–71,119] ±0% |
| msmarco: mean, ms | 1.94 [1.92–2.03] ±4% | — | 6.40 [6.29–6.50] ±2% |
| msmarco: p50, ms | 1.39 [1.37–1.42] ±2% | — | 6.81 [6.73–6.94] ±2% |
| msmarco: filter 50%, mean, ms | — | 3.45 [3.40–3.51] ±3% | — |
| msmarco: filter 50%, p50, ms | — | 2.78 [2.72–2.92] ±5% | — |
| msmarco: filter 10%, mean, ms | — | 7.51 [7.25–8.47] ±8% | — |
| msmarco: filter 10%, p50, ms | — | 6.54 [6.33–7.39] ±9% | — |
| msmarco: filter 1%, mean, ms | — | 30.5 [26.7–36.9] ±18% ⚠ | — |
| msmarco: filter 1%, p50, ms | — | 29.9 [26.0–36.3] ±19% ⚠ | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — paradedb pg_search 0.26.6 — nucleus 4e15edf5c — 2026-11-17T11:43:55+00:00

ParadeDB here with `shared_buffers` 1 GB, `maintenance_work_mem` 512 MB
and two build workers: with its image's settings (2.4 GB, five workers
of 620 MB) the index build outgrew Docker's 4 GB VM and a backend was
killed, twice.  The 1 % filter's time spreads between runs (26–37 ms
here, 18 in a run of the same binary three hours before): its ~1,100
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
| scifact text: mean, ms | 0.257 [0.255–0.276] ±3% | 0.841 [0.822–0.874] ±2% |
| scifact text: p50, ms | 0.253 [0.250–0.270] ±3% | 0.805 [0.791–0.828] ±2% |
| scifact vector: nDCG@10, nDCG@10 | 0.713 [0.713–0.713] ±0% | 0.716 [0.716–0.717] ±0% |
| scifact vector: mean, ms | 0.377 [0.367–0.411] ±5% | 0.410 [0.408–0.441] ±3% |
| scifact vector: p50, ms | 0.374 [0.367–0.400] ±4% | 0.410 [0.404–0.436] ±3% |
| scifact rrf: nDCG@10, nDCG@10 | 0.733 [0.733–0.733] ±0% | 0.733 [0.733–0.733] ±0% |
| scifact rrf: mean, ms | 1.55 [1.49–1.58] ±2% | 7.12 [7.09–7.19] ±1% |
| scifact rrf: p50, ms | 1.49 [1.48–1.54] ±2% | 7.03 [6.99–7.10] ±1% |
| scifact convex: nDCG@10, nDCG@10 | 0.744 [0.744–0.744] ±0% | — |
| scifact convex: mean, ms | 1.69 [1.67–1.72] ±1% | — |
| scifact convex: p50, ms | 1.68 [1.67–1.70] ±1% | — |
| nfcorpus text: nDCG@10, nDCG@10 | 0.329 [0.329–0.329] ±0% | 0.322 [0.322–0.322] ±0% |
| nfcorpus text: mean, ms | 0.168 [0.165–0.176] ±3% | 0.441 [0.428–0.468] ±3% |
| nfcorpus text: p50, ms | 0.170 [0.168–0.178] ±2% | 0.413 [0.404–0.444] ±4% |
| nfcorpus vector: nDCG@10, nDCG@10 | 0.316 [0.316–0.316] ±0% | 0.330 [0.330–0.334] ±1% |
| nfcorpus vector: mean, ms | 0.307 [0.301–0.324] ±3% | 0.390 [0.387–0.433] ±5% |
| nfcorpus vector: p50, ms | 0.306 [0.301–0.320] ±2% | 0.393 [0.383–0.424] ±4% |
| nfcorpus rrf: nDCG@10, nDCG@10 | 0.360 [0.360–0.360] ±0% | 0.364 [0.364–0.364] ±0% |
| nfcorpus rrf: mean, ms | 1.31 [1.28–1.34] ±2% | 4.95 [4.85–4.98] ±1% |
| nfcorpus rrf: p50, ms | 1.37 [1.35–1.39] ±1% | 4.87 [4.78–4.89] ±1% |
| nfcorpus convex: nDCG@10, nDCG@10 | 0.363 [0.363–0.363] ±0% | — |
| nfcorpus convex: mean, ms | 1.46 [1.45–1.53] ±2% | — |
| nfcorpus convex: p50, ms | 1.56 [1.55–1.59] ±1% | — |
| arguana text: nDCG@10, nDCG@10 | 0.496 [0.496–0.496] ±0% | 0.481 [0.481–0.481] ±0% |
| arguana text: mean, ms | 2.26 [2.26–2.29] ±1% | 5.57 [5.55–5.61] ±0% |
| arguana text: p50, ms | 0.866 [0.861–0.876] ±1% | 5.08 [5.08–5.10] ±0% |
| arguana vector: nDCG@10, nDCG@10 | 0.597 [0.597–0.597] ±0% | 0.595 [0.595–0.595] ±0% |
| arguana vector: mean, ms | 0.360 [0.356–0.368] ±1% | 0.366 [0.358–0.416] ±6% |
| arguana vector: p50, ms | 0.358 [0.355–0.365] ±1% | 0.363 [0.356–0.401] ±5% |
| arguana rrf: nDCG@10, nDCG@10 | 0.598 [0.598–0.598] ±0% | 0.589 [0.589–0.589] ±0% |
| arguana rrf: mean, ms | 3.55 [3.52–3.68] ±2% | 6.53 [6.46–6.58] ±1% |
| arguana rrf: p50, ms | 2.17 [2.15–2.25] ±2% | 6.03 [5.98–6.10] ±1% |
| arguana convex: nDCG@10, nDCG@10 | 0.617 [0.617–0.617] ±0% | — |
| arguana convex: mean, ms | 3.77 [3.74–3.85] ±1% | — |
| arguana convex: p50, ms | 2.37 [2.36–2.44] ±1% | — |
| fiqa text: nDCG@10, nDCG@10 | 0.253 [0.253–0.253] ±0% | 0.250 [0.250–0.250] ±0% |
| fiqa text: mean, ms | 0.405 [0.386–0.443] ±5% | 1.69 [1.69–1.71] ±1% |
| fiqa text: p50, ms | 0.374 [0.356–0.380] ±3% | 1.65 [1.64–1.67] ±1% |
| fiqa vector: nDCG@10, nDCG@10 | 0.367 [0.367–0.367] ±0% | 0.374 [0.373–0.374] ±0% |
| fiqa vector: mean, ms | 0.459 [0.449–0.527] ±7% | 0.658 [0.643–0.667] ±1% |
| fiqa vector: p50, ms | 0.453 [0.442–0.469] ±2% | 0.649 [0.637–0.654] ±1% |
| fiqa rrf: nDCG@10, nDCG@10 | 0.357 [0.357–0.357] ±0% | 0.360 [0.360–0.360] ±0% |
| fiqa rrf: mean, ms | 2.11 [2.09–2.20] ±2% | 3.50 [3.47–3.52] ±1% |
| fiqa rrf: p50, ms | 2.08 [2.07–2.10] ±1% | 3.47 [3.44–3.49] ±1% |
| fiqa convex: nDCG@10, nDCG@10 | 0.383 [0.383–0.383] ±0% | — |
| fiqa convex: mean, ms | 2.31 [2.29–2.33] ±1% | — |
| fiqa convex: p50, ms | 2.28 [2.27–2.31] ±1% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — paradedb pg_search 0.26.6, pgvector 0.8.6 — nucleus 7ff7b48e3+changes — 2026-11-17T01:01:46+00:00

With learned sparse vectors (`bash bench/all.sh hybrid-sparse`; PLAN.md
§M14.21 T7b): SPLADE++ (`prithivida/Splade_PP_en_v1`) in a
`sparsevec(30522)` column, nucleus's `USING sparse` index (exact) against
pgvector's HNSW on `sparsevec_ip_ops` (`ef_search` 40); `ORDER BY sv <#>
$1, id LIMIT 11`, and nucleus's `rrf` of all three rankings:

| hybrid-sparse | nucleus | paradedb |
|---|---:|---:|
| scifact: sparse index, ms | 141 [136–144] ±2% | — |
| scifact text: nDCG@10, nDCG@10 | 0.689 [0.689–0.689] ±0% | 0.684 [0.684–0.684] ±0% |
| scifact text: mean, ms | 0.270 [0.265–0.272] ±1% | 0.830 [0.828–0.837] ±0% |
| scifact text: p50, ms | 0.266 [0.261–0.269] ±1% | 0.803 [0.796–0.808] ±1% |
| scifact vector: nDCG@10, nDCG@10 | 0.713 [0.713–0.713] ±0% | 0.719 [0.719–0.719] ±0% |
| scifact vector: mean, ms | 0.367 [0.366–0.374] ±1% | 0.431 [0.425–0.439] ±1% |
| scifact vector: p50, ms | 0.367 [0.366–0.373] ±1% | 0.427 [0.425–0.439] ±1% |
| scifact rrf: nDCG@10, nDCG@10 | 0.733 [0.733–0.733] ±0% | 0.733 [0.733–0.733] ±0% |
| scifact rrf: mean, ms | 1.50 [1.49–1.57] ±2% | 7.97 [7.71–8.28] ±3% |
| scifact rrf: p50, ms | 1.48 [1.48–1.53] ±1% | 7.83 [7.55–8.14] ±3% |
| scifact convex: nDCG@10, nDCG@10 | 0.744 [0.744–0.744] ±0% | — |
| scifact convex: mean, ms | 1.69 [1.68–1.87] ±5% | — |
| scifact convex: p50, ms | 1.69 [1.68–1.73] ±1% | — |
| scifact splade: nDCG@10, nDCG@10 | 0.709 [0.709–0.709] ±0% | 0.699 [0.699–0.701] ±0% |
| scifact splade: mean, ms | 1.11 [1.09–1.12] ±1% | 0.823 [0.808–0.845] ±2% |
| scifact splade: p50, ms | 1.07 [1.05–1.10] ±2% | 0.814 [0.797–0.837] ±2% |
| scifact rrf3: nDCG@10, nDCG@10 | 0.737 [0.737–0.737] ±0% | — |
| scifact rrf3: mean, ms | 3.02 [3.00–3.20] ±3% | — |
| scifact rrf3: p50, ms | 2.99 [2.93–3.05] ±1% | — |
| nfcorpus: sparse index, ms | 95.5 [94.6–96.3] ±1% | — |
| nfcorpus text: nDCG@10, nDCG@10 | 0.329 [0.329–0.329] ±0% | 0.322 [0.322–0.322] ±0% |
| nfcorpus text: mean, ms | 0.179 [0.175–0.181] ±1% | 0.427 [0.423–0.430] ±1% |
| nfcorpus text: p50, ms | 0.183 [0.180–0.186] ±1% | 0.400 [0.398–0.407] ±1% |
| nfcorpus vector: nDCG@10, nDCG@10 | 0.316 [0.316–0.316] ±0% | 0.330 [0.330–0.334] ±1% |
| nfcorpus vector: mean, ms | 0.315 [0.303–0.337] ±4% | 0.394 [0.390–0.412] ±2% |
| nfcorpus vector: p50, ms | 0.309 [0.301–0.322] ±3% | 0.393 [0.387–0.407] ±2% |
| nfcorpus rrf: nDCG@10, nDCG@10 | 0.360 [0.360–0.360] ±0% | 0.364 [0.364–0.364] ±0% |
| nfcorpus rrf: mean, ms | 1.30 [1.28–1.41] ±4% | 5.48 [5.32–5.60] ±2% |
| nfcorpus rrf: p50, ms | 1.37 [1.34–1.40] ±1% | 5.43 [5.23–5.58] ±2% |
| nfcorpus convex: nDCG@10, nDCG@10 | 0.363 [0.363–0.363] ±0% | — |
| nfcorpus convex: mean, ms | 1.45 [1.44–1.47] ±1% | — |
| nfcorpus convex: p50, ms | 1.55 [1.54–1.56] ±1% | — |
| nfcorpus splade: nDCG@10, nDCG@10 | 0.343 [0.343–0.343] ±0% | 0.322 [0.321–0.322] ±0% |
| nfcorpus splade: mean, ms | 0.371 [0.370–0.374] ±0% | 0.551 [0.545–0.555] ±1% |
| nfcorpus splade: p50, ms | 0.333 [0.328–0.335] ±1% | 0.542 [0.530–0.549] ±1% |
| nfcorpus rrf3: nDCG@10, nDCG@10 | 0.359 [0.359–0.359] ±0% | — |
| nfcorpus rrf3: mean, ms | 1.99 [1.98–2.04] ±1% | — |
| nfcorpus rrf3: p50, ms | 2.01 [1.98–2.05] ±1% | — |
| arguana: sparse index, ms | 237 [235–241] ±1% | — |
| arguana text: nDCG@10, nDCG@10 | 0.496 [0.496–0.496] ±0% | 0.481 [0.481–0.481] ±0% |
| arguana text: mean, ms | 2.27 [2.25–2.29] ±1% | 5.42 [5.38–5.43] ±0% |
| arguana text: p50, ms | 0.865 [0.859–0.869] ±1% | 4.93 [4.89–4.93] ±0% |
| arguana vector: nDCG@10, nDCG@10 | 0.597 [0.597–0.597] ±0% | 0.595 [0.595–0.595] ±0% |
| arguana vector: mean, ms | 0.361 [0.355–0.378] ±3% | 0.367 [0.363–0.380] ±2% |
| arguana vector: p50, ms | 0.359 [0.355–0.369] ±1% | 0.364 [0.361–0.369] ±1% |
| arguana rrf: nDCG@10, nDCG@10 | 0.598 [0.598–0.598] ±0% | 0.589 [0.589–0.589] ±0% |
| arguana rrf: mean, ms | 3.61 [3.59–3.63] ±0% | 6.45 [6.44–6.53] ±1% |
| arguana rrf: p50, ms | 2.21 [2.20–2.21] ±0% | 5.98 [5.96–6.05] ±1% |
| arguana convex: nDCG@10, nDCG@10 | 0.617 [0.617–0.617] ±0% | — |
| arguana convex: mean, ms | 3.79 [3.79–3.91] ±1% | — |
| arguana convex: p50, ms | 2.41 [2.39–2.45] ±1% | — |
| arguana splade: nDCG@10, nDCG@10 | 0.474 [0.474–0.474] ±0% | 0.473 [0.472–0.473] ±0% |
| arguana splade: mean, ms | 3.95 [3.95–3.96] ±0% | 1.37 [1.37–1.38] ±0% |
| arguana splade: p50, ms | 3.95 [3.95–3.97] ±0% | 1.36 [1.35–1.37] ±0% |
| arguana rrf3: nDCG@10, nDCG@10 | 0.584 [0.584–0.584] ±0% | — |
| arguana rrf3: mean, ms | 8.37 [8.36–8.45] ±0% | — |
| arguana rrf3: p50, ms | 7.24 [7.21–7.30] ±0% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — paradedb pg_search 0.26.6, pgvector 0.8.6 — nucleus 7ff7b48e3+changes — 2026-11-17T01:14:46+00:00

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
| scifact: index built, ms | 388 [386–400] ±1% | — | 753 [750–768] ±1% |
| scifact all index: mean, ms | — | 0.164 [0.162–0.181] ±5% | — |
| scifact all index: p50, ms | — | 0.162 [0.159–0.176] ±4% | — |
| scifact all index: found, documents | — | 29.0 [29.0–29.0] ±0% | — |
| scifact all values: mean, ms | — | 17.2 [17.1–17.8] ±2% | — |
| scifact all values: p50, ms | — | 17.0 [17.0–17.4] ±1% | — |
| scifact all values: found, documents | — | 29.0 [29.0–29.0] ±0% | — |
| scifact any index: mean, ms | — | 0.416 [0.412–0.425] ±1% | — |
| scifact any index: p50, ms | — | 0.416 [0.408–0.423] ±1% | — |
| scifact any index: found, documents | — | 750,257 [750,257–750,257] ±0% | — |
| scifact any values: mean, ms | — | 17.6 [17.5–18.1] ±1% | — |
| scifact any values: p50, ms | — | 17.3 [17.2–17.5] ±1% | — |
| scifact any values: found, documents | — | 750,257 [750,257–750,257] ±0% | — |
| nfcorpus: index built, ms | 364 [362–385] ±3% | — | 556 [550–583] ±2% |
| nfcorpus all index: mean, ms | — | 0.126 [0.125–0.127] ±1% | — |
| nfcorpus all index: p50, ms | — | 0.124 [0.123–0.125] ±1% | — |
| nfcorpus all index: found, documents | — | 2,790 [2,790–2,790] ±0% | — |
| nfcorpus all values: mean, ms | — | 13.8 [13.5–15.2] ±5% | — |
| nfcorpus all values: p50, ms | — | 13.4 [13.0–15.3] ±7% | — |
| nfcorpus all values: found, documents | — | 2,790 [2,790–2,790] ±0% | — |
| nfcorpus any index: mean, ms | — | 0.196 [0.193–0.197] ±1% | — |
| nfcorpus any index: p50, ms | — | 0.172 [0.170–0.174] ±1% | — |
| nfcorpus any index: found, documents | — | 160,305 [160,305–160,305] ±0% | — |
| nfcorpus any values: mean, ms | — | 13.8 [13.2–15.3] ±6% | — |
| nfcorpus any values: p50, ms | — | 13.0 [13.0–15.4] ±8% | — |
| nfcorpus any values: found, documents | — | 160,305 [160,305–160,305] ±0% | — |
| fiqa: index built, ms | 669 [661–687] ±2% | — | 1,632 [1,614–1,658] ±1% |
| fiqa all index: mean, ms | — | 0.263 [0.258–0.271] ±2% | — |
| fiqa all index: p50, ms | — | 0.250 [0.246–0.258] ±2% | — |
| fiqa all index: found, documents | — | 13,878 [13,878–13,878] ±0% | — |
| fiqa all values: mean, ms | — | 81.1 [80.6–81.9] ±1% | — |
| fiqa all values: p50, ms | — | 80.2 [79.7–81.0] ±1% | — |
| fiqa all values: found, documents | — | 13,878 [13,878–13,878] ±0% | — |
| fiqa any index: mean, ms | — | 1.66 [1.65–1.69] ±1% | — |
| fiqa any index: p50, ms | — | 1.57 [1.56–1.60] ±1% | — |
| fiqa any index: found, documents | — | 11,473,938 [11,473,938–11,473,938] ±0% | — |
| fiqa any values: mean, ms | — | 83.5 [82.9–85.4] ±1% | — |
| fiqa any values: p50, ms | — | 82.8 [81.7–84.5] ±1% | — |
| fiqa any values: found, documents | — | 11,473,938 [11,473,938–11,473,938] ±0% | — |
| scifact all: mean, ms | — | — | 0.081 [0.080–0.085] ±2% |
| scifact all: p50, ms | — | — | 0.078 [0.077–0.081] ±2% |
| scifact all: found, documents | — | — | 29.0 [29.0–29.0] ±0% |
| scifact any: mean, ms | — | — | 16.8 [16.7–17.1] ±1% |
| scifact any: p50, ms | — | — | 0.675 [0.667–0.692] ±2% |
| scifact any: found, documents | — | — | 739,735 [739,735–739,735] ±0% |
| nfcorpus all: mean, ms | — | — | 0.059 [0.058–0.068] ±7% |
| nfcorpus all: p50, ms | — | — | 0.058 [0.057–0.064] ±5% |
| nfcorpus all: found, documents | — | — | 2,745 [2,745–2,745] ±0% |
| nfcorpus any: mean, ms | — | — | 0.153 [0.152–0.163] ±3% |
| nfcorpus any: p50, ms | — | — | 0.103 [0.102–0.106] ±2% |
| nfcorpus any: found, documents | — | — | 158,593 [158,593–158,593] ±0% |
| fiqa all: mean, ms | — | — | 0.229 [0.228–0.229] ±0% |
| fiqa all: p50, ms | — | — | 0.194 [0.193–0.194] ±0% |
| fiqa all: found, documents | — | — | 12,230 [12,230–12,230] ±0% |
| fiqa any: mean, ms | — | — | 7.58 [7.55–7.61] ±0% |
| fiqa any: p50, ms | — | — | 4.91 [4.87–4.94] ±1% |
| fiqa any: found, documents | — | — | 10,483,362 [10,483,362–10,483,362] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 7ff7b48e3+changes — 2026-11-17T01:29:44+00:00

A search page's query (`bash bench/all.sh tsearch-rank`; PLAN.md §M14.21
T8b): the ten best matches by `ts_rank(to_tsvector('english', body), q)`,
`id` breaking ties, and with each one's `ts_headline('english', body,
q)`; nDCG@10 and the ms of a query.  Both analyze every matching row's
text again for `ts_rank` — the "any" queries rank ~750,000 matches each;
nucleus's own BM25 ranking of the same corpora (above) is the fast way:

| tsearch-rank | nucleus | nucleus, pg-compat | postgresql |
|---|---:|---:|---:|
| scifact: index built, ms | 392 [385–394] ±1% | — | 756 [739–784] ±2% |
| scifact all index: mean, ms | — | 0.149 [0.146–0.149] ±1% | — |
| scifact all index: p50, ms | — | 0.145 [0.143–0.145] ±1% | — |
| scifact all index: found, documents | — | 29.0 [29.0–29.0] ±0% | — |
| scifact any index: mean, ms | — | 0.362 [0.359–0.376] ±2% | — |
| scifact any index: p50, ms | — | 0.355 [0.351–0.363] ±1% | — |
| scifact any index: found, documents | — | 750,257 [750,257–750,257] ±0% | — |
| scifact all rank: mean, ms | — | 0.170 [0.168–0.242] ±17% ⚠ | 0.109 [0.108–0.111] ±1% |
| scifact all rank: p50, ms | — | 0.158 [0.155–0.216] ±15% ⚠ | 0.095 [0.094–0.099] ±2% |
| scifact all rank: nDCG@10, nDCG@10 | — | 0.075 [0.075–0.075] ±0% | 0.075 [0.075–0.075] ±0% |
| scifact all headline: mean, ms | — | 0.193 [0.192–0.196] ±1% | 0.132 [0.131–0.139] ±2% |
| scifact all headline: p50, ms | — | 0.166 [0.165–0.167] ±1% | 0.106 [0.105–0.112] ±3% |
| scifact all headline: nDCG@10, nDCG@10 | — | 0.075 [0.075–0.075] ±0% | 0.075 [0.075–0.075] ±0% |
| scifact any rank: mean, ms | — | 242 [241–242] ±0% | 308 [306–308] ±0% |
| scifact any rank: p50, ms | — | 255 [253–255] ±0% | 306 [304–307] ±0% |
| scifact any rank: nDCG@10, nDCG@10 | — | 0.572 [0.572–0.572] ±0% | 0.568 [0.568–0.568] ±0% |
| scifact any headline: mean, ms | — | 243 [243–244] ±0% | 309 [307–310] ±0% |
| scifact any headline: p50, ms | — | 256 [254–257] ±0% | 307 [305–308] ±0% |
| scifact any headline: nDCG@10, nDCG@10 | — | 0.572 [0.572–0.572] ±0% | 0.568 [0.568–0.568] ±0% |
| nfcorpus: index built, ms | 346 [341–351] ±2% | — | 554 [545–560] ±1% |
| nfcorpus all index: mean, ms | — | 0.110 [0.109–0.112] ±1% | — |
| nfcorpus all index: p50, ms | — | 0.106 [0.105–0.108] ±1% | — |
| nfcorpus all index: found, documents | — | 2,790 [2,790–2,790] ±0% | — |
| nfcorpus any index: mean, ms | — | 0.147 [0.146–0.148] ±1% | — |
| nfcorpus any index: p50, ms | — | 0.124 [0.122–0.125] ±1% | — |
| nfcorpus any index: found, documents | — | 160,305 [160,305–160,305] ±0% | — |
| nfcorpus all rank: mean, ms | — | 1.11 [1.10–1.11] ±0% | 1.17 [1.15–1.18] ±1% |
| nfcorpus all rank: p50, ms | — | 0.300 [0.294–0.305] ±1% | 0.212 [0.208–0.215] ±1% |
| nfcorpus all rank: nDCG@10, nDCG@10 | — | 0.207 [0.207–0.207] ±0% | 0.203 [0.203–0.203] ±0% |
| nfcorpus all headline: mean, ms | — | 1.43 [1.42–1.45] ±1% | 1.52 [1.52–1.60] ±2% |
| nfcorpus all headline: p50, ms | — | 0.423 [0.421–0.427] ±1% | 0.347 [0.341–0.355] ±2% |
| nfcorpus all headline: nDCG@10, nDCG@10 | — | 0.207 [0.207–0.207] ±0% | 0.203 [0.203–0.203] ±0% |
| nfcorpus any rank: mean, ms | — | 51.4 [51.3–52.0] ±1% | 62.2 [61.6–62.3] ±0% |
| nfcorpus any rank: p50, ms | — | 22.0 [21.9–22.4] ±1% | 26.8 [26.5–27.4] ±1% |
| nfcorpus any rank: nDCG@10, nDCG@10 | — | 0.304 [0.304–0.304] ±0% | 0.299 [0.299–0.299] ±0% |
| nfcorpus any headline: mean, ms | — | 52.5 [52.2–52.9] ±0% | 63.3 [62.7–63.4] ±0% |
| nfcorpus any headline: p50, ms | — | 23.7 [23.1–24.0] ±2% | 28.0 [27.8–28.3] ±1% |
| nfcorpus any headline: nDCG@10, nDCG@10 | — | 0.304 [0.304–0.304] ±0% | 0.299 [0.299–0.299] ±0% |
| scifact all: mean, ms | — | — | 0.080 [0.080–0.083] ±2% |
| scifact all: p50, ms | — | — | 0.078 [0.077–0.080] ±2% |
| scifact all: found, documents | — | — | 29.0 [29.0–29.0] ±0% |
| scifact any: mean, ms | — | — | 16.9 [16.7–16.9] ±1% |
| scifact any: p50, ms | — | — | 0.675 [0.650–0.690] ±2% |
| scifact any: found, documents | — | — | 739,735 [739,735–739,735] ±0% |
| nfcorpus all: mean, ms | — | — | 0.058 [0.057–0.060] ±2% |
| nfcorpus all: p50, ms | — | — | 0.056 [0.055–0.057] ±1% |
| nfcorpus all: found, documents | — | — | 2,745 [2,745–2,745] ±0% |
| nfcorpus any: mean, ms | — | — | 0.152 [0.150–0.162] ±3% |
| nfcorpus any: p50, ms | — | — | 0.103 [0.101–0.107] ±2% |
| nfcorpus any: found, documents | — | — | 158,593 [158,593–158,593] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 7ff7b48e3+changes — 2026-11-17T01:53:58+00:00

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
| bge-small-en-v1.5, 1 thread: tokens/s, tokens/s | 1,617 [1,615–1,617] ±0% | 502 [502–504] ±0% | 508 [508–509] ±0% |
| bge-small-en-v1.5, 1 thread: texts/s, texts/s | 9.82 [9.81–9.82] ±0% | 3.05 [3.05–3.06] ±0% | 3.09 [3.08–3.09] ±0% |
| bge-small-en-v1.5, 10 threads: tokens/s, tokens/s | 10,628 [10,567–10,631] ±0% | 2,566 [2,560–2,574] ±0% | 2,452 [2,445–2,465] ±0% |
| bge-small-en-v1.5, 10 threads: texts/s, texts/s | 64.6 [64.2–64.6] ±0% | 15.6 [15.6–15.6] ±0% | 14.9 [14.9–15.0] ±0% |
| multilingual-e5-small, 1 thread: tokens/s, tokens/s | 1,601 [1,588–1,601] ±0% | 502 [496–503] ±1% | — |
| multilingual-e5-small, 1 thread: texts/s, texts/s | 9.53 [9.45–9.53] ±0% | 2.99 [2.95–2.99] ±1% | — |
| multilingual-e5-small, 10 threads: tokens/s, tokens/s | 10,468 [10,326–10,523] ±1% | 2,553 [2,548–2,580] ±1% | — |
| multilingual-e5-small, 10 threads: texts/s, texts/s | 62.3 [61.4–62.6] ±1% | 15.2 [15.2–15.3] ±1% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — fastembed 0.9.1 — nucleus 7ff7b48e3+changes — 2026-11-17T03:03:03+00:00

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
| scifact: vectors/s, vectors/s | 50.3 [50.3–50.7] ±0% |
| scifact: tokens/s, tokens/s | 10,101 [10,092–10,174] ±0% |
| nfcorpus: documents, count | 3,633 [3,633–3,633] ±0% |
| nfcorpus: chunks, count | 8,748 [8,748–8,748] ±0% |
| nfcorpus: vectors, count | 12,381 [12,381–12,381] ±0% |
| nfcorpus: vectors/s, vectors/s | 49.4 [49.3–49.4] ±0% |
| nfcorpus: tokens/s, tokens/s | 10,071 [10,065–10,080] ±0% |
| arguana: documents, count | 8,674 [8,674–8,674] ±0% |
| arguana: chunks, count | 13,215 [13,215–13,215] ±0% |
| arguana: vectors, count | 21,889 [21,889–21,889] ±0% |
| arguana: vectors/s, vectors/s | 65.6 [65.3–65.8] ±0% |
| arguana: tokens/s, tokens/s | 10,390 [10,346–10,425] ±0% |
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
| nfcorpus: nDCG@10, chunks after, nDCG@10 | 0.338 [0.337–0.339] ±0% |
| arguana: nDCG@10, documents, nDCG@10 | 0.597 [0.597–0.597] ±0% |
| arguana: nDCG@10, chunks, nDCG@10 | 0.577 [0.577–0.577] ±0% |
| arguana: nDCG@10, documents, exact, nDCG@10 | 0.597 [0.597–0.597] ±0% |
| arguana: nDCG@10, fastembed, exact, nDCG@10 | 0.597 [0.597–0.597] ±0% |
| arguana: nDCG@10, documents after, nDCG@10 | 0.598 [0.598–0.598] ±0% |
| arguana: nDCG@10, chunks after, nDCG@10 | 0.577 [0.577–0.577] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T05:30:57+00:00

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
| recall, not analyzed: median, ms | — | — | — | — | — | 17.0 [17.0–17.3] ±1% |
| recall, not analyzed: p95, ms | — | — | — | — | — | 22.8 [22.4–24.5] ±5% |
| bm25, not analyzed: median, ms | — | — | — | — | — | 3.56 [3.53–3.56] ±1% |
| bm25, not analyzed: p95, ms | — | — | — | — | — | 4.36 [4.35–4.39] ±0% |
| vector, not analyzed: median, ms | — | — | — | — | — | 1.21 [1.21–1.21] ±0% |
| vector, not analyzed: p95, ms | — | — | — | — | — | 1.35 [1.33–1.37] ±2% |
| recall, analyzed: median, ms | — | — | — | — | — | 5.09 [5.06–5.15] ±1% |
| recall, analyzed: p95, ms | — | — | — | — | — | 6.48 [6.29–6.54] ±2% |
| bm25, analyzed: median, ms | — | — | — | — | — | 3.46 [3.45–3.47] ±0% |
| bm25, analyzed: p95, ms | — | — | — | — | — | 4.40 [4.33–4.50] ±2% |
| vector, analyzed: median, ms | — | — | — | — | — | 1.18 [1.18–1.18] ±0% |
| vector, analyzed: p95, ms | — | — | — | — | — | 1.25 [1.25–1.30] ±3% |
| recall, k=10, analyzed: median, ms | — | — | — | — | — | 3.94 [3.92–3.94] ±0% |
| recall, k=10, analyzed: p95, ms | — | — | — | — | — | 4.96 [4.95–5.00] ±1% |
| REMEMBER: median, ms | — | — | — | — | — | 28.4 [28.1–28.9] ±2% |
| REMEMBER: p95, ms | — | — | — | — | — | 58.5 [58.2–58.7] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T06:51:09+00:00

**In the process** (`bash bench/all.sh recall`; PLAN.md §M14.24 R4,
`tests/recall_bench.rs`): a store of 100,000 memories of 1,000 subjects
(8 made-up words each), real files, the hashing model (64 dimensions —
the database's time, not a model's), analyzed, `embedding.pending =
wait`; 1,000 questions of 3 words, a statement parsed once and run with
parameters, and again with a request id given (`REQUEST_ID=gen`).  The
target: `recall(…, 10)` under 1 ms at the median.

| recall | nucleus (ms) | request id (ms) |
|---|---:|---:|
| recall, a question a statement: p50 | 0.805 [0.795–0.820] ±1% | 1.50 [1.39–1.62] ±6% |
| recall, a question a statement: p95 | 0.867 [0.815–0.914] ±5% | 1.77 [1.75–2.00] ±6% |
| recall, 16 questions a statement: p50 | 0.626 [0.619–0.634] ±1% | 0.685 [0.675–0.700] ±1% |
| recall, 16 questions a statement: p95 | 0.637 [0.625–0.658] ±2% | 0.718 [0.697–0.724] ±2% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T10:01:37+00:00

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
| the search of the files, ms | 91.6 [91.5–92.9] ±1% |
| DELETE a subject's rows, ms | 6.08 [5.89–6.34] ±7% |
| FORGET SUBJECT, ms | 419 [408–449] ±5% |
| planted texts found, after FORGET SUBJECT, places | 0.000 [0.000–0.000] ±0% |
| after the erasures, MiB | 68.0 [61.0–76.0] ±9% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus b4a2517f1 — 2026-11-17T11:42:56+00:00

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
| BACKUP TO, the notes' rows sealed under their subjects' keys, s | 0.547 [0.535–0.550] ±1% |
| BACKUP TO, the notes' rows sealed under their subjects' keys: stored, MiB | 16.5 [16.5–16.5] ±0% |
| BACKUP TO, the same with no subject column (nothing sealed), s | 0.161 [0.158–0.171] ±3% |
| BACKUP TO, the same with no subject column (nothing sealed): stored, MiB | 4.04 [4.04–4.04] ±0% |
| nucleus restore, sealed, s | 0.848 [0.844–0.856] ±1% |
| nucleus restore, unsealed, s | 0.707 [0.695–0.723] ±2% |
| BACKUP TO, the first: full, s | 0.684 [0.674–0.686] ±1% |
| BACKUP TO, the first: full: stored, MiB | 29.2 [29.2–29.2] ±0% |
| BACKUP TO, nothing changed, s | 0.021 [0.018–0.022] ±7% |
| BACKUP TO, nothing changed: stored, MiB | 0.000194 [0.000194–0.000194] ±0% |
| BACKUP TO, 10,000 rows updated, adjacent (1 %), s | 0.033 [0.030–0.035] ±7% |
| BACKUP TO, 10,000 rows updated, adjacent (1 %): stored, MiB | 0.285 [0.285–0.285] ±0% |
| BACKUP TO, 10,000 rows updated, every 100th (1 %: nearly every leaf), s | 0.481 [0.478–0.488] ±1% |
| BACKUP TO, 10,000 rows updated, every 100th (1 %: nearly every leaf): stored, MiB | 0.300 [0.300–0.300] ±0% |
| BACKUP TO, 100 rows updated, every 10,000th, s | 0.068 [0.068–0.072] ±3% |
| BACKUP TO, 100 rows updated, every 10,000th: stored, MiB | 0.004 [0.004–0.004] ±0% |
| BACKUP TO, 10,000 rows deleted, adjacent, s | 0.436 [0.433–0.448] ±2% |
| BACKUP TO, 10,000 rows deleted, adjacent: stored, MiB | 0.048 [0.048–0.048] ±0% |
| BACKUP FULL TO, s | 0.691 [0.679–0.696] ±1% |
| BACKUP FULL TO: stored, MiB | 29.0 [29.0–29.0] ±0% |
| nucleus restore, the chain of 6, s | 1.52 [1.52–1.54] ±1% |
| nucleus restore, the full one after it, s | 1.45 [1.44–1.50] ±2% |
| BACKUP FULL TO, compressed (LZ4 block), s | 0.682 [0.675–0.701] ±2% |
| BACKUP FULL TO, compressed (LZ4 block): stored, MiB | 29.2 [29.2–29.2] ±0% |
| nucleus restore, compressed (LZ4 block), s | 1.47 [1.45–1.47] ±1% |
| BACKUP FULL TO, compressed and encrypted (ChaCha20-Poly1305), s | 0.744 [0.740–0.757] ±1% |
| BACKUP FULL TO, compressed and encrypted (ChaCha20-Poly1305): stored, MiB | 29.2 [29.2–29.2] ±0% |
| nucleus restore, compressed and encrypted (ChaCha20-Poly1305), s | 1.49 [1.49–1.53] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus nucleus 0.1.0 — nucleus 7ff7b48e3+changes — 2026-11-17T06:53:06+00:00

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
| idle, ALTER COLUMN … TYPE, s | 5.31 [3.71–6.69] ±22% ⚠ |
| idle, ALTER COLUMN … TYPE CONCURRENTLY, s | 10.4 [10.1–11.1] ±4% |
| a writer alone, updates/s | 3,107 [3,107–3,107] ±0% |
| a writer alone: p50, ms | 0.274 [0.274–0.274] ±0% |
| a writer alone: p99, ms | 0.768 [0.768–0.768] ±0% |
| under writes, ALTER COLUMN … TYPE, s | 142 [136–163] ±8% |
| under writes, TYPE: not done in 3 tries, runs | 5.00 [5.00–5.00] ±0% |
| under writes, TYPE: the writer, updates/s | 2,938 [2,538–3,131] ±8% |
| under writes, TYPE: the writer's p50, ms | 0.302 [0.273–0.348] ±10% ⚠ |
| under writes, TYPE: the writer's p99, ms | 0.735 [0.625–0.840] ±12% ⚠ |
| under writes, ALTER COLUMN … TYPE CONCURRENTLY, s | 25.0 [23.9–25.5] ±2% |
| under writes, TYPE CONCURRENTLY: not done in 3 tries, runs | 0.000 [0.000–0.000] ±0% |
| under writes, TYPE CONCURRENTLY: the writer, updates/s | 3,176 [2,908–3,397] ±6% |
| under writes, TYPE CONCURRENTLY: the writer's p50, ms | 0.245 [0.237–0.253] ±2% |
| under writes, TYPE CONCURRENTLY: the writer's p99, ms | 1.85 [1.12–2.76] ±33% ⚠ |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T06:55:04+00:00

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
| crc32c: MB/s, MB/s | 22,624 [19,451–22,814] ±7% |
| crc32c: µs a page, µs | 0.362 [0.359–0.421] ±7% |
| chacha20-poly1305 seal: MB/s, MB/s | 660 [649–662] ±1% |
| chacha20-poly1305 seal: µs a page, µs | 12.4 [12.4–12.6] ±1% |
| siv seal (in place): MB/s, MB/s | 667 [648–668] ±1% |
| siv seal (in place): µs a page, µs | 12.3 [12.3–12.6] ±1% |
| siv seal + open: MB/s, MB/s | 334 [323–335] ±2% |
| siv seal + open: µs a page, µs | 24.5 [24.5–25.4] ±2% |
| aes-gcm-siv seal (in place): MB/s, MB/s | 4,943 [4,928–4,969] ±0% |
| aes-gcm-siv seal (in place): µs a page, µs | 1.66 [1.65–1.66] ±0% |
| aes-gcm-siv seal + open: MB/s, MB/s | 2,487 [2,479–2,490] ±0% |
| aes-gcm-siv seal + open: µs a page, µs | 3.29 [3.29–3.30] ±0% |
| in the clear: scan, ms | 49.5 [48.8–51.3] ±2% |
| chacha20-siv: load + checkpoint, ms | 292 [288–294] ±1% |
| chacha20-siv: scan, ms | 127 [127–128] ±1% |
| aes-gcm-siv: load + checkpoint, ms | 156 [154–159] ±1% |
| aes-gcm-siv: scan, ms | 57.6 [57.2–60.0] ±2% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T07:18:28+00:00

The AEADs' parts (`bash bench/all.sh aead`; `nucleus-crypto`'s speed
test): ChaCha20 runs four blocks side by side and Poly1305 in 64-bit
limbs, portable Rust the compiler vectorizes.

| aead | nucleus (MB/s) |
|---|---:|
| chacha20 | 998 [982–998] ±1% |
| chacha20, a block at a time | 623 [622–624] ±0% |
| poly1305 | 2,196 [2,186–2,198] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T07:18:52+00:00

AES-256-GCM-SIV by size (`bash bench/all.sh aes`; `nucleus-kernels`'
`aes_speed`): by ARMv8's AES and PMULL — POLYVAL eight blocks at a time,
reduced once, the counter blocks encrypted eight at a time, a fixed
~0.25 µs a message besides (its keys from the nonce, the powers of H) —
and the portable, constant-time AES for a file opened on a CPU without
the instructions.  A page is sealed when it is written and opened when
it is read from the file — never while it is cached.

| aes | nucleus (µs) |
|---|---:|
| instructions, 64 bytes: seal | 0.311 [0.311–0.312] ±0% |
| instructions, 64 bytes: open | 0.305 [0.283–0.308] ±4% |
| instructions, 4096 bytes: seal | 0.976 [0.975–0.978] ±0% |
| instructions, 4096 bytes: open | 0.938 [0.862–0.941] ±4% |
| instructions, 8168 bytes: seal | 1.75 [1.75–1.76] ±0% |
| instructions, 8168 bytes: open | 1.71 [1.40–1.73] ±8% |
| instructions, 65536 bytes: seal | 11.2 [11.2–11.2] ±0% |
| instructions, 65536 bytes: open | 11.2 [11.0–11.3] ±1% |
| portable, 8168 bytes: seal | 8,898 [8,834–9,092] ±1% |
| portable, 8168 bytes: open | 8,813 [8,759–9,047] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T07:18:53+00:00

Through SQL (`bash bench/all.sh encrypted`; K2e, `crates/sql/tests/encrypted.rs`,
real files): 300,000 rows (`id`, `g`, a 40-byte text) inserted and
checkpointed, an aggregate over them all through an 8 MiB page cache,
and an `ORDER BY` past a 1 MB `work_mem` (its runs in scratch files,
sealed in 4 KiB blocks when the database is); encrypted with
AES-256-GCM-SIV:

| encrypted | nucleus (ms) |
|---|---:|
| in the clear: load | 346 [337–363] ±3% |
| in the clear: scan | 9.71 [9.49–10.1] ±2% |
| in the clear: sort past work_mem | 100 [99.0–105] ±2% |
| encrypted: load | 324 [319–334] ±2% |
| encrypted: scan | 10.3 [9.83–10.3] ±2% |
| encrypted: sort past work_mem | 120 [119–121] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T07:18:43+00:00

A new data key (`bash bench/all.sh keys`; K3f, `bench/keys.py`, `nucleus
keys <dir> rotate data` on real files): a database of 1,000,000 rows of
~210 bytes sealed again under the new key in place, 256 pages a batch —
each batch's old bytes journaled and synced, then the pages written and
synced; the rows read back the same, none in the clear.  `ROTATE KEY
data` with the server running does the same in the background, paced
while transactions run.

| keys | nucleus |
|---|---:|
| rotate data, s | 0.607 [0.602–0.628] ±2% |
| database, MB | 340 [340–340] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus nucleus 0.1.0 — nucleus 7ff7b48e3+changes — 2026-11-17T07:19:39+00:00

Sealed rows and sealed memory stores (`bash bench/all.sh sealing`;
K4a, K4c, `crates/sql/tests/sealed.rs`, in memory): 200,000 rows of ~60
bytes over 1,000 subjects, stored `row` and `sealed`; memory stores of
1,000 subjects with a 384-dimension hashing model, `recall` of 10
averaged over 300 calls, indexed and sealed (at 100 memories a subject
the sealed one's exact ranking costs less than the shared index's
upkeep):

| sealing | nucleus |
|---|---:|
| 20 memories a subject, indexed: load, s | 3.03 [3.01–3.08] ±1% |
| 20 memories a subject, indexed: recall, ms | 0.270 [0.269–0.270] ±0% |
| 20 memories a subject, sealed: load, s | 0.761 [0.756–0.781] ±1% |
| 20 memories a subject, sealed: recall, ms | 0.377 [0.365–0.380] ±2% |
| 100 memories a subject, indexed: load, s | 32.0 [31.9–32.2] ±0% |
| 100 memories a subject, indexed: recall, ms | 17.3 [17.2–17.5] ±1% |
| 100 memories a subject, sealed: load, s | 27.6 [27.5–27.8] ±0% |
| 100 memories a subject, sealed: recall, ms | 14.3 [14.3–14.4] ±0% |
| row: load 200000 rows, ms | 213 [210–227] ±3% |
| row: scan, ms | 8.37 [8.16–9.12] ±4% |
| row: lookup, µs | 15.3 [15.1–15.6] ±1% |
| sealed: load 200000 rows, ms | 505 [490–514] ±2% |
| sealed: scan, ms | 37.3 [36.8–39.0] ±2% |
| sealed: lookup, µs | 15.9 [15.7–16.4] ±2% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T07:12:09+00:00

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
| parse (canada.json), ms | 3.86 [3.80–4.54] ±8% | 6.80 [6.79–6.81] ±0% |
| output (canada.json), ms | 5.00 [5.00–5.13] ±1% | 2.09 [2.09–2.13] ±1% |
| field (canada.json), µs | 0.024 [0.024–0.024] ±0% | 6,790 [6,774–6,803] ±0% |
| size (canada.json), KB | 1,667 [1,667–1,667] ±0% | — |
| parse (citm_catalog.json), ms | 1.68 [1.68–1.69] ±0% | 2.47 [2.46–2.59] ±2% |
| output (citm_catalog.json), ms | 1.06 [1.06–1.08] ±1% | 0.418 [0.415–0.433] ±2% |
| field (citm_catalog.json), µs | 0.035 [0.035–0.035] ±0% | 2,461 [2,460–2,541] ±1% |
| size (citm_catalog.json), KB | 407 [407–407] ±0% | — |
| parse (twitter.json), ms | 0.745 [0.742–0.759] ±1% | 1.29 [1.28–1.32] ±1% |
| output (twitter.json), ms | 0.535 [0.533–0.554] ±2% | 0.299 [0.296–0.320] ±3% |
| field (twitter.json), µs | 0.025 [0.025–0.025] ±0% | 1,283 [1,280–1,292] ±0% |
| size (twitter.json), KB | 431 [431–431] ±0% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T07:19:54+00:00

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
| parse (tweets), s | 0.384 [0.382–0.386] ±5% | 1.32 [1.31–1.32] ±1% |
| output (tweets), s | 0.134 [0.133–0.136] ±15% ⚠ | 0.996 [0.995–0.998] ±1% |
| field (tweets), s | 0.067 [0.066–0.068] ±8% | 0.254 [0.253–0.254] ±1% |
| path (tweets), s | 0.063 [0.062–0.064] ±11% ⚠ | 0.251 [0.250–0.252] ±2% |
| value (tweets), s | 0.063 [0.063–0.064] ±4% | 0.253 [0.253–0.254] ±1% |
| table (tweets), s | 0.069 [0.068–0.070] ±4% | 0.266 [0.266–0.267] ±2% |
| parse (performances), s | 0.089 [0.088–0.089] ±3% | 0.717 [0.716–0.718] ±1% |
| output (performances), s | 0.047 [0.046–0.047] ±5% | 0.497 [0.496–0.497] ±1% |
| field (performances), s | 0.009 [0.009–0.009] ±10% | 0.036 [0.036–0.036] ±3% |
| path (performances), s | 0.017 [0.017–0.017] ±4% | 0.051 [0.051–0.051] ±2% |
| value (performances), s | 0.009 [0.008–0.009] ±10% ⚠ | 0.037 [0.036–0.037] ±2% |
| table (performances), s | 0.298 [0.297–0.298] ±1% | 0.321 [0.320–0.322] ±1% |
| parse (features), s | 0.025 [0.025–0.025] ±2% | 0.064 [0.064–0.064] ±1% |
| output (features), s | 0.029 [0.029–0.029] ±2% | 0.040 [0.040–0.040] ±2% |
| field (features), s | 0.004 [0.004–0.004] ±3% | 0.011 [0.010–0.011] ±2% |
| path (features), s | 0.004 [0.004–0.004] ±11% ⚠ | 0.011 [0.011–0.011] ±4% |
| value (features), s | 0.004 [0.004–0.004] ±26% ⚠ | 0.011 [0.010–0.011] ±1% |
| table (features), s | 0.106 [0.105–0.106] ±2% | 0.123 [0.123–0.124] ±2% |
| size (canada.json), KB | — | 2,599 [2,599–2,599] ±0% |
| size (citm_catalog.json), KB | — | 739 [739–739] ±0% |
| size (twitter.json), KB | — | 497 [497–497] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 8e8a1a467+changes — 2026-11-18T09:36:18+00:00

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
| id, no index | 0.008 [0.008–0.009] ±16% ⚠ | 0.009 [0.009–0.009] ±3% |
| id, indexed | 0.000114 [0.000111–0.000117] ±17% ⚠ | 3.7e-05 [3.5e-05–3.9e-05] ±26% ⚠ |
| score, no index | 0.016 [0.016–0.016] ±4% | 0.014 [0.014–0.014] ±2% |
| score, indexed | 0.00096 [0.000958–0.000964] ±6% | 0.000111 [0.000105–0.000121] ±24% ⚠ |
| user, no index | 0.009 [0.009–0.009] ±4% | 0.013 [0.012–0.013] ±2% |
| user, indexed | 0.000149 [0.000147–0.000153] ±12% ⚠ | 7e-05 [6.7e-05–7.3e-05] ±30% ⚠ |
| kind+tag, no index | 0.008 [0.008–0.008] ±3% | 0.010 [0.010–0.010] ±2% |
| kind+tag, indexed | 0.002 [0.002–0.002] ±8% | 0.001 [0.001–0.001] ±10% |
| rare, no index | 0.007 [0.007–0.007] ±3% | 0.006 [0.006–0.006] ±4% |
| rare, indexed | 0.000305 [0.000303–0.000307] ±8% | 0.006 [0.006–0.006] ±7% |
| build id index | 0.108 [0.107–0.111] ±3% | 0.041 [0.040–0.041] ±2% |
| build score index | 0.088 [0.087–0.090] ±3% | 0.038 [0.037–0.038] ±2% |
| build containment index | 0.827 [0.820–0.829] ±1% | 0.266 [0.266–0.270] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — postgresql 18.7 (Homebrew) — nucleus 7ff7b48e3+changes — 2026-11-17T07:36:31+00:00

`jsonb_path_ops` cannot serve `?` (PostgreSQL scans for "rare"); the
`json` index can.  Since this run the `json` index builds on the
workers, in 207 ms (§M14.33 S18); PostgreSQL still builds the B-tree
expression indexes 2.3× faster (open: issue #9).  And a count over an index's range
reads no row (§M14.33 S17): score, indexed, 165 µs, measured alone; the
point lookups' remaining gap is the clients' (nucleus's Python driver
over TCP against psql), the statement itself 5 µs.

JSON Schema checks (`bash bench/all.sh json-schema`; J5,
`bench/json/src/bin/schema.rs`): 100,000 made-up tool calls, one in ten
not fitting, against a schema with `$ref`, `enum`, `pattern`, `anyOf`,
`uniqueItems` and bounds; each document parsed already, against the
`jsonschema` crate 0.61 ("compiled, kept": the check by a schema
compiled once and held, as a `CHECK` uses it):

| json-schema | nucleus | jsonschema |
|---|---:|---:|
| fits (a document), ns | 275 [272–280] ±1% | 148 [142–153] ±3% |
| errors (a document), ns | 398 [396–401] ±0% | 253 [251–260] ±1% |
| compile (the schema), µs | 18.1 [17.9–20.2] ±5% | 22.6 [22.3–23.2] ±1% |
| compiled, kept, ns | 188 [185–189] ±1% | — |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus a8a0111b0+changes — 2026-11-18T10:04:04+00:00

In SQL (`bash bench/all.sh json-schema-pg`; `bench/json/schema.py`,
90,000 rows): `INSERT … SELECT` into a table with `CHECK
(json_matches_schema(args, '…'))` against one without, `count(*)`, and
`count(*) … WHERE json_matches_schema(…)` and the sum of
`json_schema_errors`' counts:

| json-schema-pg | nucleus (s) |
|---|---:|
| load without a check | 1.78 [1.77–1.79] ±1% |
| load with the CHECK | 2.32 [2.31–2.32] ±1% |
| count(*) | 0.002 [0.001–0.002] ±9% |
| count matching | 0.012 [0.012–0.012] ±5% |
| sum of errors | 0.013 [0.013–0.013] ±3% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T07:39:00+00:00

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
100 a transaction; and the steady state, 5,000,000 rows 100 a
transaction into the random table:

| wamp | nucleus |
|---|---:|
| bulk, 1M rows a transaction, keys in order: write amplification, x | 4.08 [3.60–4.08] ±7% |
| bulk, 1M rows a transaction, keys in order: time, s | 8.36 [8.25–8.62] ±2% |
| bulk, 1M rows a transaction, keys in order: files, MiB | 1,560 [1,560–1,560] ±0% |
| bulk, 1M rows a transaction, random keys: write amplification, x | 5.71 [5.71–5.72] ±0% |
| bulk, 1M rows a transaction, random keys: time, s | 69.3 [68.1–70.0] ±1% |
| bulk, 1M rows a transaction, random keys: files, MiB | 4,646 [4,646–4,646] ±0% |
| a row a transaction, keys in order: write amplification, x | 152 [152–153] ±0% |
| a row a transaction, keys in order: time, s | 18.4 [18.3–18.4] ±1% |
| a row a transaction, keys in order: files, MiB | 4,589 [4,589–4,589] ±0% |
| a row a transaction, random keys: write amplification, x | 216 [216–216] ±0% |
| a row a transaction, random keys: time, s | 19.5 [18.8–19.8] ±3% |
| a row a transaction, random keys: files, MiB | 4,591 [4,591–4,591] ±0% |
| 100 rows a transaction, random keys: write amplification, x | 77.0 [77.0–77.0] ±0% |
| 100 rows a transaction, random keys: time, s | 4.47 [4.33–4.48] ±2% |
| 100 rows a transaction, random keys: files, MiB | 4,590 [4,590–4,590] ±0% |
| 100 rows a transaction, random keys, 5M rows: write amplification, x | 136 [123–138] ±6% |
| 100 rows a transaction, random keys, 5M rows: time, s | 611 [603–617] ±1% |
| 100 rows a transaction, random keys, 5M rows: files, MiB | 7,352 [6,315–7,352] ±9% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T07:41:55+00:00

A commit costs ~16 KiB on APFS whatever the WAL does — APFS writes a
changed block elsewhere, so an overwrite costs as much as an append
there (§M14.28 L0).  Random keys among ten million rows put each row in
a leaf the 128 MiB cache rarely holds, so nearly every row costs a page
written when it is evicted or checkpointed: 8 KiB for 108 bytes.  That
is the B+tree's cost an LSM tree's runs avoid (§M14.28 L2–L5).  The
comparison with PostgreSQL and the runs on ext4 have no step yet
(PLAN.md §M14.28's study).

**An LSM table beside the B+tree** (`bash bench/all.sh wamp-lsm`;
`wamp.py --engine=lsm`, phases 0–7: the steady state, then a million
of its rows updated and a million deleted, 100 a transaction; with
`--values=random` the text is 100 random hex digits, which the LSM
tree's LZ4 runs cannot shrink as they do a letter repeated).  One run
each so far (PLAN.md §M14.28 L5); the whole run's will replace them:

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

The restart is the server's after a kill with no checkpoint: recovery
(the B+tree's replays a GiB of WAL of random keys through its cache).

**YCSB** (`bash bench/all.sh ycsb`; `crates/sql/tests/ycsb.rs`, PLAN.md
§M14.28 L5): a million rows of a `bigint` key and 100 bytes of text,
then 200,000 operations of each workload, keys by YCSB's scrambled
Zipfian (θ = 0.99), each a statement in a transaction of its own, one
client, in the process.  One run:

| workload | B+tree ops/s | LSM ops/s | read or scan p50 / p99, µs, B+tree | LSM |
|---|---:|---:|---:|---:|
| A: 50 % reads, 50 % updates | 9 716 | 10 336 | 14.1 / 24.7 | 16.0 / 25.1 |
| B: 95 % reads, 5 % updates | 32 330 | 32 227 | 12.0 / 24.6 | 13.9 / 23.0 |
| E: 95 % scans of 1–100 rows, 5 % inserts | 18 963 | 13 516 | 26.8 / 58.1 | 36.0 / 77.8 |

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
| single-row commits | 5,540 [5,248–5,690] ±13% ⚠ | 5,626 [5,276–5,816] ±15% ⚠ |
| rows in 500-row commits | 1,227,047 [1,201,687–1,254,335] ±3% | 1,212,274 [1,201,055–1,246,475] ±8% |
| group commit, 8 threads | 15,864 [15,181–17,028] ±7% | 15,467 [15,253–16,951] ±7% |
| point reads | 1,123,690 [1,060,547–1,205,717] ±7% | 1,196,810 [1,153,866–1,216,602] ±4% |
| full scan | 9,413,055 [9,157,556–9,550,264] ±5% | 9,532,403 [9,373,293–9,600,672] ±16% ⚠ |
| snapshot_at | — | 178,057 [176,330–180,092] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T09:14:12+00:00

| history-sql | no history (/s) | history (/s) |
|---|---:|---:|
| one-row INSERTs, 8 sessions | 14,347 [14,195–14,586] ±1% | 13,827 [12,629–13,977] ±4% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus — nucleus 7ff7b48e3+changes — 2026-11-17T09:14:41+00:00

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
| 100000 rows: in use, MiB | 13.7 [13.7–13.7] ±0% |
| 100000 rows: create, ms | 2.97 [2.29–3.68] ±20% ⚠ |
| 100000 rows: +space, MiB | 0.008 [0.008–0.008] ±0% |
| 100000 rows: drop, ms | 2.77 [2.01–3.26] ±18% ⚠ |
| 100000 rows: as of 10k commits before, ms | 41.5 [40.7–48.4] ±23% ⚠ |
| 100000 rows: reset, ms | 8.82 [8.34–9.16] ±4% |
| 1000000 rows: in use, MiB | 137 [137–137] ±0% |
| 1000000 rows: create, ms | 6.44 [5.76–7.93] ±14% ⚠ |
| 1000000 rows: +space, MiB | 0.000 [0.000–0.000] ±0% |
| 1000000 rows: drop, ms | 3.89 [3.50–4.55] ±15% ⚠ |
| 1000000 rows: as of 10k commits before, ms | 255 [253–261] ±1% |
| 1000000 rows: reset, ms | 39.9 [38.7–40.5] ±2% |
| 4000000 rows: in use, MiB | 548 [548–548] ±0% |
| 4000000 rows: create, ms | 7.77 [7.25–9.92] ±17% ⚠ |
| 4000000 rows: +space, MiB | 0.000 [0.000–0.000] ±0% |
| 4000000 rows: drop, ms | 3.75 [2.71–4.63] ±25% ⚠ |
| 4000000 rows: as of 10k commits before, ms | 869 [861–887] ±2% |
| 4000000 rows: reset, ms | 122 [121–127] ±3% |
| 1000 changed, together: +space, MiB | 0.312 [0.312–0.312] ±0% |
| 1000 changed, together: drop, ms | 2.95 [2.62–3.63] ±16% ⚠ |
| 1000 changed, together: freed, MiB | 0.312 [0.312–0.312] ±0% |
| 100000 changed, together: +space, MiB | 27.4 [27.4–27.4] ±0% |
| 100000 changed, together: drop, ms | 16.9 [16.3–17.8] ±7% |
| 100000 changed, together: freed, MiB | 27.4 [27.4–27.4] ±0% |
| 1000 changed, spread: +space, MiB | 11.1 [11.1–11.1] ±0% |
| 1000 changed, spread: drop, ms | 145 [139–146] ±3% |
| 1000 changed, spread: freed, MiB | 11.1 [11.1–11.1] ±0% |
| 10000 changed, spread: +space, MiB | 81.5 [81.5–81.5] ±0% |
| 10000 changed, spread: drop, ms | 169 [163–176] ±4% |
| 10000 changed, spread: freed, MiB | 81.5 [81.5–81.5] ±0% |
| 100000 changed, spread: +space, MiB | 549 [549–549] ±0% |
| 100000 changed, spread: drop, ms | 320 [316–329] ±2% |
| 100000 changed, spread: freed, MiB | 548 [548–548] ±0% |
| 1000000 changed, spread: +space, MiB | 1,094 [1,094–1,094] ±0% |
| 1000000 changed, spread: drop, ms | 518 [503–523] ±2% |
| 1000000 changed, spread: freed, MiB | 1,093 [1,093–1,093] ±0% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T09:16:38+00:00

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
| INSERT … SELECT of 200000 rows, ms | 225 [224–233] ±2% |
| GROUP BY over them, ms | 42.5 [42.1–45.1] ±3% |
| a point query, µs | 25.7 [25.2–25.8] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus — nucleus 7ff7b48e3+changes — 2026-11-17T09:18:05+00:00

In a browser (`bash bench/all.sh browser`; `drivers/js/test/browser.mjs`,
headless Chrome): the first answer after the page starts — the engine
fetched and compiled, the database opened from OPFS, three statements —
on a new profile's first visit, after a reload, with the engine left
open by the page, and after the browser restarted:

| browser | nucleus (ms) |
|---|---:|
| first answer, a new profile's first visit (cold) | 164 [162–174] ±3% |
| first answer, after a reload | 108 [107–109] ±1% |
| first answer, left open | 65.0 [61.0–69.0] ±5% |
| first answer, after a reload of a page that left it open | 65.0 [63.0–68.0] ±3% |
| first answer, after the browser restarted (cold) | 124 [123–127] ±1% |

Apple M2 Pro; 10 logical CPUs; 32 GiB; macOS-26.7.3-arm64-arm-64bit-Mach-O — nucleus 7ff7b48e3+changes — 2026-11-17T09:18:08+00:00

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
| TPC-H SF 1, column tables: geometric mean ≤ 0.8× PostgreSQL's time | 0.46× | yes |
| ClickBench 1M rows, column table: geometric mean ≤ 0.5× | 0.16× | yes |
| column tables: no TPC-H or ClickBench query above 3× | the highest TPC-H Q20 2.78×, ClickBench Q23 1.70× | yes |
| row tables: geometric mean ≤ 2× (TPC-H, ClickBench) | TPC-H 0.65×, ClickBench 0.57× | yes |
| loads ≤ 2× (`bench/run.py`, TPC-H, ClickBench) | 0.96×; TPC-H 1.40× row, 1.10× column; ClickBench 1.11×, 0.98× | yes |
| updates and deletes ≤ 2× (`bench/run.py`) | update 0.63×, delete 1.41× (an LSM table: 0.73×, 2.27×) | yes (the B+tree, the default) |
| scans, aggregates, sorts and joins ≤ 3× (`bench/run.py`) | 0.24–0.99× (count(*) 0.38×, the filtered count cold 0.99×) | yes |
| reads by key ≤ 1× (`bench/run.py`) | 1.02×, the intervals overlapping (24.5 ms against 24.0 for 1,000 reads; cold 0.85×) | yes |

Against PostgreSQL in Docker with its defaults (the runs before
§M14.31) every target was met, reads by key at 0.5×; natively, with
its buffers sized for the machine, PostgreSQL's 1,000 reads by key took
24 ms, nucleus's 59: nucleus's own per-statement cost — planning every
literal anew and loopback TCP — which §M14.33 R2 took away (a read by
key's plan kept by its shape, the server on a Unix socket, the
session's floor lowered).  §M14.33 R3–R4 brought row tables from 1.27×
(TPC-H) and 1.10× (ClickBench) to 0.65× and 0.57×; their worst queries
now TPC-H Q6 1.68× and Q20 1.58×, ClickBench Q0 0.99×.  The CI's shared
runners are too noisy to fail on a ratio, so a target is checked here,
by a local run.
