# Changelog

## Unreleased

- The shared Java conformance cases include live queries (17 cases).
- The size budget is 10 MiB on arm64-v8a (was 9 MiB, at first 8 MiB):
  encryption at rest, JSON Schema, backups, the native protocol's server
  and faster scans and joins (nucleus PLAN.md §M14.15–§M14.33) took it
  to 9.02 MiB; the engine's JSON (§M14.15 J1–J3) had taken it to 8.17.
- The first version (nucleus PLAN.md §M14.23 step 3): `Database.open(path,
  options)`, `open(context, name)`, `connect(user, database)` (the Java
  driver's `NucleusClient` over a session in the process), `close()`,
  `engineVersion`; the engine and its JNI glue in
  `libnucleus_android.so` for arm64-v8a and x86_64.
