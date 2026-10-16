#!/usr/bin/env bash
# Builds a release of the `nucleus` binary for one Rust target and packs it
# for that platform (PLAN.md, "Packaging and install").
#
#   scripts/package.sh <rust-target> [<out-dir>]      (default out-dir: dist)
#
# Linux and macOS: nucleus-<version>-<os>-<arch>[-musl].tar.gz
# Windows:         nucleus-<version>-windows-<arch>.zip
# musl targets also produce the d-os app archive:
#                  nucleus-<version>-d-os-<arch>.tar.gz (manifest included)
# Every archive has one top directory, nucleus-<version>/, with bin/ inside.
set -euo pipefail

target=${1:?usage: scripts/package.sh <rust-target> [<out-dir>]}
out=${2:-dist}
root=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
name="nucleus-$version"

cargo build --release --locked --target "$target" --bin nucleus --manifest-path "$root/Cargo.toml"

arch=${target%%-*}
exe=nucleus
case "$target" in
  *-linux-musl)   kinds="linux-musl d-os" ;;
  *-linux-gnu)    kinds="linux" ;;
  *-apple-darwin) kinds="macos" ;;
  *-windows-*)    kinds="windows"; exe=nucleus.exe ;;
  *) echo "package.sh: no packaging rule for $target" >&2; exit 1 ;;
esac
bin="$root/target/$target/release/$exe"

mkdir -p "$out"
out=$(cd "$out" && pwd)
for kind in $kinds; do
  stage=$(mktemp -d)
  mkdir -p "$stage/$name/bin"
  cp "$bin" "$stage/$name/bin/"
  cp "$root/LICENSE" "$root/README.md" "$stage/$name/"
  case $kind in
    linux)      file="$name-linux-$arch.tar.gz" ;;
    linux-musl) file="$name-linux-$arch-musl.tar.gz" ;;
    macos)      file="$name-macos-$arch.tar.gz" ;;
    d-os)
      # d-os names the 32-bit x86 architecture i386.
      dos_arch=$arch; [ "$arch" = i686 ] && dos_arch=i386
      file="$name-d-os-$dos_arch.tar.gz"
      cp "$root/integrations/d-os/nucleus.dos-app" "$stage/$name/.dos-app" ;;
    windows)    file="$name-windows-$arch.zip" ;;
  esac
  if [ "$kind" = windows ]; then
    (cd "$stage" && powershell -NoProfile -Command "Compress-Archive -Path '$name' -DestinationPath '$file'")
    mv "$stage/$file" "$out/"
  else
    tar -C "$stage" -czf "$out/$file" "$name"
  fi
  rm -rf "$stage"
  echo "$out/$file"
done
