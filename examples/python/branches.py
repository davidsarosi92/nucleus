"""An experiment on a branch (PLAN.md section M14.13): a copy of the
database made in constant time and space, changed freely — a migration
tried, its effect measured — while the database it came from goes on
unchanged; then thrown away (or kept, to try the next idea on).

    python3 examples/python/branches.py                    # the local server
    python3 examples/python/branches.py file:app.db        # in this process
    python3 examples/python/branches.py nucleus://nucleus:nucleus@127.0.0.1:20035/nucleus

A branch is a database of its own: a client names it as its database
(nucleus://…/experiment, or database="experiment").  An agent can be
given one to work in, so whatever it does stays there.
"""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "drivers", "python"))
import nucleus  # noqa: E402

port = int(os.environ.get("NUCLEUS_PORT", nucleus.DEFAULT_PORT))
url = sys.argv[1] if len(sys.argv) > 1 else f"nucleus://nucleus:nucleus@127.0.0.1:{port}/nucleus"

with nucleus.connect(url) as db:
    print("connected:", db.server_version)
    db.execute("DROP BRANCH IF EXISTS experiment")
    db.execute("DROP TABLE IF EXISTS orders")
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, customer TEXT, total NUMERIC(10, 2))")
    db.execute(
        "INSERT INTO orders SELECT i, 'c' || (i % 100), (i % 997) / 10.0 "
        "FROM generate_series(1, 20000) i"
    )

    # The copy: no rows are copied, whatever the size.
    db.execute("CREATE BRANCH experiment")
    print("branches:", db.execute("SELECT name, parent FROM nc_catalog.branches").rows)

    with nucleus.connect(url, database="experiment") as exp:
        # The migration tried on the branch: a column, filled, indexed.
        exp.execute("ALTER TABLE orders ADD COLUMN tier TEXT")
        exp.execute(
            "UPDATE orders SET tier = CASE WHEN total >= 50 THEN 'large' ELSE 'small' END"
        )
        exp.execute("CREATE INDEX orders_tier ON orders (tier)")
        large = exp.execute("SELECT count(*) FROM orders WHERE tier = 'large'").rows[0][0]
        print("on the branch, large orders:", large)
        assert large > 0

        # Meanwhile the database goes on, unchanged by it.
        db.execute("INSERT INTO orders VALUES (20001, 'c1', 12.50)")
        try:
            db.execute("SELECT tier FROM orders")
            raise AssertionError("the branch's column leaked into the database")
        except nucleus.Error as e:
            assert e.code == "42703", e.code  # undefined_column
        assert exp.execute("SELECT count(*) FROM orders").rows[0][0] == 20000
        assert db.execute("SELECT count(*) FROM orders").rows[0][0] == 20001

    # Done with it: dropped, in time proportional to what it changed.
    db.execute("DROP BRANCH experiment")
    assert db.execute("SELECT count(*) FROM nc_catalog.branches").rows[0][0] == 1
    print("ok")
