# The Swift driver

A Swift package (iOS 15+, macOS 12+, Linux) over the C driver, compiled
as it is.  Two products:

- **`Nucleus`** connects to a server.
- **`NucleusEmbedded`** opens a database inside the app — the engine as a
  static library in `NucleusEmbedded.xcframework` (nucleus PLAN.md
  §M14.23).  `build-xcframework.sh` makes it; without it the package has
  `Nucleus` alone.

```swift
import Nucleus

let db = try Connection.connect()          // 127.0.0.1:20035, nucleus / nucleus
try db.execute("CREATE TABLE t (id bigint PRIMARY KEY, price numeric(10,2))")
try db.execute("INSERT INTO t VALUES ($1, $2)", [1, .numeric(Numeric("9.99")!)])
for row in try db.query("SELECT * FROM t").rows { print(row["id"]!, row["price"]!) }
try db.transaction { try db.execute("UPDATE t SET price = price * 2") }
```

In an app, a database of its own (kept in Application Support):

```swift
import NucleusEmbedded

let database = try Database.open(named: "notes")   // or Database.open(path), Database.open(":memory:")
let db = try database.connect()                    // a Connection, as above
```

Values are `Value`, an enum with literals: `null`, `bool`, `int` (Int64),
`int32`, `int16`, `double`, `float`, `text`, `numeric` (`Numeric`, its
exact text), `bytes` (`Data`), `date(year:month:day:)`, `time` (µs since
midnight), `timestamp` and `timestamptz` (`Date`),
`interval(months:days:micros:)`, `uuid` (`UUID`), `inet`, `cidr`,
`array`, `vector` (`[Float]`), `sparsevec(dimensions:indices:values:)`,
`json` (its text, for `JSONDecoder`), `ext(type:data:)` (another registered type).  `query` returns a
`QueryResult` (`columns`, `rows`, `command`, `count`; a `Row` by
position or column name); also `execute`, `executeMany`, `pipeline`,
`cursor(sql, params, fetch:)` (`next()`, `all()`), `copy(sql, text:)` /
`copy(sql, data:)`, `status`, `serverVersion`, `close()`.  Errors are
`NucleusError` with `code` (SQLSTATE), `message` and `hint`.  No TLS in
this build.

## Building the engine for iOS

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
drivers/swift/build-xcframework.sh        # ARCHS="arm64" for Apple silicon only
```

The script builds the embedded edition (LTO, one codegen unit, without
the model modules) for iOS, the simulator and macOS, makes the
XCFramework and checks the size budget: what the engine adds to an iOS
app on arm64, linked and stripped, at most 11 MiB (now 10.04 MiB).

## Tests

`swift test --package-path drivers/swift` with a server (`NUCLEUS_PORT`,
`NUCLEUS_HOST`; `scripts/driver-tests.sh … swift` starts one): the
conformance cases every driver passes.  With the XCFramework, the same
cases on a database in the process too — in memory and in a directory —
also on the iOS simulator:

```sh
cd drivers/swift && xcodebuild test -scheme Nucleus-Package \
  -destination "platform=iOS Simulator,name=iPhone 16"
```

## Encrypted columns

A column's values encrypted here and decrypted here: the server stores
`bytea` ciphertext and never holds the key; a blind index beside it finds
equal values (nucleus PLAN.md §M14.30 K5, DOCS.md §11 — what it leaves
visible is said there).  `master` is 32 bytes the application keeps.

```swift
let key = ColumnKey(master: master, name: "users.email")
try db.query("INSERT INTO users VALUES ($1, $2, $3)",
             [1, .bytes(try key.encrypt(.text("ann@example.com"))), .bytes(key.blindIndex(.text("ann@example.com")))])
let row = try db.query("SELECT email FROM users WHERE email_index = $1",
                       [.bytes(key.blindIndex(.text("ann@example.com")))]).rows[0]
if case .bytes(let sealed) = row[0] { print(try key.decrypt(sealed)) }   // .text("ann@example.com")
```

A sealed table's rows whose key their user holds: `try db.presentSubjectKey(subject, secret: secret)`.

## Documents

A collection's documents by MongoDB's requests (`CREATE COLLECTION`;
nucleus DOCS.md §8.41, §10 *Documents*), in the connection's transaction:

```swift
let people = db.collection("people")
try people.insertOne(#"{"name": "ann", "age": 34}"#)
let found = try people.find(#"{"age": {"$gte": 30}}"#, FindOptions(sort: #"{"age": -1}"#, limit: 10))
try people.updateOne(#"{"name": "ann"}"#, #"{"$inc": {"age": 1}}"#, upsert: true)
```

## Streams

A stream's entries (`CREATE STREAM`; nucleus DOCS.md §8.43): appended in a
transaction, numbered by its commit; a consumer group's next batch
handled exactly once — the work's writes and the group's position
committed together, a conflict with another consumer run again; a wait
for the next entry that holds no transaction on the server:

```swift
let jobs = db.streamOf("jobs")
try jobs.append(["name": .text("resize"), "image": .int(42)])
try jobs.process("workers", limit: 100, timeoutMs: 30000) { entries in   // seq first
    for e in entries { try db.query("INSERT INTO done VALUES ($1)", [e[0]]) }
}                                                                          // entries processed
_ = try jobs.wait(after: 0, timeoutMs: 1000)                               // the newest seq
```

## Licence

The driver: MIT — see [LICENSE](LICENSE).  `NucleusEmbedded` (its XCFramework) is the database,
AGPL-3.0-only (the repository's [COPYRIGHT](../../COPYRIGHT)): a program that
opens a database in its own process with it is a work based on the
database; one that only reaches a nucleus server is not.
