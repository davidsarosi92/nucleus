# libnucleus — the C driver

C99, no dependencies (POSIX sockets; Winsock on Windows — not yet tested in
CI).  `nucleus.h` documents the API: `nc_connect`, `nc_query` (the last
statement's result, or NULL with an `nc_error` holding the SQLSTATE, the message and the
hint),
`nc_value_at`, `nc_format`, `nc_close`.  Values are `nc_value` (numeric as a
128-bit mantissa and scale, dates as days and timestamps as µs since
2000-01-01, `uuid` as an `nc_uuid` of 16 bytes — `nc_uuid_value`,
`nc_uuid_parse` — and another registered type as `NC_EXT`, its name and
binary form; `nc_column_type_name` names a registered column type or an
enum).  Protocol version 3: `NC_SMALLINT` and `NC_INTEGER` (`as.integer`),
`NC_REAL` (`as.real4`), `NC_BYTEA` (a pointer and length), `NC_TIME` (µs
since midnight), `NC_INET` / `NC_CIDR` (family, mask length, address;
`nc_inet_parse`); `NC_CHAR` and `NC_ENUM` columns hold text values.
Protocol version 4: `nc_connect` proves the password by SCRAM-SHA-256
(with the driver's own SHA-256, HMAC, PBKDF2 and base64) and never sends
it; a server that cannot sign the exchange is refused (`08006`).  TLS
in a build with OpenSSL (`make TLS=openssl`, which defines
`NC_WITH_OPENSSL`; link with `pkg-config --libs openssl`): `cfg.tls = 1`
checks the server against the system's CAs, `cfg.tls_ca` against a PEM
file, `cfg.tls_no_verify = 1` not at all; `cfg.tls_cert` / `tls_key`
show a client certificate; `nc_tls_supported()` says whether the build
can.  `cfg.token` logs in with an OpenID Connect identity token instead of
the password.

```sh
make                # build/libnucleus.a
make test           # build/test_nucleus (needs a running server)
```

Or compile `nucleus.c` into your program.  Example: `examples/c/quickstart.c`.
