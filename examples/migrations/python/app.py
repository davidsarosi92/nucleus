"""The sample project's Python program (PLAN.md §M14.16 M6): the functions
nucleus codegen made of queries.sql (queries.py), on a server whose schema
migrations/ made.  Run by ../run.sh."""

import datetime
import os
import sys

here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(here, "..", "..", "..", "drivers", "python"))
import nucleus  # noqa: E402
import queries  # noqa: E402

conn = nucleus.connect(port=int(os.environ.get("NUCLEUS_PORT", nucleus.DEFAULT_PORT)))

# A transaction: the user and their posts, or none of them.
with conn.transaction():
    ann = queries.create_user(conn, 200, "ann@py.example", "Ann")
    assert ann is not None and ann.plan == "free" and isinstance(ann.joined, datetime.datetime)
    queries.add_post(conn, 200, 200, "Hello", "The first post.")
    queries.add_post(conn, 201, 200, "Again", None)

ann = queries.user_by_email(conn, "ann@py.example")
assert ann is not None and ann.display_name == "Ann"
assert queries.user_by_email(conn, "nobody@py.example") is None
assert queries.upgrade(conn, ann.id) == 1

posts = [p for p in queries.posts_on_plan(conn, "pro", 10) if p.author == "ann@py.example"]
assert [(p.title, p.body) for p in posts] == [("Again", None), ("Hello", "The first post.")], posts
counts = {c.email: c.posts for c in queries.post_counts(conn)}
assert counts["ann@py.example"] == 2, counts
conn.close()
print("migrations example (python): ok")
