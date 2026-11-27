// The Swift driver against a running nucleus server (NUCLEUS_HOST,
// NUCLEUS_PORT; scripts/driver-tests.sh starts one).  Without
// NUCLEUS_PORT the tests are skipped.

import Foundation
import Nucleus
import XCTest

final class NucleusTests: XCTestCase {
    func connect() throws -> Connection {
        let env = ProcessInfo.processInfo.environment
        guard let port = env["NUCLEUS_PORT"].flatMap(UInt16.init) else {
            throw XCTSkip("no server: NUCLEUS_PORT is not set")
        }
        return try Connection.connect(host: env["NUCLEUS_HOST"] ?? "127.0.0.1", port: port)
    }

    func testTypesBothWays() throws { try Conformance.typesBothWays(connect()) }
    func testCoreTypes() throws { try Conformance.coreTypes(connect()) }
    func testUuid() throws { try Conformance.uuid(connect()) }
    func testVectors() throws { try Conformance.vectors(connect()) }
    func testJson() throws { try Conformance.json(connect()) }
    func testErrors() throws { try Conformance.errors(connect()) }
    func testTransactions() throws { try Conformance.transactions(connect()) }
    func testDocuments() throws { try Conformance.documents(connect()) }
    func testStreams() throws { try Conformance.streams(connect()) }
    func testCursors() throws { try Conformance.cursors(connect()) }
    func testCopyAndPipelines() throws { try Conformance.copyAndPipelines(connect()) }

    func testEncryptedColumns() throws { try Conformance.encryptedColumns(connect()) }

    /// The format's vectors, shared by every driver (NUCLEUS_VECTORS).
    func testEncryptedColumnVectors() throws {
        guard let path = ProcessInfo.processInfo.environment["NUCLEUS_VECTORS"] else {
            throw XCTSkip("NUCLEUS_VECTORS is not set")
        }
        func unhex(_ s: Substring) -> [UInt8] {
            s == "-" ? [] : stride(from: 0, to: s.count, by: 2).map {
                UInt8(s.dropFirst($0).prefix(2), radix: 16)!
            }
        }
        func hex(_ d: Data) -> String { d.map { String(format: "%02x", $0) }.joined() }
        var master: [UInt8] = []
        var cases = 0
        for line in try String(contentsOfFile: path, encoding: .utf8).split(separator: "\n") where !line.hasPrefix("#") {
            let f = line.split(separator: " ", omittingEmptySubsequences: false)
            if f[0] == "master" {
                master = unhex(f[1])
                continue
            }
            let value: ColumnKey.Plain = f[1] == "bytes" ? .bytes(Data(unhex(f[2])))
                : f[1] == "text" ? .text(String(decoding: unhex(f[2]), as: UTF8.self)) : .int(Int64(f[2])!)
            let key = ColumnKey(master: master, name: String(f[0]))
            XCTAssertEqual(hex(try key.encrypt(value, nonce: unhex(f[3]))), String(f[4]))
            XCTAssertEqual(hex(key.blindIndex(value)), String(f[5]))
            XCTAssertEqual(try key.decrypt(Data(unhex(f[4]))), value)
            cases += 1
        }
        XCTAssertEqual(cases, 11)
    }

    /// A subject's key its user holds (the k message, protocol 8).
    func testSubjectKey() throws {
        let admin = try connect(), other = try connect()
        let t = table(), who = t + "-who"
        let secret = (0..<32).map { UInt8($0) }
        try admin.query("CREATE TABLE \(t) (id INT PRIMARY KEY, owner TEXT, body TEXT) WITH (storage = sealed)")
        try admin.query("ALTER TABLE \(t) SET SUBJECT (owner)")
        try admin.query("INSERT INTO \(t) VALUES (1, $1, 'held')", [.text(who)])
        try admin.holdSubjectKey(who, secret: secret)
        assertFails("28000") { try other.query("SELECT body FROM \(t)") }
        assertFails("28000") { try other.presentSubjectKey(who, secret: [UInt8](repeating: 1, count: 32)) }
        try other.presentSubjectKey(who, secret: secret)
        XCTAssertEqual(try other.query("SELECT body FROM \(t)").rows[0][0], .text("held"))
    }

    func testPassword() throws {
        _ = try connect()
        let env = ProcessInfo.processInfo.environment
        XCTAssertThrowsError(try Connection.connect(host: env["NUCLEUS_HOST"] ?? "127.0.0.1",
                                                    port: UInt16(env["NUCLEUS_PORT"]!)!, password: "wrong")) {
            XCTAssertEqual(($0 as? NucleusError)?.code, "28P01")
        }
    }
}
