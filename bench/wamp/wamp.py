"""Write amplification on write-heavy loads (PLAN.md §M14.28): bytes
written to disk per byte of rows inserted (a bigint key and 100 bytes of
text: 108 bytes a row), each phase's pages made durable before it is
counted done — nucleus: the server killed after the phase and restarted
(recovery replays the WAL and checkpoints), the bytes the server process
wrote (macOS's proc_pid_rusage); PostgreSQL 18 in Docker: CHECKPOINT, the
bytes the container's cgroup wrote.

    python3 -I bench/wamp/wamp.py nucleus BINARY DB-DIR PORT [PHASES]
    python3 -I bench/wamp/wamp.py pg [PHASES]

PHASES: phase numbers, comma-separated (default 0,1,2,3,4); 5 is the
steady state, 5M rows 100 a transaction into the table phase 1 filled.
The random keys of phases 3-5 fall among phase 1's."""
import ctypes, os, re, subprocess, sys, time, random, socket
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "drivers", "python"))

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
CHOSEN = [int(p) for p in sys.argv.pop().split(",")] if re.fullmatch(r"[0-5](,[0-5])*", sys.argv[-1]) else [0, 1, 2, 3, 4]
STEADY_RND = keys(S) if 5 in CHOSEN else []
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

def report(name, rows, secs, wrote, files):
    print(f"{name:46} {rows:>9} rows {secs:7.1f} s  written {wrote / 2**20:8.0f} MiB"
          f"  write amp {wrote / (rows * ROW):6.1f}  files {files / 2**20:7.0f} MiB", flush=True)

PHASES = [
    ("bulk, 1M rows a transaction, keys in order", N),
    ("bulk, 1M rows a transaction, random keys", N),
    ("a row a transaction, keys in order", M),
    ("a row a transaction, random keys", M),
    ("100 rows a transaction, random keys", M),
    ("100 rows a transaction, random keys, 5M rows", S),
]

if sys.argv[1] == "nucleus":
    import nucleus
    binary, db, port = sys.argv[2], sys.argv[3], int(sys.argv[4])
    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
    class Info(ctypes.Structure):
        _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(f"f{i}", ctypes.c_uint64) for i in range(40)]
    def written(pid):
        info = Info()
        assert libc.proc_pid_rusage(pid, 2, ctypes.byref(info)) == 0
        return info.f17
    def size():
        return sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(db) for f in fs)
    def start():
        s = subprocess.Popen([binary, "server", db, "--listen", f"127.0.0.1:{port}", "--no-auth"],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(6000):
            try:
                socket.create_connection(("127.0.0.1", port)).close(); break
            except OSError:
                time.sleep(0.1)
        return s
    s = start()
    c = nucleus.connect("127.0.0.1", port, password=None)
    for t in ("seq", "rnd"):
        c.execute(f"CREATE TABLE {t} (k BIGINT PRIMARY KEY, v TEXT)")
    c.close(); s.terminate(); s.wait()
    def phase(i):
        s = start()
        c = nucleus.connect("127.0.0.1", port, password=None)
        w0, t0 = stable(lambda: written(s.pid)), time.perf_counter()
        if i < 2:
            t, key = ("seq", "g") if i == 0 else ("rnd", RANDOM_KEY)
            for j in range(N // CH):
                c.execute(f"INSERT INTO {t} SELECT {key}, repeat('x', 100) FROM generate_series({j * CH + 1}, {(j + 1) * CH}) g")
        elif i < 4:
            t, keys = ("seq", TRICKLE_SEQ) if i == 2 else ("rnd", TRICKLE_RND)
            for j in range(0, M, 1000):
                c.pipeline([(f"INSERT INTO {t} VALUES ($1, repeat('y', 100))", [k]) for k in keys[j:j + 1000]])
        else:
            keys = BATCH_RND if i == 4 else STEADY_RND
            for j in range(0, len(keys), 100):
                c.execute_many("INSERT INTO rnd VALUES ($1, repeat('z', 100))", [[k] for k in keys[j:j + 100]])
        secs = time.perf_counter() - t0
        w1 = stable(lambda: written(s.pid))
        c.close(); s.terminate(); s.wait()
        # The pages the phase dirtied, made durable by recovery's checkpoint.
        s = start()
        w2 = stable(lambda: written(s.pid))
        s.terminate(); s.wait()
        report(PHASES[i][0], PHASES[i][1], secs, (w1 - w0) + w2, size())
        print(f"    (during {(w1 - w0) / 2**20:.0f} MiB, its checkpoint after {w2 / 2**20:.0f} MiB)", flush=True)
    for i in CHOSEN:
        phase(i)
else:
    NAME = "nucleus-wamp-pg"
    def sh(*a, **k):
        return subprocess.run(a, check=True, capture_output=True, text=True, **k).stdout
    def psql(sql=None, file=None):
        cmd = ["docker", "exec", "-i", NAME, "psql", "-q", "-U", "postgres", "-v", "ON_ERROR_STOP=1"]
        if sql:
            cmd += ["-c", sql]
        return subprocess.run(cmd, check=True, capture_output=True, text=True, input=file).stdout
    def written():
        total = 0
        for line in sh("docker", "exec", NAME, "cat", "/sys/fs/cgroup/io.stat").splitlines():
            for f in line.split()[1:]:
                k, v = f.split("=")
                if k == "wbytes":
                    total += int(v)
        return total
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
                psql(file="".join(f"INSERT INTO {t} SELECT {key}, repeat('x', 100) FROM generate_series({j * CH + 1}, {(j + 1) * CH}) g;\n" for j in range(N // CH)))
            elif i < 4:
                t, keys = ("seq", TRICKLE_SEQ) if i == 2 else ("rnd", TRICKLE_RND)
                psql(file="".join(f"INSERT INTO {t} VALUES ({k}, repeat('y', 100));\n" for k in keys))
            else:
                keys = BATCH_RND if i == 4 else STEADY_RND
                psql(file="".join("BEGIN;\n" + "".join(f"INSERT INTO rnd VALUES ({k}, repeat('z', 100));\n" for k in keys[j:j + 100]) + "COMMIT;\n" for j in range(0, len(keys), 100)))
            secs = time.perf_counter() - t0
            w1 = stable(written)
            psql("CHECKPOINT")
            w2 = stable(written)
            report(name, rows, secs, w2 - w0, size())
            print(f"    (during {(w1 - w0) / 2**20:.0f} MiB, its checkpoint after {(w2 - w1) / 2**20:.0f} MiB)", flush=True)
    finally:
        subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
