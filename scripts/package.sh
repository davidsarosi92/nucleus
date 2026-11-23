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
# and, where dpkg-deb / rpmbuild are installed, Linux packages of the
# static binaries (any distribution's): nucleus_<version>_<arch>.deb,
#                  nucleus-<version>-1.<arch>.rpm
# Every archive has one top directory, nucleus-<version>/, with bin/ inside.
set -euo pipefail

target=${1:?usage: scripts/package.sh <rust-target> [<out-dir>]}
out=${2:-dist}
root=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
name="nucleus-$version"
# Where cargo builds (CARGO_TARGET_DIR, as cargo takes it).
tdir=${CARGO_TARGET_DIR:-$root/target}

cargo build --release --locked --target "$target" --bin nucleus --bin ncsql --manifest-path "$root/Cargo.toml"
case "$target" in
  *-linux-musl) # the d-os settings service ships in the d-os archive
    cargo build --release --locked --target "$target" --bin nucleus-settings --manifest-path "$root/Cargo.toml" ;;
esac

arch=${target%%-*}
exe=nucleus; shell=ncsql
case "$target" in
  *-linux-musl)
    kinds="linux-musl d-os"
    if command -v dpkg-deb >/dev/null; then kinds="$kinds deb"; else echo "package.sh: no dpkg-deb, no .deb" >&2; fi
    if command -v rpmbuild >/dev/null; then kinds="$kinds rpm"; else echo "package.sh: no rpmbuild, no .rpm" >&2; fi ;;
  *-linux-gnu)    kinds="linux" ;;
  *-apple-darwin) kinds="macos" ;;
  *-windows-*)    kinds="windows"; exe=nucleus.exe; shell=ncsql.exe ;;
  *) echo "package.sh: no packaging rule for $target" >&2; exit 1 ;;
esac
bin="$tdir/$target/release/$exe"

mkdir -p "$out"
out=$(cd "$out" && pwd)
summary="A database: SQL, documents, vectors and text search in one engine"
for kind in $kinds; do
  stage=$(mktemp -d)
  case $kind in
    deb)
      case $arch in x86_64) da=amd64 ;; aarch64) da=arm64 ;; i686) da=i386 ;; esac
      file="nucleus_${version}_$da.deb"
      pkg="$stage/pkg"
      mkdir -p "$pkg/DEBIAN" "$pkg/usr/bin" "$pkg/usr/share/doc/nucleus"
      cp "$bin" "$tdir/$target/release/$shell" "$pkg/usr/bin/"
      cat "$root/COPYRIGHT" "$root/LICENSE" "$root/THIRD_PARTY.md" >"$pkg/usr/share/doc/nucleus/copyright"
      cat > "$pkg/DEBIAN/control" <<CONTROL
Package: nucleus
Version: $version
Architecture: $da
Maintainer: nucleus <nucleus@users.noreply.github.com>
Section: database
Priority: optional
Homepage: https://github.com/davidsarosi92/nucleus
Description: $summary
 The nucleus command (server, SQL shell, tools) and ncsql, statically
 linked.
CONTROL
      dpkg-deb --root-owner-group -Zgzip --build "$pkg" "$out/$file" >/dev/null
      rm -rf "$stage"; echo "$out/$file"; continue ;;
    rpm)
      file="nucleus-$version-1.$arch.rpm"
      mkdir -p "$stage/files"
      cp "$bin" "$tdir/$target/release/$shell" "$stage/files/"
      cp "$root/LICENSE" "$root/COPYRIGHT" "$root/THIRD_PARTY.md" "$stage/files/"
      cat > "$stage/nucleus.spec" <<SPEC
Name: nucleus
Version: $version
Release: 1
Summary: $summary
License: AGPL-3.0-only
URL: https://github.com/davidsarosi92/nucleus
%define debug_package %{nil}
%define __strip /bin/true
%global _build_id_links none
%description
The nucleus command (server, SQL shell, tools) and ncsql, statically linked.
%install
mkdir -p %{buildroot}/usr/bin %{buildroot}/usr/share/licenses/nucleus
install -m 755 $stage/files/nucleus $stage/files/ncsql %{buildroot}/usr/bin/
install -m 644 $stage/files/LICENSE $stage/files/COPYRIGHT $stage/files/THIRD_PARTY.md %{buildroot}/usr/share/licenses/nucleus/
%files
/usr/bin/nucleus
/usr/bin/ncsql
%license /usr/share/licenses/nucleus/LICENSE
%license /usr/share/licenses/nucleus/COPYRIGHT
%license /usr/share/licenses/nucleus/THIRD_PARTY.md
SPEC
      rpmbuild -bb --quiet --target "$arch" --define "_topdir $stage/rpm" "$stage/nucleus.spec" >/dev/null
      mv "$stage/rpm/RPMS/$arch/$file" "$out/"
      rm -rf "$stage"; echo "$out/$file"; continue ;;
  esac
  mkdir -p "$stage/$name/bin"
  cp "$bin" "$tdir/$target/release/$shell" "$stage/$name/bin/"
  cp "$root/LICENSE" "$root/COPYRIGHT" "$root/THIRD_PARTY.md" "$root/AUTHORS" "$root/README.md" "$stage/$name/"
  case $kind in
    linux)      file="$name-linux-$arch.tar.gz" ;;
    linux-musl) file="$name-linux-$arch-musl.tar.gz" ;;
    macos)      file="$name-macos-$arch.tar.gz" ;;
    d-os)
      # d-os names the 32-bit x86 architecture i386.
      dos_arch=$arch; [ "$arch" = i686 ] && dos_arch=i386
      file="$name-d-os-$dos_arch.tar.gz"
      cp "$root/integrations/d-os/nucleus.dos-app" "$stage/$name/.dos-app"
      cp "$tdir/$target/release/nucleus-settings" "$stage/$name/bin/" ;;
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
