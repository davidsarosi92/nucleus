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
        Connection db(config());
        auto t = table();
        db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY, v vector(3))");
        auto ins = "INSERT INTO " + t + " VALUES ($1, $2)";
        db.query(ins, {1, Vector{1, 2, 3}});
        db.query(ins, {2, Vector{4, 6, 3}});
        db.query(ins, {3, Vector{1, 2, 3.5f}});
        auto r = db.query("SELECT id, v FROM " + t + " ORDER BY v <-> $1 LIMIT 2", {Vector{1, 2, 3.4f}});
        CHECK(r.columns[1].type == NC_VECTOR && r.columns[1].type_name == "vector");
        CHECK(r.rows[0][0].as<Integer>().value == 3);
        CHECK(r.rows[1][1] == Value(Vector{1, 2, 3}));
        CHECK(r.rows[0][1].str() == "[1,2,3.5]");
        CHECK(error_code(db, "INSERT INTO " + t + " VALUES (9, '[1,2]')") == "22000");
    }
    {
        Connection db(config());
        auto t = table();
        db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY, v sparsevec(100000))");
        auto ins = "INSERT INTO " + t + " VALUES ($1, $2)";
        SparseVector a{100000, {0, 99999}, {1, 2}};
        db.query(ins, {1, a});
        db.query(ins, {2, SparseVector{100000, {5}, {-3.5f}}});
        auto r = db.query("SELECT id, v FROM " + t + " ORDER BY v <-> $1 LIMIT 1",
                          {SparseVector{100000, {0, 99999}, {1, 2.5f}}});
        CHECK(r.columns[1].type == NC_SPARSEVEC && r.columns[1].type_name == "sparsevec");
        CHECK(r.rows[0][0].as<Integer>().value == 1);
        CHECK(r.rows[0][1] == Value(a));
        CHECK(r.rows[0][1].str() == "{1:1,100000:2}/100000");
        CHECK(error_code(db, "INSERT INTO " + t + " VALUES (9, '{4:1}/3')") == "22000");
    }
    {
        Connection db(config());
        auto t = table();
        db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY, doc JSON)");
        db.query("INSERT INTO " + t + " VALUES (1, $1)", {Json{"{\"b\": [1, 2.50], \"a\": null}"}});
        auto r = db.query("SELECT doc FROM " + t);
        CHECK(r.columns[0].type == NC_JSON && r.columns[0].type_name == "json");
        CHECK(r.rows[0][0] == Value(Json{"{\"a\": null, \"b\": [1, 2.5]}"}));
        CHECK(r.rows[0][0].str() == "{\"a\": null, \"b\": [1, 2.5]}");
        CHECK(error_code(db, "SELECT '{bad'::json") == "22P02");
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
    {
        // Pipelines, and a statement for many rows all or none.
        Connection db(config());
        auto t = table();
        db.query("CREATE TABLE " + t + " (k BIGINT PRIMARY KEY, v TEXT)");
        std::vector<std::vector<Value>> rows;
        for (int64_t i = 0; i < 3000; i++) rows.push_back({i, "x"});
        CHECK(db.execute_many("INSERT INTO " + t + " VALUES ($1, $2)", rows) == 3000);
        std::vector<Connection::Statement> reads;
        for (int64_t i = 2999; i >= 0; i--) reads.push_back({"SELECT k FROM " + t + " WHERE k = $1", {i}});
        reads.insert(reads.begin() + 10, {"SELECT * FROM nosuch", {}});
        auto r = db.pipeline(reads);
        CHECK(r.size() == 3001);
        CHECK(std::holds_alternative<Error>(r[10]) && std::get<Error>(r[10]).code() == "42P01");
        bool in_order = true;
        for (size_t i = 0, k = 2999; i < r.size(); i++) {
            if (i == 10) continue;
            const auto* res = std::get_if<Result>(&r[i]);
            if (!res || res->rows.size() != 1 || res->rows[0][0].str() != std::to_string(k)) in_order = false;
            k--;
        }
        CHECK(in_order);
        rows.clear();
        for (int64_t i = 3000; i < 4000; i++) rows.push_back({i, "y"});
        rows[500][0] = int64_t{10};
        try {
            db.execute_many("INSERT INTO " + t + " VALUES ($1, $2)", rows);
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "23505");
        }
        CHECK(db.query("SELECT count(*) FROM " + t).rows[0][0].str() == "3000");
    }
    {
        // A column the driver encrypts (nucleus PLAN.md §M14.30 K5).
        std::array<uint8_t, 32> master;
        master.fill(0x5a);
        ColumnKey email(master, "users.email"), balance(master, "users.balance"), phone(master, "users.phone");
        try {
            phone.decrypt(email.encrypt(std::string("x")));
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "22023");
        }
        Connection db(config());
        auto t = table();
        db.query("CREATE TABLE " + t + " (id BIGINT PRIMARY KEY, email BYTEA, email_index BYTEA, balance BYTEA)");
        std::vector<std::pair<std::string, int64_t>> people = {{"ann@example.com", 100}, {"bob@example.com", -5},
                                                              {"ann@example.com", 7}};
        for (size_t i = 0; i < people.size(); i++) {
            db.query("INSERT INTO " + t + " VALUES ($1, $2, $3, $4)",
                     {int64_t(i + 1), email.encrypt(people[i].first), email.blind_index(people[i].first),
                      balance.encrypt(people[i].second)});
        }
        auto r = db.query("SELECT id, email, balance FROM " + t + " WHERE email_index = $1 ORDER BY id",
                          {email.blind_index(std::string("ann@example.com"))});
        CHECK(r.rows.size() == 2 && r.rows[1][0].as<int64_t>() == 3);
        CHECK(std::get<std::string>(email.decrypt(r.rows[1][1].as<Bytes>())) == "ann@example.com");
        CHECK(std::get<int64_t>(balance.decrypt(r.rows[1][2].as<Bytes>())) == 7);
    }
    {
        // A subject's key its user holds (the k message, protocol 8).
        Connection admin(config()), other(config());
        auto t = table();
        auto who = t + "-who";
        std::array<uint8_t, 32> secret, wrong;
        for (uint8_t i = 0; i < 32; i++) secret[i] = i;
        wrong.fill(1);
        admin.query("CREATE TABLE " + t + " (id INT PRIMARY KEY, owner TEXT, body TEXT) WITH (storage = sealed)");
        admin.query("ALTER TABLE " + t + " SET SUBJECT (owner)");
        admin.query("INSERT INTO " + t + " VALUES (1, $1, 'held')", {who});
        admin.hold_subject_key(who, secret);
        CHECK(error_code(other, "SELECT body FROM " + t) == "28000");
        try {
            other.present_subject_key(who, wrong);
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "28000");
        }
        other.present_subject_key(who, secret);
        CHECK(other.query("SELECT body FROM " + t).rows[0][0].str() == "held");
    }
    {
        // A collection's documents by MongoDB's requests (the O message, protocol 9).
        Connection db(config());
        auto t = table();
        db.query("CREATE COLLECTION " + t);
        db.query("CREATE TABLE " + t + "_audit (n INT)");
        auto p = db.collection(t);
        CHECK(p.insert_one(R"({"_id": 1, "name": "ann", "age": 34})").as<Json>().text == "1");
        auto ids = p.insert_many({R"({"_id": 2, "name": "bob", "age": 25})", R"({"_id": 3, "name": "cy", "age": 40})"});
        CHECK(ids.size() == 2 && ids[1].as<Json>().text == "3");
        FindOptions o;
        o.projection = R"({"name": 1, "_id": 0})";
        o.sort = R"({"age": -1})";
        o.skip = 1;
        o.limit = 1;
        auto found = p.find(R"({"age": {"$gte": 25}})", o);
        CHECK(found.size() == 1 && found[0].text == R"({"name": "ann"})");
        CHECK(p.count_documents(R"({"age": {"$gt": 30}})") == 2);
        auto u = p.update_many("{}", R"({"$max": {"age": 30}})");
        CHECK(u.matched == 3 && u.modified == 1 && u.upserted_id.is_null());
        u = p.update_one(R"({"name": "dan"})", R"({"$set": {"age": 5}})", true);
        CHECK(u.matched == 0 && u.modified == 0 && !u.upserted_id.is_null());
        u = p.replace_one(R"({"_id": 3})", R"({"name": "cyrus"})");
        CHECK(u.matched == 1 && u.modified == 1);
        auto one = p.find_one(R"({"_id": 3})");
        CHECK(one && one->text == R"({"_id": 3, "name": "cyrus"})");
        // An update needs operators; a replacement has none.
        for (int replace = 0; replace < 2; replace++) {
            try {
                if (replace) p.replace_one("{}", R"({"$set": {"a": 1}})");
                else p.update_one("{}", R"({"name": "x"})");
                CHECK(false);
            } catch (const Error& e) {
                CHECK(e.code() == "22023");
            }
        }
        CHECK(p.delete_one(R"({"age": {"$gte": 30}})") == 1);
        CHECK(p.delete_many("{}") == 3);
        // In a transaction with a table: rolled back, then committed.
        for (int commit = 0; commit < 2; commit++) {
            db.query("BEGIN");
            db.query("INSERT INTO " + t + "_audit VALUES (1)");
            p.insert_one(R"({"_id": 9})");
            db.query(commit ? "COMMIT" : "ROLLBACK");
            CHECK(p.count_documents() == static_cast<uint64_t>(commit));
            CHECK(db.query("SELECT count(*) FROM " + t + "_audit").rows[0][0].as<int64_t>() == commit);
        }
        try {
            db.collection("cpp_nosuch").count_documents();
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "42P01");
        }
    }
    {
        // A stream appended, a group's batches processed exactly once with
        // their work, waits (protocol 10).
        Connection db(config());
        auto t = table();
        db.query("CREATE STREAM " + t + " (name TEXT NOT NULL, n INT)");
        db.query("CREATE TABLE " + t + "_done (seq BIGINT PRIMARY KEY, name TEXT)");
        auto s = db.stream_of(t);
        CHECK(s.wait(0, 20) == 0);
        for (int i = 0; i < 5; i++) s.append({"name", "n"}, {Value("job" + std::to_string(i)), Value(i)});
        CHECK(s.wait(0, 20) == 5);
        std::vector<int64_t> done;
        auto work = [&](const Result& entries) {
            for (auto& e : entries.rows) {
                db.query("INSERT INTO " + t + "_done VALUES ($1, $2)", {e[0], e[2]});
                done.push_back(e[0].as<int64_t>());
            }
        };
        CHECK(s.process("workers", 3, 0, work) == 3);
        CHECK(s.process("workers", 3, 0, work) == 2);
        CHECK(s.process("workers", 3, 20, work) == 0);
        CHECK((done == std::vector<int64_t>{1, 2, 3, 4, 5}));
        CHECK(s.position("workers") == 5);
        // The work failing: all rolled back, the exception rethrown.
        s.append({"name"}, {Value("bad")});
        bool threw = false;
        try {
            s.process("workers", 10, 0, [&](const Result&) {
                db.query("INSERT INTO " + t + "_done VALUES (99, 'x')");
                throw std::runtime_error("the work failed");
            });
        } catch (const std::runtime_error&) {
            threw = true;
        }
        CHECK(threw && db.status() == 0 && s.position("workers") == 5);
        CHECK(db.query("SELECT count(*) FROM " + t + "_done").rows[0][0].as<int64_t>() == 5);
        try {
            db.wait_stream("cpp_nosuch", 0, 10);
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "42P01");
        }
    }
    {
        // Tasks added, a priority first, taken under a lease, finished with
        // their work, failed into a retry and a dead letter; process doing
        // each once.
        Connection db(config());
        auto t = table();
        db.query("CREATE QUEUE " + t + " (kind TEXT NOT NULL) WITH (max_attempts = 2, backoff = '1 millisecond')");
        db.query("CREATE TABLE " + t + "_worked (id BIGINT PRIMARY KEY, kind TEXT)");
        auto q = db.queue_of(t);
        int64_t first = q.add({"kind"}, {Value("one")});
        int64_t urgent = q.add({"kind", "priority"}, {Value("urgent"), Value(-1)});
        auto taken = q.take(1, 60000);
        CHECK(taken.size() == 1 && taken[0].id == urgent && taken[0].attempt == 1 &&
              taken[0].values[0].as<std::string>() == "urgent");
        q.finish(taken[0]);
        auto next = q.take(5);
        CHECK(next.size() == 1 && next[0].id == first);
        CHECK(q.fail(next[0], std::string("try later"), 0) == "retry");
        auto again = q.take();
        CHECK(again.size() == 1 && again[0].attempt == 2);
        try {
            q.finish(next[0]);
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "55000");
        }
        CHECK(q.fail(again[0]) == "dead");
        for (int i = 0; i < 6; i++) q.add({"kind"}, {Value(i == 2 ? std::string("bad") : "t" + std::to_string(i))});
        auto work = [&](const Task& task) {
            db.query("INSERT INTO " + t + "_worked VALUES ($1, $2)", {Value(task.id), task.values[0]});
            if (task.values[0].as<std::string>() == "bad") throw std::runtime_error("bad work");
        };
        size_t finished = 0;
        for (int i = 0; i < 2000; i++) {
            if (db.query("SELECT count(*) FROM " + t).rows[0][0].as<int64_t>() == 0) break;
            finished += q.process(4, -1, work);
        }
        CHECK(finished == 5);
        CHECK(db.query("SELECT count(*) FROM " + t + "_worked").rows[0][0].as<int64_t>() == 5);
        auto dead = db.query("SELECT kind, error FROM " + t + "_dead ORDER BY seq").rows;
        CHECK(dead.size() == 2 && dead[0][1].as<std::string>() == "failed on attempt 2" &&
              dead[1][0].as<std::string>() == "bad" && dead[1][1].as<std::string>() == "bad work");
        try {
            db.queue_of("cpp_nosuch").take();
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "42P01");
        }
    }
    {
        // A live query: its first result, a batch per change, its end
        // (DOCS.md §8.47).
        Connection db(config());
        auto t = table();
        db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY, title TEXT, done BOOLEAN)");
        db.query("INSERT INTO " + t + " VALUES (1, 'a', false), (2, 'b', true)");
        auto s = db.subscribe("SELECT id, title FROM " + t + " WHERE done = $1 ORDER BY id", {Value(false)});
        CHECK(s.columns().size() == 2 && s.columns()[0].name == "id");
        CHECK(s.rows().size() == 1 && s.rows()[0][1].as<std::string>() == "a");
        int64_t first = s.at();
        CHECK(s.changes().empty());
        db.query("INSERT INTO " + t + " VALUES (3, 'c', false)");
        db.query("UPDATE " + t + " SET title = 'A' WHERE id = 1");
        auto got = s.changes(1000);
        CHECK(got.size() == 3);
        CHECK(s.rows().size() == 2 && s.at() > first);
        bool a = false, c = false;
        for (const auto& r : s.rows()) {
            a = a || r[1].as<std::string>() == "A";
            c = c || r[1].as<std::string>() == "c";
        }
        CHECK(a && c);
        auto name = s.name();
        s.close();
        try {
            db.query("CHANGES " + name);
            CHECK(false);
        } catch (const Error& e) {
            CHECK(e.code() == "42704");
        }
    }
    if (failures) {
        std::cerr << failures << " check(s) failed\n";
        return 1;
    }
    std::cout << "nucleus.hpp: all checks passed\n";
    return 0;
}
