#!/usr/bin/env python3
"""Backups measured (PLAN.md §M14.24 R6, §M14.18 O3, O3c): `BACKUP TO`
and `nucleus restore`, each timed with its process's start (`nucleus sql
DIR -c …`, `nucleus restore …`), on real files.

    python3 bench/backup.py --nucleus target/release/nucleus
        [--parts subjects,incremental,compression] [--notes 100000] [--rows 1000000]

- subjects: 100,000 notes of 1,000 subjects (a subject column, a B-tree
  index on it, a text index on the note) and 100,000 rows of a table of
  no one (a B-tree index), analyzed; `BACKUP TO` with the notes' rows
  sealed under their subjects' keys, and the same database without the
  subject column (nothing sealed); each restored.  Then `FORGET SUBJECT`
  of one subject and the first backup restored again: that subject's
  rows are left out (checked).
- incremental: a table of --rows rows of about 110 bytes with a B-tree
  index, backed up into one repository one change after another (the
  first full, then nothing changed, 1 % of the rows updated adjacent,
  1 % spread over every leaf, 100 rows, 1 % deleted), then `BACKUP
  FULL`; the chain restored, and the full backup after it; both read
  back the same as the database (checked).
- compression: --rows rows of text of about 90 bytes with a B-tree
  index, `BACKUP FULL` and its restore, then the same encrypted (a
  backup key, `NUCLEUS_BACKUP_KEY_FILE`).

Every figure is printed as an `@metric` line too, for `bench/method.py
run`, which repeats the whole script (PLAN.md §M14.31); sizes are the
bytes stored in the repository (compressed, sealed).
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

BIN = None


def nucleus(*args, script=None, env=None):
    """Runs `nucleus …`; its output.  An error raises."""
    p = subprocess.run([BIN, *args], input=script, capture_output=True, text=True,
                       env=dict(os.environ, **(env or {})))
    if p.returncode != 0:
        raise SystemExit(f"nucleus {' '.join(args)}: {p.stdout}{p.stderr}")
    return p.stdout


def timed(*args, **kw):
    s = time.perf_counter()
    nucleus(*args, **kw)
    return time.perf_counter() - s


def sql(db, script, *args, **kw):
    return nucleus("sql", db, *args, script=script if script.rstrip().endswith(";") else script + ";\n", **kw)


def size(path):
    """The bytes of the files under path."""
    return sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(path) for f in fs)


def mib(b):
    return b / 2**20


def report(name, secs=None, stored=None):
    line = f"{name:<68}"
    if secs is not None:
        method.emit(name, secs, unit="s", phase="write")
        line += f" {secs:7.2f} s"
    if stored is not None:
        method.emit(f"{name}: stored", mib(stored), unit="MiB", phase="write")
        line += f" {mib(stored):9.1f} MiB"
    print(line, flush=True)


def subjects(work, n):
    # The subjects' keys are sealed under a keyring's root (§M14.30 K1d).
    root = os.path.join(work, "root.key")
    with open(root, "w") as f:
        f.write(os.urandom(32).hex() + "\n")
    rk = ["--root-key", root]
    notes = (f"CREATE TABLE notes (id INT PRIMARY KEY, subject TEXT, note TEXT);\n"
             "{subject}"
             "CREATE INDEX notes_subject ON notes (subject);\n"
             "CREATE INDEX notes_note ON notes USING text (note);\n"
             f"INSERT INTO notes SELECT i, 'user' || (i % 1000), 'a note of the conversation ' || i || "
             f"', of word' || (i * 7 % 500) || ' and word' || (i * 13 % 300) || ', kept for later' "
             f"FROM generate_series(1, {n}) i;\n"
             "CREATE TABLE other (id INT PRIMARY KEY, v INT, s TEXT);\n"
             "CREATE INDEX other_v ON other (v);\n"
             f"INSERT INTO other SELECT i, i::bigint * 7919 % 1000003, 'a row of no one ' || i "
             f"FROM generate_series(1, {n}) i;\n"
             "ANALYZE;\n")
    dbs = {}
    for how, subject in (("sealed", "ALTER TABLE notes SET SUBJECT (subject);\n"), ("unsealed", "")):
        db, repo = os.path.join(work, f"{how}-db"), os.path.join(work, f"{how}-repo")
        sql(db, notes.format(subject=subject), *rk)
        dbs[how] = (db, repo)
    print(f"a database of {mib(size(dbs['sealed'][0])):.0f} MiB: {n:,} notes, {n:,} rows of no one", flush=True)
    for how, (db, repo) in dbs.items():
        what = "the notes' rows sealed under their subjects' keys" if how == "sealed" else \
            "the same with no subject column (nothing sealed)"
        report(f"BACKUP TO, {what}", timed("sql", db, *rk, "-c", f"BACKUP TO '{repo}'"), size(repo))
    keys = dbs["sealed"][0] + ".subject-keys"
    for how, (db, repo) in dbs.items():
        out = os.path.join(work, f"{how}-restored")
        extra = ["--subject-keys", keys] if how == "sealed" else []
        report(f"nucleus restore, {how}", timed("restore", repo, out, *rk, *extra))
        got = sql(out, "SELECT count(*) FROM notes", *rk).split()
        assert str(n) in got, got
        shutil.rmtree(out)
    # A subject forgotten: its rows left out of the restore.
    db, repo = dbs["sealed"]
    sql(db, "FORGET SUBJECT 'user7'", *rk)
    out = os.path.join(work, "forgotten")
    nucleus("restore", repo, out, *rk, "--subject-keys", keys)
    got = sql(out, "SELECT count(*) FROM notes", *rk).split()
    assert str(n - n // 1000) in got, f"after FORGET SUBJECT: {got}"
    print(f"after FORGET SUBJECT 'user7', the restore holds {n - n // 1000:,} notes", flush=True)


def table(db, n, env=None):
    sql(db, "CREATE TABLE t (id INT PRIMARY KEY, k INT, v TEXT);\n"
            "CREATE INDEX t_k ON t (k);\n"
            f"INSERT INTO t SELECT i, i::bigint * 7919 % 1000003, 'a row of text ' || i || ' with some words in it, ' "
            f"|| (i::bigint * 7919 % 1000003) || ' and then more ' || (i * 31 % 9973) || '.' "
            f"FROM generate_series(1, {n}) i;\n"
            "ANALYZE;\n", env=env)


def incremental(work, n):
    db, repo = os.path.join(work, "incr-db"), os.path.join(work, "incr-repo")
    table(db, n)
    print(f"a table of {n:,} rows, {mib(size(db)):.0f} MiB of database", flush=True)
    steps = [
        ("the first: full", None),
        ("nothing changed", None),
        (f"{n // 100:,} rows updated, adjacent (1 %)", f"UPDATE t SET v = v || '!' WHERE id <= {n // 100}"),
        (f"{n // 100:,} rows updated, every 100th (1 %: nearly every leaf)",
         "UPDATE t SET v = v || '?' WHERE id % 100 = 0"),
        (f"{n // 10000:,} rows updated, every 10,000th", "UPDATE t SET v = v || '#' WHERE id % 10000 = 0"),
        (f"{n // 100:,} rows deleted, adjacent", f"DELETE FROM t WHERE id > {n // 2} AND id <= {n // 2 + n // 100}"),
    ]
    for name, change in steps:
        if change:
            sql(db, change)
        before = size(repo) if os.path.exists(repo) else 0
        secs = timed("sql", db, "-c", f"BACKUP TO '{repo}'")
        report(f"BACKUP TO, {name}", secs, size(repo) - before)
    before = size(repo)
    report("BACKUP FULL TO", timed("sql", db, "-c", f"BACKUP FULL TO '{repo}'"), size(repo) - before)
    want = sql(db, "SELECT count(*), sum(length(v)) FROM t")
    backups = sorted(d for d in os.listdir(repo) if d[:6].isdigit())
    chain, full = backups[len(steps) - 1], backups[-1]
    for name, at in ((f"the chain of {len(steps)}", chain), ("the full one after it", full)):
        out = os.path.join(work, f"incr-{at}")
        report(f"nucleus restore, {name}", timed("restore", repo, out, "--at", at))
        got = sql(out, "SELECT count(*), sum(length(v)) FROM t")
        assert got == want, f"{name}: {got} against {want}"
        shutil.rmtree(out)


def compression(work, n):
    key = os.path.join(work, "backup.key")
    with open(key, "w") as f:
        f.write(os.urandom(32).hex() + "\n")
    db = os.path.join(work, "comp-db")
    table(db, n)
    print(f"a table of {n:,} rows, {mib(size(db)):.0f} MiB of database", flush=True)
    want = sql(db, "SELECT count(*), sum(length(v)) FROM t")
    for how, env in (("compressed (LZ4 block)", None),
                     ("compressed and encrypted (ChaCha20-Poly1305)", {"NUCLEUS_BACKUP_KEY_FILE": key})):
        repo = os.path.join(work, "comp-repo-" + ("plain" if env is None else "sealed"))
        report(f"BACKUP FULL TO, {how}", timed("sql", db, "-c", f"BACKUP FULL TO '{repo}'", env=env), size(repo))
        out = os.path.join(work, "comp-restored")
        extra = ["--backup-key", key] if env else []
        report(f"nucleus restore, {how}", timed("restore", repo, out, *extra))
        got = sql(out, "SELECT count(*), sum(length(v)) FROM t")
        assert got == want, f"{how}: {got} against {want}"
        shutil.rmtree(out)


def main():
    global BIN
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    a.add_argument("--parts", default="subjects,incremental,compression")
    a.add_argument("--notes", type=int, default=100_000)
    a.add_argument("--rows", type=int, default=1_000_000)
    args = a.parse_args()
    BIN = os.path.abspath(args.nucleus)
    method.emit_system("nucleus", {"binary": BIN, "version": nucleus("--version").splitlines()[0]})
    work = tempfile.mkdtemp(prefix="nucleus-backup-bench-")
    try:
        for part in args.parts.split(","):
            print(f"## {part}", flush=True)
            {"subjects": lambda: subjects(work, args.notes),
             "incremental": lambda: incremental(work, args.rows),
             "compression": lambda: compression(work, args.rows)}[part]()
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
