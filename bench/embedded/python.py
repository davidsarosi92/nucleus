#!/usr/bin/env python3
"""The embedded edition from Python (PLAN.md §M14.23): `SELECT 1` on a
database in this process against the same over loopback TCP to `nucleus
server` (the median of 20,000 after 1,000 untimed), and a million rows of
two columns (`int`, `float8`) into numpy in this process, from a table of
rows and from a column table (the median of five).  Each
figure is printed as an `@metric` line too, for `bench/method.py run`,
which repeats the whole script (PLAN.md §M14.31).

    cargo build --release -p nucleus-embedded -p nucleus-cli
    python3 bench/embedded/python.py --nucleus target/release/nucleus
        [--lib target/release/libnucleus_embedded.dylib]
"""

import argparse
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "drivers", "python"))
sys.path.insert(0, os.path.join(ROOT, "bench"))

import method  # noqa: E402


def round_trips(c, n=20_000, warm=1_000):
    t = []
    for i in range(warm + n):
        s = time.perf_counter()
        c.execute("SELECT 1")
        if i >= warm:
            t.append(time.perf_counter() - s)
    return method.median(t) * 1e6


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    lib = "libnucleus_embedded." + ("dylib" if sys.platform == "darwin" else "so")
    a.add_argument("--lib", default=os.path.join(ROOT, "target", "release", lib))
    a.add_argument("--rows", type=int, default=1_000_000)
    args = a.parse_args()
    os.environ["NUCLEUS_EMBEDDED_LIB"] = os.path.abspath(args.lib)
    import nucleus

    work = tempfile.mkdtemp(prefix="nucleus-embedded-")
    try:
        c = nucleus.connect("file:" + os.path.join(work, "app.db"))
        here = round_trips(c)
        c.execute("CREATE TABLE t (a int, b float8)")
        c.execute("CREATE TABLE u (a int, b float8) WITH (storage = column)")
        c.execute(f"INSERT INTO t SELECT i, i::float8 * 0.5 FROM generate_series(1, {args.rows}) i")
        c.execute("INSERT INTO u SELECT a, b FROM t")
        numpy_ms = {}
        for table in ("t", "u"):
            t = []
            for _ in range(6):
                s = time.perf_counter()
                got = c.numpy(f"SELECT a, b FROM {table}")
                t.append(time.perf_counter() - s)
                assert len(got["a"]) == args.rows and got["b"].dtype.kind == "f"
            numpy_ms[table] = method.median(t[1:]) * 1e3
        c.close()

        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        server = subprocess.Popen([os.path.abspath(args.nucleus), "server", os.path.join(work, "srv"),
                                   "--listen", f"127.0.0.1:{port}", "--no-auth"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(600):
                try:
                    c = nucleus.connect("127.0.0.1", port, password=None)
                    break
                except (OSError, nucleus.OperationalError):
                    time.sleep(0.1)
            tcp = round_trips(c)
            c.close()
        finally:
            server.terminate()
            server.wait()
    finally:
        shutil.rmtree(work, ignore_errors=True)
    print(f"SELECT 1 in this process {here:.1f} µs, over loopback TCP {tcp:.1f} µs; "
          f"{args.rows:,} rows of two columns into numpy {numpy_ms['t']:.1f} ms "
          f"({numpy_ms['u']:.1f} from a column table)", flush=True)
    method.emit("SELECT 1, in the process", here, unit="µs")
    method.emit("SELECT 1, loopback TCP to nucleus server", tcp, unit="µs")
    method.emit(f"{args.rows:,} rows of two columns into numpy, in the process", numpy_ms["t"])
    method.emit(f"{args.rows:,} rows of two columns into numpy, in the process, a column table", numpy_ms["u"])
    method.emit_system("nucleus", {"python": sys.version.split()[0], "library": os.path.abspath(args.lib)})


if __name__ == "__main__":
    main()
