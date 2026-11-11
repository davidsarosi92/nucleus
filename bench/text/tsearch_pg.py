"""PostgreSQL's answers for pg-compat's text search (PLAN.md §M14.21 T8).

Random `tsvector`s and `tsquery`s — phrases at distances 0 to 3 over
operators, negations, prefixes, weights, a term without positions — and
the query makers over words PostgreSQL's English configuration and
nucleus's analyze alike, asked of a PostgreSQL server; written to
`modules/pg-compat/tests/tsearch_pg.tsv`, which `tests/tsearch.rs` holds
nucleus to:

    match <tab> vector <tab> query <tab> vector's text <tab> query's text <tab> t|f
    make <tab> maker <tab> text <tab> query's text
    rank <tab> vector <tab> query <tab> normalization <tab> weights <tab> ts_rank <tab>
         ts_rank_cd <tab> both normalized <tab> both weighted (6 reals)
    headline <tab> document <tab> query's text <tab> options <tab> ts_headline
    send <tab> vector <tab> query <tab> tsvectorsend <tab> tsquerysend

The binary forms (`send`) are of random values and of the edges: positions
past 16,383 and more than 256 of a term (PostgreSQL keeps neither),
quoted terms, the longest phrase distance; a query's of the value its text
form gives.

The headlines' documents are plain English — words, numbers, commas and
full stops — which PostgreSQL's parser and nucleus's words split alike.

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


PROSE = ["cat", "cats", "the", "a", "dog", "dogs", "sat", "mat", "on", "running", "of", "fish",
         "and", "in", "is", "was", "red", "big", "small", "house", "garden", "tree", "trees",
         "bird", "birds", "flew", "over", "quick", "brown", "fox", "jumps", "lazy", "river",
         "water", "green", "hills", "ran", "slowly", "together", "under", "bridge", "it"]
OPTIONS = ["", "MaxWords=10, MinWords=5", "MaxWords=8, MinWords=3, ShortWord=0",
           "MaxFragments=2, MaxWords=8, MinWords=3", "MaxFragments=1",
           "MaxFragments=3, MaxWords=6, MinWords=2, FragmentDelimiter=' | '",
           "HighlightAll=true", "StartSel=<<, StopSel=>>", "ShortWord=5, MinWords=4, MaxWords=9",
           "maxfragments=2, maxwords=5, minwords=1, startsel=\"[\", stopsel=']'",
           "HighlightAll=yes, MaxFragments=2, MaxWords=5, MinWords=2"]


def document(r):
    out, start = [], True
    for _ in range(r.randint(3, 70)):
        w = str(r.randint(1, 999)) if r.random() < 0.05 else r.choice(PROSE)
        out.append(w.capitalize() if start else w)
        start = False
        k = r.random()
        if k < 0.08:
            out[-1] += "."
            start = True
        elif k < 0.15:
            out[-1] += ","
    return " ".join(out) + "."


def hquery(r, depth):
    if depth == 0 or r.random() < 0.35:
        w = r.choice(PROSE)
        return w + ":*" if r.random() < 0.1 else w
    a, b = hquery(r, depth - 1), hquery(r, depth - 1)
    return r.choice([f"({a} & {b})", f"({a} | {b})", f"!{a}", f"({a} <-> {b})", f"({a} <2> {b})",
                     f"({a} & {b})"])


def words(r):
    return " ".join(r.choice(WORDS) for _ in range(r.randint(1, 5)))


SEND_VECTORS = [
    "",
    "a:16383,16384,20000B",
    "x:" + ",".join(str(p) for p in range(1, 301)),
    "y:" + ",".join(f"{p}{'ABCD'[p % 4]}" for p in range(16000, 16400, 3)),
    "'it''s':1 'a\\\\b':2A 'á' 'ő':3C,4B",
]
SEND_QUERIES = ["", "a <16384> b", "!!a", "'it''s':*AB & ( 'x' | !y )", "a <0> b <-> c:D"]


def dollar(s):
    return "$q$" + s + "$q$"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--container", default="nucleus-paradedb")
    p.add_argument("--out", default="modules/pg-compat/tests/tsearch_pg.tsv")
    a = p.parse_args()
    r = random.Random(7)
    matches = [(vector(r), query(r, 3)) for _ in range(2000)]
    ranks = [(vector(r), query(r, 3), r.randrange(64),
              "{" + ",".join(r.choice(["0.05", "0.3", "-1", "0.9", "1", "0.25"]) for _ in range(4)) + "}")
             for _ in range(1500)]
    makes = []
    for i in range(400):
        m = ["to_tsquery", "websearch_to_tsquery", "plainto_tsquery", "phraseto_tsquery"][i % 4]
        makes.append((m, standard(r, 3) if m == "to_tsquery" else web(r) if m.startswith("web") else words(r)))
    makes += [("to_tsquery", "cat <16384> the <-> dog"), ("to_tsquery", "a <16384> b")]
    heads = [(document(r), hquery(r, 2), r.choice(OPTIONS)) for _ in range(1200)]
    sends = [(vector(r), query(r, 3)) for _ in range(300)]
    sends += [(v, q) for v, q in zip(SEND_VECTORS, SEND_QUERIES)]
    sql = ["SET client_min_messages = warning;"]
    for v, q in matches:
        sql.append(
            f"SELECT 'match', {dollar(v)}::tsvector::text, {dollar(q)}::tsquery::text, "
            f"{dollar(v)}::tsvector @@ {dollar(q)}::tsquery;"
        )
    for m, t in makes:
        sql.append(f"SELECT 'make', {m}('english', {dollar(t)})::text;")
    for v, q, n, w in ranks:
        tv, tq, tw = dollar(v) + "::tsvector", dollar(q) + "::tsquery", f"'{w}'::real[]"
        sql.append(f"SELECT 'rank', ts_rank({tv}, {tq}), ts_rank_cd({tv}, {tq}), "
                   f"ts_rank({tv}, {tq}, {n}), ts_rank_cd({tv}, {tq}, {n}), "
                   f"ts_rank({tw}, {tv}, {tq}), ts_rank_cd({tw}, {tv}, {tq});")
    for d, q, o in heads:
        sql.append(f"SELECT 'headline', to_tsquery('english', {dollar(q)})::text, "
                   f"ts_headline('english', {dollar(d)}, to_tsquery('english', {dollar(q)}), "
                   f"{dollar(o)});")
    for v, q in sends:
        # A query as its text form gives it: PostgreSQL's prints `a & (b & c)`
        # without the parentheses, and nucleus keeps values as their text gives
        # them (DOCS.md §8.22).
        sql.append(f"SELECT 'send', tsvectorsend({dollar(v)}::tsvector), "
                   f"tsquerysend({dollar(q)}::tsquery::text::tsquery);")
    out = subprocess.run(
        ["docker", "exec", "-i", a.container, "psql", "-U", "postgres", "-d", "postgres",
         "-At", "-F", "\t", "-v", "ON_ERROR_STOP=1"],
        input="\n".join(sql), capture_output=True, text=True, check=True,
    ).stdout.splitlines()
    kinds = ("match\t", "make\t", "rank\t", "headline\t", "send\t")
    rows = [l.split("\t") for l in out if l.startswith(kinds)]
    assert len(rows) == len(matches) + len(makes) + len(ranks) + len(heads) + len(sends), len(rows)
    with open(a.out, "w") as f:
        for (v, q), row in zip(matches, rows):
            f.write("\t".join(["match", v, q, row[1], row[2], row[3]]) + "\n")
        for (m, t), row in zip(makes, rows[len(matches):]):
            f.write("\t".join(["make", m, t, row[1]]) + "\n")
        for (v, q, n, w), row in zip(ranks, rows[len(matches) + len(makes):]):
            f.write("\t".join(["rank", v, q, str(n), w] + row[1:]) + "\n")
        for (d, q, o), row in zip(heads, rows[len(matches) + len(makes) + len(ranks):]):
            f.write("\t".join(["headline", d, row[1], o, row[2] if len(row) > 2 else ""]) + "\n")
        at = len(matches) + len(makes) + len(ranks) + len(heads)
        for (v, q), row in zip(sends, rows[at:]):
            f.write("\t".join(["send", v, q, row[1], row[2]]) + "\n")
    print(f"{len(rows)} cases to {a.out}")


if __name__ == "__main__":
    main()
