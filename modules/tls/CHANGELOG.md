# Changelog

## Unreleased

- First version (nucleus PLAN.md §M14.25): `ServerTls` for the server's
  front ends (`ncp` and PostgreSQL's), `ClientTls` for the Rust client —
  verified against Mozilla's roots or a CA of one's own, or unverified;
  client certificates both ways (mutual TLS), the verified one's common
  name in `TlsInfo::client_name`; the server certificate's
  `tls-server-end-point` value in `TlsInfo::channel_binding` (the client
  side's in `nucleus_client::Secured`), for SCRAM-SHA-256-PLUS.
