# Changelog

## Unreleased

- Traces: a span per statement to `/v1/traces`, in the client's trace by
  `traceparent` (a setting or a sqlcommenter comment), sampled as the
  SDKs' variables say.
- The OTLP exporter: the metrics pushed to an OpenTelemetry collector over
  OTLP/HTTP (JSON), configured by `--otlp` or the SDKs' environment
  variables (PLAN.md §M14.18).
