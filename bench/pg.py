#!/usr/bin/env python3
"""PostgreSQL as the benchmarks' baseline (PLAN.md §M14.31 P1): native on
the machine nucleus runs on — not in a VM — in a cluster of its own,
tuned for the machine as its documentation and PGTune (Le Kov, the
widely used calculator of postgresql.conf from a machine's memory, cores
and disk) set it, every setting recorded with the results.

    with Cluster("target/bench-pg") as pg:   # initdb once, start, stop
        pg.psql                               # the psql command, for common.Postgres
        pg.describe()                         # version and settings, for method.environment

    python3 bench/pg.py [dir]                 # start one and print how to reach it; ^C stops it

The binaries: PG_BIN, else Homebrew's postgresql@18, else the PATH.
"""

import os
import shutil
import socket
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import method  # noqa: E402

HOMEBREW = "/opt/homebrew/opt/postgresql@18/bin"


def bin_dir():
    for d in (os.environ.get("PG_BIN"), HOMEBREW):
        if d and os.path.exists(os.path.join(d, "postgres")):
            return d
    found = shutil.which("postgres")
    if found:
        return os.path.dirname(found)
    raise SystemExit("no PostgreSQL binaries: set PG_BIN (or brew install postgresql@18)")


def tuning(m=None):
    """postgresql.conf for this machine: PGTune's "mixed" profile on an
    SSD — shared_buffers a quarter of memory, effective_cache_size three
    quarters, work_mem from memory over connections and workers, parallel
    workers to the cores — with every one written beside the results."""
    m = m or method.machine()
    mem_kb = (m.get("memory_bytes") or 8 << 30) // 1024
    cores = m.get("physical_cpus") or m.get("logical_cpus") or 4
    connections = 20
    workers_per_gather = max(2, cores // 2)
    shared = mem_kb // 4
    work_mem = max(4096, (mem_kb - shared) // (connections * 3) // workers_per_gather)
    return {
        "max_connections": str(connections),
        "shared_buffers": f"{shared // 1024}MB",
        "effective_cache_size": f"{mem_kb * 3 // 4 // 1024}MB",
        "maintenance_work_mem": f"{min(2 << 20, mem_kb // 16) // 1024}MB",
        "work_mem": f"{work_mem // 1024}MB",
        "wal_buffers": "16MB",
        "min_wal_size": "1GB",
        "max_wal_size": "4GB",
        "checkpoint_completion_target": "0.9",
        "default_statistics_target": "100",
        "random_page_cost": "1.1",
        "max_worker_processes": str(cores),
        "max_parallel_workers": str(cores),
        "max_parallel_workers_per_gather": str(workers_per_gather),
        "max_parallel_maintenance_workers": str(max(2, cores // 2)),
        # Not PGTune's: the benchmarks' own conditions.
        "listen_addresses": "''",
        "fsync": "on",
        "synchronous_commit": "on",
        "jit": "on",
    }


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Cluster:
    """A cluster under `data` (made once by initdb), started on a Unix
    socket in a temporary directory, stopped on exit."""

    def __init__(self, data, settings=None):
        self.bin = bin_dir()
        self.data = os.path.abspath(data)
        self.settings = settings or tuning()
        self.port = free_port()
        self.sock = tempfile.mkdtemp(prefix="pg-")

    def _tool(self, name):
        return os.path.join(self.bin, name)

    def __enter__(self):
        if not os.path.exists(os.path.join(self.data, "PG_VERSION")):
            os.makedirs(os.path.dirname(self.data), exist_ok=True)
            subprocess.run([self._tool("initdb"), "-D", self.data, "-U", "postgres", "--auth=trust",
                            "--encoding=UTF8", "--locale=C"], check=True, capture_output=True)
        # A run stopped from outside leaves its postmaster up (and this one
        # would refuse to start): stop it first — the cluster is ours.
        if os.path.exists(os.path.join(self.data, "postmaster.pid")):
            subprocess.run([self._tool("pg_ctl"), "-D", self.data, "-m", "fast", "-w", "stop"],
                           capture_output=True)
        opts = " ".join(f"-c {k}={v}" for k, v in self.settings.items())
        opts += f" -p {self.port} -k {self.sock}"
        # macOS: a postmaster without a valid locale in LC_ALL refuses to
        # start ("became multithreaded during startup").
        env = dict(os.environ, LC_ALL=os.environ.get("LC_ALL") or "en_US.UTF-8")
        subprocess.run([self._tool("pg_ctl"), "-D", self.data, "-o", opts, "-l",
                        os.path.join(self.data, "server.log"), "-w", "start"], check=True, capture_output=True,
                       env=env)
        return self

    def __exit__(self, *exc):
        self.stop()

    def stop(self):
        subprocess.run([self._tool("pg_ctl"), "-D", self.data, "-m", "fast", "-w", "stop"],
                       capture_output=True)
        shutil.rmtree(self.sock, ignore_errors=True)

    def restart(self):
        """Stopped and started again: its caches empty (not the OS's)."""
        subprocess.run([self._tool("pg_ctl"), "-D", self.data, "-m", "fast", "-w", "stop"],
                       check=True, capture_output=True)
        shutil.rmtree(self.sock, ignore_errors=True)
        self.sock = tempfile.mkdtemp(prefix="pg-")
        return self.__enter__()

    @property
    def conninfo(self):
        return f"host={self.sock} port={self.port} user=postgres dbname=postgres"

    @property
    def psql(self):
        """The psql command line (a string, as common.Postgres takes it)."""
        return f"env LC_ALL=C {self._tool('psql')} \"{self.conninfo}\""

    def tool(self, name):
        """A client tool's command line (psql, pgbench, …) reaching this
        cluster, as a list."""
        return ["env", "LC_ALL=C", self._tool(name), self.conninfo] if name == "psql" else \
            ["env", "LC_ALL=C", self._tool(name), "-h", self.sock, "-p", str(self.port), "-U", "postgres"]

    def sql(self, sql=None, file=None):
        """One command (or a script on stdin), quietly, its rows unaligned;
        an error raises."""
        cmd = self.tool("psql") + ["-X", "-q", "-At", "-v", "ON_ERROR_STOP=1"]
        if sql is not None:
            cmd += ["-c", sql]
        return subprocess.run(cmd, check=True, capture_output=True, text=True, input=file).stdout

    def describe(self):
        """Its version and settings, for the results' environment."""
        build = subprocess.run([self._tool("postgres"), "--version"], capture_output=True,
                               text=True).stdout.strip()
        # What it runs with: every setting not at its default, and how
        # it syncs.
        out = subprocess.run(["env", "LC_ALL=C", self._tool("psql"), self.conninfo, "-X", "-A", "-t", "-F", "\t",
                              "-c", "SELECT name, current_setting(name) FROM pg_settings "
                                    "WHERE source NOT IN ('default', 'override') OR name = 'wal_sync_method' "
                                    "ORDER BY name"], capture_output=True, text=True).stdout
        effective = dict(line.split("\t", 1) for line in out.splitlines() if "\t" in line)
        return {"version": build.rsplit(") ", 1)[-1], "build": build, "settings": self.settings,
                "effective": effective, "native": True}


DEFAULT = os.path.join(method.ROOT, "target", "bench-pg")


def main():
    data = sys.argv[1] if len(sys.argv) > 1 else DEFAULT
    with Cluster(data) as pg:
        print(pg.psql)
        print(pg.describe())
        try:
            input("running; Enter or ^C stops it\n")
        except (KeyboardInterrupt, EOFError):
            pass


if __name__ == "__main__":
    main()
