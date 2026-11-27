// A collection's documents (nucleus PLAN.md §M16 Q4): MongoDB's requests on
// a connection, compiled by the server (DOCS.md §10, Documents).

import CNucleus

/// What an update or a replacement did.
public struct UpdateResult: Equatable, Sendable {
    public let matched: Int64
    /// The documents it changed.
    public let modified: Int64
    /// The `_id` of the document an upsert inserted, else `.null`.
    public let upsertedId: Value

    public init(matched: Int64, modified: Int64, upsertedId: Value) {
        self.matched = matched
        self.modified = modified
        self.upsertedId = upsertedId
    }
}

/// A find's options: JSON texts (empty: none), skip, limit (0: all).
public struct FindOptions: Sendable {
    public var projection: String
    /// Its fields in its text's order.
    public var sort: String
    public var skip: UInt64
    public var limit: UInt64

    public init(projection: String = "", sort: String = "", skip: UInt64 = 0, limit: UInt64 = 0) {
        self.projection = projection
        self.sort = sort
        self.skip = skip
        self.limit = limit
    }
}

/// A collection's documents (`Connection.collection(_:)`).  Filters,
/// documents, updates, projections and sorts are JSON texts (`""` or `"{}"`:
/// every document); documents come back as their JSON text.
public struct DocumentCollection {
    public let connection: Connection
    public let name: String

    init(_ connection: Connection, _ name: String) {
        self.connection = connection
        self.name = name
    }

    /// The documents `filter` matches.
    public func find(_ filter: String = "", _ options: FindOptions = FindOptions()) throws -> [String] {
        try connection.document(
            NC_DOC_FIND, name, filter: filter, body: options.projection, sort: options.sort, skip: options.skip,
            limit: options.limit
        ).rows.map { row in
            if case let .json(text) = row[0] { return text }
            return ""
        }
    }

    /// The first document `filter` matches, or nil.
    public func findOne(_ filter: String = "") throws -> String? {
        try find(filter, FindOptions(limit: 1)).first
    }

    /// How many documents `filter` matches.
    public func countDocuments(_ filter: String = "") throws -> Int64 {
        try connection.document(NC_DOC_COUNT, name, filter: filter).rows[0][0].intValue ?? 0
    }

    /// Inserts `document`; its `_id` (made where it has none).
    @discardableResult
    public func insertOne(_ document: String) throws -> Value {
        try connection.document(NC_DOC_INSERT, name, body: document).rows[0][0]
    }

    /// Inserts `documents`, in their order; their `_id`s.
    @discardableResult
    public func insertMany(_ documents: [String]) throws -> [Value] {
        try connection.document(NC_DOC_INSERT, name, body: "[" + documents.joined(separator: ", ") + "]")
            .rows.map { $0[0] }
    }

    private func updated(_ op: nc_document_op, _ filter: String, _ body: String, _ flags: Int32) throws
        -> UpdateResult
    {
        let row = try connection.document(op, name, filter: filter, body: body, flags: UInt32(flags)).rows[0]
        return UpdateResult(matched: row[0].intValue ?? 0, modified: row[1].intValue ?? 0, upsertedId: row[2])
    }

    /// Updates the first document `filter` matches by `update`'s operators
    /// (`{"$set": …}`); with `upsert`, inserts one where none matches.
    @discardableResult
    public func updateOne(_ filter: String, _ update: String, upsert: Bool = false) throws -> UpdateResult {
        try updated(NC_DOC_UPDATE, filter, update, upsert ? NC_DOC_UPSERT : 0)
    }

    /// Updates every document `filter` matches.
    @discardableResult
    public func updateMany(_ filter: String, _ update: String, upsert: Bool = false) throws -> UpdateResult {
        try updated(NC_DOC_UPDATE, filter, update, NC_DOC_MANY | (upsert ? NC_DOC_UPSERT : 0))
    }

    /// Replaces the first document `filter` matches (its `_id` kept).
    @discardableResult
    public func replaceOne(_ filter: String, _ document: String, upsert: Bool = false) throws -> UpdateResult {
        try updated(NC_DOC_REPLACE, filter, document, upsert ? NC_DOC_UPSERT : 0)
    }

    /// Deletes the first document `filter` matches; how many (0 or 1).
    @discardableResult
    public func deleteOne(_ filter: String) throws -> UInt64 {
        try connection.document(NC_DOC_DELETE, name, filter: filter).count
    }

    /// Deletes every document `filter` matches; how many.
    @discardableResult
    public func deleteMany(_ filter: String) throws -> UInt64 {
        try connection.document(NC_DOC_DELETE, name, filter: filter, flags: UInt32(NC_DOC_MANY)).count
    }
}
