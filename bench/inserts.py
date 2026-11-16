#!/usr/bin/env python3
"""One-row `INSERT`s from several sessions at once (PLAN.md §M14.14):
commits a second through `nucleus server`, each its own transaction, the
sessions' commits grouped by the server.

    python3 bench/inserts.py --nucleus target/release/nucleus
        [--sessions 8] [--rows 20000] [--history 1h]

The server runs on a fresh database in a temporary directory (with
--history, every commit's time recorded and kept that long: the cost of
time travel).  Each session, a thread with its own connection, inserts
--rows rows one statement at a time; the rate is all the rows over the
time from the first to the last.  Printed as an `@metric` line too, for
`bench/method.py run`, which repeats the whole script — with `--also`,
the history on and off in turn (PLAN.md §M14.31).
"""

import argparse
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "drivers", "python"))
sys.path.insert(0, HERE)

import method  # noqa: E402
import nucleus  # noqa: E402


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    a.add_argument("--sessions", type=int, default=8)
    a.add_argument("--rows", type=int, default=20_000, help="rows each session inserts")
    a.add_argument("--history", default=None, help="the server's --history (e.g. 1h)")
    args = a.parse_args()
    binary = os.path.abspath(args.nucleus)
    work = tempfile.mkdtemp(prefix="nucleus-inserts-")
    port = free_port()
    cmd = [binary, "server", os.path.join(work, "db"), "--listen", f"127.0.0.1:{port}", "--no-auth"]
    if args.history:
        cmd += ["--history", args.history]
    server = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(600):
            try:
                c = nucleus.connect("127.0.0.1", port, password=None)
                break
            except (OSError, nucleus.OperationalError):
                time.sleep(0.1)
        else:
            raise SystemExit("the server did not start")
        c.execute("CREATE TABLE t (id INT PRIMARY KEY, session INT, v TEXT)")
        conns = [nucleus.connect("127.0.0.1", port, password=None) for _ in range(args.sessions)]
        start = threading.Barrier(args.sessions + 1)

        def session(k):
            db = conns[k]
            start.wait()
            for i in range(args.rows):
                db.execute("INSERT INTO t VALUES ($1, $2, $3)", (k * args.rows + i, k, f"row {i} of {k}"))

        threads = [threading.Thread(target=session, args=(k,)) for k in range(args.sessions)]
        for t in threads:
            t.start()
        start.wait()
        s = time.perf_counter()
        for t in threads:
            t.join()
        secs = time.perf_counter() - s
        n = c.execute("SELECT count(*) FROM t").rows[0][0]
        assert n == args.sessions * args.rows, n
        rate = n / secs
        how = f"history {args.history}" if args.history else "no history"
        print(f"{args.sessions} sessions, {n:,} one-row INSERTs ({how}): {rate:,.0f} /s", flush=True)
        method.emit(f"one-row INSERTs, {args.sessions} sessions", rate, unit="/s", phase="write")
        method.emit_system("nucleus", {"binary": binary, "settings": {"history": args.history or "none"}})
        for db in conns + [c]:
            db.close()
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
