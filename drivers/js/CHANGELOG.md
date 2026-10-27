# Changelog

## Unreleased

- Protocol version 6 (nucleus PLAN.md §M14.11): results stream.
  `Client.stream(sql, params, { fetch })` returns a `Stream` to read with
  `for await`, a batch at a time; leaving the loop, `close()` or the next
  query stops the statement.
- Protocol version 5 (nucleus PLAN.md §M14.10): errors carry a hint,
  `NucleusError.hint` (`null` when there is none).
- A directory's (LDAP) password through PLAIN, over TLS only.
- An identity token instead of the password: `connect({ token })`.
- Over TLS, SCRAM bound to the session (SCRAM-SHA-256-PLUS) when the
  server offers it.
- Client certificates (mutual TLS): `connect({ tlsCert, tlsKey })`.
- TLS (nucleus PLAN.md §M14.25): `connect({ tls, tlsCa, tlsVerify })`, over
  `node:tls`.
- Protocol version 4 (nucleus PLAN.md §M14.25): the password is proved by
  SCRAM-SHA-256 (`node:crypto`), never sent; the server's signature is
  checked.
- Protocol version 3 (nucleus PLAN.md §M14.26): `smallint`, `integer` and
  `real` (its shortest digits) as `number`, `bytea` as `Buffer`, `time` as
  `Time`, `inet` and `cidr` as `Inet` and `Cidr`, `char(n)` and enums as
  `string` with their own column type names.

## 0.1.0 — 2026-10-18

- Protocol version 2: `uuid` as the language's native uuid (a string in
  JavaScript), other registered types as their name and binary form.
- First version (nucleus PLAN.md §M14.8): every nucleus type both ways,
  parameters, errors with their SQLSTATE, transactions, passwords.
