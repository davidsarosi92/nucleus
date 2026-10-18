"""The Python driver against a running nucleus server (NUCLEUS_HOST,
NUCLEUS_PORT; scripts/driver-tests.sh starts one)."""

import datetime as dt
import decimal
import os
import sys
import unittest
import uuid

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import nucleus  # noqa: E402

HOST = os.environ.get("NUCLEUS_HOST", "127.0.0.1")
PORT = int(os.environ.get("NUCLEUS_PORT", str(nucleus.DEFAULT_PORT)))


def connect(**kw):
    return nucleus.connect(HOST, PORT, **kw)


def table():
    return "py_" + uuid.uuid4().hex[:8]


class Conformance(unittest.TestCase):
    def test_types_both_ways(self):
        t = table()
        with connect() as c:
            c.execute(f"CREATE TABLE {t} (id BIGINT PRIMARY KEY, b BOOLEAN, f FLOAT, s TEXT, n NUMERIC(10, 2), "
                      "d DATE, ts TIMESTAMP, tz TIMESTAMPTZ, iv INTERVAL, tags TEXT[])")
            row = (
                1, True, 0.5, "héllo", decimal.Decimal("123.45"), dt.date(2024, 2, 29),
                dt.datetime(2024, 3, 10, 14, 5, 6, 500000),
                dt.datetime(2024, 3, 10, 12, 0, tzinfo=dt.timezone.utc),
                nucleus.Interval(1, 2, 3_000_000), ["a", None, "c"],
            )
            r = c.execute(f"INSERT INTO {t} VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)", row)
            self.assertEqual((r.command, r.count), ("INSERT", 1))
            r = c.execute(f"SELECT * FROM {t}")
            self.assertEqual(r.rows, [row])
            self.assertEqual([col.type for col in r.columns][4:6], ["numeric", "date"])
            self.assertEqual(r.columns[9].type, "text[]")
            # NULLs, big integers, timedelta and text parameters.
            c.execute(f"INSERT INTO {t} (id, iv) VALUES ($1, $2)", [2**62, dt.timedelta(days=1, seconds=5)])
            self.assertEqual(c.execute(f"SELECT b, iv FROM {t} WHERE id = $1", [2**62]).rows,
                             [(None, nucleus.Interval(0, 1, 5_000_000))])
            self.assertEqual(c.execute(f"SELECT id FROM {t} WHERE d = $1", ["2024-02-29"]).rows, [(1,)])
            self.assertEqual(c.execute("SELECT 1.50 + 1, 7 / 2.0").rows,
                             [(decimal.Decimal("2.50"), decimal.Decimal("3.5000000000000000"))])

    def test_uuid(self):
        t = table()
        u = uuid.UUID("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")
        with connect() as c:
            c.execute(f"CREATE TABLE {t} (id UUID PRIMARY KEY, n INT)")
            c.execute(f"INSERT INTO {t} VALUES ($1, 1)", [u])
            r = c.execute(f"SELECT id, id::text, gen_random_uuid() FROM {t}")
            self.assertEqual(r.columns[0].type, "uuid")
            self.assertEqual(r.rows[0][:2], (u, str(u)))
            self.assertIsInstance(r.rows[0][2], uuid.UUID)
            self.assertEqual(r.rows[0][2].version, 4)
            self.assertEqual(c.execute(f"SELECT n FROM {t} WHERE id = $1", [str(u).upper()]).rows, [(1,)])

    def test_errors(self):
        with connect() as c:
            with self.assertRaises(nucleus.ProgrammingError) as e:
                c.execute("SELECT * FROM nosuch")
            self.assertEqual(e.exception.code, "42P01")
            with self.assertRaises(nucleus.DataError):
                c.execute("SELECT 1 / 0")
            t = table()
            c.execute(f"CREATE TABLE {t} (id INT PRIMARY KEY)")
            c.execute(f"INSERT INTO {t} VALUES (1)")
            with self.assertRaises(nucleus.IntegrityError) as e:
                c.execute(f"INSERT INTO {t} VALUES (1)")
            self.assertEqual(e.exception.code, "23505")
            # Several statements: results, until the first error.
            self.assertEqual(len(c.query("SELECT 1; SELECT 2")), 2)
            self.assertEqual(c.execute("SELECT 3").rows, [(3,)])

    def test_transactions(self):
        t = table()
        with connect() as c:
            c.execute(f"CREATE TABLE {t} (id INT PRIMARY KEY)")
            with c.transaction():
                c.execute(f"INSERT INTO {t} VALUES (1)")
                self.assertTrue(c.in_transaction)
            with self.assertRaises(RuntimeError):
                with c.transaction():
                    c.execute(f"INSERT INTO {t} VALUES (2)")
                    raise RuntimeError("roll back")
            self.assertFalse(c.in_transaction)
            self.assertEqual(c.execute(f"SELECT id FROM {t}").rows, [(1,)])
            c.execute("BEGIN")
            with self.assertRaises(nucleus.DataError):
                c.execute("SELECT 1 / 0")
            self.assertEqual(c.status, 2)
            c.rollback()
            self.assertEqual(c.status, 0)

    def test_cursor(self):
        t = table()
        with connect() as c:
            cur = c.cursor()
            cur.execute(f"CREATE TABLE {t} (id INT PRIMARY KEY, name TEXT)")
            cur.executemany(f"INSERT INTO {t} VALUES ($1, $2)", [(1, "a"), (2, "b")])
            self.assertEqual(cur.rowcount, 2)
            cur.execute(f"SELECT id, name FROM {t} ORDER BY id")
            self.assertEqual(cur.description[1][0], "name")
            self.assertEqual(cur.fetchone(), (1, "a"))
            self.assertEqual(cur.fetchall(), [(2, "b")])

    def test_authentication(self):
        with self.assertRaises(nucleus.OperationalError) as e:
            connect(password="wrong")
        self.assertEqual(e.exception.code, "28P01")


if __name__ == "__main__":
    unittest.main()
