"""PostgreSQL's answers for pg-compat's text search (PLAN.md §M14.21 T8).

Random `tsvector`s and `tsquery`s — phrases at distances 0 to 3 over
operators, negations, prefixes, weights, a term without positions — and
the query makers over words PostgreSQL's English configuration and
nucleus's analyze alike, asked of a PostgreSQL server; written to
`modules/pg-compat/tests/tsearch_pg.tsv`, which `tests/tsearch.rs` holds
nucleus to:

    match <tab> vector <tab> query <tab> vector's text <tab> query's text <tab> t|f
    make <tab> maker <tab> text <tab> query's text

    python3 bench/text/tsearch_pg.py [--container nucleus-paradedb]
"""

import argparse
import random
import subprocess

LEXEMES = ["a", "ab", "abc", "b", "c", "d"]
WORDS = ["cat", "cats", "the", "a", "dog", "dogs", "sat", "mat", "on", "running", "of", "fish"]


def vector(r):
    parts = []
    for lex in LEXEMES:
        if r.random() < 0.45:
            continue
        if r.random() < 0.05:
            parts.append(lex)  # no positions
            continue
        ps = sorted(r.sample(range(1, 9), r.randint(1, 3)))
        parts.append(lex + ":" + ",".join(f"{p}{r.choice(['', '', 'A', 'B', 'C'])}" for p in ps))
    return " ".join(parts)


def operand(r):
    w = r.choice(LEXEMES)
    mods = ""
    if r.random() < 0.15:
        mods += "*"
    if r.random() < 0.1:
        mods += "".join(r.sample("ABCD", r.randint(1, 2)))
    return w + (":" + mods if mods else "")


def query(r, depth):
    if depth == 0 or r.random() < 0.3:
        return operand(r)
    a, b = query(r, depth - 1), query(r, depth - 1)
    k = r.randrange(7)
    if k == 0:
        return f"({a} & {b})"
    if k == 1:
        return f"({a} | {b})"
    if k == 2:
        return f"!{a}"
    if k in (3, 4):
        return f"({a} <-> {b})"
    return f"({a} <{r.randrange(4)}> {b})"


def standard(r, depth):
    if depth == 0 or r.random() < 0.3:
        return r.choice(WORDS)
    a, b = standard(r, depth - 1), standard(r, depth - 1)
    return r.choice([f"({a} & {b})", f"({a} | {b})", f"!{a}", f"({a} <-> {b})", f"({a} <2> {b})"])


def web(r):
    out = []
    for _ in range(r.randint(1, 4)):
        k = r.randrange(5)
        if k == 0:
            out.append("-" + r.choice(WORDS))
        elif k == 1:
            out.append('"' + " ".join(r.choice(WORDS) for _ in range(r.randint(1, 3))) + '"')
        elif k == 2:
            out.append("or")
        else:
            out.append(r.choice(WORDS))
    return " ".join(out)


def words(r):
    return " ".join(r.choice(WORDS) for _ in range(r.randint(1, 5)))


def dollar(s):
    return "$q$" + s + "$q$"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--container", default="nucleus-paradedb")
    p.add_argument("--out", default="modules/pg-compat/tests/tsearch_pg.tsv")
    a = p.parse_args()
    r = random.Random(7)
    matches = [(vector(r), query(r, 3)) for _ in range(2000)]
    makes = []
    for i in range(400):
        m = ["to_tsquery", "websearch_to_tsquery", "plainto_tsquery", "phraseto_tsquery"][i % 4]
        makes.append((m, standard(r, 3) if m == "to_tsquery" else web(r) if m.startswith("web") else words(r)))
    sql = ["SET client_min_messages = warning;"]
    for v, q in matches:
        sql.append(
            f"SELECT 'match', {dollar(v)}::tsvector::text, {dollar(q)}::tsquery::text, "
            f"{dollar(v)}::tsvector @@ {dollar(q)}::tsquery;"
        )
    for m, t in makes:
        sql.append(f"SELECT 'make', {m}('english', {dollar(t)})::text;")
    out = subprocess.run(
        ["docker", "exec", "-i", a.container, "psql", "-U", "postgres", "-d", "postgres",
         "-At", "-F", "\t", "-v", "ON_ERROR_STOP=1"],
        input="\n".join(sql), capture_output=True, text=True, check=True,
    ).stdout.splitlines()
    rows = [l.split("\t") for l in out if l.startswith(("match\t", "make\t"))]
    assert len(rows) == len(matches) + len(makes), (len(rows), len(matches) + len(makes))
    with open(a.out, "w") as f:
        for (v, q), row in zip(matches, rows):
            f.write("\t".join(["match", v, q, row[1], row[2], row[3]]) + "\n")
        for (m, t), row in zip(makes, rows[len(matches):]):
            f.write("\t".join(["make", m, t, row[1]]) + "\n")
    print(f"{len(rows)} cases to {a.out}")


if __name__ == "__main__":
    main()
