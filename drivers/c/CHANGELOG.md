# Changelog

## Unreleased

- Protocol version 5 (nucleus PLAN.md §M14.10): errors carry a hint,
  `nc_error.hint` (empty when there is none); `nc_error` grew by it.
- A directory's (LDAP) password through PLAIN, over TLS only.
- An identity token instead of the password: `cfg.token` (OAUTHBEARER).
- Over TLS, SCRAM bound to the session (SCRAM-SHA-256-PLUS,
  `tls-server-end-point` by OpenSSL) when the server offers it.
- Client certificates (mutual TLS): `cfg.tls_cert`, `cfg.tls_key`.
- TLS (nucleus PLAN.md §M14.25) in a build with `NC_WITH_OPENSSL` (`make
  TLS=openssl`): `cfg.tls`, `cfg.tls_ca`, `cfg.tls_no_verify`,
  `nc_tls_supported()`; the certificate checked against the system's CAs
  or a CA file, and the host name (or IP address).
- Protocol version 4 (nucleus PLAN.md §M14.25): the password is proved by
  SCRAM-SHA-256 (SHA-256, HMAC, PBKDF2 and base64 in the driver, the nonce
  from `/dev/urandom` or `rand_s`), never sent; the server's signature is
  checked.
- Protocol version 3 (nucleus PLAN.md §M14.26): `NC_SMALLINT`,
  `NC_INTEGER`, `NC_REAL`, `NC_BYTEA`, `NC_TIME`, `NC_INET`, `NC_CIDR`
  values (with constructors and `nc_inet_parse`), `NC_CHAR` and `NC_ENUM`
  columns.

## 0.1.0 — 2026-10-18

- Protocol version 2: `uuid` as the language's native uuid (a string in
  JavaScript), other registered types as their name and binary form.
- First version (nucleus PLAN.md §M14.8): every nucleus type both ways,
  parameters, errors with their SQLSTATE, transactions, passwords.
