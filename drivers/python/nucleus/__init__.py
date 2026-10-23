"""Python driver for nucleus: the nucleus protocol (ncp, DOCS.md section 10).

Pure Python, no dependencies.  The API follows DB-API 2.0 where it fits::

    import nucleus

    conn = nucleus.connect()            # 127.0.0.1:20035, user/password "nucleus"
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)")
    conn.execute("INSERT INTO t VALUES ($1, $2)", [1, "one"])
    for row in conn.execute("SELECT id, name FROM t").rows:
        print(row)
    with conn.transaction():
        conn.execute("UPDATE t SET name = $1 WHERE id = $2", ["uno", 1])

Values map to Python types: boolean -> bool, smallint, integer and bigint
-> int, real and double -> float (a real by its shortest digits), numeric
-> decimal.Decimal, text, char(n) and enums -> str, bytea -> bytes, date
-> datetime.date, time -> datetime.time (24:00:00, which it cannot hold,
as time.max), timestamp -> datetime.datetime (naive), timestamptz ->
datetime.datetime (UTC), interval -> nucleus.Interval, inet ->
ipaddress.ip_interface, cidr -> ipaddress.ip_network, uuid -> uuid.UUID,
arrays -> list, NULL -> None; another registered type -> nucleus.Ext (its
name and binary form).  Parameters take the same types (and
datetime.timedelta for intervals, bytearray for bytea, an ipaddress
address for an inet).

The connection runs each statement in its own transaction unless a
transaction block is open (``BEGIN`` or ``with conn.transaction()``).
"""

import datetime as _dt
import base64 as _b64
import decimal as _decimal
import hashlib as _hashlib
import hmac as _hmac
import ipaddress as _ip
import os as _os
import socket as _socket
import ssl as _ssl
import struct as _struct
import uuid as _uuid
from dataclasses import dataclass as _dataclass

__all__ = [
    "connect", "Connection", "Cursor", "Result", "Column", "Interval", "Ext",
    "Error", "InterfaceError", "DatabaseError", "DataError",
    "OperationalError", "IntegrityError", "ProgrammingError",
    "NotSupportedError", "DEFAULT_PORT",
]

apilevel = "2.0"
threadsafety = 1
paramstyle = "numeric"  # $1, $2, ...

# 2: registered types (uuid, modules' types) arrive in their binary form;
# 3: smallint, integer, real, bytea, time, inet and cidr in theirs;
# 4: the password is proved by SCRAM-SHA-256, never sent.
PROTOCOL_VERSION = 4
DEFAULT_PORT = 20035

_EPOCH_DATE = _dt.date(2000, 1, 1)
_EPOCH = _dt.datetime(2000, 1, 1)
_EPOCH_TZ = _dt.datetime(2000, 1, 1, tzinfo=_dt.timezone.utc)
_TYPE_NAMES = {
    0: "unknown", 1: "boolean", 2: "bigint", 3: "double precision", 4: "text",
    5: "numeric", 6: "date", 7: "timestamp", 8: "timestamptz", 9: "interval",
    10: "array", 12: "smallint", 13: "integer", 14: "real", 15: "bytea",
    16: "time", 17: "character", 18: "inet", 19: "cidr",
}
_ENUM = 20
_DAY_US = 86_400_000_000
_EXT = 11


class Error(Exception):
    """A nucleus error; ``code`` is the SQLSTATE (or None)."""

    def __init__(self, message, code=None):
        super().__init__(f"{message} ({code})" if code else message)
        self.message = message
        self.code = code


class InterfaceError(Error):
    """A problem with the driver or the connection itself."""


class DatabaseError(Error):
    """An error the server reported."""


class DataError(DatabaseError):
    """Bad data: SQLSTATE class 22."""


class OperationalError(DatabaseError):
    """Connection, authentication, serialization: classes 08, 28, 40, …"""


class IntegrityError(DatabaseError):
    """A constraint: class 23."""


class ProgrammingError(DatabaseError):
    """Syntax, names, types: class 42."""


class NotSupportedError(DatabaseError):
    """A feature nucleus does not have: 0A000."""


def _server_error(code, message):
    cls = {
        "22": DataError, "23": IntegrityError, "42": ProgrammingError,
        "0A": NotSupportedError, "08": OperationalError,
        "28": OperationalError, "40": OperationalError,
    }.get(code[:2], DatabaseError)
    return cls(message, code)


@_dataclass(frozen=True)
class Interval:
    """An interval: months, days and microseconds, kept apart (a month is
    not a fixed number of days)."""

    months: int = 0
    days: int = 0
    microseconds: int = 0

    def to_timedelta(self):
        """As a timedelta; fails if there are months."""
        if self.months:
            raise ValueError("an interval with months has no fixed length")
        return _dt.timedelta(days=self.days, microseconds=self.microseconds)


@_dataclass(frozen=True)
class Ext:
    """A value of a registered type this driver has no Python type for:
    the type's name and the value's binary (canonical) form."""

    type_name: str
    data: bytes


@_dataclass(frozen=True)
class Column:
    """A result column: its name and type name."""

    name: str
    type: str


@_dataclass
class Result:
    """One statement's result."""

    columns: list
    rows: list
    command: str
    count: int

    def records(self):
        """The rows as dicts by column name."""
        names = [c.name for c in self.columns]
        return [dict(zip(names, r)) for r in self.rows]


# ------------------------------------------------------------------ values

def _put_str(out, s):
    b = s.encode("utf-8")
    out += _struct.pack("<I", len(b))
    out += b


def _encode(out, v):
    if v is None:
        out.append(0)
    elif isinstance(v, bool):
        out += bytes([1, 1 if v else 0])
    elif isinstance(v, int):
        if not -(1 << 63) <= v < (1 << 63):
            raise DataError("integer out of range for bigint", "22003")
        out.append(2)
        out += _struct.pack("<q", v)
    elif isinstance(v, float):
        out.append(3)
        out += _struct.pack("<d", v)
    elif isinstance(v, str):
        out.append(4)
        _put_str(out, v)
    elif isinstance(v, _decimal.Decimal):
        if not v.is_finite():
            raise DataError("NaN and infinity are not numeric values", "22003")
        sign, digits, exp = v.as_tuple()
        mantissa = int("".join(map(str, digits)) or "0")
        if exp > 0:
            mantissa *= 10 ** exp
            exp = 0
        if sign:
            mantissa = -mantissa
        out.append(5)
        out += (mantissa & ((1 << 128) - 1)).to_bytes(16, "little")
        out.append(-exp)
    elif isinstance(v, _dt.datetime):
        if v.tzinfo is None:
            out.append(7)
            delta = v - _EPOCH
        else:
            out.append(8)
            delta = v - _EPOCH_TZ
        out += _struct.pack("<q", (delta.days * 86400 + delta.seconds) * 1_000_000 + delta.microseconds)
    elif isinstance(v, _dt.date):
        out.append(6)
        out += _struct.pack("<i", (v - _EPOCH_DATE).days)
    elif isinstance(v, _dt.time):
        out.append(16)
        out += _struct.pack("<q", ((v.hour * 60 + v.minute) * 60 + v.second) * 1_000_000 + v.microsecond)
    elif isinstance(v, (bytes, bytearray, memoryview)):
        data = bytes(v)
        out.append(15)
        out += _struct.pack("<I", len(data))
        out += data
    elif isinstance(v, (_ip.IPv4Interface, _ip.IPv6Interface)):
        # (Before addresses: an interface is a kind of address.)
        _encode_net(out, 18, v.ip, v.network.prefixlen)
    elif isinstance(v, (_ip.IPv4Address, _ip.IPv6Address)):
        _encode_net(out, 18, v, v.max_prefixlen)
    elif isinstance(v, (_ip.IPv4Network, _ip.IPv6Network)):
        _encode_net(out, 19, v.network_address, v.prefixlen)
    elif isinstance(v, _dt.timedelta):
        out.append(9)
        out += _struct.pack("<iiq", 0, v.days, v.seconds * 1_000_000 + v.microseconds)
    elif isinstance(v, Interval):
        out.append(9)
        out += _struct.pack("<iiq", v.months, v.days, v.microseconds)
    elif isinstance(v, _uuid.UUID):
        _encode_ext(out, "uuid", v.bytes)
    elif isinstance(v, Ext):
        _encode_ext(out, v.type_name, v.data)
    elif isinstance(v, (list, tuple)):
        out.append(10)
        out += _struct.pack("<I", len(v))
        for item in v:
            _encode(out, item)
    else:
        raise InterfaceError(f"cannot send a {type(v).__name__} as a nucleus value")


def _encode_net(out, tag, addr, bits):
    out.append(tag)
    out += bytes([addr.version, bits])
    out += addr.packed


def _real(bits):
    """A real (its 4 bytes) as the float of its shortest digits."""
    (f,) = _struct.unpack("<f", bits)
    if f != f or f in (float("inf"), float("-inf")):
        return f
    for digits in range(1, 10):
        s = "%.*g" % (digits, f)
        if _struct.pack("<f", float(s)) == bits:
            return float(s)
    return f


def _encode_ext(out, name, data):
    out.append(_EXT)
    _put_str(out, name)
    out += _struct.pack("<I", len(data))
    out += data


class _Reader:
    def __init__(self, b):
        self.b = b
        self.at = 0

    def take(self, n):
        if self.at + n > len(self.b):
            raise InterfaceError("message too short")
        s = self.b[self.at:self.at + n]
        self.at += n
        return s

    def u8(self):
        return self.take(1)[0]

    def unpack(self, fmt):
        return _struct.unpack(fmt, self.take(_struct.calcsize(fmt)))

    def str(self):
        (n,) = self.unpack("<I")
        return self.take(n).decode("utf-8")

    def value(self):
        tag = self.u8()
        if tag == 0:
            return None
        if tag == 1:
            return self.u8() != 0
        if tag == 2:
            return self.unpack("<q")[0]
        if tag == 3:
            return self.unpack("<d")[0]
        if tag == 4:
            return self.str()
        if tag == 5:
            mantissa = int.from_bytes(self.take(16), "little", signed=True)
            scale = self.u8()
            return _decimal.Decimal(mantissa).scaleb(-scale)
        if tag == 6:
            return _EPOCH_DATE + _dt.timedelta(days=self.unpack("<i")[0])
        if tag == 7:
            return _EPOCH + _dt.timedelta(microseconds=self.unpack("<q")[0])
        if tag == 8:
            return _EPOCH_TZ + _dt.timedelta(microseconds=self.unpack("<q")[0])
        if tag == 9:
            months, days, micros = self.unpack("<iiq")
            return Interval(months, days, micros)
        if tag == 10:
            (n,) = self.unpack("<I")
            return [self.value() for _ in range(n)]
        if tag == 12:
            return self.unpack("<h")[0]
        if tag == 13:
            return self.unpack("<i")[0]
        if tag == 14:
            return _real(self.take(4))
        if tag == 15:
            (n,) = self.unpack("<I")
            return bytes(self.take(n))
        if tag == 16:
            (us,) = self.unpack("<q")
            if us >= _DAY_US:
                return _dt.time.max
            s, us = divmod(us, 1_000_000)
            m, s = divmod(s, 60)
            return _dt.time(m // 60, m % 60, s, us)
        if tag in (18, 19):
            family, bits = self.u8(), self.u8()
            addr = _ip.ip_address(bytes(self.take(4 if family == 4 else 16)))
            if tag == 18:
                return _ip.ip_interface(f"{addr}/{bits}")
            return _ip.ip_network(f"{addr}/{bits}")
        if tag == _EXT:
            name = self.str()
            (n,) = self.unpack("<I")
            data = bytes(self.take(n))
            if name == "uuid" and n == 16:
                return _uuid.UUID(bytes=data)
            return Ext(name, data)
        raise InterfaceError(f"unknown value tag {tag}")


# -------------------------------------------------------------- connection

class _Scram:
    """The client's side of a SCRAM-SHA-256 exchange (RFC 5802)."""

    def __init__(self, password):
        self.password = password.encode("utf-8")
        self.bare = "n=,r=" + _b64.b64encode(_os.urandom(18)).decode()

    def first(self):
        return "n,," + self.bare

    def respond(self, server_first):
        attrs = dict(p.split("=", 1) for p in server_first.split(","))
        nonce, salt, iterations = attrs["r"], _b64.b64decode(attrs["s"]), int(attrs["i"])
        if not nonce.startswith(self.bare[5:]) or len(nonce) == len(self.bare) - 5:
            raise InterfaceError("malformed SCRAM message")
        salted = _hashlib.pbkdf2_hmac("sha256", self.password, salt, iterations)
        client_key = _hmac.new(salted, b"Client Key", "sha256").digest()
        stored = _hashlib.sha256(client_key).digest()
        self.server_key = _hmac.new(salted, b"Server Key", "sha256").digest()
        without_proof = "c=biws,r=" + nonce
        self.auth = f"{self.bare},{server_first},{without_proof}".encode()
        signature = _hmac.new(stored, self.auth, "sha256").digest()
        proof = bytes(a ^ b for a, b in zip(client_key, signature))
        return without_proof + ",p=" + _b64.b64encode(proof).decode()

    def verify(self, server_final):
        want = _hmac.new(self.server_key, self.auth, "sha256").digest()
        if not _hmac.compare_digest(_b64.b64decode(server_final[2:]), want):
            raise InterfaceError("the server could not prove it knows the password")


class Connection:
    """A connection to a nucleus server (see ``connect``)."""

    def __init__(self, host="127.0.0.1", port=DEFAULT_PORT, user="nucleus",
                 password="nucleus", database="nucleus", timeout=10.0,
                 client_name="nucleus-python", tls=False, tls_ca=None,
                 tls_verify=True):
        try:
            self._sock = _socket.create_connection((host, port), timeout=timeout)
            self._sock.setsockopt(_socket.IPPROTO_TCP, _socket.TCP_NODELAY, 1)
            if tls or tls_ca is not None:
                # The handshake starts at once; the server sees it (DOCS §10).
                ctx = _ssl.create_default_context(cafile=tls_ca)
                if not tls_verify:
                    ctx.check_hostname = False
                    ctx.verify_mode = _ssl.CERT_NONE
                self._sock = ctx.wrap_socket(self._sock, server_hostname=host)
        except OSError as e:
            raise OperationalError(f"cannot connect to {host}:{port}: {e}") from e
        self._sock.settimeout(None)
        self._buf = bytearray()
        self.status = 0  # 0 idle, 1 in a transaction block, 2 failed block
        hello = bytearray(_struct.pack("<H", PROTOCOL_VERSION))
        for s in (user, database, "", client_name):
            _put_str(hello, s)
        self._send(b"H", hello)
        scram = None
        while True:
            kind, body = self._read()
            r = _Reader(body)
            if kind == b"A":
                msg = r.str()
                if scram is None:
                    if msg != "SCRAM-SHA-256":
                        raise InterfaceError(f"unsupported authentication {msg!r}")
                    scram = _Scram(password or "")
                    reply = scram.first()
                elif msg.startswith("v="):
                    scram.verify(msg)
                    reply = None
                else:
                    reply = scram.respond(msg)
                if reply is not None:
                    out = bytearray()
                    _put_str(out, reply)
                    self._send(b"a", out)
            elif kind == b"W":
                r.unpack("<H")
                self.server_version = r.str()
                (self.session_id,) = r.unpack("<I")
            elif kind == b"R":
                self.status = body[0]
                return
            elif kind == b"E":
                code, message = r.str(), r.str()
                self._sock.close()
                raise _server_error(code, message)
            else:
                raise InterfaceError(f"unexpected message {kind!r}")

    def _send(self, kind, body):
        self._sock.sendall(kind + _struct.pack("<I", len(body)) + bytes(body))

    def _fill(self, n):
        while len(self._buf) < n:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise OperationalError("the server closed the connection")
            self._buf += chunk

    def _read(self):
        self._fill(5)
        kind = bytes(self._buf[:1])
        (n,) = _struct.unpack("<I", self._buf[1:5])
        self._fill(5 + n)
        body = bytes(self._buf[5:5 + n])
        del self._buf[:5 + n]
        return kind, body

    def query(self, sql, params=(), text=False):
        """Runs ``sql`` (several statements allowed without parameters).
        Returns a list with each statement's ``Result``; a failing
        statement raises its error, whose ``results`` holds the statements
        before it."""
        body = bytearray()
        _put_str(body, sql)
        body.append(1 if text else 0)
        body += _struct.pack("<H", len(params))
        for p in params:
            _encode(body, p)
        self._send(b"Q", body)
        results, error = [], None
        columns, rows = None, []
        while True:
            kind, payload = self._read()
            r = _Reader(payload)
            if kind == b"C":
                (n,) = r.unpack("<H")
                columns = []
                for _ in range(n):
                    name = r.str()
                    tag = r.u8()
                    type_name = _TYPE_NAMES.get(tag, "unknown")
                    if tag == 10:
                        type_name = _TYPE_NAMES.get(r.u8(), "unknown") + "[]"
                    elif tag in (_EXT, _ENUM):
                        type_name = r.str()
                    columns.append(Column(name, type_name))
                rows = []
            elif kind == b"D":
                rows.append(tuple(r.value() for _ in columns))
            elif kind == b"K":
                command = r.str()
                (count,) = r.unpack("<Q")
                results.append(Result(columns or [], rows if columns is not None else [], command, count))
                columns, rows = None, []
            elif kind == b"E":
                error = _server_error(r.str(), r.str())
            elif kind == b"R":
                self.status = payload[0]
                if error is not None:
                    error.results = results
                    raise error
                return results
            else:
                raise InterfaceError(f"unexpected message {kind!r}")

    def execute(self, sql, params=()):
        """Runs one statement (or several, returning the last result)."""
        results = self.query(sql, params)
        return results[-1] if results else Result([], [], "", 0)

    def cursor(self):
        return Cursor(self)

    def transaction(self):
        """``with conn.transaction():`` — BEGIN, then COMMIT, or ROLLBACK
        if the block raises."""
        return _Transaction(self)

    @property
    def in_transaction(self):
        return self.status != 0

    def commit(self):
        if self.status != 0:
            self.execute("COMMIT")

    def rollback(self):
        if self.status != 0:
            self.execute("ROLLBACK")

    def close(self):
        if self._sock is not None:
            try:
                self._send(b"X", b"")
            except OSError:
                pass
            self._sock.close()
            self._sock = None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


class _Transaction:
    def __init__(self, conn):
        self.conn = conn

    def __enter__(self):
        self.conn.execute("BEGIN")
        return self.conn

    def __exit__(self, kind, value, tb):
        self.conn.execute("ROLLBACK" if kind is not None or self.conn.status == 2 else "COMMIT")
        return False


class Cursor:
    """A DB-API cursor."""

    arraysize = 1

    def __init__(self, conn):
        self.connection = conn
        self.description = None
        self.rowcount = -1
        self._rows = []

    def execute(self, sql, params=()):
        r = self.connection.execute(sql, list(params))
        self._rows = list(r.rows)
        self.rowcount = r.count
        self.description = (
            [(c.name, c.type, None, None, None, None, None) for c in r.columns]
            if r.columns else None
        )
        return self

    def executemany(self, sql, seq):
        total = 0
        for params in seq:
            self.execute(sql, params)
            total += self.rowcount
        self.rowcount = total

    def fetchone(self):
        return self._rows.pop(0) if self._rows else None

    def fetchmany(self, size=None):
        n = size or self.arraysize
        out, self._rows = self._rows[:n], self._rows[n:]
        return out

    def fetchall(self):
        out, self._rows = self._rows, []
        return out

    def __iter__(self):
        while self._rows:
            yield self._rows.pop(0)

    def close(self):
        self._rows = []


def connect(host="127.0.0.1", port=DEFAULT_PORT, user="nucleus", password="nucleus",
            database="nucleus", timeout=10.0, tls=False, tls_ca=None, tls_verify=True):
    """Opens a connection (the defaults reach a local server as it starts).

    ``tls=True`` encrypts it, the server's certificate checked against the
    system's CA certificates — or against ``tls_ca`` (a PEM file; giving it
    implies ``tls``); ``tls_verify=False`` skips the check (encrypted, but
    anyone can pose as the server)."""
    return Connection(host, port, user, password, database, timeout,
                      tls=tls, tls_ca=tls_ca, tls_verify=tls_verify)
