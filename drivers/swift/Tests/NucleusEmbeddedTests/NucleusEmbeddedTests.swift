// The same cases on a database inside the process (the embedded edition),
// and what is its own: a directory kept across opens.

import Foundation
import NucleusEmbedded
import XCTest

final class NucleusEmbeddedTests: XCTestCase {
    static var db: Database!

    override class func setUp() {
        db = try! Database.open(":memory:")
    }

    override class func tearDown() {
        try! db.close()
    }

    func connect() throws -> Connection { try Self.db.connect() }

    func testTypesBothWays() throws { try Conformance.typesBothWays(connect()) }
    func testCoreTypes() throws { try Conformance.coreTypes(connect()) }
    func testUuid() throws { try Conformance.uuid(connect()) }
    func testVectors() throws { try Conformance.vectors(connect()) }
    func testErrors() throws { try Conformance.errors(connect()) }
    func testTransactions() throws { try Conformance.transactions(connect()) }
    func testCursors() throws { try Conformance.cursors(connect()) }
    func testCopyAndPipelines() throws { try Conformance.copyAndPipelines(connect()) }

    /// A directory keeps what was committed: closed, opened again.
    func testDirectoryKeepsData() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("nucleus-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        do {
            let db = try Database.open(dir.path, options: "cache=16MB")
            let c = try db.connect()
            try c.query("CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)")
            try c.executeMany("INSERT INTO notes VALUES ($1, $2)", (1...1000).map { [.int32(Int32($0)), .text("note \($0)")] })
            c.close()
            try db.close()
        }
        let db = try Database.open(dir.path)
        let c = try db.connect()
        XCTAssertEqual(try c.query("SELECT count(*), max(body) FROM notes").rows[0].values, [.int(1000), "note 999"])
        // Two sessions, one database: the second sees the first's commit.
        let other = try db.connect(user: "nucleus")
        try c.query("INSERT INTO notes VALUES (1001, 'from c')")
        XCTAssertEqual(try other.query("SELECT body FROM notes WHERE id = 1001").rows[0][0], "from c")
        other.close()
        c.close()
        try db.close()
    }

    /// The app's own directory (Application Support), as an app opens it.
    func testOpenNamed() throws {
        let name = "nucleus-test-\(UUID().uuidString)"
        let db = try Database.open(named: name)
        defer {
            try? db.close()
            try? FileManager.default.removeItem(atPath: db.path)
        }
        XCTAssertTrue(db.path.hasSuffix(name))
        XCTAssertEqual(try db.connect().query("SELECT 1 + 1").rows[0][0].intValue, 2)
    }
}
