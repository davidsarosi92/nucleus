# Changelog

## Unreleased

- Live queries (nucleus PLAN.md §M16.6 S3): `db.subscribe(sql, params)`
  → `Subscription` — `rows` (the result, kept on this side), `at`,
  `changes(timeoutMs:)` (the next batch as `Change(diff:row:)`, applied),
  `close()`.  Embedded too.
- Task queues (nucleus PLAN.md §M16.5 S7): `db.queueOf(name)` →
  `QueueOf` — `add(_:)` (its id), `take(limit:leaseMs:)` (rows: id,
  attempt, the columns), `finish(_:)`, `fail(_:error:retryAfterMs:)`
  ("retry" or "dead"), `process(limit:leaseMs:_:)`: each task done exactly
  once with its work, a failing one failed, a conflict run again, a lost
  lease left.
- Streams (nucleus PLAN.md §M16.5 S4, protocol version 10, through
  `nc_wait_stream`): `db.streamOf(name)` → `StreamOf` — `append([column:
  value])`, `consume(group, limit:)` (in the connection's transaction),
  `position(group)`, `wait(after:timeoutMs:)`, `process(group, limit:,
  timeoutMs:) { entries in … }`: a consumer group's next batch exactly
  once, the work's writes and the position committed together, a
  conflict run again, an error rolling back; `db.waitStream(_:after:
  timeoutMs:)`; embedded too.
- A collection's documents (nucleus PLAN.md §M16 Q4, protocol version
  9, through `nc_document`): `db.collection(name)` → `DocumentCollection`
  — `find(filter, FindOptions)`, `findOne`, `countDocuments`,
  `insertOne` / `insertMany`, `updateOne` / `updateMany` /
  `replaceOne(filter, doc, upsert:)` → `UpdateResult`, `deleteOne` /
  `deleteMany`; JSON texts in and out; embedded too.
- A server's Unix socket (nucleus PLAN.md §M14.33 R2), through the C
  driver: a path as the host reaches `nucleus server --socket`.
- Client-side encrypted columns (nucleus PLAN.md §M14.30 K5):
  `ColumnKey(master:name:)` — `encrypt`, `decrypt`, `blindIndex`;
  `Connection.presentSubjectKey` and `holdSubjectKey` (protocol version 8,
  the `k` message).
- The size budget is 10 MiB on arm64 (was 9 MiB): an app's engine,
  9,443,936 bytes, passed 9 MiB by 6.6 KB (nucleus PLAN.md §M14.23).
- The size budget is 9 MiB on arm64 (was 8 MiB): the engine's JSON
  (nucleus PLAN.md §M14.15 J1–J3) took an app's engine to 7.71 MiB, and
  Android's to 8.17 MiB.
- json (nucleus PLAN.md §M14.15): `Value.json`, a `json` value's text,
  both ways.
- The first version (nucleus PLAN.md §M14.23 step 3): `Nucleus` over the
  C driver — `Connection` (`query`, `execute`, `executeMany`, `pipeline`,
  `cursor`, `copy`, `transaction`), `Value` for every nucleus type — and
  `NucleusEmbedded`, a database in the app over
  `NucleusEmbedded.xcframework` (`Database.open`, `open(named:)`,
  `connect`, `close`).
