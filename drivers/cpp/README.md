# nucleus.hpp — the C++ driver

Header-only C++17 over the C driver: `nucleus::Connection` (RAII; throws
`nucleus::Error` with `code()`), `query(sql, {params…})` returning a
`Result` of `nucleus::Value`s (`std::variant` of NULL, bool, int64, double,
text, `Numeric`, `Date`, `Timestamp`, `TimestampTz`, `Interval`, `Array`,
`Uuid` — 16 bytes, `Uuid::parse` — and `Ext`, another registered type's name
and binary form; protocol version 3's `SmallInt`, `Integer`, `Real`,
`Bytes`, `Time`, `Inet` and `Cidr` — `Inet::parse`, `Cidr::parse`),
`transaction([](Connection&) { … })`.  Protocol version 4 (through the C
driver): the password is proved by SCRAM-SHA-256, never sent.  TLS with
a C driver built with OpenSSL: `Config::tls`, `tls_ca`, `tls_no_verify`,
`tls_cert`, `tls_key`; `Config::token` for an identity token.

```sh
cc -std=c99 -c ../c/nucleus.c -o nucleus.o
c++ -std=c++17 -I../c -I. your.cpp nucleus.o
```

Example: `examples/cpp/quickstart.cpp`.
