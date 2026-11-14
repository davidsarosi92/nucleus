#!/usr/bin/env bash
# The container image's build context (packaging/Dockerfile, PLAN.md
# §M14.17 E1) from scripts/package.sh's archives: the static binaries of
# each Linux musl archive in <ctx>/linux/<amd64|arm64>/, an empty data/.
#
#   scripts/image-context.sh <dist> <ctx>
set -euo pipefail

dist=${1:?usage: scripts/image-context.sh <dist> <ctx>}
ctx=${2:?usage: scripts/image-context.sh <dist> <ctx>}
rm -rf "$ctx"
mkdir -p "$ctx/data"
found=0
for f in "$dist"/nucleus-*-linux-*-musl.tar.gz; do
  [ -e "$f" ] || continue
  case $f in
    *-linux-x86_64-musl.tar.gz) platform=linux/amd64 ;;
    *-linux-aarch64-musl.tar.gz) platform=linux/arm64 ;;
    *) continue ;;
  esac
  mkdir -p "$ctx/$platform" "$ctx/unpacked"
  tar -C "$ctx/unpacked" -xzf "$f"
  mv "$ctx"/unpacked/nucleus-*/bin/* "$ctx/$platform/"
  rm -rf "$ctx/unpacked"
  echo "$ctx/$platform: $(ls "$ctx/$platform" | tr '\n' ' ')"
  found=1
done
[ "$found" = 1 ] || { echo "image-context.sh: no Linux musl archive in $dist" >&2; exit 1; }
