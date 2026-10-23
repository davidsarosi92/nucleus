// The C++ driver for nucleus: a header-only C++17 layer over libnucleus
// (drivers/c): RAII connections, exceptions, std::variant values.
//
//   nucleus::Connection db;                      // 127.0.0.1:20035, nucleus / nucleus
//   db.query("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)");
//   db.query("INSERT INTO t VALUES ($1, $2)", {1, "one"});
//   for (const auto& row : db.query("SELECT id, name FROM t").rows)
//       std::cout << row[0].str() << " " << row[1].str() << "\n";
//
// Build with drivers/c/nucleus.c (or libnucleus.a) and -Idrivers/c.
#ifndef NUCLEUS_HPP
#define NUCLEUS_HPP

#include <algorithm>
#include <array>
#include <cstdint>
#include <deque>
#include <stdexcept>
#include <string>
#include <utility>
#include <variant>
#include <vector>

#include "nucleus.h"

/// This driver's version.
#define NUCLEUS_HPP_VERSION "0.1.0"

namespace nucleus {

/// An error from the server (`code()` is the SQLSTATE, `hint()` what to do
/// about it — empty: none) or the driver.
class Error : public std::runtime_error {
public:
    Error(const std::string& message, std::string code, std::string hint = "")
        : std::runtime_error(code.empty() ? message : message + " (" + code + ")"),
          code_(std::move(code)),
          hint_(std::move(hint)) {}
    const std::string& code() const { return code_; }
    /// The name meant, the cast that would work (protocol version 5).
    const std::string& hint() const { return hint_; }

private:
    std::string code_;
    std::string hint_;
};

/// An exact decimal (mantissa × 10^-scale), parsed from and printed as text.
struct Numeric {
    uint64_t lo = 0;
    int64_t hi = 0;
    uint8_t scale = 0;

    static Numeric parse(const std::string& text) {
        nc_value v;
        if (nc_numeric(text.c_str(), &v) != 0) throw Error("invalid numeric: " + text, "22P02");
        return Numeric{v.as.numeric.lo, v.as.numeric.hi, v.as.numeric.scale};
    }
    bool operator==(const Numeric& o) const { return lo == o.lo && hi == o.hi && scale == o.scale; }
};

/// Days since 2000-01-01.
struct Date {
    int32_t days = 0;
    static Date ymd(int y, int m, int d) { return Date{nc_days_from_ymd(y, m, d)}; }
    bool operator==(const Date& o) const { return days == o.days; }
};

/// Microseconds since 2000-01-01 00:00.
struct Timestamp {
    int64_t micros = 0;
    bool operator==(const Timestamp& o) const { return micros == o.micros; }
};

/// Microseconds since 2000-01-01 00:00 UTC.
struct TimestampTz {
    int64_t micros = 0;
    bool operator==(const TimestampTz& o) const { return micros == o.micros; }
};

/// Months, days and microseconds, kept apart.
struct Interval {
    int32_t months = 0, days = 0;
    int64_t micros = 0;
    bool operator==(const Interval& o) const { return months == o.months && days == o.days && micros == o.micros; }
};

/// A uuid: its 16 bytes.
struct Uuid {
    std::array<uint8_t, 16> bytes{};

    static Uuid parse(const std::string& text) {
        nc_uuid u;
        if (nc_uuid_parse(text.c_str(), &u) != 0) throw Error("invalid uuid: " + text, "22P02");
        Uuid out;
        std::copy(std::begin(u.bytes), std::end(u.bytes), out.bytes.begin());
        return out;
    }
    bool operator==(const Uuid& o) const { return bytes == o.bytes; }
};

/// A `smallint`.
struct SmallInt {
    int16_t value = 0;
    bool operator==(const SmallInt& o) const { return value == o.value; }
};

/// An `integer`.
struct Integer {
    int32_t value = 0;
    bool operator==(const Integer& o) const { return value == o.value; }
};

/// A `real`.
struct Real {
    float value = 0;
    bool operator==(const Real& o) const { return value == o.value; }
};

/// A `bytea`.
struct Bytes {
    std::vector<uint8_t> data;
    bool operator==(const Bytes& o) const { return data == o.data; }
};

/// A `time`: microseconds since midnight (`24:00:00` included).
struct Time {
    int64_t micros = 0;
    bool operator==(const Time& o) const { return micros == o.micros; }
};

/// An `inet` (an address and its mask length) or, as `Cidr`, a network.
struct Inet {
    uint8_t family = 4; // 4 or 6
    uint8_t bits = 32;
    std::array<uint8_t, 16> addr{}; // the first 4 for IPv4

    /// Parses "10.1.2.3/8", "::1".
    static Inet parse(const std::string& text) {
        nc_value v;
        if (nc_inet_parse(text.c_str(), &v) != 0) throw Error("invalid address: " + text, "22P02");
        Inet out;
        out.family = v.as.net.family;
        out.bits = v.as.net.bits;
        std::copy(std::begin(v.as.net.addr), std::end(v.as.net.addr), out.addr.begin());
        return out;
    }
    bool operator==(const Inet& o) const { return family == o.family && bits == o.bits && addr == o.addr; }
};

/// A `cidr`: a network (its host bits zero).
struct Cidr : Inet {
    static Cidr parse(const std::string& text) { return Cidr{Inet::parse(text)}; }
    bool operator==(const Cidr& o) const { return Inet::operator==(o); }
};

/// A value of another registered type: its name and binary form.
struct Ext {
    std::string type_name;
    std::vector<uint8_t> data;
    bool operator==(const Ext& o) const { return type_name == o.type_name && data == o.data; }
};

struct Value;
using Array = std::vector<Value>;
using Variant = std::variant<std::monostate, bool, int64_t, double, std::string, Numeric, Date, Timestamp,
                             TimestampTz, Interval, Array, Uuid, Ext, SmallInt, Integer, Real, Bytes, Time,
                             Inet, Cidr>;

/// A value: NULL (monostate), bool, int64 (bigint), double, text (also
/// char(n) and enums), numeric, date, timestamp, timestamptz, interval, an
/// array, a uuid, another registered type's value, smallint, integer,
/// real, bytea, time, inet, cidr.
struct Value : Variant {
    using Variant::Variant;
    Value() = default;
    Value(int v) : Variant(int64_t{v}) {}
    Value(long v) : Variant(int64_t{v}) {}
    Value(long long v) : Variant(int64_t{v}) {}
    Value(const char* s) : Variant(std::string(s)) {}

    bool is_null() const { return std::holds_alternative<std::monostate>(*this); }
    template <class T> const T& as() const { return std::get<T>(*this); }

    /// As nucleus prints it (NULL as an empty string).
    std::string str() const;
};

/// Where and as whom to connect (the defaults reach a local server).
struct Config {
    std::string host = "127.0.0.1";
    uint16_t port = NC_DEFAULT_PORT;
    std::string user = "nucleus";
    std::string password = "nucleus";
    std::string database = "nucleus";
    std::string client_name = "nucleus-cpp";
    // TLS (the C driver built with NC_WITH_OPENSSL): the server's
    // certificate checked against the system's CAs, or tls_ca's (a PEM
    // file; setting it implies tls); tls_no_verify skips the check.
    bool tls = false;
    std::string tls_ca;
    bool tls_no_verify = false;
    // A client certificate and its key (PEM files), for mutual TLS.
    std::string tls_cert;
    std::string tls_key;
    // An OpenID Connect identity token, instead of the password.
    std::string token;
};

struct Column {
    std::string name;
    nc_type type;
    nc_type element;       // for arrays
    std::string type_name; // for registered types ("uuid", …) and enums
};

/// One statement's result.
struct Result {
    std::vector<Column> columns;
    std::vector<std::vector<Value>> rows;
    std::string command; // "SELECT", "INSERT", …
    uint64_t count = 0;  // rows returned or affected
};

namespace detail {

inline Value from_c(const nc_value& v) {
    switch (v.type) {
    case NC_BOOL: return Value(bool(v.as.boolean));
    case NC_INT: return Value(int64_t{v.as.integer});
    case NC_SMALLINT: return Value(SmallInt{static_cast<int16_t>(v.as.integer)});
    case NC_INTEGER: return Value(Integer{static_cast<int32_t>(v.as.integer)});
    case NC_REAL: return Value(Real{v.as.real4});
    case NC_BYTEA: return Value(Bytes{std::vector<uint8_t>(v.as.bytes.ptr, v.as.bytes.ptr + v.as.bytes.len)});
    case NC_TIME: return Value(Time{v.as.time});
    case NC_INET:
    case NC_CIDR: {
        Inet n;
        n.family = v.as.net.family;
        n.bits = v.as.net.bits;
        std::copy(std::begin(v.as.net.addr), std::end(v.as.net.addr), n.addr.begin());
        return v.type == NC_INET ? Value(n) : Value(Cidr{n});
    }
    case NC_FLOAT: return Value(v.as.real);
    case NC_TEXT: return Value(std::string(v.as.text.ptr, v.as.text.len));
    case NC_NUMERIC: return Value(Numeric{v.as.numeric.lo, v.as.numeric.hi, v.as.numeric.scale});
    case NC_DATE: return Value(Date{v.as.date});
    case NC_TIMESTAMP: return Value(Timestamp{v.as.timestamp});
    case NC_TIMESTAMPTZ: return Value(TimestampTz{v.as.timestamp});
    case NC_INTERVAL: return Value(Interval{v.as.interval.months, v.as.interval.days, v.as.interval.micros});
    case NC_ARRAY: {
        Array items;
        for (size_t i = 0; i < v.as.array.len; i++) items.push_back(from_c(v.as.array.items[i]));
        return Value(std::move(items));
    }
    case NC_UUID: {
        Uuid u;
        std::copy(std::begin(v.as.uuid.bytes), std::end(v.as.uuid.bytes), u.bytes.begin());
        return Value(u);
    }
    case NC_EXT:
        return Value(Ext{v.as.ext.type_name, std::vector<uint8_t>(v.as.ext.data, v.as.ext.data + v.as.ext.len)});
    default: return Value();
    }
}

/// Converts values for a call; nested arrays live in `store`.
inline nc_value to_c(const Value& v, std::deque<std::vector<nc_value>>& store) {
    struct Visit {
        std::deque<std::vector<nc_value>>& store;
        nc_value operator()(std::monostate) const { return nc_null(); }
        nc_value operator()(bool b) const { return nc_bool(b); }
        nc_value operator()(int64_t i) const { return nc_int(i); }
        nc_value operator()(double f) const { return nc_float(f); }
        nc_value operator()(const std::string& s) const {
            nc_value r = nc_text(s.c_str());
            r.as.text.len = s.size();
            return r;
        }
        nc_value operator()(const Numeric& n) const {
            nc_value r = nc_null();
            r.type = NC_NUMERIC;
            r.as.numeric.lo = n.lo;
            r.as.numeric.hi = n.hi;
            r.as.numeric.scale = n.scale;
            return r;
        }
        nc_value operator()(const Date& d) const { return nc_date(d.days); }
        nc_value operator()(const Timestamp& t) const { return nc_timestamp(t.micros); }
        nc_value operator()(const TimestampTz& t) const { return nc_timestamptz(t.micros); }
        nc_value operator()(const Interval& i) const { return nc_interval(i.months, i.days, i.micros); }
        nc_value operator()(const Uuid& u) const {
            nc_uuid c;
            std::copy(u.bytes.begin(), u.bytes.end(), c.bytes);
            return nc_uuid_value(c);
        }
        nc_value operator()(const Ext& e) const { return nc_ext(e.type_name.c_str(), e.data.data(), e.data.size()); }
        nc_value operator()(const SmallInt& i) const { return nc_smallint(i.value); }
        nc_value operator()(const Integer& i) const { return nc_integer(i.value); }
        nc_value operator()(const Real& f) const { return nc_real(f.value); }
        nc_value operator()(const Bytes& b) const { return nc_bytea(b.data.data(), b.data.size()); }
        nc_value operator()(const Time& t) const { return nc_time(t.micros); }
        nc_value operator()(const Inet& n) const { return nc_inet(n.family, n.addr.data(), n.bits); }
        nc_value operator()(const Cidr& n) const { return nc_cidr(n.family, n.addr.data(), n.bits); }
        nc_value operator()(const Array& a) const {
            std::vector<nc_value> items;
            for (const auto& item : a) items.push_back(to_c(item, store));
            store.push_back(std::move(items));
            return nc_array(store.back().data(), store.back().size());
        }
    };
    return std::visit(Visit{store}, static_cast<const Variant&>(v));
}

} // namespace detail

inline std::string Value::str() const {
    std::deque<std::vector<nc_value>> store;
    nc_value v = detail::to_c(*this, store);
    size_t n = nc_format(&v, nullptr, 0);
    std::string s(n, '\0');
    nc_format(&v, s.data(), n + 1);
    return s;
}

/// A connection; closes when destroyed.  One thread at a time.
class Connection {
public:
    explicit Connection(const Config& config = Config()) {
        nc_config c = nc_default_config();
        c.host = config.host.c_str();
        c.port = config.port;
        c.user = config.user.c_str();
        c.password = config.password.c_str();
        c.database = config.database.c_str();
        c.client_name = config.client_name.c_str();
        c.tls = config.tls ? 1 : 0;
        c.tls_ca = config.tls_ca.empty() ? nullptr : config.tls_ca.c_str();
        c.tls_no_verify = config.tls_no_verify ? 1 : 0;
        c.tls_cert = config.tls_cert.empty() ? nullptr : config.tls_cert.c_str();
        c.tls_key = config.tls_key.empty() ? nullptr : config.tls_key.c_str();
        c.token = config.token.empty() ? nullptr : config.token.c_str();
        nc_error err;
        conn_ = nc_connect(&c, &err);
        if (!conn_) throw Error(err.message, err.code, err.hint);
    }
    Connection(const Connection&) = delete;
    Connection& operator=(const Connection&) = delete;
    Connection(Connection&& o) noexcept : conn_(std::exchange(o.conn_, nullptr)) {}
    Connection& operator=(Connection&& o) noexcept {
        if (this != &o) {
            nc_close(conn_);
            conn_ = std::exchange(o.conn_, nullptr);
        }
        return *this;
    }
    ~Connection() { nc_close(conn_); }

    /// Runs one statement (or several: the last one's result).
    Result query(const std::string& sql, const std::vector<Value>& params = {}) {
        std::deque<std::vector<nc_value>> store;
        std::vector<nc_value> args;
        for (const auto& p : params) args.push_back(detail::to_c(p, store));
        nc_error err;
        nc_result* r = nc_query(conn_, sql.c_str(), args.data(), args.size(), &err);
        if (!r) throw Error(err.message, err.code, err.hint);
        Result out;
        for (size_t c = 0; c < nc_columns(r); c++)
            out.columns.push_back({nc_column_name(r, c), nc_column_type(r, c), nc_column_element(r, c),
                                   nc_column_type_name(r, c) ? nc_column_type_name(r, c) : ""});
        for (size_t i = 0; i < nc_rows(r); i++) {
            std::vector<Value> row;
            for (size_t c = 0; c < nc_columns(r); c++) row.push_back(detail::from_c(*nc_value_at(r, i, c)));
            out.rows.push_back(std::move(row));
        }
        out.command = nc_command(r);
        out.count = nc_count(r);
        nc_result_free(r);
        return out;
    }

    /// BEGIN, `body(*this)`, then COMMIT — or ROLLBACK if it throws.
    template <class F> auto transaction(F body) -> decltype(body(*this)) {
        query("BEGIN");
        try {
            if constexpr (std::is_void_v<decltype(body(*this))>) {
                body(*this);
                query(status() == 2 ? "ROLLBACK" : "COMMIT");
            } else {
                auto v = body(*this);
                query(status() == 2 ? "ROLLBACK" : "COMMIT");
                return v;
            }
        } catch (...) {
            query("ROLLBACK");
            throw;
        }
    }

    /// 0 idle, 1 in a transaction block, 2 in a failed block.
    int status() const { return nc_status(conn_); }
    std::string server_version() const { return nc_server_version(conn_); }

private:
    nc_conn* conn_ = nullptr;
};

} // namespace nucleus

#endif // NUCLEUS_HPP
