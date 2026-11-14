#!/usr/bin/env python3
"""Runs a test-kit script (crates/testkit/slt/*.slt) against PostgreSQL.

    python3 scripts/slt-pg.py check crates/testkit/slt/65_json.slt
    python3 scripts/slt-pg.py fill  crates/testkit/slt/65_json.slt > filled.slt

`check` compares each record's expected result — rows, or an error's
SQLSTATE — with PostgreSQL's and prints the records that differ; `fill`
prints the script with PostgreSQL's results in place of the expected
ones.  A record after a comment line starting `# nucleus-only:` is left as
it is (and not run): what nucleus does differently on purpose, recorded.

PostgreSQL is reached through `psql` (`--psql`, default: a `postgres:18-alpine`
container started for the run, removed after).  Each script runs in a
fresh database; each record in a session of its own, so a script keeps
its state in tables, not in session settings.  Values are formatted as
the test kit's runner formats them (`NULL`, `(empty)`, booleans `t`/`f`
in text columns and `true`/`false` in `B` ones, reals to three places).
"""

import argparse
import subprocess
import sys
import time

SEP = "\x1f"


class Pg:
    def __init__(self, psql):
        self.container = None
        if psql:
            self.base = psql.split()
        else:
            self.container = "nucleus-slt-pg"
            subprocess.run(["docker", "rm", "-f", self.container], capture_output=True)
            subprocess.run(["docker", "run", "-d", "--name", self.container, "-e",
                            "POSTGRES_HOST_AUTH_METHOD=trust", "postgres:18-alpine"],
                           check=True, capture_output=True)
            # Over TCP: the image's first, initializing server listens on
            # its socket only, and is restarted.
            self.base = ["docker", "exec", "-i", self.container, "psql", "-h", "127.0.0.1",
                         "-U", "postgres"]
            for _ in range(60):
                if self.raw("postgres", "SELECT 1")[0].strip() == "1":
                    break
                time.sleep(0.5)

    def raw(self, db, sql):
        script = "\\pset null NULL\n\\set VERBOSITY sqlstate\n" + sql + "\n;\n"
        p = subprocess.run(self.base + ["-X", "-q", "-A", "-t", "-F", SEP, "-d", db],
                           input=script, capture_output=True, text=True)
        return p.stdout, p.stderr

    def fresh(self, db):
        self.raw("postgres", f"DROP DATABASE IF EXISTS {db} WITH (FORCE)")
        self.raw("postgres", f"CREATE DATABASE {db}")

    def close(self):
        if self.container:
            subprocess.run(["docker", "rm", "-f", self.container], capture_output=True)


def records(text):
    """(start line, header, sql lines, expected lines, nucleus-only) in order;
    and the raw lines, to rewrite."""
    lines = text.split("\n")
    out = []
    i = 0
    only = False
    while i < len(lines):
        line = lines[i].rstrip()
        if not line.strip():
            i += 1
            continue
        if line.lstrip().startswith("#"):
            if line.lstrip().startswith("# nucleus-only:"):
                only = True
            i += 1
            continue
        start = i
        header = line
        i += 1
        sql = []
        while i < len(lines) and lines[i].strip() and lines[i].strip() != "----":
            sql.append(lines[i])
            i += 1
        expected = []
        has_results = False
        if i < len(lines) and lines[i].strip() == "----":
            has_results = True
            i += 1
            while i < len(lines) and lines[i].strip():
                expected.append(lines[i].rstrip())
                i += 1
        out.append({"start": start, "end": i, "header": header, "sql": "\n".join(sql),
                    "expected": expected, "results": has_results, "only": only})
        only = False
    return lines, out


def cell(s, ty):
    if s == "NULL":
        return "NULL"
    if ty == "R":
        try:
            return f"{float(s):.3f}"
        except ValueError:
            return s
    if ty == "B":
        return {"t": "true", "f": "false"}.get(s, s)
    return "(empty)" if s == "" else s


def run(pg, db, rec):
    """('error', code) or ('rows', [lines]) or ('ok', None)."""
    out, err = pg.raw(db, rec["sql"])
    errors = [l for l in err.splitlines() if l.startswith("ERROR:")]
    if errors:
        return "error", errors[0].split()[1]
    words = rec["header"].split()
    if words[0] != "query" or words[1] == "error":
        return "ok", None
    types = words[1]
    rows = []
    for line in out.split("\n"):
        if line == "" and not rows and out.strip() == "":
            continue
        if line == "":
            continue
        cells = line.split(SEP)
        rows.append(" ".join(cell(c, types[j] if j < len(types) else "T") for j, c in enumerate(cells)))
    if len(words) > 2 and words[2] == "rowsort":
        rows.sort()
    return "rows", rows


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("mode", choices=["check", "fill"])
    a.add_argument("script")
    a.add_argument("--psql", help="a psql command line (default: a container)")
    args = a.parse_args()
    text = open(args.script, encoding="utf-8").read()
    lines, recs = records(text)
    pg = Pg(args.psql)
    db = "slt_check"
    bad = 0
    try:
        pg.fresh(db)
        replaced = {}
        for rec in recs:
            if rec["only"] or rec["header"].startswith("connection"):
                continue
            kind, got = run(pg, db, rec)
            words = rec["header"].split()
            want_error = words[1] == "error"
            if args.mode == "check":
                if want_error:
                    want = " ".join(words[2:])
                    if kind != "error" or (want and want != got):
                        bad += 1
                        print(f"{args.script}:{rec['start'] + 1}: expected error {want}, PostgreSQL: "
                              f"{got if kind == 'error' else 'ok'}\n  {rec['sql']}")
                elif kind == "error":
                    bad += 1
                    print(f"{args.script}:{rec['start'] + 1}: PostgreSQL fails {got}\n  {rec['sql']}")
                elif kind == "rows":
                    want = list(rec["expected"])
                    if len(words) > 2 and words[2] == "rowsort":
                        want.sort()
                    if want != got:
                        bad += 1
                        print(f"{args.script}:{rec['start'] + 1}: differs\n  {rec['sql']}\n  expected:\n    "
                              + "\n    ".join(want) + "\n  PostgreSQL:\n    " + "\n    ".join(got))
            else:
                if kind == "error":
                    header = f"{words[0]} error {got}"
                    replaced[rec["start"]] = (rec["end"], [header] + rec["sql"].split("\n"))
                elif kind == "rows":
                    replaced[rec["start"]] = (rec["end"], [rec["header"]] + rec["sql"].split("\n")
                                              + ["----"] + got)
        if args.mode == "fill":
            i = 0
            out = []
            while i < len(lines):
                if i in replaced:
                    end, new = replaced[i]
                    out.extend(new)
                    i = end
                else:
                    out.append(lines[i])
                    i += 1
            sys.stdout.write("\n".join(out))
        else:
            n = sum(1 for r in recs if not r["only"])
            print(f"{n - bad} of {n} records as PostgreSQL has them")
    finally:
        pg.close()
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
