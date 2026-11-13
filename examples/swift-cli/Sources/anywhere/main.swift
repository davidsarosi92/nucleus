// The same code against a server or a database inside the app — only
// where it is changes:
//
//   swift run --package-path examples/swift anywhere ./notes.db    # a directory, kept
//   swift run --package-path examples/swift anywhere :memory:      # gone at exit
//   swift run --package-path examples/swift anywhere 127.0.0.1:20035
//
// (NucleusEmbedded needs drivers/swift/build-xcframework.sh first.)

import Foundation
import NucleusEmbedded

let target = CommandLine.arguments.dropFirst().first ?? ":memory:"
var database: Database?
let db: Connection
if let colon = target.lastIndex(of: ":"), let port = UInt16(target[target.index(after: colon)...]) {
    db = try Connection.connect(host: String(target[..<colon]), port: port)
} else {
    database = try Database.open(target)
    db = try database!.connect()
}

try db.query("CREATE TABLE IF NOT EXISTS notes (id INT PRIMARY KEY, body TEXT, embedding vector(3))")
let next = try db.query("SELECT coalesce(max(id), 0) + 1 FROM notes").rows[0][0]
try db.query("INSERT INTO notes VALUES ($1, $2, $3)",
             [next, "note \(next)", .vector([Float.random(in: 0...1), 0.5, 0.25])])
let nearest = try db.query("SELECT id, body FROM notes ORDER BY embedding <-> $1 LIMIT 3", [.vector([1, 0.5, 0.25])])
print("\(try db.query("SELECT count(*) FROM notes").rows[0][0]) notes; nearest: " +
    nearest.rows.map { "\($0["body"]!)" }.joined(separator: ", "))
db.close()
try database?.close()
