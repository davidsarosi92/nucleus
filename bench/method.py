#!/usr/bin/env python3
"""The method every published number goes through (PLAN.md §M14.31 P1).

A benchmark measures through this module, and its numbers carry:

- *the environment*: the machine (CPU model, cores, memory, the disk the
  data is on), the OS and kernel, the power state (macOS: low-power mode,
  on battery; Linux: the frequency governor, turbo, SMT, NUMA nodes), the
  load around the run, the version and settings of every system measured,
  the data's checksums, and nucleus's commit;
- *its runs*: n of them (≥ 5) per measurement, cold and warm apart;
  reported as the median with a bootstrap confidence interval (95 %, of
  the median) and the coefficient of variation — a measurement whose CV
  passes a bound is run again, and still past it is flagged, never shown
  as a point;
- *comparisons interleaved*: systems run in turn, counterbalanced (AB BA…), on one machine
  in one session; a difference claimed only when the two intervals do not
  overlap;
- *checked answers*: a fast wrong answer is no result;
- *raw results*: every time written to a JSON file beside the summary.

    python3 bench/method.py env                     # this machine, as recorded
    python3 bench/method.py show results.json       # a results file as a table
    python3 bench/method.py reproduce a.json b.json # does b reproduce a?

Prior art: Raasveldt et al., "Fair Benchmarking Considered Difficult"
(DBTest 2018); Georges, Buytaert, Eeckhout, "Statistically Rigorous Java
Performance Evaluation" (OOPSLA 2007); Mytkowicz et al., "Producing Wrong
Data Without Doing Anything Obviously Wrong!" (ASPLOS 2009); Uta et al.,
"Is Big Data Performance Reproducible in Modern Cloud Networks?" (NSDI
2020).  No dependencies.
"""

import datetime as _dt
import hashlib
import json
import math
import os
import platform
import random
import re
import shlex
import subprocess
import sys
import time

METHOD = 2
# Runs per measurement unless asked for more; the plan's floor.
RUNS = 5
# A measurement whose coefficient of variation passes this is run again.
CV_BOUND = 0.10
# Times it is run again before it is flagged.
RETRIES = 2
# A command whose rounds took longer than this (seconds, all of an
# attempt's) is not run again: its noisy metrics are flagged instead.  The
# spread a retry can cure is a passing disturbance; a metric whose own
# spread passes the bound keeps it however many rounds are added, and a
# long command's retry costs hours (bm25-filters: 27 minutes a round).
RETRY_BUDGET = 600
BOOTSTRAP = 10_000
LEVEL = 0.95
# Between two sessions on one machine the medians moved 3–5 % (90th
# percentile 8–13 %, 2026-11-17: 500 measurements run twice) — more than
# one session's interval holds, as Kalibera and Jones found ("Rigorous
# benchmarking in reasonable time", 2013).  A difference within this is
# not claimed, and a second run within it reproduces.
NOISE = 0.10
# A round starts when the machine's other work is under this (cores, the
# CPU of every process but the measurement's own, its servers' and the
# kernel's), waiting at most QUIET_WAIT seconds; what it saw is recorded.
QUIET_CORES = 1.5
QUIET_WAIT = 1800

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


# ------------------------------------------------------------- statistics

def median(xs):
    s = sorted(xs)
    n = len(s)
    if n == 0:
        raise ValueError("no runs")
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2


def cv(xs):
    """The coefficient of variation: the sample standard deviation over
    the mean (0 for fewer than two runs)."""
    if len(xs) < 2:
        return 0.0
    m = sum(xs) / len(xs)
    if m == 0:
        return 0.0
    var = sum((x - m) ** 2 for x in xs) / (len(xs) - 1)
    return math.sqrt(var) / m


def bootstrap_ci(xs, level=LEVEL, resamples=BOOTSTRAP, seed=0):
    """A percentile bootstrap interval of the median: `resamples` draws
    with replacement, their medians' `level` middle.  Seeded, so a file's
    interval is the same each time it is computed."""
    rng = random.Random(seed)
    n = len(xs)
    if n == 1:
        return (xs[0], xs[0])
    meds = sorted(median([xs[rng.randrange(n)] for _ in range(n)]) for _ in range(resamples))
    lo = meds[int(math.floor((1 - level) / 2 * resamples))]
    hi = meds[min(resamples - 1, int(math.ceil((1 + level) / 2 * resamples)) - 1)]
    return (lo, hi)


class Sample:
    """One measurement's runs: what was measured, on which system, cold or
    warm, and its times (ms)."""

    def __init__(self, name, system, times, phase="warm", flagged=False, attempts=1, unit="ms"):
        self.name, self.system, self.phase = name, system, phase
        self.times = list(times)
        self.flagged, self.attempts, self.unit = flagged, attempts, unit

    @property
    def median(self):
        return median(self.times)

    @property
    def ci(self):
        return bootstrap_ci(self.times)

    @property
    def cv(self):
        return cv(self.times)

    def to_json(self):
        lo, hi = self.ci
        return {
            "name": self.name, "system": self.system, "phase": self.phase, "unit": self.unit,
            "times": self.times, "median": self.median, "ci": [lo, hi], "cv": self.cv,
            "flagged": self.flagged, "attempts": self.attempts,
        }

    @classmethod
    def from_json(cls, d):
        return cls(d["name"], d["system"], d["times"], d.get("phase", "warm"), d.get("flagged", False),
                   d.get("attempts", 1), d.get("unit", "ms"))


def disjoint(a, b):
    """Whether two samples' intervals do not overlap."""
    (alo, ahi), (blo, bhi) = a.ci, b.ci
    return ahi < blo or bhi < alo


def differ(a, b):
    """Whether a difference between two samples is claimed: their
    intervals apart and their medians more than NOISE apart."""
    lo, hi = sorted((a.median, b.median))
    return disjoint(a, b) and (lo <= 0 or hi / lo > 1 + NOISE)


# ----------------------------------------------------------------- running

def repeat(measure, name, system, runs=RUNS, phase="warm", bound=CV_BOUND, retries=RETRIES):
    """`measure()` run `runs` times — it returns one run's time in ms, as
    the system's own client reports it — again while the CV passes
    `bound` (at most `retries` times), flagged if it still does."""
    attempts = 0
    while True:
        attempts += 1
        times = [measure() for _ in range(runs)]
        if cv(times) <= bound or attempts > retries:
            return Sample(name, system, times, phase, flagged=cv(times) > bound, attempts=attempts)


def turn(systems, r):
    """The systems' order in round `r`: A B, then B A, … (counterbalanced:
    neither always first — the first of a round runs on a machine the
    last left, and a fixed order shows as a difference)."""
    return list(systems) if r % 2 == 0 else list(reversed(systems))


def interleave(measures, name, runs=RUNS, phase="warm", bound=CV_BOUND, retries=RETRIES):
    """Each system's `measure()` in turn (A, B, B, A, …), so a drift of the
    machine falls on all of them alike; a sample per system, each run
    again (all of them, interleaved) while one's CV passes `bound`."""
    attempts = 0
    while True:
        attempts += 1
        times = {system: [] for system in measures}
        for r in range(runs):
            for system, measure in turn(list(measures.items()), r):
                times[system].append(measure())
        worst = max(cv(t) for t in times.values())
        if worst <= bound or attempts > retries:
            return {s: Sample(name, s, t, phase, flagged=cv(t) > bound, attempts=attempts)
                    for s, t in times.items()}


def check(name, got, want, normalize=None):
    """Raises unless `got` equals `want` (rows, compared after
    `normalize` of each cell): a fast wrong answer is no result."""
    norm = (lambda rows: [[normalize(c) for c in r] for r in rows]) if normalize else (lambda rows: rows)
    if norm(got) != norm(want):
        raise AssertionError(f"{name}: wrong answer\n got  {str(got)[:500]}\n want {str(want)[:500]}")


# ------------------------------------------------------------- environment

def _run(cmd):
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
        return out.stdout.strip() if out.returncode == 0 else None
    except (OSError, subprocess.SubprocessError):
        return None


def _read(path):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return None


def _sysctl(name):
    return _run(["sysctl", "-n", name])


def machine():
    """The machine: CPU, cores, memory, OS, kernel, power state."""
    m = {
        "arch": platform.machine(),
        "os": platform.platform(),
        "kernel": platform.release(),
        "python": platform.python_version(),
        "logical_cpus": os.cpu_count(),
    }
    if sys.platform == "darwin":
        m["cpu"] = _sysctl("machdep.cpu.brand_string")
        m["model"] = _sysctl("hw.model")
        m["physical_cpus"] = int(_sysctl("hw.physicalcpu") or 0) or None
        perf = _sysctl("hw.perflevel0.physicalcpu")
        eff = _sysctl("hw.perflevel1.physicalcpu")
        if perf:
            m["performance_cores"], m["efficiency_cores"] = int(perf), int(eff or 0)
        m["memory_bytes"] = int(_sysctl("hw.memsize") or 0) or None
        pm = _run(["pmset", "-g"]) or ""
        low = re.search(r"lowpowermode\s+(\d)", pm)
        m["low_power_mode"] = low and low.group(1) == "1"
        batt = _run(["pmset", "-g", "batt"]) or ""
        m["on_battery"] = "Battery Power" in batt
        therm = _run(["pmset", "-g", "therm"]) or ""
        m["thermal_warning"] = None if not therm else "No thermal warning level" not in therm or \
            "No performance warning level" not in therm
    else:
        info = _read("/proc/cpuinfo") or ""
        model = re.search(r"model name\s*:\s*(.+)", info)
        m["cpu"] = model.group(1) if model else None
        mem = re.search(r"MemTotal:\s*(\d+) kB", _read("/proc/meminfo") or "")
        m["memory_bytes"] = int(mem.group(1)) * 1024 if mem else None
        m["governor"] = _read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        no_turbo = _read("/sys/devices/system/cpu/intel_pstate/no_turbo")
        boost = _read("/sys/devices/system/cpu/cpufreq/boost")
        m["turbo"] = (no_turbo == "0") if no_turbo is not None else (boost == "1" if boost is not None else None)
        m["smt"] = _read("/sys/devices/system/cpu/smt/control")
        nodes = [d for d in os.listdir("/sys/devices/system/node") if re.fullmatch(r"node\d+", d)] \
            if os.path.isdir("/sys/devices/system/node") else []
        m["numa_nodes"] = len(nodes) or None
        dmi = _read("/sys/devices/virtual/dmi/id/product_name")
        m["model"] = dmi
        # A cloud instance's type, where its metadata says it (§M27's runs).
        m["instance_type"] = os.environ.get("BENCH_INSTANCE_TYPE")
    return m


def disk(path):
    """The disk `path` is on: its device and file system, free space."""
    path = os.path.abspath(path)
    while not os.path.exists(path):
        path = os.path.dirname(path)
    st = os.statvfs(path)
    d = {"path": path, "free_bytes": st.f_bavail * st.f_frsize, "total_bytes": st.f_blocks * st.f_frsize}
    df = _run(["df", "-P", path])
    if df:
        d["device"] = df.splitlines()[-1].split()[0]
    if sys.platform == "darwin" and d.get("device"):
        info = _run(["diskutil", "info", d["device"]]) or ""
        for key, label in (("solid_state", "Solid State"), ("protocol", "Protocol"), ("file_system", "Type (Bundle)")):
            mm = re.search(rf"{re.escape(label)}:\s*(.+)", info)
            if mm:
                d[key] = mm.group(1).strip()
        if "solid_state" in d:
            d["solid_state"] = d["solid_state"] == "Yes"
    elif d.get("device"):
        mnt = _read("/proc/mounts") or ""
        for line in mnt.splitlines():
            f = line.split()
            if len(f) > 2 and f[0] == d["device"]:
                d["file_system"] = f[2]
    return d


# Processes that are the measurement's even when not its children: the
# servers it starts (PostgreSQL's postmaster leaves the tree) and the
# kernel's I/O for them.
OURS = re.compile(r"(^|/)(postgres|nucleus|kernel_task)$")
QUIET_LOG = []


def background():
    """The cores busy with other work: every process's CPU (ps's decaying
    average) but this one's, its descendants' and OURS's."""
    try:
        out = _run(["ps", "-A", "-o", "pid=,ppid=,%cpu=,comm="])
    except Exception:
        return None
    rows = []
    for line in out.splitlines():
        f = line.split(None, 3)
        if len(f) == 4:
            rows.append((int(f[0]), int(f[1]), float(f[2]), f[3]))
    mine, grew = {os.getpid()}, True
    while grew:
        grew = False
        for pid, ppid, _, _ in rows:
            if ppid in mine and pid not in mine:
                mine.add(pid)
                grew = True
    # This process's ancestors (the shell and bench/all.sh) too.
    parent = {pid: ppid for pid, ppid, _, _ in rows}
    p = os.getppid()
    while p > 1 and p not in mine:
        mine.add(p)
        p = parent.get(p, 1)
    return sum(cpu for pid, _, cpu, comm in rows
               if pid not in mine and not OURS.search(comm)) / 100


def quiet(echo=True):
    """Waits (at most QUIET_WAIT s) until the machine's other work is under
    QUIET_CORES; logs what it saw in QUIET_LOG."""
    began = time.monotonic()
    seen = background()
    while seen is not None and seen > QUIET_CORES and time.monotonic() - began < QUIET_WAIT:
        if echo:
            print(f"[waiting: other work {seen:.1f} cores > {QUIET_CORES}]", file=sys.stderr, flush=True)
        time.sleep(15)
        seen = background()
    QUIET_LOG.append({"waited": round(time.monotonic() - began, 1),
                      "other_cores": None if seen is None else round(seen, 2)})
    return seen


def quiet_summary():
    """What the rounds' waits saw, for the results' environment."""
    if not QUIET_LOG:
        return None
    seen = [q["other_cores"] for q in QUIET_LOG if q["other_cores"] is not None]
    return {"rounds": len(QUIET_LOG), "waited_s": round(sum(q["waited"] for q in QUIET_LOG), 1),
            "other_cores_max": max(seen) if seen else None,
            "over_bound": sum(1 for x in seen if x > QUIET_CORES), "bound": QUIET_CORES}


def load():
    """The load averages (1, 5, 15 minutes)."""
    try:
        return list(os.getloadavg())
    except OSError:
        return None


def checksum(path, limit=None):
    """SHA-256 of a file, or of a directory's files in name order (their
    names and contents)."""
    h = hashlib.sha256()
    files = [path] if os.path.isfile(path) else sorted(
        os.path.join(d, f) for d, _, fs in os.walk(path) for f in fs)
    for f in files:
        h.update(os.path.relpath(f, path if os.path.isdir(path) else os.path.dirname(path)).encode())
        with open(f, "rb") as fh:
            read = 0
            while chunk := fh.read(1 << 20):
                h.update(chunk)
                read += len(chunk)
                if limit and read >= limit:
                    break
    return h.hexdigest()


def commit():
    """nucleus's commit, and whether the tree had changes."""
    sha = _run(["git", "-C", ROOT, "rev-parse", "HEAD"])
    # Mode bits and Markdown are not what a run measures.
    dirty = _run(["git", "-C", ROOT, "-c", "core.fileMode=false", "status", "--porcelain",
                  "--untracked-files=no", "--", ".", ":!*.md"])
    return {"commit": sha, "dirty": bool(dirty)}


def environment(systems=None, data=(), where="."):
    """What a result carries: the machine, the disk under `where`, the
    load, the systems measured (`{name: {"version": …, "settings": …}}`),
    the data's checksums, nucleus's commit and the time."""
    return {
        "method": METHOD,
        "at": _dt.datetime.now(_dt.timezone.utc).isoformat(timespec="seconds"),
        "machine": machine(),
        "disk": disk(where),
        "load_before": load(),
        "systems": systems or {},
        "data": {os.path.basename(p.rstrip("/")): checksum(p) for p in data},
        "nucleus": commit(),
    }


# ----------------------------------------------------------------- results

def invocation(argv=None):
    """The command that made a result, as run from the repository's root."""
    argv = list(sys.argv if argv is None else argv)
    if argv and os.path.exists(argv[0]):
        argv[0] = os.path.relpath(os.path.abspath(argv[0]), ROOT)
    return " ".join(shlex.quote(a) for a in argv)


def write(path, name, env, samples, extra=None, script=None):
    """A results file: the command that made it, the environment, every
    sample's runs, and the load after."""
    env = dict(env, load_after=load())
    if quiet_summary():
        env["quiet"] = quiet_summary()
    doc = {"method": METHOD, "name": name, "script": script or invocation(), "environment": env,
           "samples": [s.to_json() for s in samples]}
    if extra:
        doc.update(extra)
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    # Paths as the repository and the home directory name them: a results
    # file is published, the machine's user name is not.
    text = json.dumps(doc, indent=1).replace(ROOT, ".").replace(os.path.expanduser("~"), "~")
    with open(path, "w") as f:
        f.write(text + "\n")
    return json.loads(text)


def read(path):
    with open(path) as f:
        doc = json.load(f)
    doc["samples"] = [Sample.from_json(s) for s in doc["samples"]]
    return doc


def fmt(x):
    if x >= 100:
        return f"{x:,.0f}"
    if x >= 10:
        return f"{x:,.1f}"
    if x >= 1:
        return f"{x:,.2f}"
    if x == 0 or x >= 0.001:
        return f"{x:,.3f}"
    return f"{x:.3g}"  # three significant digits below a thousandth


def cell(s):
    """A sample as a table shows it: median [interval] ±CV, ⚠ when flagged."""
    lo, hi = s.ci
    flag = " ⚠" if s.flagged else ""
    return f"{fmt(s.median)} [{fmt(lo)}–{fmt(hi)}] ±{s.cv * 100:.0f}%{flag}"


def table(samples, systems=None, baseline=None, title="measurement"):
    """A Markdown table: a row per (name, phase), a column per system; with
    `baseline`, each other system's ratio to it — `≈` where the intervals
    overlap (no difference claimed)."""
    systems = systems or list(dict.fromkeys(s.system for s in samples))
    rows = list(dict.fromkeys((s.name, s.phase) for s in samples))
    by = {(s.name, s.phase, s.system): s for s in samples}
    units = {s.name: s.unit for s in samples}
    mixed = len(set(units.values())) > 1
    head = f"| {title} | " + " | ".join(s if mixed else f"{s} ({samples[0].unit})" for s in systems) + " |"
    rule = "|---|" + "---:|" * len(systems)
    others = [s for s in systems if baseline and s != baseline]
    for o in others:
        head += f" {o} / {baseline} |"
        rule += "---:|"
    lines = [head, rule]
    for name, phase in rows:
        label = f"{name} ({phase})" if phase == "cold" else name
        if mixed:
            label += f", {units[name]}"
        line = f"| {label} | " + " | ".join(cell(by[(name, phase, s)]) if (name, phase, s) in by else "—"
                                            for s in systems) + " |"
        for o in others:
            a, b = by.get((name, phase, o)), by.get((name, phase, baseline))
            if a and b and b.median > 0:
                ratio = a.median / b.median
                line += f" {ratio:.2f}× |" if differ(a, b) else f" ≈ ({ratio:.2f}×) |"
            else:
                line += " — |"
        lines.append(line)
    return "\n".join(lines)


def describe(env):
    """The environment in a sentence, for under a table."""
    m = env.get("machine", {})
    parts = [m.get("cpu") or m.get("arch"), f"{m.get('logical_cpus')} logical CPUs"]
    if m.get("memory_bytes"):
        parts.append(f"{m['memory_bytes'] / 2**30:.0f} GiB")
    parts.append(m.get("os"))
    if m.get("low_power_mode"):
        parts.append("low-power mode")
    if m.get("on_battery"):
        parts.append("on battery")
    if m.get("governor"):
        parts.append(f"governor {m['governor']}")
    systems = ", ".join(f"{k} {v.get('version', '')}".strip() for k, v in env.get("systems", {}).items())
    n = env.get("nucleus", {})
    sha = (n.get("commit") or "")[:9] + ("+changes" if n.get("dirty") else "")
    return "; ".join(p for p in parts if p) + (f" — {systems}" if systems else "") + \
        (f" — nucleus {sha}" if sha else "") + f" — {env.get('at', '')}"


# ------------------------------------------------- a script's own numbers

MARK = "@metric "


def emit(name, value, unit="ms", system="nucleus", phase="warm"):
    """A number on stdout for `method.py run` to gather: a script that
    measures in its own way (a Rust test, a long Python benchmark) prints
    each of its numbers with this, once a run (or several values)."""
    values = value if isinstance(value, (list, tuple)) else [value]
    print(MARK + json.dumps({"name": name, "values": [float(v) for v in values], "unit": unit,
                             "system": system, "phase": phase}), flush=True)


def emit_system(name, info):
    """A system's version and settings, for the environment."""
    print("@system " + json.dumps({name: info}), flush=True)


def gather(text, into, systems, label=None):
    """The `@metric` and `@system` lines of a run's output; with `label`
    (one of several commands run in turn), the command's label names the
    system "nucleus"; another system keeps its name under the label
    "nucleus" or its own name, and is "system (label)" under another)."""
    for line in text.splitlines():
        if line.startswith(MARK):
            m = json.loads(line[len(MARK):])
            system = m.get("system", "nucleus")
            if label:
                if system in ("nucleus", label):
                    system = label
                elif label != "nucleus":
                    system = f"{system} ({label})"
            key = (m["name"], m.get("phase", "warm"), system)
            into.setdefault(key, {"unit": m.get("unit", "ms"), "values": []})["values"].extend(m["values"])
        elif line.startswith("@system "):
            systems.update(json.loads(line[len("@system "):]))


def run_commands(commands, runs=RUNS, bound=CV_BOUND, retries=RETRIES, echo=True,
                 budget=RETRY_BUDGET, saved=None):
    """Each command (`{label: argv}`) run in turn, `runs` rounds (A, B, A,
    B, …); their `@metric` lines as samples.  While a command's metric
    spreads past `bound` (at most `retries` times), that command — only
    it — runs `runs` rounds more, added to the ones before: a noisy
    metric of one system does not run the others' long builds again, and
    no round is thrown away (more rounds narrow the median's interval
    even where the spread is the measurement's own).  A command whose
    attempt took longer than `budget` seconds is not run again (its noisy
    metrics flagged).  `saved(samples, systems)`, when given, is called
    after each attempt: a run stopped later keeps what it measured.
    Returns (samples, systems)."""
    got, systems, owner, tries, took = {}, {}, {}, {}, {}
    todo = list(commands.items())
    attempts = 0
    while True:
        attempts += 1
        for label, _ in todo:
            tries[label] = attempts
            took[label] = 0.0
        for r in range(runs):
            for label, argv in turn(todo, r):
                quiet(echo)
                if echo:
                    print(f"[{label} run {r + 1}/{runs}, attempt {attempts}]", file=sys.stderr, flush=True)
                began = time.monotonic()
                p = subprocess.Popen(argv, stdout=subprocess.PIPE, text=True, cwd=ROOT)
                lines = []
                for line in p.stdout:
                    lines.append(line)
                    if echo and not line.startswith(("@metric ", "@system ")):
                        sys.stderr.write(line)
                if p.wait() != 0:
                    raise SystemExit(f"{label}: exit {p.returncode}")
                took[label] += time.monotonic() - began
                mine = {}
                gather("".join(lines), mine, systems, label if len(commands) > 1 else None)
                for key, v in mine.items():
                    owner[key] = label
                    got.setdefault(key, {"unit": v["unit"], "values": []})["values"].extend(v["values"])
        samples = [Sample(name, system, v["values"], phase, flagged=cv(v["values"]) > bound,
                          attempts=tries[owner[(name, phase, system)]], unit=v["unit"])
                   for (name, phase, system), v in got.items()]
        if saved:
            saved(samples, systems)
        noisy = {owner[k] for k, v in got.items() if cv(v["values"]) > bound}
        slow = {label for label in noisy if took[label] > budget}
        if echo and slow:
            print(f"[not run again, an attempt past {budget} s: {', '.join(sorted(slow))}]",
                  file=sys.stderr, flush=True)
        noisy -= slow
        if not noisy or attempts > retries:
            return samples, systems
        todo = [(label, argv) for label, argv in todo if label in noisy]


def reproduce(first, second):
    """Which of `first`'s samples `second` reproduces: their intervals
    overlap, or their medians are within NOISE.  Returns the ones it does
    not (name, phase, system)."""
    a = {(s.name, s.phase, s.system): s for s in first["samples"]}
    b = {(s.name, s.phase, s.system): s for s in second["samples"]}
    missed = []
    for key, s in a.items():
        t = b.get(key)
        if t is None or differ(s, t):
            missed.append(key)
    return missed


def cli_run(argv):
    """method.py run NAME OUT [--runs N] [--retries K] [--retry-budget S] [--data PATH]…
    [--where DIR] [--also LABEL=COMMAND]… -- COMMAND…"""
    import argparse
    cut = argv.index("--")
    ap = argparse.ArgumentParser(prog="method.py run")
    ap.add_argument("name")
    ap.add_argument("out")
    ap.add_argument("--runs", type=int, default=RUNS)
    ap.add_argument("--retries", type=int, default=RETRIES)
    ap.add_argument("--retry-budget", type=float, default=RETRY_BUDGET,
                    help="seconds: a command whose attempt took longer is not run again")
    ap.add_argument("--data", action="append", default=[])
    ap.add_argument("--where", default=".")
    ap.add_argument("--label", default="nucleus", help="the command's label (with --also)")
    ap.add_argument("--also", action="append", default=[], help="LABEL=COMMAND, run in turn")
    a = ap.parse_args(argv[:cut])
    commands = {a.label: argv[cut + 1:]}
    for also in a.also:
        label, cmd = also.split("=", 1)
        commands[label] = shlex.split(cmd)
    env = environment(data=a.data, where=a.where)
    script = invocation([os.path.join(HERE, "method.py"), "run"] + argv)

    def saved(samples, systems):
        write(a.out, a.name, dict(env, systems=systems), samples, script=script)

    samples, systems = run_commands(commands, a.runs, retries=a.retries, budget=a.retry_budget,
                                    saved=saved)
    env["systems"] = systems
    write(a.out, a.name, env, samples, script=script)
    print(table(samples, title=a.name))
    return 0


def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0
    if argv[0] == "env":
        print(json.dumps(environment(where=argv[1] if len(argv) > 1 else "."), indent=1))
        return 0
    if argv[0] == "show" and len(argv) == 2:
        doc = read(argv[1])
        print(table(doc["samples"], baseline=doc.get("baseline"), title=doc["name"]))
        print()
        print(describe(doc["environment"]))
        return 0
    if argv[0] == "run" and "--" in argv:
        return cli_run(argv[1:])
    if argv[0] == "reproduce" and len(argv) == 3:
        first, second = read(argv[1]), read(argv[2])
        missed = reproduce(first, second)
        total = len(first["samples"])
        for name, phase, system in missed:
            print(f"not reproduced: {name} ({phase}) on {system}")
        print(f"{total - len(missed)} of {total} reproduced")
        return 1 if missed else 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
