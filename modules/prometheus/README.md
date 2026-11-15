# nucleus-prometheus

Prometheus for [nucleus](https://github.com/davidsarosi92/nucleus)
(PLAN.md §M14.18): the process's metrics served as `GET /metrics`, and
the health checks an orchestrator probes.

```sh
nucleus server ./mydb --metrics            # http://127.0.0.1:20037/metrics
```

- **`GET /metrics`**: Prometheus's text exposition format (0.0.4), or
  OpenMetrics 1.0 when the scraper's `Accept` asks for
  `application/openmetrics-text`.  The numbers are the registry's
  (`nucleus_platform::metrics`): what `nc_catalog.metrics` shows too.
- **`GET /health/live`**: 200 while the process answers.
- **`GET /health/ready`**: 200 once the server is ready — recovery done,
  the store open, the listeners up — and 503 before.  The listener
  starts before the database opens, so a long recovery is alive and not
  ready.
- **An exporter** (`nucleus_platform::metrics::Exporter`, a pull one): a
  listener of its own, a thread per connection (16 at most), one request
  a connection, HTTP/1.1 over the standard library.  Over TLS when the
  server has a certificate; the edition refuses to serve it beyond
  loopback over plain HTTP unless `--insecure`.

The metrics, their names and what each means are in DOCS.md §8.38.

Licence: AGPL-3.0-only (see `LICENSE` and the repository's `COPYRIGHT`).
