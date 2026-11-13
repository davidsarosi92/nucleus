// Values both ways: Swift's types for nucleus's.

import CNucleus
import Foundation

/// A value of a column or a parameter.
///
/// Literals make values: `[1, "one", 2.5, true, nil]` is
/// `[.int(1), .text("one"), .double(2.5), .bool(true), .null]`.
public enum Value: Equatable, Hashable, Sendable {
    case null
    case bool(Bool)
    /// bigint (and integer literals).
    case int(Int64)
    /// integer.
    case int32(Int32)
    /// smallint.
    case int16(Int16)
    /// double precision.
    case double(Double)
    /// real.
    case float(Float)
    /// text, varchar, char(n), and an enum's label.
    case text(String)
    /// numeric: exact, its scale kept ("2.50").
    case numeric(Numeric)
    /// bytea.
    case bytes(Data)
    /// date: a day of the calendar.
    case date(year: Int, month: Int, day: Int)
    /// time: microseconds since midnight (24:00:00 included).
    case time(Int64)
    /// timestamp (without a time zone: its time as if in UTC).
    case timestamp(Date)
    /// timestamptz.
    case timestamptz(Date)
    case interval(months: Int32, days: Int32, micros: Int64)
    case uuid(UUID)
    /// inet, in its text form ("10.1.2.3/8", "2001:db8::1").
    case inet(String)
    /// cidr, in its text form.
    case cidr(String)
    /// An array of values (one dimension).
    case array([Value])
    /// vector: its components.
    case vector([Float])
    /// sparsevec: dimensions, and the non-zero components' positions (from
    /// 0, ascending) and values.
    case sparsevec(dimensions: Int, indices: [Int], values: [Float])
    /// Another registered type: its name and binary form.
    case ext(type: String, data: Data)

    /// Whether it is NULL.
    public var isNull: Bool { self == .null }

    /// An integer of any width, or nil.
    public var intValue: Int64? {
        switch self {
        case let .int(i): i
        case let .int32(i): Int64(i)
        case let .int16(i): Int64(i)
        default: nil
        }
    }

    /// A number as a double, or nil.
    public var doubleValue: Double? {
        switch self {
        case let .double(d): d
        case let .float(f): Double(f)
        case let .numeric(n): Double(n.text)
        default: intValue.map(Double.init)
        }
    }

    /// Text, or nil.
    public var stringValue: String? {
        if case let .text(s) = self { return s }
        return nil
    }

    /// A boolean, or nil.
    public var boolValue: Bool? {
        if case let .bool(b) = self { return b }
        return nil
    }
}

extension Value: ExpressibleByNilLiteral, ExpressibleByBooleanLiteral, ExpressibleByIntegerLiteral,
    ExpressibleByFloatLiteral, ExpressibleByStringLiteral, ExpressibleByArrayLiteral
{
    public init(nilLiteral: ()) { self = .null }
    public init(booleanLiteral b: Bool) { self = .bool(b) }
    public init(integerLiteral i: Int64) { self = .int(i) }
    public init(floatLiteral d: Double) { self = .double(d) }
    public init(stringLiteral s: String) { self = .text(s) }
    public init(arrayLiteral items: Value...) { self = .array(items) }
}

extension Value: CustomStringConvertible {
    /// Its text, as ncsql prints it.
    public var description: String {
        let arena = Arena()
        guard let v = try? arena.value(self) else { return "?" }
        return withUnsafePointer(to: v) { format($0) }
    }
}

/// An exact decimal number as nucleus's numeric holds it (up to 38
/// digits), its scale kept: "2.50" stays "2.50".
public struct Numeric: Hashable, Sendable, CustomStringConvertible, ExpressibleByStringLiteral {
    /// Its text: digits, a point, a sign ("-12.50").
    public let text: String

    /// Parses "-12.50" (nil if it is not a number nucleus can hold).
    public init?(_ text: String) {
        var v = nc_value()
        guard nc_numeric(text, &v) == 0 else { return nil }
        self.text = withUnsafePointer(to: v) { format($0) }
    }

    /// A Foundation decimal's value.
    public init(_ d: Decimal) {
        self.init(exact: NSDecimalNumber(decimal: d).description(withLocale: Locale(identifier: "en_US_POSIX")))
    }

    init(exact text: String) { self.text = text }

    public init(stringLiteral text: String) {
        guard let n = Numeric(text) else { preconditionFailure("not a numeric: \(text)") }
        self = n
    }

    /// Its value as a Foundation decimal (which keeps no scale).
    public var decimal: Decimal { Decimal(string: text, locale: Locale(identifier: "en_US_POSIX")) ?? .nan }

    public var description: String { text }
}

/// Seconds from 2000-01-01 (the protocol's epoch) to 2001-01-01 (Foundation's).
private let epochGap: Double = 366 * 86400

func dateOf(micros: Int64) -> Date {
    Date(timeIntervalSinceReferenceDate: Double(micros) / 1e6 - epochGap)
}

func microsOf(_ d: Date) -> Int64 {
    Int64(((d.timeIntervalSinceReferenceDate + epochGap) * 1e6).rounded())
}

/// The text of a value, as ncsql prints it.
func format(_ v: UnsafePointer<nc_value>) -> String {
    var buf = [CChar](repeating: 0, count: 256)
    let n = nc_format(v, &buf, buf.count)
    if n >= buf.count {
        buf = [CChar](repeating: 0, count: n + 1)
        _ = nc_format(v, &buf, buf.count)
    }
    return String(cString: buf)
}

func string(_ p: UnsafePointer<CChar>?, _ n: Int) -> String {
    guard let p, n > 0 else { return "" }
    return String(decoding: UnsafeRawBufferPointer(start: p, count: n), as: UTF8.self)
}

/// A value received.
func value(_ p: UnsafePointer<nc_value>) -> Value {
    let v = p.pointee
    switch v.type {
    case NC_NULL: return .null
    case NC_BOOL: return .bool(v.as.boolean != 0)
    case NC_INT: return .int(v.as.integer)
    case NC_INTEGER: return .int32(Int32(truncatingIfNeeded: v.as.integer))
    case NC_SMALLINT: return .int16(Int16(truncatingIfNeeded: v.as.integer))
    case NC_FLOAT: return .double(v.as.real)
    case NC_REAL: return .float(v.as.real4)
    case NC_TEXT: return .text(string(v.as.text.ptr, v.as.text.len))
    case NC_NUMERIC: return .numeric(Numeric(exact: format(p)))
    case NC_BYTEA:
        guard let b = v.as.bytes.ptr else { return .bytes(Data()) }
        return .bytes(Data(bytes: b, count: v.as.bytes.len))
    case NC_DATE:
        var y: Int32 = 0, m: Int32 = 0, d: Int32 = 0
        nc_ymd_from_days(v.as.date, &y, &m, &d)
        return .date(year: Int(y), month: Int(m), day: Int(d))
    case NC_TIME: return .time(v.as.time)
    case NC_TIMESTAMP: return .timestamp(dateOf(micros: v.as.timestamp))
    case NC_TIMESTAMPTZ: return .timestamptz(dateOf(micros: v.as.timestamp))
    case NC_INTERVAL:
        return .interval(months: v.as.interval.months, days: v.as.interval.days, micros: v.as.interval.micros)
    case NC_UUID: return .uuid(UUID(uuid: v.as.uuid.bytes))
    case NC_INET: return .inet(format(p))
    case NC_CIDR: return .cidr(format(p))
    case NC_ARRAY:
        let a = v.as.array
        return .array((0..<a.len).map { value(a.items + $0) })
    case NC_VECTOR:
        let a = v.as.vector
        return .vector(a.ptr.map { Array(UnsafeBufferPointer(start: $0, count: a.len)) } ?? [])
    case NC_SPARSEVEC:
        let s = v.as.sparse
        let indices = s.indices.map { Array(UnsafeBufferPointer(start: $0, count: s.nnz)).map(Int.init) } ?? []
        let values = s.values.map { Array(UnsafeBufferPointer(start: $0, count: s.nnz)) } ?? []
        return .sparsevec(dimensions: Int(s.dims), indices: indices, values: values)
    case NC_EXT:
        let e = v.as.ext
        let data = e.data.map { Data(bytes: $0, count: e.len) } ?? Data()
        return .ext(type: e.type_name.map { String(cString: $0) } ?? "", data: data)
    default: return .ext(type: "unknown", data: Data())
    }
}

/// Memory for a statement's parameters, freed with it.
final class Arena {
    private var blocks: [UnsafeMutableRawPointer] = []

    deinit { for b in blocks { free(b) } }

    func cString(_ s: String) -> UnsafePointer<CChar> {
        let p = strdup(s)!
        blocks.append(p)
        return UnsafePointer(p)
    }

    func buffer<T>(_ items: [T]) -> UnsafePointer<T> {
        let raw = malloc(MemoryLayout<T>.stride * max(items.count, 1))!
        blocks.append(raw)
        let p = raw.bindMemory(to: T.self, capacity: max(items.count, 1))
        p.initialize(from: items, count: items.count)
        return UnsafePointer(p)
    }

    func bytes(_ d: Data) -> UnsafePointer<UInt8> { buffer([UInt8](d)) }

    /// A value to send.
    func value(_ v: Value) throws -> nc_value {
        switch v {
        case .null: return nc_null()
        case let .bool(b): return nc_bool(b ? 1 : 0)
        case let .int(i): return nc_int(i)
        case let .int32(i): return nc_integer(i)
        case let .int16(i): return nc_smallint(i)
        case let .double(d): return nc_float(d)
        case let .float(f): return nc_real(f)
        case let .text(s): return nc_text(cString(s))
        case let .numeric(n):
            var out = nc_value()
            guard nc_numeric(cString(n.text), &out) == 0 else {
                throw NucleusError(message: "not a numeric: \(n.text)")
            }
            return out
        case let .bytes(d): return nc_bytea(bytes(d), d.count)
        case let .date(y, m, d): return nc_date(nc_days_from_ymd(Int32(y), Int32(m), Int32(d)))
        case let .time(t): return nc_time(t)
        case let .timestamp(d): return nc_timestamp(microsOf(d))
        case let .timestamptz(d): return nc_timestamptz(microsOf(d))
        case let .interval(months, days, micros): return nc_interval(months, days, micros)
        case let .uuid(u):
            var x = nc_uuid()
            x.bytes = u.uuid
            return nc_uuid_value(x)
        case let .inet(s), let .cidr(s):
            var out = nc_value()
            guard nc_inet_parse(cString(s), &out) == 0 else {
                throw NucleusError(message: "not an address: \(s)")
            }
            if case .cidr = v { out.type = NC_CIDR }
            return out
        case let .array(items):
            let values = try items.map { try value($0) }
            return nc_array(buffer(values), values.count)
        case let .vector(c): return nc_vector(buffer(c), c.count)
        case let .sparsevec(dims, indices, values):
            guard indices.count == values.count else {
                throw NucleusError(message: "a sparsevec's indices and values differ in number")
            }
            return nc_sparsevec(UInt32(dims), buffer(indices.map { UInt32($0) }), buffer(values), values.count)
        case let .ext(type, data): return nc_ext(cString(type), bytes(data), data.count)
        }
    }

    func values(_ vs: [Value]) throws -> UnsafePointer<nc_value> { buffer(try vs.map { try value($0) }) }
}
