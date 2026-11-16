"""Client-side encrypted columns (nucleus PLAN.md §M14.30 K5, DOCS.md §11).

A value the driver encrypts before it is sent and decrypts after it
arrives: the server stores ``bytea`` ciphertext and never holds the key.
Equality is queryable through a blind index stored beside it; nothing
else is.  Format 1, every driver's (``drivers/encrypted-columns.txt``
holds its test vectors):

- keys: ``HMAC-SHA-256(master, "nucleus column 1 encrypt\\0" + name)``
  and ``HMAC-SHA-256(master, "nucleus column 1 index\\0" + name)``;
- a value: a type byte (0 bytes, 1 text in UTF-8, 2 an integer: 8 bytes
  little-endian) and its bytes;
- ciphertext: the byte 1, a random 24-byte nonce, XChaCha20-Poly1305 of
  the value (the byte 1 authenticated beside it);
- blind index: the first 16 bytes of HMAC-SHA-256 of the value under the
  index key.

    key = ColumnKey(master, "users.email")       # master: 32 bytes you keep
    db.execute("INSERT INTO users VALUES ($1, $2, $3)",
               [1, key.encrypt("ann@example.com"), key.blind_index("ann@example.com")])
    row = db.execute("SELECT email FROM users WHERE email_index = $1",
                     [key.blind_index("ann@example.com")]).rows[0]
    key.decrypt(row[0])                          # 'ann@example.com'

Plain Python, without a library: ChaCha20 and Poly1305 as RFC 8439
writes them, HChaCha20 as draft-irtf-cfrg-xchacha does.
"""

import hashlib as _hashlib
import hmac as _hmac
import os as _os
import struct as _struct

__all__ = ["ColumnKey", "FORMAT", "INDEX_LEN"]

FORMAT = 1
INDEX_LEN = 16

_M = 0xFFFFFFFF
_SIGMA = (0x61707865, 0x3320646E, 0x79622D32, 0x6B206574)


def _rounds(s):
    def q(a, b, c, d):
        s[a] = (s[a] + s[b]) & _M
        x = s[d] ^ s[a]
        s[d] = ((x << 16) | (x >> 16)) & _M
        s[c] = (s[c] + s[d]) & _M
        x = s[b] ^ s[c]
        s[b] = ((x << 12) | (x >> 20)) & _M
        s[a] = (s[a] + s[b]) & _M
        x = s[d] ^ s[a]
        s[d] = ((x << 8) | (x >> 24)) & _M
        s[c] = (s[c] + s[d]) & _M
        x = s[b] ^ s[c]
        s[b] = ((x << 7) | (x >> 25)) & _M

    for _ in range(10):
        q(0, 4, 8, 12)
        q(1, 5, 9, 13)
        q(2, 6, 10, 14)
        q(3, 7, 11, 15)
        q(0, 5, 10, 15)
        q(1, 6, 11, 12)
        q(2, 7, 8, 13)
        q(3, 4, 9, 14)


def _hchacha20(key, nonce16):
    s = list(_SIGMA) + list(_struct.unpack("<8I", key)) + list(_struct.unpack("<4I", nonce16))
    _rounds(s)
    return _struct.pack("<8I", *(s[:4] + s[12:]))


def _chacha20(key, counter, nonce12, data):
    init = list(_SIGMA) + list(_struct.unpack("<8I", key)) + [0] + list(_struct.unpack("<3I", nonce12))
    out = bytearray(data)
    for at in range(0, len(data), 64):
        init[12] = (counter + at // 64) & _M
        s = init[:]
        _rounds(s)
        block = _struct.pack("<16I", *((s[i] + init[i]) & _M for i in range(16)))
        for i in range(min(64, len(data) - at)):
            out[at + i] ^= block[i]
    return bytes(out)


def _poly1305(key, msg):
    r = int.from_bytes(key[:16], "little") & 0x0FFFFFFC0FFFFFFC0FFFFFFC0FFFFFFF
    s = int.from_bytes(key[16:], "little")
    p = (1 << 130) - 5
    acc = 0
    for at in range(0, len(msg), 16):
        n = int.from_bytes(msg[at:at + 16] + b"\x01", "little")
        acc = (acc + n) * r % p
    return ((acc + s) & ((1 << 128) - 1)).to_bytes(16, "little")


def _pad16(b):
    return b"\0" * (-len(b) % 16)


def _tag(key, nonce12, aad, ct):
    otk = _chacha20(key, 0, nonce12, b"\0" * 32)
    mac = aad + _pad16(aad) + ct + _pad16(ct) + _struct.pack("<QQ", len(aad), len(ct))
    return _poly1305(otk, mac)


def _xkey(key, nonce24):
    return _hchacha20(key, nonce24[:16]), b"\0\0\0\0" + nonce24[16:]


def xseal(key, nonce24, aad, plaintext):
    """XChaCha20-Poly1305: the ciphertext and its 16-byte tag."""
    k, n = _xkey(key, nonce24)
    ct = _chacha20(k, 1, n, plaintext)
    return ct + _tag(k, n, aad, ct)


def xopen(key, nonce24, aad, sealed):
    """What :func:`xseal` made, or ``None`` when it does not open."""
    if len(sealed) < 16:
        return None
    k, n = _xkey(key, nonce24)
    ct, tag = sealed[:-16], sealed[-16:]
    if not _hmac.compare_digest(_tag(k, n, aad, ct), tag):
        return None
    return _chacha20(k, 1, n, ct)


def _encode(value):
    if isinstance(value, bool):
        raise TypeError("an encrypted column holds bytes, text or an integer, not a bool")
    if isinstance(value, (bytes, bytearray, memoryview)):
        return b"\x00" + bytes(value)
    if isinstance(value, str):
        return b"\x01" + value.encode()
    if isinstance(value, int):
        return b"\x02" + _struct.pack("<q", value)
    raise TypeError(f"an encrypted column holds bytes, text or an integer, not {type(value).__name__}")


def _decode(b):
    kind, rest = b[:1], b[1:]
    if kind == b"\x00":
        return bytes(rest)
    if kind == b"\x01":
        return rest.decode()
    if kind == b"\x02" and len(rest) == 8:
        return _struct.unpack("<q", rest)[0]
    raise ValueError("an encrypted value of no type known")


class ColumnKey:
    """A column's keys, from a 32-byte ``master`` key the application keeps
    (a KMS's, a keychain's: never the database's) and the column's
    ``name``: encrypts values (``bytes``, ``str``, ``int``), decrypts them,
    and makes their blind indexes."""

    def __init__(self, master, name):
        master = bytes(master)
        if len(master) != 32:
            raise ValueError("a master key is 32 bytes")
        name = name.encode()
        self._encrypt = _hmac.new(master, b"nucleus column 1 encrypt\0" + name, _hashlib.sha256).digest()
        self._index = _hmac.new(master, b"nucleus column 1 index\0" + name, _hashlib.sha256).digest()

    def __repr__(self):
        return "ColumnKey(..)"

    def encrypt(self, value, nonce=None):
        """``value`` encrypted, the ``bytes`` to store: a fresh random nonce
        each time (``nonce`` for test vectors only — one used twice gives
        both values away)."""
        nonce = _os.urandom(24) if nonce is None else bytes(nonce)
        if len(nonce) != 24:
            raise ValueError("a nonce is 24 bytes")
        head = bytes([FORMAT])
        return head + nonce + xseal(self._encrypt, nonce, head, _encode(value))

    def decrypt(self, sealed):
        """The value in ``sealed`` (``None`` stays ``None``); ``ValueError``
        when this key did not make it, or it was changed."""
        if sealed is None:
            return None
        sealed = bytes(sealed)
        if len(sealed) < 25 or sealed[0] != FORMAT:
            raise ValueError("an encrypted value this column's key does not open")
        plain = xopen(self._encrypt, sealed[1:25], sealed[:1], sealed[25:])
        if plain is None:
            raise ValueError("an encrypted value this column's key does not open")
        return _decode(plain)

    def blind_index(self, value):
        """``value``'s blind index (16 bytes): equal values have equal ones,
        so ``WHERE email_index = $1`` with it finds a value's rows."""
        return _hmac.new(self._index, _encode(value), _hashlib.sha256).digest()[:INDEX_LEN]
