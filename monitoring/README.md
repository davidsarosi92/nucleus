# Monitoring nucleus

What PLAN.md §M14.18 ships for watching a server: alert rules, their
tests and a dashboard, over the metrics `nucleus server --metrics` serves
(DOCS.md §8.38; the runbook, an entry per alert, is DOCS.md §8.39).

| file | what |
|---|---|
| `prometheus/nucleus.rules.yml` | the alert rules: down, not ready, disk low or filling, WAL not checkpointed, a long transaction, conflicts, the cache's hit ratio, failed logins, a locked role, failing and slow statements, a backup or a restore test overdue or failing |
| `prometheus/nucleus.rules.test.yml` | `promtool test rules`: each alert provoked by series made for it, none on healthy ones |
| `grafana/nucleus.json` | a Grafana dashboard (import it; pick the Prometheus data source) |
| `test.sh` | all of it against the real tools: promtool, a Prometheus scraping a server, an OpenTelemetry Collector receiving OTLP |

```yaml
# prometheus.yml
rule_files:
  - nucleus.rules.yml
scrape_configs:
  - job_name: nucleus            # the rules expect this name
    static_configs:
      - targets: ["db.example.com:20037"]
```

The disk rules read node_exporter's file-system metrics (nucleus does
not measure the disk): run it beside the server and scope the rules to
the data directory's file system with a `mountpoint` matcher.

```sh
monitoring/test.sh target/debug/nucleus   # promtool, prometheus, otelcol on the PATH
```

Licence: MIT, as the examples — copy the rules and the dashboard into
your own configuration (`LICENSE`; the repository's `COPYRIGHT`).
