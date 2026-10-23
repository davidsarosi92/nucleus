#!/usr/bin/env python3
"""A directory for the driver tests (scripts/driver-tests.sh): it speaks
just enough LDAP (RFC 4511) — simple binds, unbind — and knows one entry,
uid=drivers_ldap,ou=people,dc=test with the password ldap-secret.
Prints the port it listens on (127.0.0.1), then serves until killed."""

import socket
import sys
import threading

ENTRIES = {"uid=drivers_ldap,ou=people,dc=test": b"ldap-secret"}


def tlv(tag, contents):
    n = len(contents)
    if n < 0x80:
        return bytes([tag, n]) + contents
    return bytes([tag, 0x82, n >> 8, n & 0xFF]) + contents


def split(b):
    """(tag, contents, rest) of the first element."""
    tag, n, i = b[0], b[1], 2
    if n & 0x80:
        k = n & 0x7F
        n, i = int.from_bytes(b[2:2 + k], "big"), 2 + k
    return tag, b[i:i + n], b[i + n:]


def read_exact(s, n):
    out = b""
    while len(out) < n:
        chunk = s.recv(n - len(out))
        if not chunk:
            raise EOFError
        out += chunk
    return out


def conversation(s):
    try:
        while True:
            head = read_exact(s, 2)
            n = head[1]
            if n & 0x80:
                n = int.from_bytes(read_exact(s, n & 0x7F), "big")
            body = read_exact(s, n)
            _, msg_id, rest = split(body)
            op, contents, _ = split(rest)
            if op != 0x60:  # anything but a bind: unbind, or not supported
                return
            _, _version, rest = split(contents)
            _, dn, rest = split(rest)
            _, password, _ = split(rest)
            ok = password and ENTRIES.get(dn.decode()) == password
            result = tlv(0x0A, bytes([0 if ok else 49])) + tlv(4, b"") + tlv(4, b"")
            s.sendall(tlv(0x30, tlv(2, msg_id) + tlv(0x61, result)))
    except (EOFError, OSError, IndexError):
        pass
    finally:
        s.close()


def main():
    server = socket.socket()
    server.bind(("127.0.0.1", 0))
    server.listen()
    print(server.getsockname()[1], flush=True)
    while True:
        s, _ = server.accept()
        threading.Thread(target=conversation, args=(s,), daemon=True).start()


if __name__ == "__main__":
    sys.exit(main())
