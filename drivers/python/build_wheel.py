"""Builds the nucleus-db wheel for this platform: the driver with the
embedded edition's library inside (PLAN.md section M14.23).

    cargo build --release -p nucleus-embedded
    python3 drivers/python/build_wheel.py target/release/libnucleus_embedded.dylib --out dist

One wheel serves every Python 3 version (the library is loaded with
ctypes, not built against Python): py3-none-<platform>.  Without a
library, `pip install drivers/python` still makes the pure-Python wheel
(a server's driver only).  Standard library only.
"""

import argparse
import base64
import hashlib
import os
import platform
import re
import sys
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
NAME, DIST = "nucleus-db", "nucleus_db"


def version():
    text = open(os.path.join(HERE, "pyproject.toml"), encoding="utf-8").read()
    return re.search(r'^version = "([^"]+)"', text, re.M).group(1)


def platform_tag(lib):
    """The wheel's platform tag, from the library's own format."""
    machine = platform.machine().lower()
    if sys.platform == "darwin":
        arch = "arm64" if machine in ("arm64", "aarch64") else "x86_64"
        # Rust's oldest macOS for each architecture, unless told another.
        target = os.environ.get("MACOSX_DEPLOYMENT_TARGET") or ("11.0" if arch == "arm64" else "10.12")
        major, minor = (target.split(".") + ["0"])[:2]
        return f"macosx_{major}_{minor}_{arch}"
    if sys.platform == "win32":
        return "win_arm64" if machine in ("arm64", "aarch64") else "win_amd64"
    arch = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    libc, ver = platform.libc_ver()
    if libc == "glibc":
        major, minor = ver.split(".")[:2]
        return f"manylinux_{major}_{minor}_{arch}"
    return f"musllinux_1_2_{arch}"


def record_line(name, data):
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
    return f"{name},sha256={digest},{len(data)}"


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("lib", help="the embedded edition's shared library")
    p.add_argument("--out", default="dist", help="where the wheel goes")
    p.add_argument("--plat", help="the platform tag (default: this machine's)")
    a = p.parse_args()

    v = version()
    plat = a.plat or platform_tag(a.lib)
    lib_name = {"darwin": "libnucleus_embedded.dylib", "win32": "nucleus_embedded.dll"}.get(
        sys.platform, "libnucleus_embedded.so")
    files = {}
    pkg = os.path.join(HERE, "nucleus")
    for name in sorted(os.listdir(pkg)):
        if name.endswith(".py"):
            files[f"nucleus/{name}"] = open(os.path.join(pkg, name), "rb").read()
    files[f"nucleus/{lib_name}"] = open(a.lib, "rb").read()
    info = f"{DIST}-{v}.dist-info"
    readme = open(os.path.join(HERE, "README.md"), encoding="utf-8").read()
    files[f"{info}/METADATA"] = (
        "Metadata-Version: 2.1\n"
        f"Name: {NAME}\nVersion: {v}\n"
        "Summary: Python driver for the nucleus database, with the embedded edition "
        "(a database in this process)\n"
        "Home-page: https://github.com/davidsarosi92/nucleus\n"
        "License: MIT\nRequires-Python: >=3.8\n"
        "Description-Content-Type: text/markdown\n\n" + readme).encode()
    files[f"{info}/WHEEL"] = (
        "Wheel-Version: 1.0\nGenerator: nucleus build_wheel.py\nRoot-Is-Purelib: false\n"
        f"Tag: py3-none-{plat}\n").encode()
    files[f"{info}/LICENSE"] = open(os.path.join(HERE, "LICENSE"), "rb").read()
    record = [record_line(n, d) for n, d in files.items()] + [f"{info}/RECORD,,"]
    files[f"{info}/RECORD"] = ("\n".join(record) + "\n").encode()

    os.makedirs(a.out, exist_ok=True)
    wheel = os.path.join(a.out, f"{DIST}-{v}-py3-none-{plat}.whl")
    with zipfile.ZipFile(wheel, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in files.items():
            zi = zipfile.ZipInfo(name, date_time=(2000, 1, 1, 0, 0, 0))
            zi.compress_type = zipfile.ZIP_DEFLATED
            # The library executable, the rest readable.
            zi.external_attr = (0o755 if name.endswith(lib_name) else 0o644) << 16
            z.writestr(zi, data)
    print(wheel, os.path.getsize(wheel))


if __name__ == "__main__":
    main()
