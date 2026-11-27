// The drivers' conformance cases (drivers/README.md), on a connection to a
// server (NucleusTests) or to a database in the process
// (NucleusEmbeddedTests: this file, linked there).

import Foundation
import XCTest

import Nucleus

func table() -> String { "swift_" + UUID().uuidString.prefix(8).lowercased() }

/// Asserts that `body` throws a NucleusError with SQLSTATE `code`.
func assertFails(_ code: String, file: StaticString = #filePath, line: UInt = #line, _ body: () throws -> Void) {
    do {
        try body()
        XCTFail("no error, \(code) expected", file: file, line: line)
    } catch let e as NucleusError {
        XCTAssertEqual(e.code, code, e.description, file: file, line: line)
    } catch {
        XCTFail("\(error)", file: file, line: line)
    }
}

enum Conformance {
    static func typesBothWays(_ db: Connection) throws {
        let t = table()
        try db.query("""
            CREATE TABLE \(t) (id INT PRIMARY KEY, b BOOLEAN, f FLOAT, s TEXT, n NUMERIC(10, 2),
              d DATE, ts TIMESTAMP, tz TIMESTAMPTZ, iv INTERVAL, tags TEXT[], big BIGINT)
            """)
        let when = Date(timeIntervalSince1970: 1_710_072_000.25)
        let row: [Value] = [
            .int32(1), true, 0.5, "héllo", .numeric("123.45"), .date(year: 2024, month: 2, day: 29),
            .timestamp(when), .timestamptz(when), .interval(months: 1, days: 2, micros: 3_000_000), ["a", nil, "c"],
            .int(1 << 62),
        ]
        let r = try db.query("INSERT INTO \(t) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)", row)
        XCTAssertEqual(r.command, "INSERT")
        XCTAssertEqual(r.count, 1)
        let got = try db.query("SELECT * FROM \(t)")
        XCTAssertEqual(got.columns.map(\.type), [
            "integer", "boolean", "double precision", "text", "numeric", "date", "timestamp", "timestamptz",
            "interval", "array", "bigint",
        ])
        XCTAssertEqual(got.rows[0].values, row)
        XCTAssertEqual(got.rows[0]["s"], "héllo")
        XCTAssertEqual(got.rows[0][4].description, "123.45")
        // Exact decimals, NULL, and text parameters parsed where needed.
        let sums = try db.query("SELECT 1.50 + 1, NULL, $1 + 1", ["41"])
        XCTAssertEqual(sums.rows[0][0].description, "2.50")
        XCTAssertEqual(sums.rows[0][1], .null)
        XCTAssertEqual(sums.rows[0][2].intValue, 42)
    }

    static func coreTypes(_ db: Connection) throws {
        let t = table()
        let mood = "\(t)_mood"
        try db.query("CREATE TYPE \(mood) AS ENUM ('sad', 'happy')")
        try db.query("""
            CREATE TABLE \(t) (s SMALLINT, i INTEGER, r REAL, b BYTEA, tm TIME, ch CHAR(3),
              ip INET, net CIDR, m \(mood))
            """)
        let row: [Value] = [
            .int16(-2), .int32(70000), .float(0.1), .bytes(Data([0, 255])), .time(45_296_500_000), "ab ",
            .inet("10.1.2.3/8"), .cidr("2001:db8::/32"), "happy",
        ]
        try db.query("INSERT INTO \(t) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)", row)
        let got = try db.query("SELECT * FROM \(t)")
        XCTAssertEqual(got.columns.map(\.type), [
            "smallint", "integer", "real", "bytea", "time", "character", "inet", "cidr", mood,
        ])
        XCTAssertEqual(got.rows[0].values, row)
        XCTAssertEqual(got.rows[0][4].description, "12:34:56.5")
        let n = try db.query("SELECT count(*) FROM \(t) WHERE b = $1 AND ip <<= $2",
                             [.bytes(Data([0, 255])), .cidr("10.0.0.0/8")])
        XCTAssertEqual(n.rows[0][0].intValue, 1)
    }

    static func uuid(_ db: Connection) throws {
        let t = table()
        let u = UUID(uuidString: "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")!
        try db.query("CREATE TABLE \(t) (id UUID PRIMARY KEY, n INT)")
        try db.query("INSERT INTO \(t) VALUES ($1, 1)", [.uuid(u)])
        let r = try db.query("SELECT id, gen_random_uuid() FROM \(t)")
        XCTAssertEqual(r.columns[0].type, "uuid")
        XCTAssertEqual(r.rows[0][0], .uuid(u))
        guard case .uuid = r.rows[0][1] else { return XCTFail("not a uuid: \(r.rows[0][1])") }
        XCTAssertEqual(try db.query("SELECT n FROM \(t) WHERE id = $1", [.uuid(u)]).rows[0][0], .int32(1))
    }

    static func vectors(_ db: Connection) throws {
        let t = table()
        try db.query("CREATE TABLE \(t) (id INT PRIMARY KEY, v vector(3), s sparsevec(100000))")
        let rows: [[Value]] = [
            [1, .vector([1, 2, 3]), .sparsevec(dimensions: 100_000, indices: [0, 99999], values: [1, 2])],
            [2, .vector([4, 6, 3]), .sparsevec(dimensions: 100_000, indices: [5], values: [-3.5])],
            [3, .vector([1, 2, 3.5]), nil],
            [4, nil, nil],
        ]
        XCTAssertEqual(try db.executeMany("INSERT INTO \(t) VALUES ($1, $2, $3)", rows), 4)
        let r = try db.query("SELECT id, v FROM \(t) WHERE v IS NOT NULL ORDER BY v <-> $1 LIMIT 2",
                             [.vector([1, 2, 3.4])])
        XCTAssertEqual(r.columns[1].type, "vector")
        XCTAssertEqual(r.rows.map { $0[0] }, [.int32(3), .int32(1)])
        XCTAssertEqual(r.rows[1][1], .vector([1, 2, 3]))
        let s = try db.query("SELECT s FROM \(t) WHERE id = 1")
        XCTAssertEqual(s.columns[0].type, "sparsevec")
        XCTAssertEqual(s.rows[0][0], .sparsevec(dimensions: 100_000, indices: [0, 99999], values: [1, 2]))
        XCTAssertEqual(s.rows[0][0].description, "{1:1,100000:2}/100000")
        assertFails("22000") { try db.query("INSERT INTO \(t) VALUES (7, $1)", [.vector([1, 2])]) }
    }

    static func json(_ db: Connection) throws {
        let t = table()
        try db.query("CREATE TABLE \(t) (id INT PRIMARY KEY, doc JSON)")
        try db.query("INSERT INTO \(t) VALUES (1, $1)", [.json("{\"b\": [1, 2.50], \"a\": null}")])
        let r = try db.query("SELECT doc FROM \(t)")
        XCTAssertEqual(r.columns[0].type, "json")
        XCTAssertEqual(r.rows[0][0], .json("{\"a\": null, \"b\": [1, 2.5]}"))
        assertFails("22P02") { try db.query("SELECT '{bad'::json") }
    }

    static func errors(_ db: Connection) throws {
        assertFails("42P01") { try db.query("SELECT * FROM nosuch") }
        assertFails("22012") { try db.query("SELECT 1 / 0") }
        let t = table()
        try db.query("CREATE TABLE \(t) (id INT PRIMARY KEY)")
        try db.query("INSERT INTO \(t) VALUES (1)")
        assertFails("23505") { try db.query("INSERT INTO \(t) VALUES (1)") }
        do {
            try db.query("SELECT ib FROM \(t)")
            XCTFail("no error")
        } catch let e as NucleusError {
            XCTAssertEqual(e.hint, "Perhaps you meant column \"id\".")
        }
        // The connection goes on after errors.
        XCTAssertEqual(try db.query("SELECT 2").rows[0][0].intValue, 2)
    }

    static func transactions(_ db: Connection) throws {
        let t = table()
        try db.query("CREATE TABLE \(t) (id INT PRIMARY KEY)")
        try db.transaction {
            try db.query("INSERT INTO \(t) VALUES (1)")
            XCTAssertEqual(db.status, .inTransaction)
        }
        struct RollBack: Error {}
        XCTAssertThrowsError(try db.transaction {
            try db.query("INSERT INTO \(t) VALUES (2)")
            throw RollBack()
        })
        XCTAssertEqual(db.status, .idle)
        XCTAssertEqual(try db.query("SELECT id FROM \(t)").rows.map { $0[0] }, [.int32(1)])
    }

    /// A collection's documents by MongoDB's requests (the O message, protocol 9).
    static func documents(_ db: Connection) throws {
        let t = table()
        try db.query("CREATE COLLECTION \(t)")
        try db.query("CREATE TABLE \(t)_audit (n INT)")
        let p = db.collection(t)
        XCTAssertEqual(try p.insertOne(#"{"_id": 1, "name": "ann", "age": 34}"#), .json("1"))
        XCTAssertEqual(
            try p.insertMany([#"{"_id": 2, "name": "bob", "age": 25}"#, #"{"_id": 3, "name": "cy", "age": 40}"#]),
            [.json("2"), .json("3")])
        XCTAssertEqual(
            try p.find(#"{"age": {"$gte": 25}}"#,
                       FindOptions(projection: #"{"name": 1, "_id": 0}"#, sort: #"{"age": -1}"#, skip: 1, limit: 1)),
            [#"{"name": "ann"}"#])
        XCTAssertEqual(try p.countDocuments(#"{"age": {"$gt": 30}}"#), 2)
        XCTAssertEqual(try p.updateMany("{}", #"{"$max": {"age": 30}}"#),
                       UpdateResult(matched: 3, modified: 1, upsertedId: .null))
        let u = try p.updateOne(#"{"name": "dan"}"#, #"{"$set": {"age": 5}}"#, upsert: true)
        XCTAssertEqual(u.matched + u.modified, 0)
        XCTAssertNotEqual(u.upsertedId, .null)
        XCTAssertEqual(try p.replaceOne(#"{"_id": 3}"#, #"{"name": "cyrus"}"#),
                       UpdateResult(matched: 1, modified: 1, upsertedId: .null))
        XCTAssertEqual(try p.findOne(#"{"_id": 3}"#), #"{"_id": 3, "name": "cyrus"}"#)
        // An update needs operators; a replacement has none.
        assertFails("22023") { try p.updateOne("{}", #"{"name": "x"}"#) }
        assertFails("22023") { try p.replaceOne("{}", #"{"$set": {"a": 1}}"#) }
        XCTAssertEqual(try p.deleteOne(#"{"age": {"$gte": 30}}"#), 1)
        XCTAssertEqual(try p.deleteMany("{}"), 3)
        // In a transaction with a table: rolled back, then committed.
        for commit in [false, true] {
            try db.query("BEGIN")
            try db.query("INSERT INTO \(t)_audit VALUES (1)")
            try p.insertOne(#"{"_id": 9}"#)
            try db.query(commit ? "COMMIT" : "ROLLBACK")
            XCTAssertEqual(try p.countDocuments(), commit ? 1 : 0)
            XCTAssertEqual(try db.query("SELECT count(*) FROM \(t)_audit").rows[0][0].intValue, commit ? 1 : 0)
        }
        assertFails("42P01") { _ = try db.collection("swift_nosuch").countDocuments() }
    }

    static func cursors(_ db: Connection) throws {
        // Ten billion rows: only what is read is produced.
        let big = try db.cursor("SELECT a, b FROM generate_series(1, 100000) a, generate_series(1, 100000) b",
                                fetch: 500)
        XCTAssertEqual(big.columns.map(\.name), ["a", "b"])
        var n = 0
        while let row = try big.next() {
            n += 1
            if n == 1200 {
                XCTAssertEqual(row[1].intValue, 1200)
                break
            }
        }
        big.close()
        XCTAssertEqual(try db.query("SELECT 1").rows[0][0].intValue, 1)
        let all = try db.cursor("SELECT * FROM generate_series(1, 2500)", fetch: 1000).all()
        XCTAssertEqual(all.count, 2500)
        XCTAssertEqual(all.last?[0].intValue, 2500)
        // An error after rows: the statement's.
        let failing = try db.cursor("SELECT 10 / (3 - x) FROM generate_series(1, 5) x", fetch: 1)
        XCTAssertThrowsError(try failing.all()) { XCTAssertEqual(($0 as? NucleusError)?.code, "22012") }
    }

    static func copyAndPipelines(_ db: Connection) throws {
        let t = table()
        try db.query("CREATE TABLE \(t) (id INT PRIMARY KEY, name TEXT)")
        XCTAssertEqual(try db.copy("COPY \(t) FROM STDIN WITH (FORMAT csv)", text: "1,one\n2,\"t,wo\"\n"), 2)
        assertFails("22P02") { try db.copy("COPY \(t) FROM STDIN WITH (FORMAT csv)", text: "3,three\nx,four\n") }
        XCTAssertEqual(try db.query("SELECT name FROM \(t) ORDER BY id").rows.map { $0[0] }, ["one", "t,wo"])
        let results = try db.pipeline([
            Statement("INSERT INTO \(t) VALUES ($1, $2)", [3, "three"]),
            Statement("SELECT 1 / 0"),
            Statement("SELECT count(*) FROM \(t)"),
        ])
        XCTAssertEqual(try results[0].get().count, 1)
        XCTAssertThrowsError(try results[1].get()) { XCTAssertEqual(($0 as? NucleusError)?.code, "22012") }
        XCTAssertEqual(try results[2].get().rows[0][0].intValue, 3)
        // All or none: a duplicate key stores none of them.
        assertFails("23505") { try db.executeMany("INSERT INTO \(t) VALUES ($1, $2)", [[4, "four"], [1, "again"]]) }
        XCTAssertEqual(try db.query("SELECT count(*) FROM \(t)").rows[0][0].intValue, 3)
    }

    /// A column the driver encrypts (nucleus PLAN.md §M14.30 K5).
    static func encryptedColumns(_ db: Connection) throws {
        let master = [UInt8](repeating: 0x5a, count: 32)
        let email = ColumnKey(master: master, name: "users.email")
        let balance = ColumnKey(master: master, name: "users.balance")
        assertFails("22023") { _ = try ColumnKey(master: master, name: "users.phone").decrypt(try email.encrypt(.text("x"))) }
        let t = table()
        try db.query("CREATE TABLE \(t) (id BIGINT PRIMARY KEY, email BYTEA, email_index BYTEA, balance BYTEA)")
        for (i, who, money) in [(1, "ann@example.com", 100), (2, "bob@example.com", -5), (3, "ann@example.com", 7)] {
            try db.query("INSERT INTO \(t) VALUES ($1, $2, $3, $4)",
                         [.int(Int64(i)), .bytes(try email.encrypt(.text(who))), .bytes(email.blindIndex(.text(who))),
                          .bytes(try balance.encrypt(.int(Int64(money))))])
        }
        let rows = try db.query("SELECT id, email, balance FROM \(t) WHERE email_index = $1 ORDER BY id",
                                [.bytes(email.blindIndex(.text("ann@example.com")))]).rows
        XCTAssertEqual(rows.count, 2)
        guard rows.count == 2, case .bytes(let e) = rows[1][1], case .bytes(let b) = rows[1][2] else { return }
        XCTAssertEqual(try email.decrypt(e), .text("ann@example.com"))
        XCTAssertEqual(try balance.decrypt(b), .int(7))
    }

    static func all(_ connect: () throws -> Connection) throws {
        for test in [typesBothWays, coreTypes, uuid, vectors, json, errors, transactions, cursors, copyAndPipelines, encryptedColumns] {
            let db = try connect()
            try test(db)
            db.close()
        }
    }
}
