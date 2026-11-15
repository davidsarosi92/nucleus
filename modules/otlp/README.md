# nucleus-otlp

OpenTelemetry for [nucleus](https://github.com/davidsarosi92/nucleus)
(PLAN.md §M14.18): the process's metrics and statements' spans pushed
to a collector over OTLP/HTTP, JSON encoded.

```sh
nucleus server ./mydb --otlp http://127.0.0.1:4318
# or, as the OpenTelemetry SDKs are configured:
OTEL_EXPORTER_OTLP_ENDPOINT=https://otlp.example.com \
OTEL_EXPORTER_OTLP_HEADERS='api-key=…' \
OTEL_METRIC_EXPORT_INTERVAL=10000 nucleus server ./mydb
```

- **The data model**: cumulative temporality from the process's start; a
  counter is a monotonic `sum` (`asInt`), a gauge a `gauge`
  (`asDouble`), a histogram a `histogram` with explicit bounds (each
  bucket's own count).  Labels are attributes; the resource is
  `service.name = nucleus` with `service.version`.
- **An exporter** (`nucleus_platform::metrics::Exporter`, a push one): a
  thread that sends a snapshot every interval (60 s by default) to
  `<endpoint>/v1/metrics`; a failure is logged once, and again when
  sending works again.  Over TLS to anything but a loopback address
  (`nucleus-http`).
- **Traces**: a span per statement to `<endpoint>/v1/traces`, batched
  every 5 s (2048 waiting at most), the database semantic conventions'
  attributes, the text without constants.  A client's own trace by `SET
  traceparent = '…'` or a sqlcommenter comment; otherwise sampled by
  `OTEL_TRACES_SAMPLER` (`parentbased_traceidratio` and
  `OTEL_TRACES_SAMPLER_ARG`), none with `OTEL_TRACES_EXPORTER=none`.

Licence: AGPL-3.0-only (see `LICENSE` and the repository's `COPYRIGHT`).
