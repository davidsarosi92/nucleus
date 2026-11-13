// A database inside the app (PLAN.md §M14.23): the embedded edition — the
// engine, linked in from NucleusEmbedded.xcframework — with the driver's
// API.  Each connect() is a session with the API a server's connection has.
//
//   import NucleusEmbedded
//   let db = try Database.open(named: "app")    // Application Support/app
//   let conn = try db.connect()
//   try conn.query("CREATE TABLE IF NOT EXISTS notes (id INT PRIMARY KEY, body TEXT)")
//   try conn.query("INSERT INTO notes VALUES ($1, $2)", [1, "hello"])

import CNucleus
import CNucleusEmbedded
import Foundation
@_exported import Nucleus
import NucleusEmbeddedC

/// A database in the process: in a directory (kept), or in memory.
public final class Database {
    private var db: OpaquePointer?
    /// Its directory, or ":memory:".
    public let path: String

    private init(_ db: OpaquePointer, path: String) {
        self.db = db
        self.path = path
    }

    deinit { try? close() }

    /// Opens the database in directory `path` (made if missing), or a new
    /// one in memory for ":memory:".  `options`: "cache=64MB; sync=ordered|full;
    /// wal=minimal|full".  A directory this process has open already is
    /// shared; another process's is refused.
    public static func open(_ path: String = ":memory:", options: String = "") throws -> Database {
        var error: UnsafeMutablePointer<CChar>?
        guard let db = nce_open(path, options, &error) else {
            let message = error.map { String(cString: $0) } ?? "cannot open \(path)"
            nce_free_error(error)
            throw NucleusError(message: message)
        }
        return Database(db, path: path)
    }

    /// Opens (or makes) the database `name` in the app's Application
    /// Support directory: kept with the app's data, backed up with it.
    public static func open(named name: String, options: String = "") throws -> Database {
        let dir = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                              appropriateFor: nil, create: true)
        return try open(dir.appendingPathComponent(name, isDirectory: true).path, options: options)
    }

    /// A new session, as `user` (who it runs as: there is no login).
    public func connect(user: String = "nucleus", database: String = "nucleus",
                        clientName: String = "nucleus-swift (embedded)") throws -> Connection
    {
        guard let db else { throw NucleusError(message: "the database is closed") }
        var cfg = nc_default_config()
        var err = nc_error()
        let c = user.withCString { u in
            database.withCString { d in
                clientName.withCString { n in
                    cfg.user = u
                    cfg.database = d
                    cfg.client_name = n
                    return nc_connect_embedded(db, &cfg, &err)
                }
            }
        }
        guard let c else { throw NucleusError(err) }
        return Connection(adopting: c)
    }

    /// Closes it: its sessions end, a directory is checkpointed and free
    /// again.  Close its connections first.
    public func close() throws {
        guard let db else { return }
        self.db = nil
        var error: UnsafeMutablePointer<CChar>?
        if nce_close(db, &error) != 0 {
            let message = error.map { String(cString: $0) } ?? "cannot close \(path)"
            nce_free_error(error)
            throw NucleusError(message: message)
        }
    }

    /// The engine's version.
    public static var engineVersion: String { String(cString: nce_version()) }
}
