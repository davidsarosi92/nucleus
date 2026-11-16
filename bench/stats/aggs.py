#!/usr/bin/env python3
"""Statistical aggregates timed (PLAN.md §M14.27): each build given runs the
same queries over the same table, alternating builds round by round.

    python3 bench/stats/aggs.py target/base/release/nucleus target/release/nucleus \
        [--rows 10000000] [--rounds 1] [--query NAME …] [--work-mem 1GB] \
        [--pg | --psql "psql …"] [--numpy] [--python]

The table `t (id BIGINT PRIMARY KEY, g INT, x DOUBLE PRECISION, y DOUBLE
PRECISION)`: `x` uniform on a large offset (1e9 + U(0,1), as timestamps
and ids are), `y` spread over magnitudes; `g` 1000 groups.  Each query's
median of five runs, in ms; its result printed once, so builds can be
compared for the same answer.  `--pg` runs the same queries on
PostgreSQL (those it has; native, bench/pg.py's cluster), `--psql` on one
already running; `--numpy` numpy's counterparts on one core, `--python`
scipy's and scikit-learn's tests; `--work-mem` sets `work_mem` in every
system's session.  Every number is printed as an `@metric` line too, for
`bench/method.py run`, which repeats the script (PLAN.md §M14.31).
"""

import argparse
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "drivers", "python"))
sys.path.insert(0, os.path.join(HERE, ".."))

import method  # noqa: E402
import nucleus  # noqa: E402

QUERIES = {
    "sum": "SELECT sum(x) FROM t",
    "avg spread": "SELECT avg(y) FROM t",
    "sum 1000 groups": "SELECT g, sum(x) FROM t GROUP BY g ORDER BY g LIMIT 3",
    "avg 1M groups": "SELECT sum(s) FROM (SELECT id % 1000000 AS k, avg(y) AS s FROM t GROUP BY k) q",
    "variance": "SELECT variance(x) FROM t",
    "stddev 1000 groups": "SELECT g, stddev(x) FROM t GROUP BY g ORDER BY g LIMIT 3",
    "variance of integers": "SELECT variance(id) FROM t",
    "skewness, kurtosis": "SELECT skewness(x), kurtosis(x) FROM t",
    "corr": "SELECT corr(x, id) FROM t",
    "regression 1000 groups": "SELECT g, regr_slope(x, id), regr_intercept(x, id) "
                              "FROM t GROUP BY g ORDER BY g LIMIT 3",
    # Window functions (S4), each folded to one row so the output is
    # small: the sort and the frames are the cost.
    "row_number by groups": "SELECT max(r) FROM (SELECT row_number() OVER "
                            "(PARTITION BY g ORDER BY x) AS r FROM t) q",
    "top 3 by groups": "SELECT count(*) FROM (SELECT rank() OVER "
                       "(PARTITION BY g ORDER BY y DESC) AS r FROM t) q WHERE r <= 3",
    "running sum": "SELECT max(s) FROM (SELECT sum(x) OVER (ORDER BY id) AS s FROM t) q",
    "moving avg 10 rows": "SELECT sum(a) FROM (SELECT avg(x) OVER (PARTITION BY g ORDER BY id "
                          "ROWS BETWEEN 9 PRECEDING AND CURRENT ROW) AS a FROM t) q",
    "lag": "SELECT sum(d) FROM (SELECT x - lag(x) OVER (ORDER BY id) AS d FROM t) q",
    # Ordered sets (S5): exact quantiles, the mode, a histogram.
    "median": "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x) FROM t",
    "p50, p95, p99": "SELECT percentile_cont(ARRAY[0.5, 0.95, 0.99]) WITHIN GROUP "
                     "(ORDER BY y) FROM t",
    "percentile_disc": "SELECT percentile_disc(0.9) WITHIN GROUP (ORDER BY x) FROM t",
    "median 1000 groups": "SELECT g, percentile_cont(0.5) WITHIN GROUP (ORDER BY x) "
                          "FROM t GROUP BY g ORDER BY g LIMIT 3",
    "mode": "SELECT mode() WITHIN GROUP (ORDER BY id % 1000) FROM t",
    "histogram": "SELECT histogram(x, 1e9, 1e9 + 1, 100)[51] FROM t",
    # Sketches (S6), beside their exact counterparts.
    "count distinct": "SELECT count(DISTINCT id) FROM t",
    "approx_count_distinct": "SELECT approx_count_distinct(id) FROM t",
    "approx_count_distinct 1000 groups": "SELECT g, approx_count_distinct(id) FROM t "
                                         "GROUP BY g ORDER BY g LIMIT 3",
    "count distinct 1000 groups": "SELECT g, count(DISTINCT id) FROM t GROUP BY g ORDER BY g LIMIT 3",
    "approx_percentile": "SELECT approx_percentile(y, ARRAY[0.5, 0.95, 0.99]) FROM t",
    "approx_top_k": "SELECT approx_top_k(id % 1000, 3) FROM t",
    # Sampling (S7): a tenth of the table, a stable 80 % split, 1000 rows.
    "tablesample": "SELECT count(*), sum(x) FROM t TABLESAMPLE BERNOULLI (10)",
    "tablesample repeatable": "SELECT count(*), sum(x) FROM t TABLESAMPLE BERNOULLI (10) "
                              "REPEATABLE (42)",
    "hash_unit split": "SELECT count(*), sum(x) FROM t WHERE hash_unit(id) < 0.8",
    "reservoir_sample": "SELECT array_length(reservoir_sample(x, 1000), 1) FROM t",
    "order by random() limit": "SELECT count(*), sum(x) FROM (SELECT x FROM t "
                               "ORDER BY random() LIMIT 1000) q",
    # Drift and tests (S8): two halves of the table compared.
    "ks_test": "SELECT ks_test(x, id % 2 = 0) FROM t",
    "auc": "SELECT auc(y, id % 2 = 0) FROM t",
    "psi": "SELECT psi(x, id % 2 = 0) FROM t",
    "chisq_test": "SELECT chisq_test(id % 100, g < 500) FROM t",
    "js_divergence": "SELECT js_divergence(id % 100, g < 500) FROM t",
    # SUMMARIZE (S8b): every column's figures, one scan.
    "summarize": "SUMMARIZE t",
}

# PostgreSQL has no skewness, kurtosis, histogram, sketches, hash_unit,
# reservoir_sample or tests.
NOT_PG = {"skewness, kurtosis", "histogram", "approx_count_distinct",
          "approx_count_distinct 1000 groups", "approx_percentile", "approx_top_k",
          "hash_unit split", "reservoir_sample", "ks_test", "auc", "psi", "chisq_test",
          "js_divergence", "summarize"}


def start(binary, port):
    d = tempfile.mkdtemp(prefix="nucleus-aggs-")
    s = subprocess.Popen(
        [binary, "server", os.path.join(d, "db"), "--listen", f"127.0.0.1:{port}",
         "--no-auth", "--cache", "1GB"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(600):
        try:
            return s, d, nucleus.connect("127.0.0.1", port, password=None)
        except (OSError, nucleus.OperationalError):
            time.sleep(0.1)
    s.kill()
    raise RuntimeError("the server did not start")


def load(c, rows):
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, g INT, x DOUBLE PRECISION, "
              "y DOUBLE PRECISION)")
    # Deterministic values from the id (no random()): a hash's bits.
    c.execute(f"""INSERT INTO t SELECT i, i % 1000,
        1e9::float8 + ((i::bigint * 2654435761) % 1000003)::float8 / 1000003,
        (((i::bigint * 40503) % 65537) - 32768)::float8
            * power(10::float8, ((i * 7) % 31) - 15)
        FROM generate_series(1, {rows}) AS s(i)""")
    c.execute("ANALYZE t")


def timed(c, sql, times=5):
    t, out = [], None
    for _ in range(times):
        s = time.perf_counter()
        out = c.execute(sql).rows
        t.append((time.perf_counter() - s) * 1000)
    return method.median(t), out


def generate(rows):
    import numpy as np
    i = np.arange(1, rows + 1, dtype=np.int64)
    x = 1e9 + ((i * 2654435761) % 1000003).astype(np.float64) / 1000003
    return i, x


def numpy_times(rows):
    import numpy as np
    i, x = generate(rows)
    out = {}
    for name, f in [("variance", lambda: np.var(x, ddof=1)),
                    ("sum", lambda: np.sum(x)),
                    ("corr", lambda: np.corrcoef(x, i)[0, 1]),
                    ("median", lambda: np.median(x)),
                    ("percentile_disc", lambda: np.percentile(x, 90, method="inverted_cdf"))]:
        t = []
        for _ in range(5):
            s = time.perf_counter()
            r = f()
            t.append((time.perf_counter() - s) * 1000)
        out[name] = (method.median(t), r)
    return out


def python_times(rows):
    """scipy's and scikit-learn's tests on the same halves, the values
    already in numpy arrays."""
    import numpy as np
    from scipy.stats import ks_2samp
    from sklearn.metrics import roc_auc_score
    i, x = generate(rows)
    y = (((i * 40503) % 65537) - 32768).astype(np.float64) * np.power(10.0, ((i * 7) % 31) - 15)
    side = i % 2 == 0
    out = {}
    for name, f in [("ks_test", lambda: ks_2samp(x[side], x[~side]).statistic),
                    ("auc", lambda: roc_auc_score(side, y))]:
        t = []
        for _ in range(5):
            s = time.perf_counter()
            r = f()
            t.append((time.perf_counter() - s) * 1000)
        out[name] = (method.median(t), r)
    return out


def psql_times(cmd, rows, names, work_mem=None):
    """Each query's median of five through psql's \\timing."""
    import re
    setup = f"""DROP TABLE IF EXISTS t;
CREATE TABLE t (id BIGINT PRIMARY KEY, g INT, x DOUBLE PRECISION, y DOUBLE PRECISION);
INSERT INTO t SELECT i, i % 1000,
    1e9::float8 + ((i::bigint * 2654435761) % 1000003)::float8 / 1000003,
    (((i::bigint * 40503) % 65537) - 32768)::float8 * power(10::float8, ((i * 7) % 31) - 15)
    FROM generate_series(1, {rows}) AS s(i);
ANALYZE t;
"""
    subprocess.run(shlex.split(cmd) + ["-q", "-v", "ON_ERROR_STOP=1"], input=setup,
                   text=True, check=True, capture_output=True)
    out = {}
    for n in names:
        if n in NOT_PG:
            continue
        script = (f"SET work_mem = '{work_mem}';\n" if work_mem else "") + \
            "\\timing on\n" + "\n".join([QUERIES[n] + ";"] * 5) + "\n"
        r = subprocess.run(shlex.split(cmd) + ["-At"], input=script, text=True,
                           check=True, capture_output=True).stdout
        ms = [float(m) for m in re.findall(r"Time: ([0-9.]+) ms", r)]
        rows = [l for l in r.splitlines() if l and not l.startswith(("Time", "Timing"))]
        out[n] = (method.median(ms), rows[:3])
    return out


def main():
    a = argparse.ArgumentParser()
    a.add_argument("binaries", nargs="+")
    a.add_argument("--rows", type=int, default=10_000_000)
    a.add_argument("--rounds", type=int, default=1)
    a.add_argument("--query", action="append")
    a.add_argument("--port", type=int, default=21090)
    a.add_argument("--pg", action="store_true", help="PostgreSQL native (bench/pg.py's cluster)")
    a.add_argument("--psql", help="a running PostgreSQL's psql command instead")
    a.add_argument("--numpy", action="store_true")
    a.add_argument("--python", action="store_true", help="scipy's and scikit-learn's tests")
    a.add_argument("--work-mem", default=None, help="work_mem in every system's session")
    o = a.parse_args()
    names = o.query or list(QUERIES)
    suffix = f", work_mem {o.work_mem}" if o.work_mem else ""
    seen = {}
    for r in range(o.rounds):
        for k, b in enumerate(method.turn(o.binaries, r)):
            label = "nucleus" + (f" {o.binaries.index(b) + 1}" if len(o.binaries) > 1 else "") + suffix
            s, d, c = start(b, o.port)
            try:
                load(c, o.rows)
                if o.work_mem:
                    c.execute(f"SET work_mem = '{o.work_mem}'")
                print(f"== {b} round {r + 1}", flush=True)
                for n in names:
                    try:
                        ms, out = timed(c, QUERIES[n])
                    except nucleus.Error as e:
                        print(f"{n:<20}  (not run: {e})", flush=True)
                        continue
                    note = ""
                    if n in seen and seen[n] != out:
                        note = f"  DIFFERENT: {out[:2]}"
                    seen.setdefault(n, out)
                    print(f"{n:<20} {ms:9.1f} ms{note}", flush=True)
                    method.emit(n, ms, system=label)
            finally:
                c.close()
                s.terminate()
                s.wait()
                shutil.rmtree(d)
    for n in names:
        if n in seen:
            print(f"{n}: {seen[n][:3]}")
    if o.pg or o.psql:
        print("== PostgreSQL", flush=True)
        cluster = None
        cmd = o.psql
        if o.pg:
            import pg
            cluster = pg.Cluster(pg.DEFAULT).__enter__()
        try:
            if cluster:
                times = psql_times_cluster(cluster, o.rows, names, o.work_mem)
                method.emit_system("postgresql" + suffix, cluster.describe())
            else:
                times = psql_times(cmd, o.rows, names, o.work_mem)
            for n, (ms, out) in times.items():
                print(f"{n:<20} {ms:9.1f} ms  {out[:2]}")
                method.emit(n, ms, system="postgresql" + suffix)
        finally:
            if cluster:
                cluster.sql("DROP TABLE IF EXISTS t")
                cluster.stop()
    if o.numpy:
        print("== numpy (one core)", flush=True)
        for n, (ms, r) in numpy_times(o.rows).items():
            print(f"{n:<20} {ms:9.1f} ms  {r!r}")
            method.emit(n, ms, system="numpy")
    if o.python:
        import scipy
        import sklearn
        print("== scipy, scikit-learn", flush=True)
        method.emit_system("python", {"version": f"scipy {scipy.__version__}, scikit-learn {sklearn.__version__}"})
        for n, (ms, r) in python_times(o.rows).items():
            print(f"{n:<20} {ms:9.1f} ms  {r!r}")
            method.emit(n, ms, system="python")


def psql_times_cluster(cluster, rows, names, work_mem):
    """psql_times on bench/pg.py's cluster."""
    return psql_times(shlex.join(cluster.tool("psql")), rows, names, work_mem)


if __name__ == "__main__":
    main()
