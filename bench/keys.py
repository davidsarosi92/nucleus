#!/usr/bin/env python3
"""A new data key measured (PLAN.md §M14.30 K3f): `nucleus keys <dir>
rotate data` on an encrypted database, with its process's start, on real
files.

    python3 bench/keys.py --nucleus target/release/nucleus [--rows 1000000]

A table of --rows rows of about 210 bytes (`nucleus sql --encrypt-data
yes`), then every page sealed again under a new data key in place (256
pages a batch: each batch's old bytes journaled and synced, then the
pages written and synced).  The rows read back the same afterwards, and
the keyring holds no previous data key (checked).  The figures are
printed as `@metric` lines too, for `bench/method.py run`, which repeats
the whole script (PLAN.md §M14.31).
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import method  # noqa: E402


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    a.add_argument("--rows", type=int, default=1_000_000)
    args = a.parse_args()
    binary = os.path.abspath(args.nucleus)

    def nucleus(*cmd, script=None):
        p = subprocess.run([binary, *cmd], input=script, capture_output=True, text=True)
        if p.returncode != 0:
            raise SystemExit(f"nucleus {' '.join(cmd)}: {p.stdout}{p.stderr}")
        return p.stdout

    method.emit_system("nucleus", {"binary": binary, "version": nucleus("--version").splitlines()[0]})
    work = tempfile.mkdtemp(prefix="nucleus-keys-bench-")
    try:
        db = os.path.join(work, "db")
        nucleus("sql", db, "--encrypt-data", "yes", script=(
            "CREATE TABLE t (id INT PRIMARY KEY, k INT, v TEXT);\n"
            f"INSERT INTO t SELECT i, i::bigint * 7919 % 1000003, 'a row of about two hundred bytes, number ' || i "
            f"|| ', ' || repeat('of words and more words ', 7) || (i * 31 % 9973) "
            f"FROM generate_series(1, {args.rows}) i;\n"))
        data = sum(os.path.getsize(os.path.join(db, f)) for f in os.listdir(db)
                   if os.path.isfile(os.path.join(db, f)))
        want = nucleus("sql", db, "-c", "SELECT count(*), sum(length(v)) FROM t")
        s = time.perf_counter()
        out = nucleus("keys", db, "rotate", "data")
        secs = time.perf_counter() - s
        assert "sealed again under it" in out, out
        with open(db + ".keyring") as f:
            assert "prev data " not in f.read(), "a previous data key left in the keyring"
        got = nucleus("sql", db, "-c", "SELECT count(*), sum(length(v)) FROM t")
        assert got == want, f"{got} against {want}"
        print(f"{args.rows:,} rows, {data / 1e6:.0f} MB of database: rotate data {secs:.2f} s "
              f"({data / 1e6 / secs:.0f} MB/s)", flush=True)
        method.emit("rotate data", secs, unit="s", phase="write")
        method.emit("database", data / 1e6, unit="MB", phase="write")
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
