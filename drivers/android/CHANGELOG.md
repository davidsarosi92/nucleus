# Changelog

## Unreleased

- The size budget is 9 MiB on arm64-v8a (was 8 MiB): the engine's JSON
  (nucleus PLAN.md §M14.15 J1–J3) took it to 8.17 MiB.
- The first version (nucleus PLAN.md §M14.23 step 3): `Database.open(path,
  options)`, `open(context, name)`, `connect(user, database)` (the Java
  driver's `NucleusClient` over a session in the process), `close()`,
  `engineVersion`; the engine and its JNI glue in
  `libnucleus_android.so` for arm64-v8a and x86_64.
