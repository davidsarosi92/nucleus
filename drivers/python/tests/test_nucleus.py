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


def _has(module):
    try:
        __import__(module)
        return True
    except ImportError:
        return False


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

    def test_core_types(self):
        import ipaddress
        t = table()
        mood = t + "_mood"
        with connect() as c:
            c.execute(f"CREATE TYPE {mood} AS ENUM ('sad', 'happy')")
            c.execute(f"CREATE TABLE {t} (s SMALLINT, i INTEGER, r REAL, b BYTEA, tm TIME, "
                      f"ch CHAR(3), ip INET, net CIDR, m {mood})")
            row = (-2, 70000, 0.1, b"\x00\xff", dt.time(12, 34, 56, 500000), "ab ",
                   ipaddress.ip_interface("10.1.2.3/8"), ipaddress.ip_network("2001:db8::/32"),
                   "happy")
            c.execute(f"INSERT INTO {t} VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)", list(row))
            r = c.execute(f"SELECT * FROM {t}")
            self.assertEqual(r.rows, [row])
            self.assertEqual([col.type for col in r.columns],
                             ["smallint", "integer", "real", "bytea", "time", "character",
                              "inet", "cidr", mood])
            # A plain address is a host; 24:00:00 is the end of the day.
            r = c.execute("SELECT $1::inet, time '24:00'", [ipaddress.ip_address("::1")])
            self.assertEqual(r.rows, [(ipaddress.ip_interface("::1/128"), dt.time.max)])
            self.assertEqual(c.execute(f"SELECT count(*) FROM {t} WHERE b = $1 AND ip <<= $2",
                                       [bytearray(b"\x00\xff"), ipaddress.ip_network("10.0.0.0/8")]).rows,
                             [(1,)])

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
            # What to do about it: the hint (protocol version 5).
            with self.assertRaises(nucleus.ProgrammingError) as e:
                c.execute(f"SELECT ib FROM {t}")
            self.assertEqual(e.exception.hint, 'Perhaps you meant column "id".')
            with self.assertRaises(nucleus.DataError) as e:
                c.execute("SELECT 'x'::integer")
            self.assertIsNone(e.exception.hint)
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

    def test_stream(self):
        with connect() as c:
            big = "SELECT a, b FROM generate_series(1, 100000) a, generate_series(1, 100000) b"
            # Ten billion rows: only what is read is produced.
            with c.stream(big, fetch=500) as s:
                self.assertEqual([col.name for col in s.columns], ["a", "b"])
                for i, row in enumerate(s):
                    if i == 1200:
                        break
            self.assertEqual(s.count, 1500)
            self.assertEqual(c.execute("SELECT 1").rows, [(1,)])
            s = c.stream("SELECT i FROM generate_series(1, 2500) i", fetch=100)
            self.assertEqual(sum(1 for _ in s), 2500)
            self.assertEqual((s.command, s.count), ("SELECT", 2500))
            # An error before the first row raises at once; after rows,
            # from the iteration.
            with self.assertRaises(nucleus.ProgrammingError):
                c.stream("SELECT nope")
            got = []
            with self.assertRaises(nucleus.DataError) as e:
                for row in c.stream("SELECT 1 / (300 - i) FROM generate_series(1, 400) i", fetch=64):
                    got.append(row)
            self.assertEqual(e.exception.code, "22012")
            self.assertEqual(len(got), 299)
            # A stream left unread is closed by the next statement; a
            # DB-API cursor reads lazily.
            c.stream(big)
            cur = c.cursor()
            cur.itersize = 10
            cur.execute("SELECT i FROM generate_series(1, 25) i")
            self.assertEqual(cur.rowcount, -1)
            self.assertEqual(len(cur.fetchmany(12)), 12)
            self.assertEqual(len(cur.fetchall()), 13)
            self.assertEqual(cur.rowcount, 25)
            with self.assertRaises(nucleus.ProgrammingError):
                c.stream("SELECT 1; SELECT 2")
            self.assertEqual(c.status, 0)

    def _columnar_table(self, c):
        t = table()
        c.execute(f"CREATE TABLE {t} (id INT PRIMARY KEY, b BOOLEAN, s SMALLINT, big BIGINT, r REAL, f FLOAT, "
                  "n NUMERIC(10, 2), tx TEXT, ch CHAR(3), bt BYTEA, d DATE, tm TIME, ts TIMESTAMP, tz TIMESTAMPTZ, "
                  "iv INTERVAL, u UUID, ip INET, net CIDR, arr INT[])")
        c.execute(f"INSERT INTO {t} SELECT i, i % 2 = 0, i::smallint, i * 1000000000000, i / 4.0, i / 8.0, i / 3.0, "
                  "'v' || i, 'c' || (i % 10), decode('ff00', 'hex'), DATE '2024-01-01' + i, "
                  "TIME '12:00' + i * INTERVAL '1 second', TIMESTAMP '2024-01-01' + i * INTERVAL '1 hour', "
                  "TIMESTAMPTZ '2024-01-01 00:00Z' + i * INTERVAL '1 minute', i * INTERVAL '1 day', "
                  "gen_random_uuid(), '10.0.0.1'::inet, '10.0.0.0/8'::cidr, ARRAY[i, NULL] "
                  "FROM generate_series(1, 300) i")
        c.execute(f"UPDATE {t} SET b = NULL, s = NULL, big = NULL, r = NULL, f = NULL, n = NULL, tx = NULL, "
                  "ch = NULL, bt = NULL, d = NULL, tm = NULL, ts = NULL, tz = NULL, iv = NULL, u = NULL, "
                  "ip = NULL, net = NULL, arr = NULL WHERE id % 3 = 0")
        return t

    def test_columnar(self):
        with connect() as c:
            t = self._columnar_table(c)
            sql = f"SELECT * FROM {t} ORDER BY id"
            want = c.execute(sql).rows
            got, sizes = [], []
            with c.batches(sql, rows=128) as bs:
                for b in bs:
                    sizes.append(len(b))
                    got += b.rows()
            self.assertEqual(sizes, [128, 128, 44])
            self.assertEqual(bs.count, 300)
            self.assertEqual(got, want)
            # An empty result has no batch; an error after batches raises.
            self.assertEqual(list(c.batches(f"SELECT * FROM {t} WHERE false")), [])
            n = 0
            with self.assertRaises(nucleus.DataError):
                for b in c.batches("SELECT 1 / (250 - i) FROM generate_series(1, 400) i", rows=100):
                    n += len(b)
            self.assertEqual(n, 249)

    @unittest.skipUnless(_has("numpy"), "numpy is not installed")
    def test_numpy(self):
        import numpy as np
        with connect() as c:
            t = self._columnar_table(c)
            b = next(iter(c.batches(f"SELECT id, f, d, ts, tm, iv, tx FROM {t} ORDER BY id")))
            ids = b.column("id").to_numpy()
            # Read in place: the array is a view of the batch.
            self.assertFalse(ids.flags.owndata)
            self.assertEqual(ids.dtype, np.int32)
            self.assertEqual(ids[:3].tolist(), [1, 2, 3])
            f = b.column("f").to_numpy()
            self.assertTrue(np.ma.is_masked(f[2]))
            self.assertEqual(float(f[3]), 0.5)
            self.assertEqual(str(b.column("d").to_numpy()[0]), "2024-01-02")
            self.assertEqual(str(b.column("ts").to_numpy()[0]), "2024-01-01T01:00:00.000000")
            self.assertEqual(int(b.column("tm").to_numpy()[0] / np.timedelta64(1, "s")), 12 * 3600 + 1)
            self.assertEqual(int(b.column("iv").to_numpy()[0]["days"]), 1)
            self.assertEqual(b.column("tx").to_numpy()[0], "v1")
            self.assertIsNone(b.column("tx").to_numpy()[2])
            cols = c.numpy(f"SELECT id, big FROM {t} ORDER BY id")
            self.assertEqual(len(cols["id"]), 300)
            self.assertEqual(int(cols["big"].sum()), sum(i * 10**12 for i in range(1, 301) if i % 3))

    @unittest.skipUnless(_has("pyarrow"), "pyarrow is not installed")
    def test_arrow(self):
        import datetime as dt
        with connect() as c:
            t = self._columnar_table(c)
            sql = f"SELECT * FROM {t} ORDER BY id"
            want = c.execute(sql).rows
            tab = c.arrow(sql)
            self.assertEqual(tab.num_rows, 300)
            self.assertEqual(str(tab.schema.field("tx").type), "string")
            self.assertEqual(str(tab.schema.field("tz").type), "timestamp[us, tz=UTC]")
            got = tab.to_pylist()
            for w, g in zip(want, got):
                self.assertEqual(g["id"], w[0])
                self.assertEqual(g["big"], w[3])
                self.assertEqual(g["n"], w[6])
                self.assertEqual(g["tx"], w[7])
                self.assertEqual(g["d"], w[10])
                self.assertEqual(g["ts"], w[12])
                self.assertEqual(g["tz"], w[13])
                self.assertEqual(g["tm"], w[11])
                self.assertEqual(g["arr"], w[18])
            self.assertEqual(got[0]["iv"].days, 1)
            self.assertEqual(c.arrow(f"SELECT id FROM {t} WHERE false").num_rows, 0)
            if _has("pandas"):
                df = c.pandas(sql)
                self.assertEqual(df.shape, (300, 19))
                self.assertEqual(df["id"].iloc[0], 1)
            if _has("polars"):
                pl = c.polars(f"SELECT id, tx FROM {t} ORDER BY id")
                self.assertEqual(pl.shape, (300, 2))
                self.assertEqual(pl["tx"][0], "v1")
            self.assertIsInstance(got[0]["d"], dt.date)

    def test_copy_and_load(self):
        import datetime as dt
        with connect() as c:
            t = table()
            c.execute(f"CREATE TABLE {t} (id INT PRIMARY KEY, name TEXT DEFAULT 'none', score FLOAT, "
                      "born DATE, at TIMESTAMPTZ, n NUMERIC(8, 2), ok BOOLEAN, u UUID, tags TEXT[])")
            self.assertEqual(c.copy(f"COPY {t} (id, name) FROM STDIN", "1\tann\n2\t\\N\n"), 2)
            self.assertEqual(c.copy(f"COPY {t} (id, score) FROM STDIN WITH (FORMAT csv, HEADER)",
                                    iter([b"id,score\n3,", b"1.5\n"])), 1)
            with self.assertRaises(nucleus.IntegrityError) as e:
                c.copy(f"COPY {t} (id) FROM STDIN", b"9\n1\n")
            self.assertIn("line 2", str(e.exception))
            # Rows, as columnar batches.
            u = uuid.uuid4()
            n = c.load(t, [(10, "x", 0.5, dt.date(2024, 1, 2), dt.datetime(2024, 1, 2, 3, 4, tzinfo=dt.timezone.utc),
                            decimal.Decimal("1.25"), True, u, ["a", None]),
                           (11, None, None, None, None, None, None, None, None)])
            self.assertEqual(n, 2)
            got = c.execute(f"SELECT name, score, born, at, n, ok, u, tags FROM {t} WHERE id = 10").rows[0]
            self.assertEqual(got, ("x", 0.5, dt.date(2024, 1, 2), dt.datetime(2024, 1, 2, 3, 4, tzinfo=dt.timezone.utc),
                                   decimal.Decimal("1.25"), True, u, ["a", None]))
            # A dict of lists, some columns only (the rest default).
            self.assertEqual(c.load(t, {"id": [20, 21], "score": [1, 2]}), 2)
            self.assertEqual(c.execute(f"SELECT name, score FROM {t} WHERE id = 21").rows, [("none", 2.0)])
            with self.assertRaises(nucleus.DataError):
                c.load(t, {"id": [30], "born": ["not a date"]})
            self.assertEqual(c.execute(f"SELECT count(*) FROM {t} WHERE id = 30").rows, [(0,)])
            # A COPY without data, through a plain query, is refused.
            with self.assertRaises(nucleus.OperationalError):
                c.execute(f"COPY {t} FROM STDIN")
            self.assertEqual(c.execute("SELECT 1").rows, [(1,)])

    @unittest.skipUnless(_has("numpy") and _has("pandas"), "numpy and pandas are not installed")
    def test_load_dataframes(self):
        import numpy as np
        import pandas as pd
        with connect() as c:
            t = table()
            c.execute(f"CREATE TABLE {t} (id BIGINT PRIMARY KEY, small SMALLINT, x FLOAT, r REAL, d DATE, "
                      "ts TIMESTAMP, tz TIMESTAMPTZ, label TEXT, flag BOOLEAN)")
            n = 200_000
            ids = np.arange(n, dtype=np.int64)
            df = pd.DataFrame({
                "id": ids,
                "small": (ids % 1000).astype(np.int16),
                "x": ids * 0.5,
                "r": (ids % 7).astype(np.float32),
                "d": pd.Timestamp("2024-01-01") + pd.to_timedelta(ids % 365, unit="D"),
                "ts": pd.Timestamp("2024-01-01") + pd.to_timedelta(ids, unit="s"),
                "tz": pd.Timestamp("2024-01-01", tz="Europe/Budapest") + pd.to_timedelta(ids, unit="m"),
                "label": ["l" + str(i % 10) for i in range(n)],
                "flag": ids % 2 == 0,
            })
            df.loc[3, "x"] = np.nan          # pandas' missing value: NULL
            df["small"] = df["small"].astype("Int16")
            df.loc[4, "small"] = pd.NA
            self.assertEqual(c.load(t, df), n)
            back = c.pandas(f"SELECT * FROM {t} ORDER BY id")
            self.assertEqual(len(back), n)
            self.assertTrue(pd.isna(back["x"][3]) and pd.isna(back["small"][4]))
            self.assertEqual(int(back["small"][1999]), 999)
            self.assertEqual(float(back["x"][10]), 5.0)
            self.assertEqual(str(back["d"][1].date() if hasattr(back["d"][1], "date") else back["d"][1]), "2024-01-02")
            self.assertEqual(str(pd.Timestamp(back["ts"][61])), "2024-01-01 00:01:01")
            # 2024-01-01 00:00 in Budapest is 2023-12-31 23:00 UTC.
            self.assertEqual(pd.Timestamp(back["tz"][0]).tz_convert("UTC").strftime("%Y-%m-%d %H:%M"), "2023-12-31 23:00")
            self.assertEqual(back["label"][12], "l2")
            self.assertEqual(bool(back["flag"][2]), True)
            # numpy arrays (a masked one for NULLs) in a dict.
            t2 = table()
            c.execute(f"CREATE TABLE {t2} (id INT, v FLOAT)")
            v = np.ma.masked_array(np.arange(5, dtype=float), mask=[0, 1, 0, 0, 0])
            self.assertEqual(c.load(t2, {"id": np.arange(5, dtype=np.int32), "v": v}), 5)
            self.assertEqual(c.execute(f"SELECT count(v), sum(v) FROM {t2}").rows, [(4, 9.0)])
            with self.assertRaises(nucleus.DataError):
                c.load(t2, {"id": np.array([2**40]), "v": np.array([1.0])})
            if _has("pyarrow"):
                import pyarrow as pa
                self.assertEqual(c.load(t2, pa.table({"id": pa.array([7, None], pa.int32()), "v": [1.5, 2.5]})), 2)
                self.assertEqual(c.execute(f"SELECT count(*), count(id) FROM {t2} WHERE v > 1").rows, [(5, 4)])

    def test_authentication(self):
        with self.assertRaises(nucleus.OperationalError) as e:
            connect(password="wrong")
        self.assertEqual(e.exception.code, "28P01")

    @unittest.skipUnless(os.environ.get("NUCLEUS_LDAP_USER"), "the server has no directory")
    def test_directory(self):
        user, pw = os.environ["NUCLEUS_LDAP_USER"], os.environ["NUCLEUS_LDAP_PASSWORD"]
        ca = os.environ["NUCLEUS_TLS_CA"]
        with connect(user=user, password=pw, tls_ca=ca) as c:
            self.assertEqual(c.execute("SELECT current_user").rows, [(user,)])
        with self.assertRaises(nucleus.OperationalError):
            connect(user=user, password="wrong", tls_ca=ca)
        # Not without TLS.
        with self.assertRaises(nucleus.Error):
            connect(user=user, password=pw)

    @unittest.skipUnless(os.environ.get("NUCLEUS_TOKEN"), "the server has no issuer")
    def test_token(self):
        with connect(user="", password=None, token=os.environ["NUCLEUS_TOKEN"]) as c:
            got = c.execute("SELECT current_user, current_setting('nucleus.claims.tenant')").rows
            self.assertEqual(got, [("drivers_oidc", "acme")])
        with self.assertRaises(nucleus.OperationalError) as e:
            connect(token="not.a.token")
        self.assertEqual(e.exception.code, "28000")

    @unittest.skipUnless(os.environ.get("NUCLEUS_TLS_CA"), "the server has no TLS")
    def test_tls(self):
        ca = os.environ["NUCLEUS_TLS_CA"]
        with connect(tls_ca=ca) as c:
            self.assertEqual(c.execute("SELECT 'over tls'").rows, [("over tls",)])
        with connect(tls=True, tls_verify=False) as c:
            self.assertEqual(c.execute("SELECT 1").rows, [(1,)])
        # The test CA is not among the system's.
        with self.assertRaises(nucleus.OperationalError):
            connect(tls=True)
        # A client certificate naming the user stands in for its password.
        cert, key = os.environ.get("NUCLEUS_TLS_CERT"), os.environ.get("NUCLEUS_TLS_KEY")
        if cert:
            with connect(tls_ca=ca, tls_cert=cert, tls_key=key, password="wrong") as c:
                self.assertEqual(c.execute("SELECT current_user").rows, [("nucleus",)])


if __name__ == "__main__":
    unittest.main()
