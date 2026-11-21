"""Write amplification on write-heavy loads (PLAN.md §M14.28): bytes
written to disk per byte of rows inserted (a bigint key and 100 bytes of
text: 108 bytes a row), each phase's pages made durable before it is
counted done — nucleus: the server killed after the phase and restarted
(recovery replays the WAL and checkpoints), the bytes the server process
wrote (macOS's proc_pid_rusage); PostgreSQL 18 in Docker: CHECKPOINT, the
bytes the container's cgroup wrote.

    python3 -I bench/wamp/wamp.py nucleus BINARY DB-DIR PORT [PHASES]
    python3 -I bench/wamp/wamp.py nucleus-docker VOLUME BINARY PORT [PHASES]
    python3 -I bench/wamp/wamp.py pg [PHASES]

nucleus-docker runs a Linux build of the server (BINARY, a path inside
the Docker volume VOLUME, mounted at /t) in a container, its database on
an ext4 volume, and counts the bytes the Docker VM's disk wrote, the
file system's journal included (nothing else may run in Docker then).

PHASES: phase numbers, comma-separated (default 0,1,2,3,4); 5 is the
steady state, 5M rows 100 a transaction into the table phase 1 filled;
6 updates 1M of phase 1's rows, 7 deletes 1M of them, 100 a transaction
(PLAN.md §M14.28 L5).  The random keys of phases 3-5 fall among phase
1's.  `--engine=lsm` makes nucleus's tables LSM trees' (§M14.28);
`--values=random` makes each row's text 100 random hex digits (md5s)
instead of one letter repeated, which an engine that compresses (the LSM
tree's runs, LZ4) shrinks to almost nothing.  Each
phase's figures — and the space the files take per byte of rows left,
and, for nucleus, how long the restart after it took (the server
stopped without a checkpoint: recovery) — are printed as `@metric`
lines too, for `bench/method.py run`, which repeats the whole script
(PLAN.md §M14.31)."""
import ctypes, os, re, subprocess, sys, time, random, socket
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "drivers", "python"))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import method  # noqa: E402

ENGINE = "btree"
VALUES = "repeated"
for a in list(sys.argv):
    if a.startswith("--values="):
        VALUES = a.split("=", 1)[1]
        assert VALUES in ("repeated", "random"), VALUES
        sys.argv.remove(a)
    if a.startswith("--engine="):
        ENGINE = a.split("=", 1)[1]
        sys.argv.remove(a)
N, CH, M = 10_000_000, 1_000_000, 100_000
ROW = 8 + 100
rnd = random.Random(7)
TRICKLE_SEQ = list(range(N + 1, N + M + 1))
# Random keys among the bulk's (g * A mod P for g in 1..N), never one of them.
A, P = 2654435761, 4294967311
INV = pow(A, -1, P)
def fresh(k, taken=set()):
    g = k * INV % P
    if 1 <= g <= N or k in taken or k == 0:
        return False
    taken.add(k)
    return True
def keys(n):
    out = []
    while len(out) < n:
        k = rnd.randrange(1, P)
        if fresh(k):
            out.append(k)
    return out
TRICKLE_RND = keys(M)
BATCH_RND = keys(M)
S = 5_000_000
CHOSEN = [int(p) for p in sys.argv.pop().split(",")] if re.fullmatch(r"[0-7](,[0-7])*", sys.argv[-1]) else [0, 1, 2, 3, 4]
STEADY_RND = keys(S) if 5 in CHOSEN else []
# Phase 1's keys, a million of them apart: updated (6), then deleted (7).
U = 1_000_000
CHANGED = [g * A % P for g in rnd.sample(range(1, N + 1), U)] if {6, 7} & set(CHOSEN) else []
RANDOM_KEY = "(g * 2654435761) % 4294967311"

def stable(read):
    """Waits until nothing more is written for 3 s; the bytes so far."""
    last, quiet = read(), 0
    while quiet < 3:
        time.sleep(1)
        now = read()
        quiet = quiet + 1 if now == last else 0
        last = now
    return last

# The system as the results name it: the file system tells the runs apart.
SYSTEM = {"nucleus": "nucleus", "nucleus-docker": "nucleus (ext4)", "pg": "postgresql (ext4)"}[sys.argv[1]]
if ENGINE != "btree" and sys.argv[1] != "pg":
    SYSTEM = SYSTEM.replace("nucleus", f"nucleus {ENGINE}")
if VALUES == "random":
    SYSTEM += ", random values"


def text(letter):
    """A row's 100 bytes of text: `letter` repeated, or random."""
    if VALUES == "random":
        return ("md5(random()::text) || md5(random()::text) || md5(random()::text) || "
                "substr(md5(random()::text), 1, 4)")
    return f"repeat('{letter}', 100)"

def report(name, rows, secs, wrote, files, live=None, recovery=None):
    method.emit(f"{name}: write amplification", wrote / (rows * ROW), unit="x", system=SYSTEM, phase="write")
    method.emit(f"{name}: time", secs, unit="s", system=SYSTEM, phase="write")
    method.emit(f"{name}: files", files / 2**20, unit="MiB", system=SYSTEM, phase="write")
    extra = ""
    if live:
        method.emit(f"{name}: space amplification", files / (live * ROW), unit="x", system=SYSTEM, phase="write")
        extra += f"  space amp {files / (live * ROW):5.2f}"
    if recovery is not None:
        method.emit(f"{name}: recovery", recovery, unit="s", system=SYSTEM, phase="write")
        extra += f"  recovery {recovery:5.1f} s"
    print(f"{name:46} {rows:>9} rows {secs:7.1f} s  written {wrote / 2**20:8.0f} MiB"
          f"  write amp {wrote / (rows * ROW):6.1f}  files {files / 2**20:7.0f} MiB{extra}", flush=True)

PHASES = [
    ("bulk, 1M rows a transaction, keys in order", N),
    ("bulk, 1M rows a transaction, random keys", N),
    ("a row a transaction, keys in order", M),
    ("a row a transaction, random keys", M),
    ("100 rows a transaction, random keys", M),
    ("100 rows a transaction, random keys, 5M rows", S),
    ("100 rows a transaction, updates", U),
    ("100 rows a transaction, deletes", U),
]

def sh(*a, **k):
    return subprocess.run(a, check=True, capture_output=True, text=True, **k).stdout

def cgroup_written(name):
    total = 0
    for line in sh("docker", "exec", name, "cat", "/sys/fs/cgroup/io.stat").splitlines():
        for f in line.split()[1:]:
            k, v = f.split("=")
            if k == "wbytes":
                total += int(v)
    return total

def wait_port(port):
    # Recovery after the steady state can take many minutes (it replays
    # the WAL through the page cache); a server not up within the hour is
    # an error, never a silent return that would time a recovery not done.
    deadline = time.monotonic() + 3600
    while True:
        try:
            socket.create_connection(("127.0.0.1", port)).close(); return
        except OSError:
            if time.monotonic() > deadline:
                raise SystemExit(f"the server on port {port} did not come up within an hour")
            time.sleep(0.1)

if sys.argv[1] in ("nucleus", "nucleus-docker"):
    import nucleus
    def connect():
        # Docker's port forward accepts before the server listens.
        for _ in range(600):
            try:
                return nucleus.connect("127.0.0.1", port, password=None)
            except (OSError, nucleus.OperationalError):
                time.sleep(0.1)
        return nucleus.connect("127.0.0.1", port, password=None)
    if sys.argv[1] == "nucleus":
        binary, db, port = sys.argv[2], sys.argv[3], int(sys.argv[4])
        libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
        class Info(ctypes.Structure):
            _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(f"f{i}", ctypes.c_uint64) for i in range(40)]
        def written(s):
            info = Info()
            assert libc.proc_pid_rusage(s.pid, 2, ctypes.byref(info)) == 0
            return info.f17
        def size():
            return sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(db) for f in fs)
        # The servers started and not stopped: killed before the database
        # goes, whatever ends the run — one left recovering would remove the
        # files of the next run's database at the same path (PLAN.md §M14.28).
        running = []
        def start():
            s = subprocess.Popen([binary, "server", db, "--listen", f"127.0.0.1:{port}", "--no-auth"],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            running.append(s)
            wait_port(port)
            if s.poll() is not None:
                # Another process answers on the port: not ours to measure.
                raise SystemExit(f"the server exited ({s.returncode}); is port {port} taken?")
            return s
        def stop(s):
            s.terminate(); s.wait()
            running.remove(s)
        # A fresh database each run: made here, removed at the end.
        if os.path.exists(db):
            sys.exit(f"{db} exists: wamp makes the database itself (and removes it after)")
        import shutil
        def cleanup():
            for s in running:
                s.kill(); s.wait()
            shutil.rmtree(db, ignore_errors=True)
    else:
        volume, binary, port = sys.argv[2], sys.argv[3], int(sys.argv[4])
        NAME = "nucleus-wamp-nc"
        subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
        sh("docker", "run", "-d", "--rm", "--name", NAME, "-p", f"127.0.0.1:{port}:{port}",
           "-v", f"{volume}:/t:ro", "-v", "/db", "debian:bookworm-slim", "sleep", "infinity")
        def written(s):
            # The VM's disk, journal included (the cgroup's count leaves
            # out the file system's journal, written by a kernel thread);
            # nothing else may run in Docker meanwhile.
            total = 0
            for line in sh("docker", "exec", NAME, "cat", "/proc/diskstats").splitlines():
                f = line.split()
                if f[2].startswith(("vd", "sd", "nvme")) and not f[2][-1].isdigit():
                    total += int(f[9]) * 512
            return total
        def size():
            return int(sh("docker", "exec", NAME, "du", "-sb", "/db").split()[0])
        def start():
            sh("docker", "exec", "-d", NAME, binary, "server", "/db/x", "--listen", f"0.0.0.0:{port}", "--no-auth", "--insecure")
            wait_port(port)
            return None
        def stop(s):
            # SIGTERM, as the process above gets: no shutdown checkpoint.
            sh("docker", "exec", NAME, "sh", "-c",
               "pids=$(for p in /proc/[0-9]*; do [ \"$(cat $p/comm 2>/dev/null)\" = nucleus ] && echo ${p#/proc/}; done); "
               "kill $pids; for p in $pids; do while [ -e /proc/$p ]; do sleep 0.1; done; done")
        cleanup = lambda: subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
    s = start()
    c = connect()
    with_engine = f" WITH (engine = {ENGINE})" if ENGINE != "btree" else ""
    for t in ("seq", "rnd"):
        c.execute(f"CREATE TABLE {t} (k BIGINT PRIMARY KEY, v TEXT){with_engine}")
    c.close(); stop(s)
    def phase(i):
        s = start()
        c = connect()
        w0, t0 = stable(lambda: written(s)), time.perf_counter()
        if i < 2:
            t, key = ("seq", "g") if i == 0 else ("rnd", RANDOM_KEY)
            for j in range(N // CH):
                c.execute(f"INSERT INTO {t} SELECT {key}, {text('x')} FROM generate_series({j * CH + 1}, {(j + 1) * CH}) g")
        elif i < 4:
            t, keys = ("seq", TRICKLE_SEQ) if i == 2 else ("rnd", TRICKLE_RND)
            for j in range(0, M, 1000):
                c.pipeline([(f"INSERT INTO {t} VALUES ($1, {text('y')})", [k]) for k in keys[j:j + 1000]])
        elif i < 6:
            keys = BATCH_RND if i == 4 else STEADY_RND
            for j in range(0, len(keys), 100):
                c.execute_many(f"INSERT INTO rnd VALUES ($1, {text('z')})", [[k] for k in keys[j:j + 100]])
        else:
            sql = (f"UPDATE rnd SET v = {text('u')} WHERE k = $1" if i == 6
                   else "DELETE FROM rnd WHERE k = $1")
            for j in range(0, U, 100):
                c.execute_many(sql, [[k] for k in CHANGED[j:j + 100]])
        secs = time.perf_counter() - t0
        w1 = stable(lambda: written(s))
        live = sum(c.execute(f"SELECT count(*) FROM {t}").rows[0][0] for t in ("seq", "rnd"))
        c.close(); stop(s)
        # The pages the phase dirtied, made durable by recovery's checkpoint.
        r0 = time.perf_counter()
        if sys.argv[1] == "nucleus":
            s = start()
            recovery = time.perf_counter() - r0
            w2 = stable(lambda: written(s))
        else:
            # The container's counter goes on: what recovery adds to it.
            w = stable(lambda: written(s))
            r0 = time.perf_counter()
            s = start()
            recovery = time.perf_counter() - r0
            w2 = stable(lambda: written(s)) - w
        stop(s)
        report(PHASES[i][0], PHASES[i][1], secs, (w1 - w0) + w2, size(), live, recovery)
        print(f"    (during {(w1 - w0) / 2**20:.0f} MiB, its checkpoint after {w2 / 2**20:.0f} MiB)", flush=True)
    try:
        for i in CHOSEN:
            phase(i)
    finally:
        cleanup()
else:
    NAME = "nucleus-wamp-pg"
    def psql(sql=None, file=None):
        cmd = ["docker", "exec", "-i", NAME, "psql", "-q", "-U", "postgres", "-v", "ON_ERROR_STOP=1"]
        if sql:
            cmd += ["-c", sql]
        return subprocess.run(cmd, check=True, capture_output=True, text=True, input=file).stdout
    def written():
        return cgroup_written(NAME)
    def size():
        return int(sh("docker", "exec", NAME, "du", "-sb", "/var/lib/postgresql").split()[0])
    subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
    sh("docker", "run", "-d", "--rm", "--name", NAME, "-e", "POSTGRES_PASSWORD=x", "postgres:18-alpine")
    for _ in range(120):
        if subprocess.run(["docker", "exec", NAME, "pg_isready", "-U", "postgres"], capture_output=True).returncode == 0:
            break
        time.sleep(1)
    time.sleep(3)
    psql("CREATE TABLE seq (k BIGINT PRIMARY KEY, v TEXT); CREATE TABLE rnd (k BIGINT PRIMARY KEY, v TEXT); CHECKPOINT")
    try:
        for i, (name, rows) in enumerate(PHASES):
            if i not in CHOSEN:
                continue
            w0, t0 = stable(written), time.perf_counter()
            if i < 2:
                t, key = ("seq", "g") if i == 0 else ("rnd", RANDOM_KEY)
                psql(file="".join(f"INSERT INTO {t} SELECT {key}, {text('x')} FROM generate_series({j * CH + 1}, {(j + 1) * CH}) g;\n" for j in range(N // CH)))
            elif i < 4:
                t, keys = ("seq", TRICKLE_SEQ) if i == 2 else ("rnd", TRICKLE_RND)
                psql(file="".join(f"INSERT INTO {t} VALUES ({k}, {text('y')});\n" for k in keys))
            elif i < 6:
                keys = BATCH_RND if i == 4 else STEADY_RND
                psql(file="".join("BEGIN;\n" + "".join(f"INSERT INTO rnd VALUES ({k}, {text('z')});\n" for k in keys[j:j + 100]) + "COMMIT;\n" for j in range(0, len(keys), 100)))
            else:
                one = (f"UPDATE rnd SET v = {text('u')} WHERE k = {{}};\n" if i == 6
                       else "DELETE FROM rnd WHERE k = {};\n")
                psql(file="".join("BEGIN;\n" + "".join(one.format(k) for k in CHANGED[j:j + 100]) + "COMMIT;\n" for j in range(0, U, 100)))
            secs = time.perf_counter() - t0
            w1 = stable(written)
            psql("CHECKPOINT")
            w2 = stable(written)
            live = sum(int(psql(f"SELECT count(*) FROM {t}").split()[-3]) for t in ("seq", "rnd"))
            report(name, rows, secs, w2 - w0, size(), live)
            print(f"    (during {(w1 - w0) / 2**20:.0f} MiB, its checkpoint after {(w2 - w1) / 2**20:.0f} MiB)", flush=True)
    finally:
        subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
