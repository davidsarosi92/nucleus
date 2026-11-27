# nucleus-mongo-compat

MongoDB compatibility for [nucleus](https://github.com/davidsarosi92/nucleus):
a module, so that `mongosh` and MongoDB's drivers can work with a nucleus
database's collections.  nucleus's own interface — SQL, the `ncp`
protocol and its drivers' `collection` API — comes first and is never
shaped by this module (nucleus PLAN.md, principle 11; §M16 Q5).

- **`Mongo`** — a front end for `nucleus-server` (`nucleus server
  --mongo-compat [addr]`, default `127.0.0.1:27017`): MongoDB's wire
  protocol as drivers speak it today — `OP_MSG`, and `OP_QUERY` for the
  first `hello`; TLS when the server has it (`tls=true`); a login by
  SCRAM-SHA-256 against the server's own verifiers (`saslStart` /
  `saslContinue`; the password proved, never sent).
- **Commands:** `find` / `getMore` / `killCursors` (filter, projection,
  sort, skip, limit, batches), `insert` (the collection made on its first
  write, an ObjectId given where a document has no `_id`, duplicate keys
  as `11000`), `update` (operators or a replacement, one or many,
  upserts), `delete`, `count`, `findAndModify`, `aggregate` of the
  pipelines drivers send for queries and counts (`$match`, `$sort`,
  `$skip`, `$limit`, `$project`, and a `$count` or counting `$group`),
  `create`, `drop`, `listCollections`, `createIndexes` (path indexes,
  unique too), `listDatabases`, and what a shell asks first (`hello`,
  `buildInfo`, `getParameter`, `getLog`, …).  Each request is compiled by
  nucleus's document compiler — the one `ncp`'s `O` message uses — to one
  SQL statement: one planner, one transaction, the path indexes serving
  the filter.
- **Types:** documents travel as BSON and are kept as nucleus's `json`,
  BSON's other types in Extended JSON (`{"$oid": …}`, `{"$date":
  "…Z"}`, `{"$numberDecimal": …}`, `{"$binary": …}`), so they come back
  as they went in; ranges on dates and ObjectIds compare their texts,
  which sort as the values do.

Not here: sessions and transactions over this protocol (nucleus's own
drivers have them), aggregation beyond the stages above (SQL answers
them), change streams, `$where`, text and geospatial queries.  JSON has
one number type: an `int32`, `int64` or `double` that is a whole number
comes back as an integer.

Licence: AGPL-3.0-only, as the database.
