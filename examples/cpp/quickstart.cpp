// nucleus from C++: connect, create, insert with parameters, query, a
// transaction.  Run a server first (`nucleus server ./mydb`), then:
//
//   cc -std=c99 -c drivers/c/nucleus.c -o nucleus.o
//   c++ -std=c++17 -Idrivers/c -Idrivers/cpp examples/cpp/quickstart.cpp nucleus.o -o quickstart
//   ./quickstart
#include <cstdlib>
#include <iostream>

#include "nucleus.hpp"

int main() {
    nucleus::Config config; // 127.0.0.1:20035, user/password nucleus
    if (const char* port = std::getenv("NUCLEUS_PORT")) config.port = static_cast<uint16_t>(std::atoi(port));
    try {
        nucleus::Connection db(config);
        std::cout << "connected to " << db.server_version() << "\n";
        db.query("DROP TABLE IF EXISTS cpp_orders");
        db.query("CREATE TABLE cpp_orders (id INT PRIMARY KEY, customer TEXT NOT NULL, total NUMERIC(10, 2), "
                 "placed DATE, tags TEXT[])");
        db.query("INSERT INTO cpp_orders VALUES ($1, $2, $3, $4, $5)",
                 {1, "Anna", nucleus::Numeric::parse("19.99"), nucleus::Date::ymd(2024, 5, 1), nucleus::Array{"new"}});

        // A transaction: both statements or neither.
        db.transaction([](nucleus::Connection& tx) {
            tx.query("INSERT INTO cpp_orders VALUES ($1, $2, $3, $4, $5)",
                     {2, "B\xc3\xa9la", nucleus::Numeric::parse("120.00"), nucleus::Date::ymd(2024, 5, 3),
                      nucleus::Array{"vip", "repeat"}});
            tx.query("UPDATE cpp_orders SET total = total * 0.9 WHERE id = 2");
        });

        auto result = db.query("SELECT id, customer, total, placed, tags FROM cpp_orders ORDER BY id");
        for (const auto& row : result.rows) {
            for (size_t i = 0; i < row.size(); i++)
                std::cout << (i ? "  " : "") << result.columns[i].name << "=" << row[i].str();
            std::cout << "\n";
        }

        try {
            db.query("INSERT INTO cpp_orders (id, customer) VALUES (1, 'dup')");
        } catch (const nucleus::Error& e) {
            std::cout << "as expected: " << e.code() << " " << e.what() << "\n";
        }
        db.query("DROP TABLE cpp_orders");
    } catch (const nucleus::Error& e) {
        std::cerr << "error: " << e.what() << "\n";
        return 1;
    }
    return 0;
}
