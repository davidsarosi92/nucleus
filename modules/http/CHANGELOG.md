# Changelog

## Unreleased

- A URL with spaces or control characters is refused (they would go
  into the request line); a chunk size near `usize::MAX` is refused, not
  wrapped.

- First version (nucleus PLAN.md §M14.22 E2b): `send` — `GET` or `POST`
  with headers and a body, over TLS (rustls, Mozilla's roots), plain HTTP
  only to a loopback address; the status, headers and body (length or
  chunked), a size limit and a timeout; `Error::transient` and
  `Response::retry_after` for callers that retry.  Taken from the oidc
  module, which now uses it.
