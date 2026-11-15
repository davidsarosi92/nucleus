#!/usr/bin/env bash
# The tools CI downloads, for running its scripts locally (macOS or Linux,
# arm64 or x86-64): Prometheus and promtool, the OpenTelemetry Collector,
# rclone — the versions .github/workflows/ci.yml pins, checked against
# their releases' SHA-256 sums where published, into target/ci-tools.
#
#   scripts/ci-tools.sh
#   T=target/ci-tools
#   PROMTOOL=$T/promtool PROMETHEUS=$T/prometheus OTELCOL=$T/otelcol \
#     monitoring/test.sh target/release/nucleus
#   RCLONE=$T/rclone scripts/s3-test.sh target/release/nucleus
#
# (psql for scripts/psql-smoke.sh: the system's package — brew install
# libpq on macOS, postgresql-client on Debian.)
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
ci=$root/.github/workflows/ci.yml
pinned() { sed -n "s/^ *$1: *//p" "$ci" | head -1; }
prom=$(pinned PROMETHEUS_VERSION)
otel=$(pinned OTELCOL_VERSION)
rcl=$(pinned RCLONE_RELEASE)
case "$(uname -s)" in Darwin) os=darwin ;; Linux) os=linux ;; *) echo "unsupported OS" >&2; exit 1 ;; esac
case "$(uname -m)" in arm64 | aarch64) arch=arm64 ;; x86_64) arch=amd64 ;; *) echo "unsupported CPU" >&2; exit 1 ;; esac
sha() { if command -v sha256sum >/dev/null; then sha256sum -c -; else shasum -a 256 -c -; fi; }
out=$root/target/ci-tools
mkdir -p "$out"
dl=$(mktemp -d)
trap 'rm -rf "$dl"' EXIT
cd "$dl"

p=prometheus-$prom.$os-$arch
curl -fsSLO "https://github.com/prometheus/prometheus/releases/download/v$prom/$p.tar.gz"
curl -fsSL "https://github.com/prometheus/prometheus/releases/download/v$prom/sha256sums.txt" | grep " $p.tar.gz\$" | sha
tar -xzf "$p.tar.gz" && cp "$p/prometheus" "$p/promtool" "$out"

o=otelcol_${otel}_${os}_$arch.tar.gz
curl -fsSLO "https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download/v$otel/$o"
tar -xzf "$o" otelcol && cp otelcol "$out"

[ "$os" = darwin ] && r=osx || r=linux
z=rclone-v$rcl-$r-$arch.zip
curl -fsSLO "https://github.com/rclone/rclone/releases/download/v$rcl/$z"
curl -fsSL "https://github.com/rclone/rclone/releases/download/v$rcl/SHA256SUMS" | grep "  $z\$" | sha
unzip -q -o "$z" && cp "rclone-v$rcl-$r-$arch/rclone" "$out"

"$out/promtool" --version | head -1
"$out/otelcol" --version
"$out/rclone" version | head -1
echo "in $out"
