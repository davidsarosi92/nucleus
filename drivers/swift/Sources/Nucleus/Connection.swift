// The Swift driver for nucleus: a layer over the C driver (drivers/c).
//
//   let db = try Connection.connect()           // 127.0.0.1:20035, nucleus / nucleus
//   try db.query("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)")
//   try db.query("INSERT INTO t VALUES ($1, $2)", [1, "one"])
//   for row in try db.query("SELECT id, name FROM t").rows {
//       print(row["id"]!, row["name"]!)
//   }
//   let rows = try db.cursor("SELECT id FROM t")  // a large result, as it arrives
//   while let row = try rows.next() { print(row[0]) }

import CNucleus
import Foundation

/// This driver's version.
public let version = "0.1.0"

/// An error from the server (`code` is the SQLSTATE, `hint` what to do
/// about it — empty: none) or the driver (`code` empty).
public struct NucleusError: Error, CustomStringConvertible, Sendable {
    public let code: String
    public let message: String
    public let hint: String

    public init(code: String = "", message: String, hint: String = "") {
        self.code = code
        self.message = message
        self.hint = hint
    }

    /// A C driver error (`nc_error`).
    public init(_ e: nc_error) {
        var e = e
        func text<T>(_ t: inout T) -> String {
            withUnsafeBytes(of: &t) { String(decoding: $0.prefix { $0 != 0 }, as: UTF8.self) }
        }
        code = text(&e.code)
        message = text(&e.message)
        hint = text(&e.hint)
    }

    public var description: String {
        let base = code.isEmpty ? message : "\(message) (\(code))"
        return hint.isEmpty ? base : "\(base)\nhint: \(hint)"
    }
}

/// A result's column: its name and type ("integer", "text", an enum's name, …).
public struct Column: Equatable, Sendable {
    public let name: String
    public let type: String
}

/// A row: its values by position or by column name.
public struct Row: Sendable {
    public let values: [Value]
    let names: [String]

    public subscript(_ i: Int) -> Value { values[i] }
    /// The value of the column named `name`, or nil.
    public subscript(_ name: String) -> Value? {
        names.firstIndex(of: name).map { values[$0] }
    }
}

/// A statement's result: its columns and rows, its command ("SELECT",
/// "INSERT", …) and the rows it returned or affected.
public struct QueryResult: Sendable {
    public let columns: [Column]
    public let rows: [Row]
    public let command: String
    public let count: UInt64
}

private let typeNames: [nc_type: String] = [
    NC_NULL: "unknown", NC_BOOL: "boolean", NC_INT: "bigint", NC_FLOAT: "double precision",
    NC_TEXT: "text", NC_NUMERIC: "numeric", NC_DATE: "date", NC_TIMESTAMP: "timestamp",
    NC_TIMESTAMPTZ: "timestamptz", NC_INTERVAL: "interval", NC_ARRAY: "array",
    NC_SMALLINT: "smallint", NC_INTEGER: "integer", NC_REAL: "real", NC_BYTEA: "bytea",
    NC_TIME: "time", NC_CHAR: "character", NC_INET: "inet", NC_CIDR: "cidr",
]

extension nc_type: @retroactive Hashable {}

func columns(_ r: OpaquePointer) -> [Column] {
    (0..<nc_columns(r)).map { i in
        let t = nc_column_type(r, i)
        let named = nc_column_type_name(r, i).map { String(cString: $0) }
        return Column(name: String(cString: nc_column_name(r, i)), type: named ?? typeNames[t] ?? "unknown")
    }
}

func result(_ r: OpaquePointer) -> QueryResult {
    let cols = columns(r)
    let names = cols.map(\.name)
    let rows = (0..<nc_rows(r)).map { row in
        Row(values: (0..<cols.count).map { value(nc_value_at(r, row, $0)) }, names: names)
    }
    return QueryResult(columns: cols, rows: rows, command: nc_command(r).map { String(cString: $0) } ?? "",
                       count: nc_count(r))
}

/// A statement of a pipeline.
public struct Statement {
    public let sql: String
    public let params: [Value]

    public init(_ sql: String, _ params: [Value] = []) {
        self.sql = sql
        self.params = params
    }
}

/// A connection — to a server, or to a database in the app
/// (`NucleusEmbedded`).  One thread at a time.
public final class Connection {
    private var c: OpaquePointer?

    /// The transaction status after the last statement.
    public enum Status: Sendable { case idle, inTransaction, failed }

    /// Takes a C driver connection (`nc_conn *`) as its own.
    public init(adopting handle: OpaquePointer) {
        c = handle
    }

    deinit { close() }

    /// Connects and logs in.  TLS is not in this package's build of the C
    /// driver: a server over TLS is reached with another driver for now.
    public static func connect(host: String = "127.0.0.1", port: UInt16 = 20035, user: String = "nucleus",
                               password: String = "nucleus", database: String = "nucleus",
                               clientName: String = "nucleus-swift", token: String? = nil) throws -> Connection
    {
        let arena = Arena()
        var cfg = nc_default_config()
        cfg.host = arena.cString(host)
        cfg.port = port
        cfg.user = arena.cString(user)
        cfg.password = arena.cString(password)
        cfg.database = arena.cString(database)
        cfg.client_name = arena.cString(clientName)
        cfg.token = token.map { arena.cString($0) }
        var err = nc_error()
        guard let c = nc_connect(&cfg, &err) else { throw NucleusError(err) }
        return Connection(adopting: c)
    }

    private func handle() throws -> OpaquePointer {
        guard let c else { throw NucleusError(message: "the connection is closed") }
        return c
    }

    /// Runs `sql` — one statement with parameters (`$1`, `$2`, …), or
    /// several without — and returns the last statement's result.
    @discardableResult
    public func query(_ sql: String, _ params: [Value] = []) throws -> QueryResult {
        let c = try handle()
        let arena = Arena()
        let p = try arena.values(params)
        var err = nc_error()
        guard let r = nc_query(c, sql, p, params.count, &err) else { throw NucleusError(err) }
        defer { nc_result_free(r) }
        return result(r)
    }

    /// Runs a statement: the rows it affected.
    @discardableResult
    public func execute(_ sql: String, _ params: [Value] = []) throws -> UInt64 {
        try query(sql, params).count
    }

    /// Runs `sql` once for each row of parameters, pipelined, all or none
    /// (one commit for all): the rows affected, all told.
    @discardableResult
    public func executeMany(_ sql: String, _ rows: [[Value]]) throws -> Int64 {
        let c = try handle()
        let width = rows.first?.count ?? 0
        guard rows.allSatisfy({ $0.count == width }) else {
            throw NucleusError(message: "every row needs the same number of parameters")
        }
        let arena = Arena()
        let p = try arena.values(rows.flatMap { $0 })
        var err = nc_error()
        let n = nc_execute_many(c, sql, p, rows.count, width, &err)
        if n < 0 { throw NucleusError(err) }
        return n
    }

    /// Runs statements sent together (a round trip for all): each one's
    /// result or error, as if each had been sent alone.
    public func pipeline(_ statements: [Statement]) throws -> [Swift.Result<QueryResult, NucleusError>] {
        let c = try handle()
        let arena = Arena()
        let stmts = try statements.map { s in
            nc_statement(sql: arena.cString(s.sql), params: try arena.values(s.params), nparams: s.params.count)
        }
        var results = [OpaquePointer?](repeating: nil, count: stmts.count)
        var errors = [nc_error](repeating: nc_error(), count: stmts.count)
        var err = nc_error()
        if nc_pipeline(c, stmts, stmts.count, &results, &errors, &err) != 0 { throw NucleusError(err) }
        return zip(results, errors).map { r, e in
            guard let r else { return .failure(NucleusError(e)) }
            defer { nc_result_free(r) }
            return .success(result(r))
        }
    }

    /// Runs a statement and reads its rows as they arrive, `fetch` at a
    /// time (0: the driver's default): a result of any size takes a batch
    /// of memory.  Another statement on the connection ends the cursor.
    public func cursor(_ sql: String, _ params: [Value] = [], fetch: UInt32 = 0) throws -> Cursor {
        let c = try handle()
        let arena = Arena()
        let p = try arena.values(params)
        var err = nc_error()
        guard let cur = nc_cursor_open(c, sql, p, params.count, fetch, &err) else { throw NucleusError(err) }
        return Cursor(cur, owner: self)
    }

    /// Loads `data` with `COPY … FROM STDIN` (text or CSV, as the statement
    /// says): the rows stored.  A bad row stores nothing.
    @discardableResult
    public func copy(_ sql: String, data: Data) throws -> Int64 {
        let c = try handle()
        var err = nc_error()
        let n = data.withUnsafeBytes { nc_copy(c, sql, $0.baseAddress, $0.count, &err) }
        if n < 0 { throw NucleusError(err) }
        return n
    }

    /// `copy` of text.
    @discardableResult
    public func copy(_ sql: String, text: String) throws -> Int64 {
        try copy(sql, data: Data(text.utf8))
    }

    /// Runs `body` in a transaction: committed if it returns, rolled back
    /// if it throws.
    public func transaction<T>(_ body: () throws -> T) throws -> T {
        try query("BEGIN")
        do {
            let v = try body()
            try query("COMMIT")
            return v
        } catch {
            if status != .idle { _ = try? query("ROLLBACK") }
            throw error
        }
    }

    public var status: Status {
        switch c.map(nc_status) ?? 0 {
        case 1: .inTransaction
        case 2: .failed
        default: .idle
        }
    }

    /// The server's version.
    public var serverVersion: String { c.flatMap { nc_server_version($0) }.map { String(cString: $0) } ?? "" }

    /// Says Bye and frees the connection (also when it is released).
    public func close() {
        if let c { nc_close(c) }
        c = nil
    }
}

/// A statement's rows as they arrive.
public final class Cursor {
    private var cur: OpaquePointer?
    // A cursor holds its connection: the C cursor points into it.
    private let owner: Connection

    init(_ cur: OpaquePointer, owner: Connection) {
        self.cur = cur
        self.owner = owner
    }

    deinit { close() }

    /// The columns.
    public var columns: [Column] { cur.map { Nucleus.columns(nc_cursor_result($0)) } ?? [] }

    /// The next row, or nil at the end; the statement's error after its rows.
    public func next() throws -> Row? {
        guard let cur else { return nil }
        var err = nc_error()
        switch nc_cursor_next(cur, &err) {
        case 1:
            let r = nc_cursor_result(cur)!
            let n = nc_columns(r)
            let names = (0..<n).map { String(cString: nc_column_name(r, $0)) }
            return Row(values: (0..<n).map { value(nc_value_at(r, 0, $0)) }, names: names)
        case 0: return nil
        default: throw NucleusError(err)
        }
    }

    /// Every row left.
    public func all() throws -> [Row] {
        var rows: [Row] = []
        while let r = try next() { rows.append(r) }
        return rows
    }

    /// Stops the statement (the server sends no more) and frees the cursor.
    public func close() {
        if let cur { nc_cursor_close(cur) }
        cur = nil
    }
}
