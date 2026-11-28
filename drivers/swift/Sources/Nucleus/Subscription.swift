// A live query (nucleus PLAN.md §M16.6, DOCS.md §8.47): its result kept on
// this side as the commits change it.

extension Connection {
    /// Subscribes to a query (`SUBSCRIBE`): its result now, kept by
    /// `Subscription.changes` as commits change it.  Its parameters are
    /// bound now.  Not inside a transaction.
    public func subscribe(_ sql: String, _ params: [Value] = []) throws -> Subscription {
        try Subscription(self, sql, params)
    }
}

/// One row's change in a live query's batch: `diff` copies of `row` came
/// (+n) or went (-n); an update is a -1 and a +1.
public struct Change: Equatable, Sendable {
    public let diff: Int64
    public let row: [Value]
}

/// A live query (`Connection.subscribe`): `rows` its result as of the last
/// batch, kept on this side — the first in the query's order, rows that
/// come after appended, rows gone taken out; `changes(timeoutMs:)` the next
/// batch, applied; `close()`.
public final class Subscription {
    let db: Connection
    /// Its name in the session (`CHANGES` and `UNSUBSCRIBE` take it).
    public let name: String
    /// The query's columns.
    public let columns: [Column]
    /// The result as of the last batch.
    public private(set) var rows: [[Value]] = []
    /// The commit the last batch was read at (`nc_ts`).
    public private(set) var at: Int64 = 0

    init(_ db: Connection, _ sql: String, _ params: [Value]) throws {
        self.db = db
        self.name = "nc_live_" + String(UInt64.random(in: 1...UInt64.max), radix: 36)
        let r = try db.query("SUBSCRIBE \(name) AS \(sql)", params)
        self.columns = Array(r.columns.dropFirst(2))
        _ = try apply(r.rows.map(\.values))
    }

    /// The next batch (`CHANGES … WAIT`), applied to `rows`: waiting up to
    /// `timeoutMs` for a commit that changes the result; empty when none
    /// came in time.  An error ends the subscription on the server (its
    /// table dropped, a privilege taken back).
    @discardableResult
    public func changes(timeoutMs: UInt32 = 0) throws -> [Change] {
        let r = try db.query("CHANGES \(name) WAIT $1", [.int(Int64(timeoutMs))])
        return try apply(r.rows.map(\.values))
    }

    /// Ends it (`UNSUBSCRIBE`).
    public func close() throws {
        try db.query("UNSUBSCRIBE \(name)")
    }

    private func apply(_ batch: [[Value]]) throws -> [Change] {
        var out: [Change] = []
        for r in batch {
            guard r.count >= 2, case .int(let ts) = r[0], case .int(let diff) = r[1] else {
                throw NucleusError(message: "a live query's row without nc_ts, nc_diff")
            }
            at = ts
            let row = Array(r.dropFirst(2))
            if diff > 0 {
                for _ in 0..<diff { rows.append(row) }
            } else {
                for _ in 0..<(-diff) {
                    guard let i = rows.firstIndex(of: row) else {
                        throw NucleusError(message: "a live query's batch took out a row it did not hold")
                    }
                    rows.remove(at: i)
                }
            }
            out.append(Change(diff: diff, row: row))
        }
        return out
    }
}
