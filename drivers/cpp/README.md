# nucleus.hpp — the C++ driver

Header-only C++17 over the C driver: `nucleus::Connection` (RAII; throws
`nucleus::Error` with `code()` and `hint()`), `query(sql, {params…})` returning a
`Result` of `nucleus::Value`s (`std::variant` of NULL, bool, int64, double,
text, `Numeric`, `Date`, `Timestamp`, `TimestampTz`, `Interval`, `Array`,
`Uuid` — 16 bytes, `Uuid::parse` — and `Ext`, another registered type's name
and binary form; protocol version 3's `SmallInt`, `Integer`, `Real`,
`Bytes`, `Time`, `Inet` and `Cidr` — `Inet::parse`, `Cidr::parse` — and
`Vector`, a `std::vector<float>`, `SparseVector` — dimensions, 0-based
positions and values — and `Json`, its text),
`transaction([](Connection&) { … })`.  Protocol version 4 (through the C
driver): the password is proved by SCRAM-SHA-256, never sent.  TLS with
a C driver built with OpenSSL: `Config::tls`, `tls_ca`, `tls_no_verify`,
`tls_cert`, `tls_key`; `Config::token` for an identity token.  Large
results stream (protocol version 6, through the C driver):
`for (const auto& row : db.cursor(sql, {params…}, fetch))` reads them as
they arrive, a batch at a time; a `Cursor` destroyed or `close()`d early
stops the statement.  Columnar results: `auto b = db.batches(sql);
while (b.next()) b.values<int32_t>(0)…` (pointers into the batch;
`text(col, row)`, `is_null(col, row)`).  Bulk load: `db.copy("COPY t FROM
STDIN", data)` with a string or an `std::istream`.  Many statements in
one round trip: `db.pipeline({{sql, params}, …})` (each one's `Result`
or `Error`, in a `std::variant`); one statement for many rows, all or
none: `db.execute_many(sql, rows)`.

```sh
cc -std=c99 -c ../c/nucleus.c -o nucleus.o
c++ -std=c++17 -I../c -I. your.cpp nucleus.o
```

Example: `examples/cpp/quickstart.cpp`.

## Encrypted columns

A column's values encrypted here and decrypted here: the server stores
`bytea` ciphertext and never holds the key; a blind index beside it finds
equal values (nucleus PLAN.md §M14.30 K5, DOCS.md §11 — what it leaves
visible is said there).  `master` is 32 bytes the application keeps.

```cpp
nucleus::ColumnKey key(master, "users.email");
db.query("INSERT INTO users VALUES ($1, $2, $3)",
         {int64_t(1), key.encrypt(std::string("ann@example.com")), key.blind_index(std::string("ann@example.com"))});
auto r = db.query("SELECT email FROM users WHERE email_index = $1", {key.blind_index(std::string("ann@example.com"))});
std::get<std::string>(key.decrypt(r.rows[0][0].as<nucleus::Bytes>()));   // "ann@example.com"
```

A sealed table's rows whose key their user holds: `db.present_subject_key(subject, secret)`.

## Documents

A collection's documents by MongoDB's requests (`CREATE COLLECTION`;
nucleus DOCS.md §8.41, §10 *Documents*), in the connection's transaction:

```cpp
auto people = db.collection("people");
people.insert_one(R"({"name": "ann", "age": 34})");
nucleus::FindOptions o;
o.sort = R"({"age": -1})";
for (auto& doc : people.find(R"({"age": {"$gte": 30}})", o)) std::cout << doc.text << "\n";
people.update_one(R"({"name": "ann"})", R"({"$inc": {"age": 1}})", true);
```

## Licence

MIT — see [LICENSE](LICENSE).  The database itself is AGPL-3.0-only; a
program that reaches a nucleus server through this driver is not bound by
it (the repository's [COPYRIGHT](../../COPYRIGHT)).
