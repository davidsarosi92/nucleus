// The C++ driver against a running nucleus server (NUCLEUS_HOST,
// NUCLEUS_PORT; scripts/driver-tests.sh starts one).
#include <cstdlib>
#include <ctime>
#include <iostream>
#include <sstream>
#include <string>

#include "nucleus.hpp"

static int failures = 0;
#define CHECK(cond)                                                                       \
    do {                                                                                  \
        if (!(cond)) {                                                                    \
            std::cerr << __FILE__ << ":" << __LINE__ << ": check failed: " #cond "\n";    \
            failures++;                                                                   \
        }                                                                                 \
    } while (0)

static nucleus::Config config() {
    nucleus::Config c;
    if (const char* h = std::getenv("NUCLEUS_HOST")) c.host = h;
    if (const char* p = std::getenv("NUCLEUS_PORT")) c.port = static_cast<uint16_t>(std::atoi(p));
    return c;
}

static std::string table() {
    static int n = 0;
    return "cpp_" + std::to_string(std::time(nullptr)) + "_" + std::to_string(++n);
}

static std::string error_code(nucleus::Connection& db, const std::string& sql) {
    try {
        db.query(sql);
    } catch (const nucleus::Error& e) {
        return e.code();
    }
    return "";
}

static std::string error_hint(nucleus::Connection& db, const std::string& sql) {
    try {
        db.query(sql);
    } catch (const nucleus::Error& e) {
        return e.hint();
    }
    return "(no error)";
}

int main() {
    using namespace nucleus;
    {
        Connection db(config());
        auto t = table();
        db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY, b BOOLEAN, f FLOAT, s TEXT, n NUMERIC(10, 2),"
                 " d DATE, tz TIMESTAMPTZ, iv INTERVAL, tags TEXT[])");
        std::vector<Value> row = {Integer{1}, true, 0.5, "h\xc3\xa9llo", Numeric::parse("-123.45"), Date::ymd(2024, 2, 29),
                                  TimestampTz{763387200000000LL}, Interval{1, 2, 3000000},
                                  Array{"a", Value(), "c"}};
        auto r = db.query("INSERT INTO " + t + " VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)", row);
        CHECK(r.command == "INSERT" && r.count == 1);
        r = db.query("SELECT * FROM " + t);
        CHECK(r.rows.size() == 1 && r.rows[0] == row);
        CHECK(r.columns[8].type == NC_ARRAY && r.columns[8].element == NC_TEXT);
        CHECK(r.rows[0][4].str() == "-123.45");
        CHECK(r.rows[0][6].str() == "2024-03-10 12:00:00+00");
        CHECK(r.rows[0][8].str() == "{a,NULL,c}");
        r = db.query("SELECT 1.50 + 1, $1 + 1, NULL", {"41"});
        CHECK(r.rows[0][0].str() == "2.50" && r.rows[0][1].as<Integer>().value == 42 && r.rows[0][2].is_null());
    }
    {
        Connection db(config());
        CHECK(error_code(db, "SELECT * FROM nosuch") == "42P01");
        CHECK(error_code(db, "SELECT 1 / 0") == "22012");
        auto t = table();
        db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY)");
        // What to do about it: the hint (protocol version 5).
        CHECK(error_hint(db, "SELECT ib FROM " + t) == "Perhaps you meant column \"id\".");
        CHECK(error_hint(db, "SELECT 'x'::integer").empty());
        db.transaction([&](Connection& c) { c.query("INSERT INTO " + t + " VALUES (1)"); });
        try {
            db.transaction([&](Connection& c) {
                c.query("INSERT INTO " + t + " VALUES (2)");
                throw std::runtime_error("roll back");
            });
            CHECK(false);
        } catch (const std::runtime_error&) {
        }
        CHECK(db.status() == 0);
        CHECK(db.query("SELECT count(*) FROM " + t).rows[0][0].as<int64_t>() == 1);
        CHECK(error_code(db, "INSERT INTO " + t + " VALUES (1)") == "23505");
    }
    {
        Connection db(config());
        auto t = table();
        auto u = Uuid::parse("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11");
        db.query("CREATE TABLE " + t + " (id UUID PRIMARY KEY, n INT)");
        db.query("INSERT INTO " + t + " VALUES ($1, 1)", {u});
        auto r = db.query("SELECT id, gen_random_uuid() FROM " + t);
        CHECK(r.columns[0].type == NC_UUID && r.columns[0].type_name == "uuid");
        CHECK(r.rows[0][0] == Value(u));
        CHECK(r.rows[0][0].str() == "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
        CHECK(r.rows[0][1].as<Uuid>().bytes[6] >> 4 == 4);
    }
    {
        // §M14.26's types, natively (protocol version 3).
        Connection db(config());
        auto t = table();
        db.query("CREATE TYPE " + t + "_mood AS ENUM ('sad', 'happy')");
        db.query("CREATE TABLE " + t + " (s SMALLINT, i INTEGER, r REAL, b BYTEA, tm TIME, ch CHAR(3), ip INET,"
                 " net CIDR, m " + t + "_mood)");
        std::vector<Value> row = {SmallInt{-2}, Integer{70000}, Real{0.1f}, Bytes{{0, 0xff, 7}},
                                  Time{45296500000LL}, "ab ", Inet::parse("10.1.2.3/8"),
                                  Cidr::parse("2001:db8::/32"), "happy"};
        db.query("INSERT INTO " + t + " VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)", row);
        auto r = db.query("SELECT * FROM " + t);
        CHECK(r.rows.size() == 1 && r.rows[0] == row);
        CHECK(r.columns[5].type == NC_CHAR && r.columns[8].type == NC_ENUM && r.columns[8].type_name == t + "_mood");
        std::vector<std::string> shown;
        for (const auto& v : r.rows[0]) shown.push_back(v.str());
        CHECK((shown == std::vector<std::string>{"-2", "70000", "0.1", "\\x00ff07", "12:34:56.5", "ab ",
                                                 "10.1.2.3/8", "2001:db8::/32", "happy"}));
    }
    {
        auto c = config();
        c.password = "wrong";
        try {
            Connection db(c);
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "28P01");
        }
    }
    if (const char* token = std::getenv("NUCLEUS_TOKEN")) {
        auto c = config();
        c.user = "";
        c.token = token;
        Connection db(c);
        CHECK(db.query("SELECT current_user").rows[0][0].str() == "drivers_oidc");
    }
    if (const char* ca = std::getenv("NUCLEUS_TLS_CA"); ca && nc_tls_supported()) {
        auto c = config();
        c.tls_ca = ca;
        Connection db(c);
        CHECK(db.query("SELECT 'over tls'").rows[0][0].str() == "over tls");
    }
    {
        // Cursors: ten billion rows, only what is read is produced.
        Connection db(config());
        const std::string big = "SELECT a, b FROM generate_series(1, 100000) a, generate_series(1, 100000) b";
        {
            Cursor cur = db.cursor(big, {}, 500);
            CHECK(cur.columns().size() == 2 && cur.columns()[1].name == "b");
            int n = 0;
            for (const auto& row : cur) {
                CHECK(row.size() == 2);
                if (++n == 1201) break;
            }
            cur.close();
            CHECK(cur.command() == "SELECT" && cur.count() == 1500);
        }
        CHECK(db.query("SELECT 1").rows.size() == 1);
        Cursor all = db.cursor("SELECT i FROM generate_series(1, 2500) i", {}, 100);
        int n = 0;
        std::vector<Value> row;
        while (all.next(row)) n++;
        CHECK(n == 2500 && all.count() == 2500);
        try {
            db.cursor("SELECT nope");
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "42703");
        }
        n = 0;
        try {
            for (const auto& r : db.cursor("SELECT 1 / (300 - i) FROM generate_series(1, 400) i", {}, 64)) {
                (void)r;
                n++;
            }
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "22012" && n == 299);
        }
        CHECK(db.status() == 0);
        // Columnar batches.
        Batches b = db.batches("SELECT i, i * 0.5::float, 'v' || i FROM generate_series(1, 300) i", {}, 128);
        size_t total = 0, batches = 0;
        while (b.next()) {
            const int32_t* ids = b.values<int32_t>(0);
            const double* half = b.values<double>(1);
            for (size_t i = 0; i < b.rows(); i++) {
                int id = static_cast<int>(total + i + 1);
                CHECK(ids[i] == id && half[i] == id * 0.5);
                CHECK(b.text(2, i) == "v" + std::to_string(id));
            }
            total += b.rows();
            batches++;
        }
        CHECK(total == 300 && batches == 3 && b.count() == 300);
        // Bulk load.
        auto t = table();
        db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY, name TEXT)");
        CHECK(db.copy("COPY " + t + " FROM STDIN", "1\tone\n2\t\\N\n") == 2);
        std::istringstream csv("3,three\n4,\"f, our\"\n");
        CHECK(db.copy("COPY " + t + " FROM STDIN WITH (FORMAT csv)", csv) == 2);
        try {
            db.copy("COPY " + t + " FROM STDIN", "9\tx\n1\tdup\n");
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "23505");
        }
        CHECK(db.query("SELECT count(*) FROM " + t).rows[0][0].str() == "4");
    }
    if (failures) {
        std::cerr << failures << " check(s) failed\n";
        return 1;
    }
    std::cout << "nucleus.hpp: all checks passed\n";
    return 0;
}
