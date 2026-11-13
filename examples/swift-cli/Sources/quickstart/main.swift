// nucleus from Swift: connect, create, insert with parameters, query, a
// transaction, a cursor.  Run a server first (`nucleus server ./mydb`), then:
//
//   swift run --package-path examples/swift quickstart

import Foundation
import Nucleus

let port = ProcessInfo.processInfo.environment["NUCLEUS_PORT"].flatMap(UInt16.init) ?? 20035
let db = try Connection.connect(port: port)  // 127.0.0.1, user/password nucleus
print("connected to \(db.serverVersion)")

try db.query("DROP TABLE IF EXISTS swift_orders")
try db.query("CREATE TABLE swift_orders (id INT PRIMARY KEY, customer TEXT NOT NULL, total NUMERIC(10, 2), placed DATE)")
try db.query("INSERT INTO swift_orders VALUES ($1, $2, $3, $4)",
             [1, "Anna", .numeric("19.99"), .date(year: 2024, month: 5, day: 1)])

// A transaction: both statements or neither.
try db.transaction {
    try db.query("INSERT INTO swift_orders VALUES ($1, $2, $3, $4)",
                 [2, "Béla", .numeric("120.00"), .date(year: 2024, month: 5, day: 3)])
    try db.query("UPDATE swift_orders SET total = total * 0.9 WHERE id = 2")
}

for row in try db.query("SELECT id, customer, total, placed FROM swift_orders ORDER BY id").rows {
    print(row.values.map(\.description).joined(separator: " | "))
}

// A large result, a batch at a time.
let rows = try db.cursor("SELECT x FROM generate_series(1, 100000) x", fetch: 10000)
var sum: Int64 = 0
while let row = try rows.next() { sum += row[0].intValue ?? 0 }
print("sum of 1..100000 = \(sum)")
db.close()
