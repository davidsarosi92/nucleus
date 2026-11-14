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
    func testCursors() throws { try Conformance.cursors(connect()) }
    func testCopyAndPipelines() throws { try Conformance.copyAndPipelines(connect()) }

    func testPassword() throws {
        _ = try connect()
        let env = ProcessInfo.processInfo.environment
        XCTAssertThrowsError(try Connection.connect(host: env["NUCLEUS_HOST"] ?? "127.0.0.1",
                                                    port: UInt16(env["NUCLEUS_PORT"]!)!, password: "wrong")) {
            XCTAssertEqual(($0 as? NucleusError)?.code, "28P01")
        }
    }
}
