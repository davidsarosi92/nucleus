#!/usr/bin/env python3
"""Statistical aggregates timed (PLAN.md §M14.27): each build given runs the
same queries over the same table, alternating builds round by round.

    python3 bench/stats/aggs.py target/base/release/nucleus target/release/nucleus \
        [--rows 10000000] [--rounds 2] [--query NAME …]

The table `t (id BIGINT PRIMARY KEY, g INT, x DOUBLE PRECISION, y DOUBLE
PRECISION)`: `x` uniform on a large offset (1e9 + U(0,1), as timestamps
and ids are), `y` spread over magnitudes; `g` 1000 groups.  Each query's
best of five runs, in ms; its result printed once, so builds can be
compared for the same answer.
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "drivers", "python"))

import nucleus  # noqa: E402

QUERIES = {
    "sum": "SELECT sum(x) FROM t",
    "avg spread": "SELECT avg(y) FROM t",
    "sum 1000 groups": "SELECT g, sum(x) FROM t GROUP BY g ORDER BY g LIMIT 3",
    "avg 1M groups": "SELECT sum(s) FROM (SELECT id % 1000000 AS k, avg(y) AS s FROM t GROUP BY k) q",
}


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


def best(c, sql, times=5):
    t, out = [], None
    for _ in range(times):
        s = time.perf_counter()
        out = c.execute(sql).rows
        t.append((time.perf_counter() - s) * 1000)
    return min(t), out


def main():
    a = argparse.ArgumentParser()
    a.add_argument("binaries", nargs="+")
    a.add_argument("--rows", type=int, default=10_000_000)
    a.add_argument("--rounds", type=int, default=2)
    a.add_argument("--query", action="append")
    a.add_argument("--port", type=int, default=21090)
    o = a.parse_args()
    names = o.query or list(QUERIES)
    seen = {}
    for r in range(o.rounds):
        for b in o.binaries:
            s, d, c = start(b, o.port)
            try:
                load(c, o.rows)
                print(f"== {b} round {r + 1}", flush=True)
                for n in names:
                    ms, out = best(c, QUERIES[n])
                    note = ""
                    if n in seen and seen[n] != out:
                        note = f"  DIFFERENT: {out[:2]}"
                    seen.setdefault(n, out)
                    print(f"{n:<20} {ms:9.1f} ms{note}", flush=True)
            finally:
                c.close()
                s.terminate()
                s.wait()
                shutil.rmtree(d)
    for n in names:
        print(f"{n}: {seen[n][:3]}")


if __name__ == "__main__":
    main()
