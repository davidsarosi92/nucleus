"""nucleus from Python: connect, create, insert with parameters, query, a
transaction.  Run a server first (`nucleus server ./mydb`), then:

    PYTHONPATH=drivers/python python3 examples/python/quickstart.py
"""

import datetime
import decimal
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "drivers", "python"))
import nucleus  # noqa: E402

# The defaults reach a local server: 127.0.0.1:20035, user/password nucleus.
conn = nucleus.connect(port=int(os.environ.get("NUCLEUS_PORT", nucleus.DEFAULT_PORT)))
print("connected to", conn.server_version)

conn.execute("DROP TABLE IF EXISTS py_orders")
conn.execute("""CREATE TABLE py_orders (
    id INT PRIMARY KEY,
    customer TEXT NOT NULL,
    total NUMERIC(10, 2),
    placed DATE,
    tags TEXT[]
)""")

orders = [
    (1, "Anna", decimal.Decimal("19.99"), datetime.date(2024, 5, 1), ["new"]),
    (2, "Béla", decimal.Decimal("120.00"), datetime.date(2024, 5, 3), ["vip", "repeat"]),
]
for order in orders:
    conn.execute("INSERT INTO py_orders VALUES ($1, $2, $3, $4, $5)", order)

# A transaction: both updates or neither.
with conn.transaction():
    conn.execute("UPDATE py_orders SET total = total * 0.9 WHERE customer = $1", ["Béla"])
    conn.execute("INSERT INTO py_orders VALUES ($1, $2, $3, $4, $5)",
                 [3, "Csilla", decimal.Decimal("5.50"), datetime.date.today(), []])

result = conn.execute("SELECT id, customer, total, placed, tags FROM py_orders ORDER BY id")
for record in result.records():
    print(record)

total = conn.execute("SELECT sum(total) FROM py_orders").rows[0][0]
print("sum of totals:", total, type(total).__name__)

try:
    conn.execute("INSERT INTO py_orders VALUES (1, 'dup', 0, NULL, NULL)")
except nucleus.IntegrityError as e:
    print("as expected:", e.code, e.message)

conn.execute("DROP TABLE py_orders")
conn.close()
