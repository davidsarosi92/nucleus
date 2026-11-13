// The notes, in a nucleus database in the app's Application Support
// directory: opened once, one session for the app.

import Foundation
import NucleusEmbedded

struct Note {
    let id: Int64
    let body: String
    let created: String
}

@MainActor
final class Notes: ObservableObject {
    @Published var items: [Note] = []
    @Published var draft = ""
    @Published var query = ""
    @Published var summary = ""
    /// This launch's number.
    private(set) var launch: Int64 = 0
    private let db: Database
    private let conn: Connection

    init() {
        do {
            db = try Database.open(named: "notes")
            conn = try db.connect()
            try conn.query("""
                CREATE TABLE IF NOT EXISTS notes (
                  id BIGINT PRIMARY KEY,
                  body TEXT NOT NULL,
                  created TIMESTAMPTZ NOT NULL DEFAULT now())
                """)
            if try conn.query("SELECT count(*) FROM notes").rows[0][0].intValue == 0 {
                // The first launch: a few to start with, loaded in bulk.
                try conn.copy("COPY notes (id, body) FROM STDIN WITH (FORMAT csv)", text: """
                    1,Welcome: this list is a database inside the app
                    2,"It is kept across launches, in Application Support"
                    3,Search filters it with SQL
                    """)
            }
            // Each launch counted: the count goes on from the last.
            try conn.query("CREATE TABLE IF NOT EXISTS launches (n INT PRIMARY KEY, at TIMESTAMPTZ DEFAULT now())")
            launch = try conn.query("INSERT INTO launches (n) SELECT coalesce(max(n), 0) + 1 FROM launches RETURNING n")
                .rows[0][0].intValue ?? 0
        } catch {
            fatalError("the notes database: \(error)")
        }
        load()
        // What the example's runner (examples/ios/run.sh) reads in the log.
        print("nucleus notes: launch \(launch), \(summary)")
    }

    func load() {
        do {
            let found = query.isEmpty
                ? try conn.query("SELECT id, body, created::text FROM notes ORDER BY id DESC")
                : try conn.query("SELECT id, body, created::text FROM notes WHERE body ILIKE $1 ORDER BY id DESC",
                                 [.text("%\(query)%")])
            items = found.rows.map {
                Note(id: $0[0].intValue ?? 0, body: $0[1].stringValue ?? "", created: $0[2].stringValue ?? "")
            }
            let total = try conn.query("SELECT count(*) FROM notes").rows[0][0]
            summary = "\(items.count) of \(total) notes, engine \(Database.engineVersion)"
        } catch {
            summary = "\(error)"
        }
    }

    func add() {
        guard !draft.isEmpty else { return }
        do {
            try conn.query("INSERT INTO notes (id, body) SELECT coalesce(max(id), 0) + 1, $1 FROM notes",
                           [.text(draft)])
            draft = ""
            load()
        } catch {
            summary = "\(error)"
        }
    }
}
