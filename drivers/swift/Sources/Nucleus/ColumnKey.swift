// Client-side encrypted columns (nucleus PLAN.md §M14.30 K5, DOCS.md §11),
// over the C driver's nc_column_key: a value encrypted before it is sent
// and decrypted after it arrives, so the server stores bytea ciphertext and
// never holds the key; equality through a blind index stored beside it.
//
//   let key = ColumnKey(master: master, name: "users.email")   // master: 32 bytes you keep
//   try db.query("INSERT INTO users VALUES ($1, $2, $3)",
//                [1, .bytes(try key.encrypt(.text("ann@example.com"))), .bytes(key.blindIndex(.text("ann@example.com")))])

import CNucleus
import Foundation

/// A column's keys, from a 32-byte master key the application keeps (a
/// keychain's, a KMS's: never the database's) and the column's name.
public struct ColumnKey {
    /// A value an encrypted column holds.
    public enum Plain: Equatable, Sendable {
        case bytes(Data)
        case text(String)
        case int(Int64)
    }

    private var key = nc_column_key()

    public init(master: [UInt8], name: String) {
        precondition(master.count == 32, "a master key is 32 bytes")
        nc_column_key_init(&key, master, name)
    }

    private func parts<T>(_ v: Plain, _ body: (nc_plain_type, UnsafeRawPointer?, Int) throws -> T) rethrows -> T {
        switch v {
        case .bytes(let d):
            return try d.withUnsafeBytes { try body(NC_PLAIN_BYTES, $0.baseAddress, $0.count) }
        case .text(let s):
            let utf8 = Array(s.utf8)
            return try utf8.withUnsafeBytes { try body(NC_PLAIN_TEXT, $0.baseAddress, $0.count) }
        case .int(let i):
            var i = i
            return try withUnsafeBytes(of: &i) { try body(NC_PLAIN_INT, $0.baseAddress, 8) }
        }
    }

    /// `value` encrypted, the bytes to store: a fresh random nonce each time
    /// (`nonce`, 24 bytes, for test vectors only).
    public func encrypt(_ value: Plain, nonce: [UInt8]? = nil) throws -> Data {
        var key = self.key
        return try parts(value) { t, data, len in
            var out = [UInt8](repeating: 0, count: len + Int(NC_COLUMN_OVERHEAD))
            let rc = nonce.map { n in nc_encrypt(&key, t, data, len, n, &out) } ?? nc_encrypt(&key, t, data, len, nil, &out)
            if rc != 0 { throw NucleusError(code: "58000", message: "no randomness for a nonce") }
            return Data(out)
        }
    }

    /// The value in `sealed`; code 22023 when this key did not make it, or it
    /// was changed.
    public func decrypt(_ sealed: Data) throws -> Plain {
        var key = self.key
        let fail = NucleusError(code: "22023", message: "an encrypted value this column's key does not open")
        guard sealed.count >= Int(NC_COLUMN_OVERHEAD) else { throw fail }
        var out = [UInt8](repeating: 0, count: sealed.count - Int(NC_COLUMN_OVERHEAD) + 1)
        var type = NC_PLAIN_BYTES
        var n = 0
        let rc = sealed.withUnsafeBytes { s in
            nc_decrypt(&key, s.bindMemory(to: UInt8.self).baseAddress, s.count, &type, &out, &n)
        }
        if rc != 0 { throw fail }
        switch type {
        case NC_PLAIN_TEXT: return .text(String(decoding: out.prefix(n), as: UTF8.self))
        case NC_PLAIN_INT: return .int(out.withUnsafeBytes { $0.loadUnaligned(as: Int64.self) })
        default: return .bytes(Data(out.prefix(n)))
        }
    }

    /// `value`'s blind index (16 bytes): equal values have equal ones, so
    /// `WHERE email_index = $1` with it finds a value's rows.
    public func blindIndex(_ value: Plain) -> Data {
        var key = self.key
        return parts(value) { t, data, len in
            var out = [UInt8](repeating: 0, count: Int(NC_BLIND_INDEX_LEN))
            nc_blind_index(&key, t, data, len, &out)
            return Data(out)
        }
    }
}
