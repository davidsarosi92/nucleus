// A task queue's tasks (nucleus PLAN.md §M16.5 S5, S7, DOCS.md §8.45):
// added, taken under a lease, finished with their work, failed.

extension Connection {
    /// A task queue's tasks: added, taken under a lease, finished with
    /// their work, failed.
    public func queueOf(_ name: String) -> QueueOf {
        QueueOf(self, name)
    }
}

/// A task queue's tasks (`Connection.queueOf`): `add` a task, `take` the
/// next ones under a lease, `finish` one in the connection's transaction
/// (its work's), `fail` one, and `process` — tasks taken and each done
/// exactly once.  A task is a row: `id`, `attempt`, then the queue's
/// columns.
public final class QueueOf {
    let db: Connection
    public let name: String

    init(_ db: Connection, _ name: String) {
        self.db = db
        self.name = name
    }

    private static func ident(_ s: String) -> String {
        "\"" + s.replacingOccurrences(of: "\"", with: "\"\"") + "\""
    }

    private static func lost(_ e: Error) -> Bool {
        guard let e = e as? NucleusError else { return false }
        return e.code == "40001" || e.code == "55000"
    }

    /// Adds a task: its columns' values — the queue's own `priority` (lower
    /// first) and `available_at` (a delay) among them if given.  In the
    /// connection's transaction.  Its id.
    @discardableResult
    public func add(_ values: KeyValuePairs<String, Value> = [:]) throws -> Int64 {
        var sql = "INSERT INTO \(Self.ident(name))"
        if values.isEmpty {
            sql += " DEFAULT VALUES"
        } else {
            let names = values.map { Self.ident($0.key) }.joined(separator: ", ")
            let marks = (0..<values.count).map { "$\($0 + 1)" }.joined(separator: ", ")
            sql += " (\(names)) VALUES (\(marks))"
        }
        guard let id = try db.query(sql + " RETURNING id", values.map(\.value)).rows[0][0].intValue
        else { throw NucleusError(message: "an added task without its id") }
        return id
    }

    /// The next available tasks, at most `limit`, leased for `leaseMs`
    /// (nil: the queue's lease) — `TAKE`, a transaction of its own unless
    /// the connection is in one.
    public func take(limit: Int64 = 1, leaseMs: Int64? = nil) throws -> [Row] {
        var sql = "TAKE FROM \(Self.ident(name)) LIMIT \(max(limit, 1))"
        if let ms = leaseMs { sql += " LEASE interval '\(ms) milliseconds'" }
        return try db.query(sql).rows
    }

    /// Finishes `task` (deleted, in the connection's transaction — the one
    /// its work is in); `55000` if its attempt no longer holds it.
    public func finish(_ task: Row) throws {
        try db.query("FINISH TASK $1 OF \(Self.ident(name)) ATTEMPT $2", [task[0], task[1]])
    }

    /// Fails `task`: available again after `retryAfterMs` (nil: the
    /// queue's backoff), or — its last attempt — a dead letter.  `"retry"`
    /// or `"dead"`.
    @discardableResult
    public func fail(_ task: Row, error: String? = nil, retryAfterMs: Int64? = nil) throws -> String {
        var sql = "FAIL TASK $1 OF \(Self.ident(name)) ATTEMPT $2"
        var params = [task[0], task[1]]
        if let error {
            sql += " ERROR $3"
            params.append(.text(error))
        }
        if let ms = retryAfterMs { sql += " RETRY AFTER interval '\(ms) milliseconds'" }
        guard case .text(let outcome) = try db.query(sql, params).rows[0][0]
        else { throw NucleusError(message: "a failed task without its outcome") }
        return outcome
    }

    /// Takes up to `limit` tasks (leased for `leaseMs`, nil: the queue's)
    /// and does each exactly once: in a transaction, `work(task)` on this
    /// connection, then `finish`, then the commit — the work and the task's
    /// end together.  A task whose work throws is rolled back and failed
    /// with the error's description (retried after the backoff, or a dead
    /// letter); a conflict (`40001`) runs its work again, until its finish
    /// says the lease ran out and another took it (`55000`): then it is
    /// left to that one.  How many were finished.  Not inside a transaction
    /// of the caller's.
    @discardableResult
    public func process(limit: Int64 = 1, leaseMs: Int64? = nil, _ work: (Row) throws -> Void) throws -> Int {
        var finished = 0
        for task in try take(limit: limit, leaseMs: leaseMs) {
            for _ in 0..<10 {
                try db.query("BEGIN")
                do {
                    try work(task)
                    try finish(task)
                    try db.query("COMMIT")
                    finished += 1
                    break
                } catch {
                    if db.status != .idle { _ = try? db.query("ROLLBACK") }
                    let code = (error as? NucleusError)?.code
                    if code == "40001" { continue }
                    if code != "55000" {
                        let why = (error as? NucleusError)?.message ?? String(describing: error)
                        do {
                            try fail(task, error: why)
                        } catch let f where Self.lost(f) {}
                    }
                    break
                }
            }
        }
        return finished
    }
}
