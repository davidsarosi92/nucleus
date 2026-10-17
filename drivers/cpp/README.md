# nucleus.hpp — the C++ driver

Header-only C++17 over the C driver: `nucleus::Connection` (RAII; throws
`nucleus::Error` with `code()`), `query(sql, {params…})` returning a
`Result` of `nucleus::Value`s (`std::variant` of NULL, bool, int64, double,
text, `Numeric`, `Date`, `Timestamp`, `TimestampTz`, `Interval`, `Array`,
`Uuid` — 16 bytes, `Uuid::parse` — and `Ext`, another registered type's name
and binary form),
`transaction([](Connection&) { … })`.

```sh
cc -std=c99 -c ../c/nucleus.c -o nucleus.o
c++ -std=c++17 -I../c -I. your.cpp nucleus.o
```

Example: `examples/cpp/quickstart.cpp`.
