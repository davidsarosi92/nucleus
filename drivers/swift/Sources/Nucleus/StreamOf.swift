// A stream's entries (nucleus PLAN.md §M16.5 S4, DOCS.md §8.43): appended,
// consumed by a group in a connection's transaction, waited for.

import CNucleus

extension Connection {
    /// A stream's entries: appended, consumed by a group in this
    /// connection's transaction, waited for.
    public func streamOf(_ name: String) -> StreamOf {
        StreamOf(self, name)
    }

    /// Waits until stream `stream` has an entry after `after`, or
    /// `timeoutMs` has passed — one message, the server waiting without
    /// holding a transaction (`nc_wait_stream`).  The stream's newest seq.
    public func waitStream(_ stream: String, after: Int64, timeoutMs: UInt32) throws -> Int64 {
        let c = try handle()
        var err = nc_error()
        let n = nc_wait_stream(c, stream, after, timeoutMs, &err)
        if n < 0 { throw NucleusError(err) }
        return n
    }
}

/// A stream's entries (`Connection.streamOf`): `append` a row, `consume` a
/// group's next entries in the connection's transaction, `position`,
/// `wait`, and `process` — a group's next batch handled exactly once.
public final class StreamOf {
    let db: Connection
    public let name: String

    init(_ db: Connection, _ name: String) {
        self.db = db
        self.name = name
    }

    private static func ident(_ s: String) -> String {
        "\"" + s.replacingOccurrences(of: "\"", with: "\"\"") + "\""
    }

    /// Appends an entry: its columns' values; its seq and time are the
    /// commit's.  In the connection's transaction.
    public func append(_ values: KeyValuePairs<String, Value>) throws {
        let names = values.map { Self.ident($0.key) }.joined(separator: ", ")
        let marks = (0..<values.count).map { "$\($0 + 1)" }.joined(separator: ", ")
        try db.query("INSERT INTO \(Self.ident(name)) (\(names)) VALUES (\(marks))", values.map(\.value))
    }

    /// Group `group`'s next entries, at most `limit` (seq first); its
    /// position moves past them in the connection's transaction —
    /// committed with the work done on them, or not at all.
    public func consume(_ group: String, limit: Int64 = 100) throws -> [Row] {
        try db.query("CONSUME FROM \(Self.ident(name)) GROUP \(Self.ident(group)) LIMIT $1", [.int(limit)]).rows
    }

    /// Group `group`'s position: the last seq it consumed.
    public func position(_ group: String) throws -> Int64 {
        guard case .int(let n) = try db.query("SELECT stream_position($1, $2)", [.text(name), .text(group)]).rows[0][0]
        else { throw NucleusError(message: "stream_position gave no number") }
        return n
    }

    /// `Connection.waitStream` on this stream.
    public func wait(after: Int64, timeoutMs: UInt32) throws -> Int64 {
        try db.waitStream(name, after: after, timeoutMs: timeoutMs)
    }

    /// One batch for `group`, exactly once: in a transaction, its next
    /// entries (at most `limit`, waiting up to `timeoutMs` for one when
    /// there is none) given to `work`, which does its work on this
    /// connection; then the commit, the group's position with it.  A
    /// conflict (another consumer of the group took them) runs it again;
    /// an error of `work` rolls all back and is rethrown.  How many entries
    /// were processed (0: none came in time).  Not inside a transaction of
    /// the caller's.
    @discardableResult
    public func process(_ group: String, limit: Int64 = 100, timeoutMs: UInt32 = 0, _ work: ([Row]) throws -> Void)
        throws -> Int
    {
        var waited = false
        while true {
            try db.query("BEGIN")
            do {
                let entries = try consume(group, limit: limit)
                if entries.isEmpty {
                    let at = try position(group)
                    try db.query("ROLLBACK")
                    if waited || timeoutMs == 0 { return 0 }
                    _ = try wait(after: at, timeoutMs: timeoutMs)
                    waited = true
                    continue
                }
                try work(entries)
                try db.query("COMMIT")
                return entries.count
            } catch let e as NucleusError where e.code == "40001" {
                if db.status != .idle { _ = try? db.query("ROLLBACK") }
            } catch {
                if db.status != .idle { _ = try? db.query("ROLLBACK") }
                throw error
            }
        }
    }
}
