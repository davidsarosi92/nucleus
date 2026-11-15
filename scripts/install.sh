#!/bin/sh
# Installs nucleus on Linux or macOS (PLAN.md §M14.17 E1):
#
#   curl -fsSL https://raw.githubusercontent.com/davidsarosi92/nucleus/main/scripts/install.sh | sh
#
# The platform's archive of a release, its SHA-256 checked against the
# release's SHA256SUMS, its binaries (nucleus, ncsql) put in
# $NUCLEUS_INSTALL/bin (default ~/.nucleus/bin).  Nothing outside that
# directory is changed: the shell's PATH is told, not edited.
#
#   NUCLEUS_VERSION        a version (0.1.0) instead of the latest release
#   NUCLEUS_INSTALL        where to install (default ~/.nucleus)
#   NUCLEUS_DOWNLOAD_URL   where the archives and SHA256SUMS are (a mirror,
#                          or a directory of scripts/package.sh's: a path)
set -eu

repo=davidsarosi92/nucleus
install=${NUCLEUS_INSTALL:-$HOME/.nucleus}

say() { printf '%s\n' "$*" >&2; }
fail() { say "install.sh: $*"; exit 1; }

case $(uname -s) in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) fail "no archive for $(uname -s): on Windows, scripts/install.ps1; elsewhere, build from source (cargo build --release -p nucleus-cli)" ;;
esac
case $(uname -m) in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  i386 | i486 | i586 | i686) arch=i686 ;;
  *) fail "no archive for the $(uname -m) architecture" ;;
esac
# Linux: the static (musl) build, which runs on any distribution.
case $os in
  linux) suffix="-linux-$arch-musl.tar.gz" ;;
  macos)
    [ "$arch" = i686 ] && fail "no archive for 32-bit macOS"
    # A shell under Rosetta reports x86_64 on Apple silicon: the native one.
    if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
      arch=aarch64
    fi
    suffix="-macos-$arch.tar.gz" ;;
esac

if [ -n "${NUCLEUS_DOWNLOAD_URL:-}" ]; then
  base=${NUCLEUS_DOWNLOAD_URL%/}
elif [ -n "${NUCLEUS_VERSION:-}" ]; then
  base="https://github.com/$repo/releases/download/v${NUCLEUS_VERSION#v}"
else
  base="https://github.com/$repo/releases/latest/download"
fi

if command -v curl >/dev/null 2>&1; then
  download() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  download() { wget -q -O "$2" "$1"; }
else
  download() { fail "needs curl or wget"; }
fi
# A local directory (file:// or a path) is copied from.
fetch() {
  case $1 in
    file://*) cp "${1#file://}" "$2" ;;
    /*) cp "$1" "$2" ;;
    *) download "$1" "$2" ;;
  esac
}
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  fail "needs sha256sum or shasum"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "could not download $base/SHA256SUMS"
# The release's archive for this platform: nucleus-<version><suffix>
# (after "  ", or " *": a sum made in binary mode, as Windows' sha256sum's).
line=$(grep -E " [ *]nucleus-[0-9][^ ]*$(printf '%s' "$suffix" | sed 's/\./\\./g')\$" "$tmp/SHA256SUMS" | head -1) ||
  fail "the release has no archive *$suffix"
sum=${line%% *}
file=${line##* }
file=${file#\*}
version=${file#nucleus-}
version=${version%"$suffix"}

say "downloading nucleus $version ($file)"
fetch "$base/$file" "$tmp/$file" || fail "could not download $base/$file"
[ "$(sha256 "$tmp/$file")" = "$sum" ] || fail "$file does not match its SHA-256 in SHA256SUMS"

tar -C "$tmp" -xzf "$tmp/$file"
mkdir -p "$install/bin"
for b in "$tmp/nucleus-$version/bin/"*; do
  # Replaced by a rename, so a running nucleus keeps its old file.
  cp "$b" "$install/bin/.$(basename "$b").new"
  chmod 755 "$install/bin/.$(basename "$b").new"
  mv -f "$install/bin/.$(basename "$b").new" "$install/bin/$(basename "$b")"
done
"$install/bin/nucleus" --version >/dev/null || fail "the installed nucleus does not run"

say "installed nucleus $version in $install/bin"
case ":$PATH:" in
  *":$install/bin:"*) ;;
  *)
    say ""
    say "add it to your PATH, e.g. in ~/.profile, ~/.bashrc or ~/.zshrc:"
    say "  export PATH=\"$install/bin:\$PATH\"" ;;
esac
say ""
say "a first query, in a database in memory:"
say "  nucleus sql :memory: -c \"SELECT 'hello, nucleus'\""
say "a server (its data in ./data): nucleus server data — then: nucleus sql"
