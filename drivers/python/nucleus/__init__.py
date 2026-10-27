"""Python driver for nucleus: the nucleus protocol (ncp, DOCS.md section 10).

Pure Python, no dependencies.  The API follows DB-API 2.0 where it fits::

    import nucleus

    conn = nucleus.connect()            # 127.0.0.1:20035, user/password "nucleus"
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)")
    conn.execute("INSERT INTO t VALUES ($1, $2)", [1, "one"])
    for row in conn.execute("SELECT id, name FROM t").rows:
        print(row)
    with conn.stream("SELECT id FROM t") as rows:   # a large result
        for row in rows:                           # read as it arrives
            print(row)
    df = conn.pandas("SELECT * FROM t")           # columnar: numpy, Arrow,
                                                  # pandas, polars
    conn.load("t", df)                            # and back, in one call
    conn.copy("COPY t FROM STDIN WITH (FORMAT csv)", open("t.csv", "rb"))
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
from dataclasses import field as _field

__all__ = [
    "connect", "Connection", "Cursor", "Stream", "Batches", "Batch", "ColumnBatch",
    "Result", "Column", "Interval", "Ext",
    "Error", "InterfaceError", "DatabaseError", "DataError",
    "OperationalError", "IntegrityError", "ProgrammingError",
    "NotSupportedError", "DEFAULT_PORT", "DEFAULT_FETCH",
]

apilevel = "2.0"
threadsafety = 1
paramstyle = "numeric"  # $1, $2, ...

# 2: registered types (uuid, modules' types) arrive in their binary form;
# 3: smallint, integer, real, bytea, time, inet and cidr in theirs;
# 4: the password is proved by SCRAM-SHA-256, never sent; 5: errors carry
# a hint; 6: results stream, fetched in batches; 7: columnar results.
PROTOCOL_VERSION = 7
DEFAULT_PORT = 20035
# Rows a stream asks the server for at a time.
DEFAULT_FETCH = 1000
# Rows a columnar batch holds when none is given.
DEFAULT_BATCH = 65536

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
    """A nucleus error; ``code`` is the SQLSTATE (or None), ``hint`` what
    to do about it (or None): the name meant, the cast that would work."""

    def __init__(self, message, code=None, hint=None):
        super().__init__(f"{message} ({code})" if code else message)
        self.message = message
        self.code = code
        self.hint = hint


class InterfaceError(Error):
    """A problem with the driver or the connection itself."""


class DatabaseError(Error):
    """An error the server reported."""


class DataError(DatabaseError):
    """Bad data: SQLSTATE class 22."""


class OperationalError(DatabaseError):
    """Connection, authentication, serialization, cancellation: classes 08,
    28, 40, 57, …"""


class IntegrityError(DatabaseError):
    """A constraint: class 23."""


class ProgrammingError(DatabaseError):
    """Syntax, names, types: class 42."""


class NotSupportedError(DatabaseError):
    """A feature nucleus does not have: 0A000."""


def _server_error(r):
    """The error an E message reports (its hint from protocol version 5)."""
    code, message = r.str(), r.str()
    hint = (r.str() or None) if r.at < len(r.b) else None
    cls = {
        "22": DataError, "23": IntegrityError, "42": ProgrammingError,
        "0A": NotSupportedError, "08": OperationalError,
        "28": OperationalError, "40": OperationalError, "57": OperationalError,
    }.get(code[:2], DatabaseError)
    return cls(message, code, hint)


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
    # The protocol's type tag (tells an enum from a registered type).
    tag: int = _field(default=0, compare=False, repr=False)


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

def _der(b):
    """One DER element: (tag, contents, the rest)."""
    tag, n, i = b[0], b[1], 2
    if n & 0x80:
        k = n & 0x7F
        n, i = int.from_bytes(b[2:2 + k], "big"), 2 + k
    return tag, b[i:i + n], b[i + n:]


# Signature algorithm OIDs (DER) → the hash of their tls-server-end-point
# (RFC 5929: SHA-256 for MD5 and SHA-1).
_END_POINT_HASHES = {
    bytes.fromhex("2a864886f70d010104"): "sha256",  # md5WithRSA
    bytes.fromhex("2a864886f70d010105"): "sha256",  # sha1WithRSA
    bytes.fromhex("2a864886f70d01010b"): "sha256",
    bytes.fromhex("2a864886f70d01010c"): "sha384",
    bytes.fromhex("2a864886f70d01010d"): "sha512",
    bytes.fromhex("2a8648ce3d0401"): "sha256",  # ecdsa-with-SHA1
    bytes.fromhex("2a8648ce3d040302"): "sha256",
    bytes.fromhex("2a8648ce3d040303"): "sha384",
    bytes.fromhex("2a8648ce3d040304"): "sha512",
}


def _server_end_point(cert):
    """A certificate's tls-server-end-point value, or None."""
    try:
        _, body, _ = _der(cert)
        _, _, rest = _der(body)  # the to-be-signed part
        _, algorithm, _ = _der(rest)
        _, oid, _ = _der(algorithm)
    except IndexError:
        return None
    name = _END_POINT_HASHES.get(bytes(oid))
    return _hashlib.new(name, cert).digest() if name else None


class _Scram:
    """The client's side of a SCRAM-SHA-256 exchange (RFC 5802); over TLS
    bound to the session (SCRAM-SHA-256-PLUS) when the server offers it, else
    saying it could have been ("y"), so a stripped offer is caught."""

    def __init__(self, password, binding=None, offered=False):
        self.password = password.encode("utf-8")
        self.bare = "n=,r=" + _b64.b64encode(_os.urandom(18)).decode()
        self.gs2, self.binding = b"n,,", b""
        if binding is not None:
            if offered:
                self.gs2, self.binding = b"p=tls-server-end-point,,", binding
            else:
                self.gs2 = b"y,,"

    def first(self):
        return self.gs2.decode() + self.bare

    def respond(self, server_first):
        attrs = dict(p.split("=", 1) for p in server_first.split(","))
        nonce, salt, iterations = attrs["r"], _b64.b64decode(attrs["s"]), int(attrs["i"])
        if not nonce.startswith(self.bare[5:]) or len(nonce) == len(self.bare) - 5:
            raise InterfaceError("malformed SCRAM message")
        salted = _hashlib.pbkdf2_hmac("sha256", self.password, salt, iterations)
        client_key = _hmac.new(salted, b"Client Key", "sha256").digest()
        stored = _hashlib.sha256(client_key).digest()
        self.server_key = _hmac.new(salted, b"Server Key", "sha256").digest()
        without_proof = "c=" + _b64.b64encode(self.gs2 + self.binding).decode() + ",r=" + nonce
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
                 tls_verify=True, tls_cert=None, tls_key=None, token=None):
        try:
            self._sock = _socket.create_connection((host, port), timeout=timeout)
            self._sock.setsockopt(_socket.IPPROTO_TCP, _socket.TCP_NODELAY, 1)
            if tls or tls_ca is not None:
                # The handshake starts at once; the server sees it (DOCS §10).
                ctx = _ssl.create_default_context(cafile=tls_ca)
                if not tls_verify:
                    ctx.check_hostname = False
                    ctx.verify_mode = _ssl.CERT_NONE
                if tls_cert is not None:
                    ctx.load_cert_chain(tls_cert, tls_key)
                self._sock = ctx.wrap_socket(self._sock, server_hostname=host)
        except OSError as e:
            raise OperationalError(f"cannot connect to {host}:{port}: {e}") from e
        self._sock.settimeout(None)
        self._buf = bytearray()
        self._version = PROTOCOL_VERSION
        self._active = None  # a Stream the server is still sending
        self._copy_pending = None  # a message read starting a COPY that did not begin
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
                if scram is None and token is not None:
                    # OAUTHBEARER's initial response (RFC 7628).
                    if "OAUTHBEARER" not in msg.split(" "):
                        raise InterfaceError("the server does not take bearer tokens")
                    out = bytearray()
                    _put_str(out, f"n,,\x01auth=Bearer {token}\x01\x01")
                    self._send(b"a", out)
                    continue
                if scram is None and "PLAIN" in msg.split(" ") and "SCRAM-SHA-256" not in msg.split(" "):
                    # A directory checks this role's password: the password
                    # itself (RFC 4616), never without TLS.
                    if not isinstance(self._sock, _ssl.SSLSocket):
                        raise InterfaceError("the server asks for the password itself, which needs TLS")
                    out = bytearray()
                    _put_str(out, f"\0{user}\0{password or ''}")
                    self._send(b"a", out)
                    continue
                if scram is None:
                    offered = msg.split(" ")
                    if "SCRAM-SHA-256" not in offered:
                        raise InterfaceError(f"unsupported authentication {msg!r}")
                    binding = None
                    if isinstance(self._sock, _ssl.SSLSocket):
                        cert = self._sock.getpeercert(binary_form=True)
                        binding = _server_end_point(cert) if cert else None
                    scram = _Scram(password or "", binding, "SCRAM-SHA-256-PLUS" in offered)
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
                (self._version,) = r.unpack("<H")
                self.server_version = r.str()
                (self.session_id,) = r.unpack("<I")
            elif kind == b"R":
                self.status = body[0]
                return
            elif kind == b"E":
                error = _server_error(r)
                self._sock.close()
                raise error
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
        self._send_query(sql, params, 1 if text else 0, 0)
        results, error = [], None
        columns, rows = None, []
        while True:
            kind, payload = self._read()
            r = _Reader(payload)
            if kind == b"C":
                columns = _columns(r)
                rows = []
            elif kind == b"D":
                rows.append(tuple(r.value() for _ in columns))
            elif kind == b"K":
                command = r.str()
                (count,) = r.unpack("<Q")
                results.append(Result(columns or [], rows if columns is not None else [], command, count))
                columns, rows = None, []
            elif kind == b"E":
                error = _server_error(r)
            elif kind == b"I":
                self._refuse_copy()
            elif kind == b"R":
                self.status = payload[0]
                if error is not None:
                    error.results = results
                    raise error
                return results
            else:
                raise InterfaceError(f"unexpected message {kind!r}")

    def _send_query(self, sql, params, format, fetch):
        # A stream left unfinished is closed first: one result at a time.
        if self._active is not None:
            self._active.close()
        body = bytearray()
        _put_str(body, sql)
        body.append(format)
        if self._version >= 6:
            body += _struct.pack("<I", fetch)
        body += _struct.pack("<H", len(params))
        for p in params:
            _encode(body, p)
        self._send(b"Q", body)

    def _refuse_copy(self):
        out = bytearray()
        _put_str(out, "this call sends no data: use Connection.copy or load for COPY FROM STDIN")
        self._send(b"f", out)

    def copy(self, sql, data):
        """Runs ``COPY … FROM STDIN`` (``sql``) with ``data`` — bytes, a str, a
        binary or text file, or an iterable of such chunks — in the text or
        CSV format the statement names; the rows stored.  The load is one
        statement: a bad row stores nothing (its error names the line)."""
        if self._copy_start(sql) is None:
            return self._copy_end(None)
        try:
            for chunk in _chunks(data):
                if chunk:
                    self._send(b"d", chunk)
        except Exception as e:
            self._copy_fail(f"the client could not read its data: {e}")
            raise
        return self._copy_end(None)

    def load(self, table, data, columns=None):
        """Loads ``data`` into ``table`` in one statement (``COPY … FROM
        STDIN`` as columnar batches): a pandas DataFrame, a pyarrow Table or
        RecordBatch, a dict of columns (numpy arrays — masked ones for NULLs
        — or lists), or a sequence of rows.  ``columns`` names the table's
        columns the data fills (default: the data's own names; every column,
        in order, for rows).  numpy columns of numbers, dates, timestamps
        and times are sent without a Python object per value.  Returns the
        rows stored; a bad row stores nothing."""
        names, cols, n = _columns_of(data, columns)
        target = f" ({', '.join(_ident(c) for c in names)})" if names else ""
        announced = self._copy_start(f"COPY {_ident(table, dotted=True)}{target} FROM STDIN")
        if announced is None:
            return self._copy_end(None)
        try:
            if len(announced) != len(cols):
                raise ProgrammingError(f"{len(cols)} columns of data for {len(announced)} columns")
            step = DEFAULT_BATCH
            for start in range(0, n, step):
                self._send(b"B", _encode_batch(announced, cols, start, min(n, start + step)))
        except Exception as e:
            self._copy_fail(f"the client could not send its data: {e}")
            raise
        return self._copy_end(None)

    def _copy_start(self, sql):
        """Sends a COPY; the columns its data holds, or None when it ended
        without taking data (an error, read by ``_copy_end``)."""
        if self._version < 7:
            raise NotSupportedError("COPY FROM STDIN needs protocol version 7")
        self._send_query(sql, (), 0, 0)
        kind, payload = self._read()
        if kind == b"I":
            return _columns(_Reader(payload))
        self._copy_pending = (kind, payload)
        return None

    def _copy_fail(self, why):
        """Gives up a COPY whose data the client could not send: the
        server's answer (its error) is read, the client's own reported."""
        try:
            self._copy_end(why)
        except DatabaseError:
            pass

    def _copy_end(self, failed):
        pending, self._copy_pending = self._copy_pending, None
        if pending is None:
            if failed is None:
                self._send(b"e", b"")
            else:
                out = bytearray()
                _put_str(out, failed)
                self._send(b"f", out)
        count, error = 0, None
        while True:
            kind, payload = pending if pending is not None else self._read()
            pending = None
            r = _Reader(payload)
            if kind == b"K":
                r.str()
                (count,) = r.unpack("<Q")
            elif kind == b"E":
                error = _server_error(r)
            elif kind == b"R":
                self.status = payload[0]
                if error is not None:
                    raise error
                return count
            elif kind not in (b"C", b"D"):
                raise InterfaceError(f"unexpected message {kind!r}")

    def stream(self, sql, params=(), fetch=DEFAULT_FETCH):
        """Runs one statement and returns a ``Stream`` of its rows, read as
        they arrive: the server sends ``fetch`` rows, then waits until they
        are read, so a result of any size takes a batch of memory.  Closing
        it early (or running another statement) stops the statement.  An
        error before the first row raises here; after rows, from the
        iteration."""
        return Stream(self, sql, list(params), fetch or DEFAULT_FETCH)

    def batches(self, sql, params=(), rows=DEFAULT_BATCH):
        """Runs one statement and returns its result as ``Batches``: ``Batch``
        after ``Batch`` of ``rows`` rows, a buffer per column, which become
        numpy or Arrow arrays without a Python object per value (protocol
        version 7).  The server waits for each batch to be read; closing
        early stops the statement."""
        if self._version < 7:
            raise NotSupportedError("columnar results need protocol version 7")
        return Batches(self, sql, list(params), rows or DEFAULT_BATCH)

    def numpy(self, sql, params=()):
        """The result as a dict of numpy arrays by column name (masked where
        a column has NULLs).  Needs numpy."""
        return self._numpy(sql, params)[0]

    def _numpy(self, sql, params):
        import numpy as np

        parts, names = {}, None
        with self.batches(sql, params) as bs:
            names = [c.name for c in bs.columns]
            for b in bs:
                for i, name in enumerate(names):
                    parts.setdefault(name, []).append(b.column(i).to_numpy())
        out = {}
        for i, name in enumerate(names):
            arrays = parts.get(name, [])
            if not arrays:
                out[name] = ColumnBatch._empty_numpy(bs.columns[i])
            elif any(isinstance(a, np.ma.MaskedArray) for a in arrays):
                out[name] = np.ma.concatenate(arrays)
            else:
                out[name] = np.concatenate(arrays)
        return out, bs.columns

    def arrow(self, sql, params=()):
        """The result as a ``pyarrow.Table``.  Needs pyarrow."""
        import pyarrow as pa

        with self.batches(sql, params) as bs:
            batches = [b.to_arrow() for b in bs]
            if batches:
                return pa.Table.from_batches(batches)
            return pa.table({c.name: pa.array([], type=_arrow_type(c)) for c in bs.columns})

    def pandas(self, sql, params=()):
        """The result as a ``pandas.DataFrame`` (through Arrow when pyarrow
        is there, else numpy).  Needs pandas."""
        import pandas as pd

        try:
            import pyarrow  # noqa: F401
        except ImportError:
            arrays, columns = self._numpy(sql, params)
            df = pd.DataFrame(arrays)
            # timestamptz in UTC, as through Arrow.
            for c in columns:
                if c.tag == 8:
                    df[c.name] = df[c.name].dt.tz_localize("UTC")
            return df
        return self.arrow(sql, params).to_pandas()

    def polars(self, sql, params=()):
        """The result as a ``polars.DataFrame``.  Needs polars and pyarrow."""
        import polars as pl

        return pl.from_arrow(self.arrow(sql, params))

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


def _columns(r):
    """``C``: the result's columns."""
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
        columns.append(Column(name, type_name, tag))
    return columns


class Stream:
    """One statement's rows as they arrive (``Connection.stream``): iterate
    it; ``columns``, and once every row is read ``command`` and ``count``.
    ``close()`` (or leaving a ``with`` block) stops the statement early."""

    _format, _data = 0, b"D"

    def __init__(self, conn, sql, params=(), fetch=DEFAULT_FETCH):
        self._conn = conn
        self._fetch = fetch if conn._version >= 6 else 0
        self._error = None
        self._ready = False
        self.columns = []
        self.command = None
        self.count = None
        conn._send_query(sql, params, self._format, self._fetch)
        while True:
            kind, payload = conn._read()
            r = _Reader(payload)
            if kind == b"C":
                self.columns = _columns(r)
                conn._active = self
                return
            if kind == b"K":
                self.command = r.str()
                (self.count,) = r.unpack("<Q")
            elif kind == b"E":
                self._error = _server_error(r)
            elif kind == b"I":
                conn._refuse_copy()
            elif kind == b"R":
                conn.status = payload[0]
                self._ready = True
                if self._error is not None:
                    raise self._error
                return
            else:
                raise InterfaceError(f"unexpected message {kind!r}")

    def __iter__(self):
        return self

    def __next__(self):
        r = _Reader(self._next_body())
        return tuple(r.value() for _ in self.columns)

    def _next_body(self):
        """The body of the next row (or batch)."""
        conn = self._conn
        while not self._ready:
            kind, payload = conn._read()
            r = _Reader(payload)
            if kind == self._data:
                return payload
            if kind == b"S":
                # The batch is read: the next.
                conn._send(b"F", _struct.pack("<I", self._fetch))
            elif kind == b"K":
                self.command = r.str()
                (self.count,) = r.unpack("<Q")
            elif kind == b"E":
                self._error = _server_error(r)
            elif kind == b"R":
                self._end(payload)
                if self._error is not None:
                    error, self._error = self._error, None
                    raise error
            elif kind == b"C":
                self._ready = True
                raise InterfaceError("a stream runs one statement; this query holds several")
            else:
                self._ready = True
                raise InterfaceError(f"unexpected message {kind!r}")
        raise StopIteration

    def _end(self, payload):
        self._conn.status = payload[0]
        self._ready = True
        if self._conn._active is self:
            self._conn._active = None

    def close(self):
        """Stops reading: the server is told to send no more, and what it
        already sent is skipped."""
        conn = self._conn
        while not self._ready:
            kind, payload = conn._read()
            r = _Reader(payload)
            if kind == b"S":
                conn._send(b"c", b"")
            elif kind == b"K":
                self.command = r.str()
                (self.count,) = r.unpack("<Q")
            elif kind == b"R":
                self._end(payload)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False



# ---------------------------------------------------------------- columnar

# Days and µs from 1970-01-01 (numpy's and Arrow's epoch) to 2000-01-01.
_DAYS_1970 = 10_957
_US_1970 = 946_684_800_000_000
# Fixed widths by type tag (DOCS.md §10, columnar results), and the numpy
# type each is read as.
_FIXED = {
    1: (1, "?"), 12: (2, "<i2"), 13: (4, "<i4"), 14: (4, "<f4"), 6: (4, "<i4"),
    2: (8, "<i8"), 3: (8, "<f8"), 7: (8, "<i8"), 8: (8, "<i8"), 16: (8, "<i8"),
    9: (16, None), 5: (17, None),
}
# The row format's tag a fixed slot is the payload of.
_ROW_TAG = {12: 12, 13: 13, 14: 14, 6: 6, 1: 1, 2: 2, 3: 3, 7: 7, 8: 8, 16: 16, 9: 9, 5: 5}


def _align(at):
    return (at + 7) & ~7


def _width(c):
    if c.tag == _EXT and c.type == "uuid":
        return 16
    return _FIXED.get(c.tag, (0, None))[0]


def _arrow_type(c):
    import pyarrow as pa

    return {
        1: pa.bool_(), 12: pa.int16(), 13: pa.int32(), 2: pa.int64(), 14: pa.float32(),
        3: pa.float64(), 6: pa.date32(), 7: pa.timestamp("us"), 8: pa.timestamp("us", tz="UTC"),
        16: pa.time64("us"), 15: pa.binary(), 9: pa.month_day_nano_interval(),
    }.get(c.tag, pa.binary(16) if _width(c) == 16 else pa.string())


class ColumnBatch:
    """One column of a ``Batch``: ``to_numpy()``, ``to_arrow()``,
    ``to_list()``; ``is_null(i)``."""

    def __init__(self, column, rows, body, valid, offsets, data):
        self.column = column
        self.rows = rows
        self._body = body
        self._valid = valid      # (offset, length) or None: no NULLs
        self._offsets = offsets  # (offset, length) for a variable width
        self._data = data        # (offset, length)

    @property
    def nulls(self):
        """Whether the column has NULLs in this batch."""
        return self._valid is not None

    def is_null(self, i):
        if self._valid is None:
            return False
        return not self._body[self._valid[0] + i // 8] >> (i % 8) & 1

    def _slot(self, i):
        w = _width(self.column)
        if w:
            start = self._data[0] + i * w
            return self._body[start:start + w]
        a, b = _struct.unpack_from("<II", self._body, self._offsets[0] + 4 * i)
        return self._body[self._data[0] + a:self._data[0] + b]

    def value(self, i):
        """The value at row ``i``, as a row of ``Connection.stream`` has it."""
        if self.is_null(i):
            return None
        c, slot = self.column, self._slot(i)
        if c.tag in _ROW_TAG:
            return _Reader(bytes([_ROW_TAG[c.tag]]) + slot).value()
        if c.tag == _EXT:
            out = bytearray([_EXT])
            _put_str(out, c.type)
            out += _struct.pack("<I", len(slot)) + slot
            return _Reader(bytes(out)).value()
        if c.tag == 15:
            return bytes(slot)
        if c.tag in (18, 19):
            return _Reader(bytes([c.tag]) + slot).value()
        if c.tag == 10:
            return _Reader(slot).value()
        return slot.decode("utf-8")

    def to_list(self):
        """The values as Python objects (one per value)."""
        return [self.value(i) for i in range(self.rows)]

    def _valid_numpy(self):
        import numpy as np

        a, n = self._valid
        bits = np.frombuffer(self._body, np.uint8, n, a)
        return np.unpackbits(bits, bitorder="little")[: self.rows].astype(bool)

    @staticmethod
    def _empty_numpy(c):
        import numpy as np

        w = _FIXED.get(c.tag)
        if w and w[1]:
            return np.empty(0, w[1])
        return np.empty(0, object)

    def to_numpy(self):
        """The column as a numpy array: fixed widths read in place (no copy)
        — booleans, integers, floats, ``time`` as ``timedelta64[us]``,
        ``interval`` as a record of months, days and µs; dates and
        timestamps as ``datetime64`` (shifted to 1970's epoch: one vector
        add); anything else an object array.  With NULLs, a masked array (an
        object array holds ``None`` instead)."""
        import numpy as np

        c, n = self.column, self.rows
        fixed = _FIXED.get(c.tag)
        if c.tag == 9:
            arr = np.frombuffer(self._body, np.dtype([("months", "<i4"), ("days", "<i4"), ("micros", "<i8")]),
                                n, self._data[0])
        elif fixed and fixed[1]:
            arr = np.frombuffer(self._body, fixed[1], n, self._data[0])
            if c.tag == 6:
                arr = (arr + _DAYS_1970).astype("datetime64[D]")
            elif c.tag in (7, 8):
                arr = (arr + _US_1970).astype("datetime64[us]")
            elif c.tag == 16:
                arr = arr.view("timedelta64[us]")
        else:
            return np.array(self.to_list() if self.nulls or n else [], dtype=object) if n else np.empty(0, object)
        if self.nulls:
            return np.ma.masked_array(arr, mask=~self._valid_numpy())
        return arr

    def to_arrow(self):
        """The column as a ``pyarrow.Array``: integers, floats, ``time``,
        text and ``bytea`` read in place from the batch (Arrow's own
        layout); booleans, dates and timestamps through numpy; ``numeric``
        as decimals, ``interval`` as ``month_day_nano`` and the rest from
        Python values."""
        import numpy as np
        import pyarrow as pa

        c, n = self.column, self.rows
        buf = pa.py_buffer(self._body)
        valid = buf.slice(*self._valid) if self.nulls else None
        t = _arrow_type(c)
        if c.tag in (12, 13, 2, 14, 3, 16) or (c.tag == _EXT and _width(c) == 16):
            return pa.Array.from_buffers(t, n, [valid, buf.slice(*self._data)])
        if c.tag in (4, 17, _ENUM, 0, 15) or (c.tag == _EXT and _width(c) == 0):
            if c.tag == _EXT:
                t = pa.binary()
            return pa.Array.from_buffers(t, n, [valid, buf.slice(*self._offsets), buf.slice(*self._data)])
        mask = ~self._valid_numpy() if self.nulls else None
        if c.tag in (1, 6, 7, 8):
            arr = np.frombuffer(self._body, _FIXED[c.tag][1], n, self._data[0])
            if c.tag == 6:
                arr = arr + _DAYS_1970
            elif c.tag in (7, 8):
                arr = arr + _US_1970
            return pa.array(arr, type=t, mask=mask)
        if c.tag == 9:
            values = [None if v is None else (v.months, v.days, v.microseconds * 1000) for v in self.to_list()]
            return pa.array(values, type=t)
        values = self.to_list()
        if c.tag == 5:
            return pa.array(values)
        if c.tag in (18, 19):
            return pa.array([None if v is None else str(v) for v in values], type=pa.string())
        return pa.array(values)


class Batch:
    """Rows of a result, a buffer per column (``Connection.batches``):
    ``len(batch)`` rows; ``column(i or name)``, ``to_numpy()`` (a dict),
    ``to_arrow()`` (a ``pyarrow.RecordBatch``), ``rows()`` (tuples)."""

    def __init__(self, columns, body):
        self.columns = columns
        (self._rows,) = _struct.unpack_from("<I", body, 0)
        n, at = self._rows, 4
        self._cols = []
        for c in columns:
            nulls = body[at]
            at += 1
            valid = None
            if nulls:
                at = _align(at)
                valid = (at, (n + 7) // 8)
                at += valid[1]
            at = _align(at)
            w = _width(c)
            offsets = None
            if w:
                data = (at, n * w)
                at += n * w
            else:
                offsets = (at, (n + 1) * 4)
                (end,) = _struct.unpack_from("<I", body, at + 4 * n)
                at = _align(at + offsets[1])
                data = (at, end)
                at += end
            at = _align(at)
            if at > len(body):
                raise InterfaceError("malformed batch")
            self._cols.append(ColumnBatch(c, n, body, valid, offsets, data))

    def __len__(self):
        return self._rows

    def column(self, key):
        if isinstance(key, str):
            key = [c.name for c in self.columns].index(key)
        return self._cols[key]

    def rows(self):
        """The rows as tuples (a Python object per value)."""
        cols = [c.to_list() for c in self._cols]
        return list(zip(*cols)) if cols else []

    def to_numpy(self):
        return {c.column.name: c.to_numpy() for c in self._cols}

    def to_arrow(self):
        import pyarrow as pa

        return pa.RecordBatch.from_arrays([c.to_arrow() for c in self._cols],
                                          names=[c.name for c in self.columns])


class Batches(Stream):
    """One statement's result as it arrives, ``Batch`` by ``Batch``
    (``Connection.batches``)."""

    _format, _data = 2, b"B"

    def __next__(self):
        return Batch(self.columns, self._next_body())



# --------------------------------------------------------------- bulk load

def _chunks(data):
    """``Connection.copy``'s data as byte chunks."""
    if isinstance(data, (bytes, bytearray, memoryview)):
        yield bytes(data)
    elif isinstance(data, str):
        yield data.encode()
    elif hasattr(data, "read"):
        while True:
            chunk = data.read(1 << 16)
            if not chunk:
                return
            yield chunk.encode() if isinstance(chunk, str) else bytes(chunk)
    else:
        for chunk in data:
            yield chunk.encode() if isinstance(chunk, str) else bytes(chunk)


def _ident(name, dotted=False):
    """A name as SQL takes it: quoted unless a plain lower-case identifier."""
    if dotted and "." in name and '"' not in name:
        return ".".join(_ident(p) for p in name.split("."))
    if name and (name[0].isalpha() or name[0] == "_") and name.isascii() \
            and all(ch.islower() or ch.isdigit() or ch == "_" for ch in name):
        return name
    return '"' + name.replace('"', '""') + '"'


def _np():
    try:
        import numpy
        return numpy
    except ImportError:
        return None


def _columns_of(data, columns):
    """(names, [(values, null mask or None)], rows) of ``load``'s data."""
    np = _np()
    names, cols = None, []
    if hasattr(data, "column_names") and hasattr(data, "num_rows"):  # pyarrow
        names = list(data.column_names)
        for i in range(len(names)):
            col = data.column(i)
            if hasattr(col, "combine_chunks"):
                col = col.combine_chunks()
            mask = col.is_null().to_numpy(zero_copy_only=False) if col.null_count else None
            values = col.to_numpy(zero_copy_only=False)
            cols.append((values, mask))
        n = data.num_rows
    elif hasattr(data, "dtypes") and hasattr(data, "columns") and hasattr(data, "iloc"):  # pandas
        names = [str(c) for c in data.columns]
        for c in data.columns:
            sr = data[c]
            mask = sr.isna().to_numpy()
            mask = mask if mask.any() else None
            dtype = sr.dtype
            if getattr(dtype, "tz", None) is not None:
                values = sr.dt.tz_convert("UTC").dt.tz_localize(None).to_numpy()
            elif hasattr(dtype, "numpy_dtype"):  # nullable integers, booleans, floats
                fill = False if dtype.numpy_dtype.kind == "b" else 0
                values = sr.to_numpy(dtype=dtype.numpy_dtype, na_value=fill)
            else:
                values = sr.to_numpy()
            cols.append((values, mask))
        n = len(data)
    elif isinstance(data, dict):
        names = list(data)
        for v in data.values():
            if np is not None and isinstance(v, np.ma.MaskedArray):
                mask = np.ma.getmaskarray(v)
                cols.append((v.data, mask if mask.any() else None))
            elif np is not None and isinstance(v, np.ndarray) and v.dtype != object:
                cols.append((v, None))
            else:
                v = list(v)
                mask = [x is None for x in v]
                cols.append((v, mask if any(mask) else None))
        lengths = {len(v) for v, _ in cols}
        if len(lengths) > 1:
            raise ProgrammingError("the columns are of different lengths")
        n = lengths.pop() if lengths else 0
    else:  # rows
        rows = [tuple(r) for r in data]
        width = {len(r) for r in rows}
        if len(width) > 1:
            raise ProgrammingError("the rows are of different lengths")
        for v in zip(*rows):
            v = list(v)
            mask = [x is None for x in v]
            cols.append((v, mask if any(mask) else None))
        n = len(rows)
    if columns is not None:
        if names is not None and len(columns) != len(names):
            raise ProgrammingError(f"{len(columns)} column names for {len(names)} columns of data")
        names = list(columns)
    return names, cols, n


# struct formats of fixed-width types by tag (per value, without numpy).
_PACK = {1: "<?", 12: "<h", 13: "<i", 2: "<q", 14: "<f", 3: "<d"}
_NP_TYPE = {1: "u1", 12: "<i2", 13: "<i4", 2: "<i8", 14: "<f4", 3: "<f8"}


def _fixed_values(c, values, mask):
    """A fixed-width column's slots as bytes (numpy, without a Python object
    per value, when the values are a numpy array of a fitting kind)."""
    np = _np()
    tag = c.tag
    if np is not None and isinstance(values, np.ndarray) and values.dtype != object:
        kind = values.dtype.kind
        arr = None
        if tag in _NP_TYPE and kind in "biuf":
            t = np.dtype(_NP_TYPE[tag])
            if tag != 1 and t.kind == "i" and kind in "fiu" and len(values):
                live = values if mask is None else values[~mask]
                if kind == "f" and len(live) and not np.all(np.floor(live) == live):
                    raise DataError(f"column \"{c.name}\" ({c.type}) takes whole numbers")
                info = np.iinfo(t)
                if len(live) and (live.min() < info.min or live.max() > info.max):
                    raise DataError(f"a value of column \"{c.name}\" is out of range for {c.type}")
            if mask is not None and kind == "f":
                values = np.where(mask, 0, values)  # (NULLs' NaN)
            arr = values.astype(t)
        elif tag == 6 and kind == "M":
            arr = (values.astype("datetime64[D]").astype("<i8") - _DAYS_1970).astype("<i4")
        elif tag in (7, 8) and kind == "M":
            arr = values.astype("datetime64[us]").astype("<i8") - _US_1970
        elif tag == 16 and kind == "m":
            arr = values.astype("timedelta64[us]").astype("<i8")
        if arr is not None:
            if mask is not None:
                arr = arr.copy()
                arr[mask] = 0
            return arr.tobytes()
    w = _FIXED.get(tag, (16, None))[0]
    out = bytearray()
    for i, v in enumerate(values):
        if (mask is not None and mask[i]) or v is None:
            out += bytes(w)
        else:
            out += _fixed_slot(c, v)
    return bytes(out)


def _fixed_slot(c, v):
    """One value's slot in a fixed-width column."""
    tag = c.tag
    np = _np()
    if np is not None and isinstance(v, np.generic):
        v = v.item()
    try:
        if tag in _PACK:
            if tag in (12, 13, 2) and isinstance(v, float):
                if v != int(v):
                    raise ValueError
                v = int(v)
            return _struct.pack(_PACK[tag], v)
        if tag == 6:
            if isinstance(v, _dt.datetime):
                v = v.date()
            return _struct.pack("<i", (v - _EPOCH_DATE).days)
        if tag in (7, 8):
            if isinstance(v, _dt.datetime) and v.tzinfo is not None:
                v = v.astimezone(_dt.timezone.utc).replace(tzinfo=None)
            elif isinstance(v, _dt.date) and not isinstance(v, _dt.datetime):
                v = _dt.datetime(v.year, v.month, v.day)
            d = v - _EPOCH
            return _struct.pack("<q", (d.days * 86_400 + d.seconds) * 1_000_000 + d.microseconds)
        if tag == 16:
            if isinstance(v, _dt.timedelta):
                us = (v.days * 86_400 + v.seconds) * 1_000_000 + v.microseconds
            else:
                us = ((v.hour * 60 + v.minute) * 60 + v.second) * 1_000_000 + v.microsecond
            return _struct.pack("<q", us)
        if tag == 9:
            if isinstance(v, _dt.timedelta):
                v = Interval(0, v.days, v.seconds * 1_000_000 + v.microseconds)
            return _struct.pack("<iiq", v.months, v.days, v.microseconds)
        if tag == 5:
            d = v if isinstance(v, _decimal.Decimal) else _decimal.Decimal(str(v))
            sign, digits, exp = d.as_tuple()
            m = int("".join(map(str, digits)) or "0")
            m = -m if sign else m
            scale = -exp if exp < 0 else 0
            if exp > 0:
                m *= 10 ** exp
            return m.to_bytes(16, "little", signed=True) + bytes([scale])
        if tag == _EXT and c.type == "uuid":
            return (v if isinstance(v, _uuid.UUID) else _uuid.UUID(str(v))).bytes
    except (TypeError, ValueError, AttributeError, _struct.error, OverflowError) as e:
        raise DataError(f"{v!r} cannot be sent as column \"{c.name}\" ({c.type})") from e
    raise DataError(f"{v!r} cannot be sent as column \"{c.name}\" ({c.type})")


def _var_slot(c, v):
    """One value's bytes in a variable-width column."""
    tag = c.tag
    if tag == 15:
        return bytes(v)
    if tag == _EXT:
        return v.data if isinstance(v, Ext) else bytes(v)
    if tag in (18, 19):
        out = bytearray()
        _encode(out, v)
        return bytes(out[1:])
    if tag == 10:
        out = bytearray()
        _encode(out, list(v))
        return bytes(out)
    if isinstance(v, bytes):
        return v
    return str(v).encode()


def _encode_batch(columns, cols, start, end):
    """A ``B`` batch of rows ``start``..``end`` of ``cols`` (DOCS.md §10)."""
    n = end - start
    out = bytearray(_struct.pack("<I", n))

    def pad():
        out.extend(bytes(-len(out) % 8))

    for c, (values, mask) in zip(columns, cols):
        values = values[start:end]
        mask = None if mask is None else mask[start:end]
        np = _np()
        if mask is not None and np is not None and isinstance(mask, np.ndarray):
            nulls = bool(mask.any())
        else:
            if mask is None and not (np is not None and isinstance(values, np.ndarray) and values.dtype != object):
                mask = [v is None for v in values]
            nulls = mask is not None and any(mask)
        out.append(1 if nulls else 0)
        if nulls:
            pad()
            if np is not None and isinstance(mask, np.ndarray):
                out += np.packbits(~mask, bitorder="little").tobytes()
            else:
                bits = bytearray((n + 7) // 8)
                for i, m in enumerate(mask):
                    if not m:
                        bits[i // 8] |= 1 << (i % 8)
                out += bits
        pad()
        if _width(c):
            out += _fixed_values(c, values, mask if nulls else None)
        else:
            parts, offs, at = [], [0], 0
            for i, v in enumerate(values):
                if not (nulls and mask[i]) and v is not None:
                    b = _var_slot(c, v)
                    parts.append(b)
                    at += len(b)
                offs.append(at)
            out += _struct.pack(f"<{n + 1}I", *offs)
            pad()
            out += b"".join(parts)
        pad()
    return bytes(out)


class Cursor:
    """A DB-API cursor.  ``execute`` runs one statement; its rows are read
    as they are fetched, ``itersize`` at a time from the server, so
    ``rowcount`` of a query is -1 until its last row is read."""

    arraysize = 1
    itersize = DEFAULT_FETCH

    def __init__(self, conn):
        self.connection = conn
        self.description = None
        self.rowcount = -1
        self._stream = None

    def execute(self, sql, params=()):
        self.close()
        s = Stream(self.connection, sql, list(params), self.itersize)
        self._stream = s
        self.rowcount = s.count if s.count is not None else -1
        self.description = (
            [(c.name, c.type, None, None, None, None, None) for c in s.columns]
            if s.columns else None
        )
        return self

    def _next(self):
        if self._stream is None:
            return None
        row = next(self._stream, None)
        if row is None and self._stream.count is not None:
            self.rowcount = self._stream.count
        return row

    def executemany(self, sql, seq):
        total = 0
        for params in seq:
            self.execute(sql, params)
            total += self.rowcount
        self.rowcount = total

    def fetchone(self):
        return self._next()

    def fetchmany(self, size=None):
        out = []
        for _ in range(size or self.arraysize):
            row = self._next()
            if row is None:
                break
            out.append(row)
        return out

    def fetchall(self):
        out = []
        while (row := self._next()) is not None:
            out.append(row)
        return out

    def __iter__(self):
        while (row := self._next()) is not None:
            yield row

    def close(self):
        if self._stream is not None:
            self._stream.close()
            self._stream = None


def connect(host="127.0.0.1", port=DEFAULT_PORT, user="nucleus", password="nucleus",
            database="nucleus", timeout=10.0, tls=False, tls_ca=None, tls_verify=True,
            tls_cert=None, tls_key=None, token=None):
    """Opens a connection (the defaults reach a local server as it starts).

    ``tls=True`` encrypts it, the server's certificate checked against the
    system's CA certificates — or against ``tls_ca`` (a PEM file; giving it
    implies ``tls``); ``tls_verify=False`` skips the check (encrypted, but
    anyone can pose as the server).  ``tls_cert`` and ``tls_key`` (PEM
    files) show the server a client certificate: one naming the user logs
    it in without a password, if the server accepts its CA.  ``token`` (an
    OpenID Connect identity token) logs in instead of the password: it
    names the user."""
    return Connection(host, port, user, password, database, timeout,
                      tls=tls, tls_ca=tls_ca, tls_verify=tls_verify,
                      tls_cert=tls_cert, tls_key=tls_key, token=token)
