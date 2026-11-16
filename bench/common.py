"""What the analytical benchmarks share (bench/tpch.py, bench/clickbench.py):
the two systems driven through their own clients, each query's best time
from the client's `\\timing`, results compared between the systems, and
the Markdown report."""

import math
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "..", "drivers", "python"))
import method  # noqa: E402


# The servers' address: NUCLEUS_BENCH_HOST=::1 where IPv4 loopback's
# ports run out (macOS past ~49 days up keeps closed connections).
HOST = os.environ.get("NUCLEUS_BENCH_HOST", "127.0.0.1")


def address(port):
    return f"[{HOST}]:{port}" if ":" in HOST else f"{HOST}:{port}"


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


class Session:
    """A client kept open (`psql`, `nucleus sql -h`): statements one at a
    time, each one's time from the client's own `\\timing` — the
    connection made once, outside every measurement.  After each
    statement an `\\echo` marker says its output is complete."""

    def __init__(self, cmd, env=None):
        self.cmd = cmd
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT, text=True, bufsize=1, env=env)
        self.n = 0
        self.send("\\timing on")

    def send(self, line):
        """One line, and its output up to the marker."""
        self.n += 1
        mark = f"@@ done {self.n}"
        try:
            self.p.stdin.write(line + "\n\\echo " + mark + "\n")
            self.p.stdin.flush()
        except BrokenPipeError:
            raise RuntimeError(f"{self.cmd[0]} quit") from None
        out = []
        while True:
            got = self.p.stdout.readline()
            if not got:
                raise RuntimeError(f"{self.cmd[0]} quit:\n{''.join(out)[-3000:]}")
            if got.rstrip("\n") == mark:
                return "".join(out)
            out.append(got)

    def time(self, sql):
        """Runs `sql`; its time in ms.  Raises on an error."""
        text = self.send(one_line(sql).rstrip(";") + ";")
        if re.search(r"^(psql:.*)?ERROR", text, re.M):
            raise RuntimeError(f"{self.cmd[0]}: {text[-3000:]}")
        t = times_of(text)
        if not t:
            raise RuntimeError(f"{self.cmd[0]}: no time in {text[-3000:]!r}")
        return t[-1]

    def close(self):
        try:
            self.p.stdin.close()
        except OSError:
            pass
        try:
            self.p.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.p.kill()
            self.p.wait()


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
        self.server = subprocess.Popen([binary, "server", self.db, "--listen", address(0), "--no-auth"],
                                       stderr=subprocess.PIPE, text=True)
        line = self.server.stderr.readline()
        m = re.search(re.escape(address("")) + r"(\d+)", line)
        if not m:
            raise RuntimeError(f"nucleus server did not start: {line}")
        self.port = int(m.group(1))
        # (`localhost` for IPv6: its addresses are tried in turn, and
        # builds before 2026-11-09 do not take `-h [::1]:port`.)
        self.client = [binary, "sql", "-h", f"localhost:{self.port}" if ":" in HOST else address(self.port)]

    def script(self, lines, timeout=None):
        return run(self.client, "\\timing\n" + "".join(one_line(s) + ";\n" for s in lines), timeout=timeout)

    def copy(self, table, path, options=""):
        run(self.client, f"\\copy {table} FROM '{path}'{options}\n")

    def results(self, sql):
        import nucleus
        with nucleus.connect(HOST, self.port, user="nucleus", password="nucleus") as c:
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


def report(title, samples, load_samples, checks, env, baseline, load_label="load (COPY, indexes, ANALYZE)"):
    """The Markdown: the queries (cold and warm), the geometric means, the
    loads, the answers' checks, the environment."""
    print(method.table(samples, baseline=baseline, title=title))
    for phase in ("warm", "cold"):
        geo, n = geometric_means(samples, phase)
        if geo:
            line = ", ".join(f"{s} {method.fmt(v)} ms" for s, v in geo.items())
            if baseline in geo:
                line += "; " + ", ".join(f"{s} / {baseline} {v / geo[baseline]:.2f}×"
                                         for s, v in geo.items() if s != baseline)
            print(f"\ngeometric mean, {phase} ({n} queries): {line}")
    print()
    print(method.table(load_samples, baseline=baseline, title=load_label))
    odd = {q: c for q, c in checks.items() if c != "same"}
    print(f"\nanswers: {len(checks) - len(odd)} of {len(checks)} the same"
          + ("".join(f"; {q}: {c}" for q, c in odd.items()) if odd else ""))
    print()
    print(method.describe(env))


# ------------------------------------------------ systems, by the method

COLD = "the server restarted before the query (its caches empty; the OS's page cache kept)"


class NucleusSystem:
    """nucleus for the method's runs: `nucleus server` on a directory of
    its own (--embedded: `nucleus sql DIR`, in-process), a session kept
    open, restarted for a cold run, emptied by `fresh`."""

    can_restart = True
    # After a bulk load: the statistics.
    after_load = "ANALYZE"

    def __init__(self, binary, name="nucleus", embedded=False):
        self.binary, self.embedded = binary, embedded
        self.name = name
        self.work = tempfile.mkdtemp(prefix="nucleus-bench-")
        self.server = self.session = None
        self._start()

    def describe(self):
        version = subprocess.run([self.binary, "--version"], capture_output=True, text=True).stdout
        return {"version": version.split("\n")[0].removeprefix("nucleus "),
                "settings": {"server": not self.embedded,
                             "sync": "ordered (the default: fsync at commit, the drive's cache not flushed)"}}

    def _start(self):
        if self.embedded:
            self.client = [self.binary, "sql", os.path.join(self.work, "db")]
        else:
            self.server = Nucleus(self.binary, "db", self.work)
            self.client = self.server.client
        self.session = Session(self.client)

    def fresh(self, tables=()):
        """An empty database."""
        self.stop()
        shutil.rmtree(self.work, ignore_errors=True)
        os.makedirs(self.work)
        self._start()

    def restart(self):
        self.stop()
        self._start()

    def script(self, lines):
        return run(self.client, "".join(one_line(x) + ";\n" for x in lines))

    def copy(self, table, path, options=""):
        run(self.client, f"\\copy {table} FROM '{path}'{options}\n")

    def results(self, sql):
        return None if self.embedded else self.server.results(sql)

    def stop(self):
        if self.session:
            self.session.close()
        if self.server:
            self.server.close()
        self.server = self.session = None

    def close(self):
        self.stop()
        shutil.rmtree(self.work, ignore_errors=True)


class PostgresSystem:
    """PostgreSQL for the method's runs: a cluster of its own under `data`
    (bench/pg.py: native, tuned for the machine, restartable), or one
    already running reached through `psql` (no cold runs: it cannot be
    restarted)."""

    name = "postgresql"
    # After a bulk load, as its documentation has it ("Populating a
    # Database"): the statistics, and the visibility map and hint bits set
    # once, not by the first queries.
    after_load = "VACUUM ANALYZE"

    def __init__(self, data=None, psql=None):
        self.cluster = None
        if psql is None:
            import pg
            self.cluster = pg.Cluster(data).__enter__()
            psql = self.cluster.psql
        self.psql = psql
        self.can_restart = self.cluster is not None
        self.tables = ()
        self.session = Session(shlex.split(self.psql) + ["-X", "-q"])

    def describe(self):
        if self.cluster:
            return self.cluster.describe()
        version = subprocess.run(shlex.split(self.psql) + ["-X", "-A", "-t", "-c", "SHOW server_version"],
                                 capture_output=True, text=True).stdout.strip()
        return {"version": version, "settings": "as the server was started", "native": None}

    def fresh(self, tables=()):
        """The benchmark's tables dropped (`tables`, kept to drop at the
        end), a checkpoint, a new session."""
        self.tables = tuple(tables)
        self.session.close()
        if self.tables:
            Postgres(self.psql).script(["DROP TABLE IF EXISTS " + ", ".join(self.tables)])
        Postgres(self.psql).script(["CHECKPOINT"])
        self.session = Session(shlex.split(self.psql) + ["-X", "-q"])

    def restart(self):
        self.session.close()
        if self.cluster:
            self.cluster.restart()
            self.psql = self.cluster.psql
        self.session = Session(shlex.split(self.psql) + ["-X", "-q"])

    def script(self, lines):
        return Postgres(self.psql).script(lines)

    def copy(self, table, path, options=""):
        Postgres(self.psql).copy(table, path, options)

    def results(self, sql):
        return Postgres(self.psql).results(sql)

    def close(self):
        self.session.close()
        try:
            if self.tables:
                Postgres(self.psql).script(["DROP TABLE IF EXISTS " + ", ".join(self.tables)])
        finally:
            if self.cluster:
                self.cluster.stop()


def loads(systems, load, runs=method.RUNS):
    """`load(system)` on an empty database, `runs` rounds with the systems
    in turn; each one's seconds as a sample ("load", unit s).  The last
    round's data stays."""
    times = {s.name: [] for s in systems}
    for r in range(runs):
        for s in systems:
            started = time.time()
            load(s)
            times[s.name].append(time.time() - started)
            print(f"  {s.name} load {r + 1}/{runs}: {times[s.name][-1]:.1f} s", file=sys.stderr, flush=True)
    return [method.Sample("load", n, t, "write", flagged=method.cv(t) > method.CV_BOUND, unit="s")
            for n, t in times.items()]


def queries_by_method(systems, queries, runs=method.RUNS, timeout=600, retries=method.RETRIES):
    """Each query `runs` rounds, the systems in turn (A, B, A, B, …): each
    restarted, then the query cold and warm in a session under a
    statement timeout of `timeout` s; the rounds again (at most `retries`
    times) while one spreads past the bound.  A query that fails or times
    out on a system is not run there again.  Returns the samples and each
    system's answer to each query (None where it failed)."""
    samples, answers = [], {s.name: {} for s in systems}
    limit = f"SET statement_timeout = {int(timeout * 1000)}"
    for q, sql in queries.items():
        failed = set()
        for attempt in range(1, retries + 2):
            got = {}
            for r in range(runs):
                for s in systems:
                    if s.name in failed:
                        continue
                    try:
                        if s.can_restart:
                            s.restart()
                        s.session.time(limit)
                        if s.can_restart:
                            got.setdefault((s.name, "cold"), []).append(s.session.time(sql))
                        got.setdefault((s.name, "warm"), []).append(s.session.time(sql))
                    except RuntimeError as e:
                        print(f"  {s.name} {q}: {str(e)[-300:]}", file=sys.stderr)
                        failed.add(s.name)
                        s.restart() if s.can_restart else None
            worst = max((method.cv(t) for k, t in got.items() if k[0] not in failed), default=0)
            if worst <= method.CV_BOUND or attempt > retries:
                break
        for (name, phase), t in got.items():
            if name not in failed:
                samples.append(method.Sample(q, name, t, phase, flagged=method.cv(t) > method.CV_BOUND,
                                             attempts=attempt))
        for s in systems:
            answers[s.name][q] = None if s.name in failed else s.results(sql)
        line = ", ".join(f"{n} {method.median(t):.1f}" for (n, p), t in got.items() if p == "warm" and n not in failed)
        print(f"  {q}: {line or 'failed'}", file=sys.stderr, flush=True)
    return samples, answers


def check_answers(answers, queries, exact=lambda q: True, reference="postgresql"):
    """Per query, each other system's answer against the reference's."""
    ref = answers.get(reference)
    out = {}
    if ref is None:
        return out
    for q in queries:
        verdicts = set()
        for name, got in answers.items():
            if name == reference or got.get(q) is None or ref.get(q) is None:
                continue
            v = same(got[q], ref[q])
            if v == "DIFFERENT" and not exact(q):
                v = "differs (ties at the LIMIT)"
            verdicts.add(v)
        out[q] = ", ".join(sorted(verdicts)) or "—"
    return out


def geometric_means(samples, phase="warm"):
    """Per system, the geometric mean of its medians over the queries every
    system finished."""
    by = {}
    for s in samples:
        if s.phase == phase:
            by.setdefault(s.name, {})[s.system] = s.median
    systems = {s.system for s in samples}
    done = [q for q, t in by.items() if set(t) == systems and all(v > 0 for v in t.values())]
    if not done:
        return {}, 0
    return {sy: math.exp(sum(math.log(by[q][sy]) for q in done) / len(done)) for sy in systems}, len(done)
