#!/usr/bin/env bash
# The monitoring shipped with nucleus, tested against the real tools
# (PLAN.md §M14.18 O2):
#
#   monitoring/test.sh <nucleus binary>
#
# - promtool checks the alert rules and runs their tests: every rule
#   fires on series made to provoke it, none on healthy ones;
# - every nucleus_* metric the rules and the dashboard read is one a
#   running server serves (a renamed metric breaks this, not an alert
#   that silently never fires);
# - a Prometheus scrapes `nucleus server --metrics` with the rules loaded,
#   and its numbers move with a workload;
# - an OpenTelemetry Collector receives the same metrics over OTLP, and
#   the statements' spans.
#
# Needs promtool, prometheus and otelcol on the PATH (or PROMTOOL,
# PROMETHEUS, OTELCOL); CI downloads them (.github/workflows/ci.yml).
set -euo pipefail
nucleus=$(cd "$(dirname "${1:?usage: monitoring/test.sh <nucleus binary>}")" && pwd)/$(basename "$1")
here=$(cd "$(dirname "$0")" && pwd)
promtool=${PROMTOOL:-promtool}
prometheus=${PROMETHEUS:-prometheus}
otelcol=${OTELCOL:-otelcol}
work=$(mktemp -d)
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
# A failure, with the logs' ends; on GitHub Actions as annotations too
# (a job's log is not readable without the repository's admin rights).
fail() {
  echo "monitoring test: $*" >&2
  for f in "$work"/*.log; do echo "== $f"; tail -20 "$f"; done >&2
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::error title=monitoring test::$*"
    for f in "$work"/*.log; do
      [ -s "$f" ] || continue
      echo "::error title=$(basename "$f")::$(tail -15 "$f" | cut -c1-300 | sed 's/%/%25/g' | awk '{printf "%s%%0A", $0}')"
    done
  fi
  exit 1
}
wait_for() { # <what> <command…>: tries for 60 s
  local what=$1; shift
  for _ in $(seq 120); do "$@" >/dev/null 2>&1 && return 0; sleep 0.5; done
  fail "$what"
}

echo "== promtool: the rules and their tests"
"$promtool" check rules "$here/prometheus/nucleus.rules.yml" >"$work/promtool.log" 2>&1 ||
  fail "promtool check rules failed"
(cd "$here/prometheus" && "$promtool" test rules nucleus.rules.test.yml) >>"$work/promtool.log" 2>&1 ||
  fail "promtool test rules failed"
cat "$work/promtool.log"

echo "== a server, an OpenTelemetry Collector, a Prometheus"
metrics=127.0.0.1:29137
cat > "$work/otelcol.yaml" <<YAML
receivers:
  otlp:
    protocols:
      http:
        endpoint: 127.0.0.1:24318
exporters:
  debug:
    verbosity: detailed
service:
  pipelines:
    metrics:
      receivers: [otlp]
      exporters: [debug]
    traces:
      receivers: [otlp]
      exporters: [debug]
YAML
"$otelcol" --config "$work/otelcol.yaml" >"$work/otelcol.log" 2>&1 &
pids+=($!)
wait_for "the collector did not start" curl -s -o /dev/null http://127.0.0.1:24318/
OTEL_METRIC_EXPORT_INTERVAL=1000 "$nucleus" server "$work/db" --listen 127.0.0.1:0 \
  --metrics "$metrics" --otlp http://127.0.0.1:24318 2>"$work/nucleus.log" &
pids+=($!)
wait_for "the server did not start" grep -q "listening on" "$work/nucleus.log"
addr=$(sed -n 's/.*listening on \(127\.0\.0\.1:[0-9]*\) (nucleus protocol).*/\1/p' "$work/nucleus.log" | head -1)
cat > "$work/prometheus.yml" <<YAML
global:
  scrape_interval: 1s
  evaluation_interval: 1s
rule_files:
  - $here/prometheus/nucleus.rules.yml
scrape_configs:
  - job_name: nucleus
    static_configs:
      - targets: ["$metrics"]
YAML
"$prometheus" --config.file="$work/prometheus.yml" --storage.tsdb.path="$work/tsdb" \
  --web.listen-address=127.0.0.1:29090 >"$work/prometheus.log" 2>&1 &
pids+=($!)
wait_for "Prometheus did not start" curl -sf http://127.0.0.1:29090/-/ready

echo "== the metrics the rules and the dashboard read are served"
curl -sf "http://$metrics/metrics" >"$work/metrics.txt" || fail "no metrics at $metrics"
if python3 - "$work/metrics.txt" "$here/prometheus/nucleus.rules.yml" "$here/grafana/nucleus.json" \
  >"$work/coverage.log" 2>&1 <<'PY'
import re, sys
served = set()
for line in open(sys.argv[1]):
    if line.startswith("# TYPE "):
        name, kind = line.split()[2:4]
        served.add(name)
        if kind == "histogram":
            served |= {name + "_bucket", name + "_sum", name + "_count"}
used = set()
for f in sys.argv[2:]:
    used |= set(re.findall(r"\bnucleus_[a-z_]+", open(f).read()))
missing = sorted(used - served)
if missing:
    sys.exit(f"read but not served: {missing}")
print(f"{len(used)} metrics read, all served")
PY
then cat "$work/coverage.log"; else fail "metrics read but not served"; fi

echo "== a workload; Prometheus sees the numbers move"
query() { # <promql>: the first value, or nothing
  curl -sf --get http://127.0.0.1:29090/api/v1/query --data-urlencode "query=$1" |
    python3 -c 'import json,sys; r=json.load(sys.stdin)["data"]["result"]; print(r[0]["value"][1] if r else "")'
}
wait_for "Prometheus did not scrape nucleus" test "$(query 'up{job="nucleus"}')" = 1
before=$(query 'nucleus_statements_total{job="nucleus",kind="write"}')
"$nucleus" sql -h "$addr" -c "CREATE TABLE m (id INT PRIMARY KEY); INSERT INTO m SELECT * FROM generate_series(1, 1000); UPDATE m SET id = id + 1000" >"$work/sql.log" 2>&1 ||
  fail "the workload failed"
moved() { python3 -c "import sys; sys.exit(0 if float('$(query 'nucleus_statements_total{job="nucleus",kind="write"}')' or 0) >= float('${before:-0}') + 3 else 1)"; }
wait_for "the statements did not move in Prometheus" moved
test "$(query 'nucleus_ready{job="nucleus"}')" = 1 || fail "not ready in Prometheus"
rules=$(curl -sf http://127.0.0.1:29090/api/v1/rules | python3 -c 'import json,sys; print(sum(len(g["rules"]) for g in json.load(sys.stdin)["data"]["groups"]))')
want=$(grep -c "^      - alert:" "$here/prometheus/nucleus.rules.yml")
test "$rules" = "$want" || fail "Prometheus loaded $rules rules, not $want"
firing=$(curl -sf http://127.0.0.1:29090/api/v1/alerts | python3 -c 'import json,sys; print([a["labels"]["alertname"] for a in json.load(sys.stdin)["data"]["alerts"] if a["state"]=="firing"])')
test "$firing" = "[]" || fail "alerts firing on a healthy server: $firing"

echo "== the collector received them over OTLP"
wait_for "the collector got no nucleus_commits_total" grep -q "nucleus_commits_total" "$work/otelcol.log"
wait_for "the collector got no statement histogram" grep -q "nucleus_statement_duration_seconds" "$work/otelcol.log"
grep -q "service.name: Str(nucleus)" "$work/otelcol.log" || fail "no service.name in what the collector got"
wait_for "the collector got no statement spans" grep -q "db.query.text" "$work/otelcol.log"
echo "the monitoring test passed: $want rules, Prometheus, OTLP metrics and spans"
