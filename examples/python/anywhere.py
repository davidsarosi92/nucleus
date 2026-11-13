"""One program, two places: a database inside this process, or a server —
only the connection string differs (PLAN.md section M14.23).

    python3 examples/python/anywhere.py                     # file:notes.db, in this process
    python3 examples/python/anywhere.py :memory:            # in memory
    python3 examples/python/anywhere.py nucleus://nucleus:nucleus@127.0.0.1:20035/nucleus

In the process it needs the embedded edition (the nucleus-db wheel for
this platform, or NUCLEUS_EMBEDDED_LIB naming the library a build made:
cargo build --release -p nucleus-embedded).
"""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "drivers", "python"))
import nucleus  # noqa: E402

url = sys.argv[1] if len(sys.argv) > 1 else "file:notes.db"
with nucleus.connect(url) as conn:
    print("connected:", conn.server_version)
    conn.execute("DROP TABLE IF EXISTS anywhere_notes")
    conn.execute("CREATE TABLE anywhere_notes (id INT PRIMARY KEY, body TEXT, v vector(3))")
    conn.execute_many("INSERT INTO anywhere_notes VALUES ($1, $2, $3)", [
        (1, "buy apples", [1.0, 0.0, 0.0]),
        (2, "call the bank", [0.0, 1.0, 0.0]),
        (3, "apple pie recipe", [0.9, 0.1, 0.0]),
    ])
    nearest = conn.execute(
        "SELECT id, body FROM anywhere_notes ORDER BY v <-> $1 LIMIT 2", [[1.0, 0.0, 0.0]]).rows
    print("nearest to apples:", nearest)
    assert [r[0] for r in nearest] == [1, 3]
    try:
        cols = conn.numpy("SELECT id, v FROM anywhere_notes ORDER BY id")
        print("as numpy:", cols["id"].dtype, cols["id"].tolist())
    except ImportError:
        pass  # numpy is optional
    conn.execute("DROP TABLE anywhere_notes")
print("done")
