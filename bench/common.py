"""What the analytical benchmarks share (bench/tpch.py, bench/clickbench.py):
the two systems driven through their own clients, each query's best time
from the client's `\\timing`, results compared between the systems, and
the Markdown report."""

import math
import os
import re
import shlex
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "drivers", "python"))


def one_line(sql):
    return " ".join(sql.split())


def times_of(text):
    return [float(m) for m in re.findall(r"^Time: ([0-9.]+) ms", text, re.M)]


def run(cmd, script, stdin_file=None, timeout=None):
    if stdin_file is not None:
        with open(stdin_file, "rb") as f:
            out = subprocess.run(cmd, stdin=f, capture_output=True, timeout=timeout)
        text = (out.stdout + out.stderr).decode(errors="replace")
    else:
        out = subprocess.run(cmd, input=script, capture_output=True, text=True, timeout=timeout)
        text = out.stdout + out.stderr
    if out.returncode != 0 or "ERROR" in text:
        raise RuntimeError(f"{cmd[0]} failed:\n{text[-3000:]}")
    return text


def normalize(cell):
    """A result value as compared: numbers to cents, text trimmed."""
    if cell is None:
        return None
    s = str(cell).strip()
    if re.fullmatch(r"-?\d+(\.\d+)?(e[+-]?\d+)?", s):
        x = float(s)
        return f"{round(x + 0.0, 2):.2f}" if math.isfinite(x) else s
    return s


def same(a, b):
    if a == b:
        return "same"
    if sorted(a, key=repr) == sorted(b, key=repr):
        return "same rows (ties ordered differently)"
    return "DIFFERENT"


class Nucleus:
    """`nucleus server` on a fresh directory, `nucleus sql -h` for timing,
    the Python driver for results."""

    def __init__(self, binary, name, work):
        self.name = name
        self.db = os.path.join(work, f"db-{name.replace(' ', '-')}")
        self.server = subprocess.Popen([binary, "server", self.db, "--listen", "127.0.0.1:0", "--no-auth"],
                                       stderr=subprocess.PIPE, text=True)
        line = self.server.stderr.readline()
        m = re.search(r"127\.0\.0\.1:(\d+)", line)
        if not m:
            raise RuntimeError(f"nucleus server did not start: {line}")
        self.port = int(m.group(1))
        self.client = [binary, "sql", "-h", f"127.0.0.1:{self.port}"]

    def script(self, lines, timeout=None):
        return run(self.client, "\\timing\n" + "".join(one_line(s) + ";\n" for s in lines), timeout=timeout)

    def copy(self, table, path, options=""):
        run(self.client, f"\\copy {table} FROM '{path}'{options}\n")

    def results(self, sql):
        import nucleus
        with nucleus.connect(port=self.port, user="nucleus", password="nucleus") as c:
            return [[normalize(v) for v in r] for r in c.query(sql, text=True)[0].rows]

    def close(self):
        self.server.terminate()
        self.server.wait()


class Postgres:
    """PostgreSQL through `psql` (the command given, e.g. with
    `docker exec -i`)."""

    name = "postgresql"

    def __init__(self, psql):
        self.psql = shlex.split(psql) + ["-X", "-q", "-v", "ON_ERROR_STOP=1"]

    def script(self, lines, timeout=None):
        return run(self.psql, "\\timing on\n" + "".join(one_line(s) + ";\n" for s in lines), timeout=timeout)

    def copy(self, table, path, options=""):
        run(self.psql + ["-c", f"COPY {table} FROM STDIN{options}"], None, stdin_file=path)

    def results(self, sql):
        out = run(self.psql + ["-A", "-t", "-F", "\x1f", "-P", "null=\\N", "-c", one_line(sql)], None)
        return [[None if v == "\\N" else normalize(v) for v in line.split("\x1f")]
                for line in out.split("\n") if line]


def measure(system, queries, runs, timeout):
    """Per query: the best of `runs` times (ms), or None past the timeout
    (the statement's own, `statement_timeout`, so the server stops it) or
    on an error (printed)."""
    out = {}
    limit = f"SET statement_timeout = {int(timeout * 1000)}"
    for q, sql in queries.items():
        try:
            t = times_of(system.script([limit] + [sql] * runs, timeout=timeout * runs + 60))[1:]
            out[q] = min(t) if len(t) == runs else None
        except subprocess.TimeoutExpired:
            out[q] = None
        except RuntimeError as e:
            print(f"  {system.name} {q}: {str(e)[-300:]}", file=sys.stderr)
            out[q] = None
        print(f"  {system.name} {q}: {out[q]}", file=sys.stderr, flush=True)
    return out


def report(title, queries, systems, times, results, loads, load_label, exact=lambda q: True):
    """The Markdown table; per query the comparison of each nucleus run's
    results with PostgreSQL's (`exact(q)`: whether they must agree — not
    where a LIMIT may cut ties differently)."""

    def fmt(t):
        return "—" if t is None else f"{t:,.1f}"

    pg = "postgresql" in systems
    ns = [s for s in systems if s != "postgresql"]
    head = f"| {title} | " + " | ".join(f"{s} (ms)" for s in systems)
    rule = "|---|" + "---:|" * len(systems)
    if pg:
        head += " | " + " | ".join(f"{s} / PostgreSQL" for s in ns) + " | results"
        rule += "---:|" * len(ns) + "---|"
    lines = [head + " |", rule]
    checks = {}
    for q in queries:
        line = f"| {q} | " + " | ".join(fmt(times[s][q]) for s in systems)
        if pg:
            ratios = []
            for s in ns:
                t, p = times[s][q], times["postgresql"][q]
                ratios.append("—" if t is None or p is None or p == 0 else f"{t / p:.2f}×")
            verdicts = set()
            for s in ns:
                if results[s].get(q) is not None and results["postgresql"].get(q) is not None:
                    v = same(results[s][q], results["postgresql"][q])
                    if v == "DIFFERENT" and not exact(q):
                        v = "differs (ties at the LIMIT)"
                    verdicts.add(v)
            checks[q] = ", ".join(sorted(verdicts)) or "—"
            line += " | " + " | ".join(ratios) + f" | {checks[q]}"
        lines.append(line + " |")
    done = [q for q in queries if all(times[s][q] for s in systems)]
    if done:
        geo = {s: math.exp(sum(math.log(times[s][q]) for q in done) / len(done)) for s in systems}
        line = f"| geometric mean ({len(done)} queries) | " + " | ".join(fmt(geo[s]) for s in systems)
        if pg:
            line += " | " + " | ".join(f"{geo[s] / geo['postgresql']:.2f}×" for s in ns) + " | "
        lines.append(line + " |")
    lines.append(f"| {load_label} | " + " | ".join(f"{loads[s]:.1f}" for s in systems)
                 + (" | " * (len(ns) + 1) if pg else "") + " |")
    return "\n".join(lines), checks
