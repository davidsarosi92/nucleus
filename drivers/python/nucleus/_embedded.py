"""The embedded edition (PLAN.md section M14.23): nucleus in this process.

The engine is a shared library (``nucleus_embedded``, its C API in
``editions/embedded/include/nucleus_embedded.h``) loaded with ctypes.  A
session on it speaks the nucleus protocol over a pipe in the process, so
``Connection`` runs over it exactly as over a socket: one API for both.

The library is looked for in ``NUCLEUS_EMBEDDED_LIB`` (a path), then
beside this file (where the platform wheel puts it).
"""

import ctypes as _ct
import os as _os
import sys as _sys

_NAMES = {
    "darwin": "libnucleus_embedded.dylib",
    "win32": "nucleus_embedded.dll",
}
_lib = None


def library_path():
    """Where the library is looked for first."""
    given = _os.environ.get("NUCLEUS_EMBEDDED_LIB")
    if given:
        return given
    name = _NAMES.get(_sys.platform, "libnucleus_embedded.so")
    return _os.path.join(_os.path.dirname(__file__), name)


def lib():
    """The library, loaded once (an OSError if it is not there)."""
    global _lib
    if _lib is not None:
        return _lib
    path = library_path()
    if not _os.path.exists(path):
        raise OSError(
            f"the embedded edition is not installed ({path} is missing): install the "
            "nucleus-db wheel for this platform, or set NUCLEUS_EMBEDDED_LIB")
    # CDLL releases the GIL in each call: a session waiting on its reply
    # holds up no other thread.
    l = _ct.CDLL(path)
    vp, sz, err = _ct.c_void_p, _ct.c_size_t, _ct.POINTER(_ct.c_void_p)
    l.nc_version.restype = _ct.c_char_p
    l.nc_version.argtypes = []
    l.nc_open.restype = vp
    l.nc_open.argtypes = [_ct.c_char_p, _ct.c_char_p, err]
    l.nc_connect.restype = vp
    l.nc_connect.argtypes = [vp, err]
    l.nc_send.restype = _ct.c_int
    l.nc_send.argtypes = [vp, _ct.c_char_p, sz]
    l.nc_recv.restype = _ct.c_int64
    l.nc_recv.argtypes = [vp, _ct.c_void_p, sz, _ct.c_int64]
    l.nc_disconnect.restype = None
    l.nc_disconnect.argtypes = [vp]
    l.nc_close.restype = _ct.c_int
    l.nc_close.argtypes = [vp, err]
    l.nc_free_error.restype = None
    l.nc_free_error.argtypes = [vp]
    _lib = l
    return l


def version():
    """The engine's version."""
    return lib().nc_version().decode()


def _message(l, e):
    text = _ct.cast(e, _ct.c_char_p).value.decode("utf-8", "replace")
    l.nc_free_error(e)
    return text


def open_database(path, options):
    """A database handle (an int), or OSError with the engine's message."""
    l = lib()
    e = _ct.c_void_p()
    db = l.nc_open(path.encode(), options.encode(), _ct.byref(e))
    if not db:
        raise OSError(_message(l, e))
    return db


def close_database(db):
    l = lib()
    e = _ct.c_void_p()
    if l.nc_close(db, _ct.byref(e)) != 0:
        raise OSError(_message(l, e))


class Pipe:
    """One session's bytes, behind the part of a socket's interface the
    driver uses."""

    _CAP = 1 << 16

    def __init__(self, db):
        l = lib()
        e = _ct.c_void_p()
        c = l.nc_connect(db, _ct.byref(e))
        if not c:
            raise OSError(_message(l, e))
        self._lib, self._c = l, c
        self._buf = _ct.create_string_buffer(self._CAP)

    def sendall(self, data):
        if self._c is None:
            raise OSError("the connection is closed")
        if not isinstance(data, bytes):
            data = bytes(data)
        if self._lib.nc_send(self._c, data, len(data)) != 0:
            raise OSError("the session has ended")

    def recv(self, n):
        if self._c is None:
            return b""
        k = self._lib.nc_recv(self._c, self._buf, min(n, self._CAP), -1)
        return _ct.string_at(self._buf, k) if k > 0 else b""

    def settimeout(self, _timeout):
        pass

    def close(self):
        if self._c is not None:
            self._lib.nc_disconnect(self._c)
            self._c = None
