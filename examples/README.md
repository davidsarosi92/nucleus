# Examples

The same small program in every language nucleus has a driver for: connect
(the defaults: `127.0.0.1:20035`, user and password `nucleus`), create a
table, insert with parameters (text, exact decimals, dates, arrays), run a
transaction, query, and catch an error by its SQLSTATE.

Start a server first:

```sh
cargo build --release -p nucleus-cli
target/release/nucleus server ./mydb
```

| Language | Run (from the repository root) |
|----------|--------------------------------|
| Rust | `cargo run -p nucleus-example` |
| Python | `python3 examples/python/quickstart.py` |
| JavaScript / TypeScript | `node examples/js/quickstart.ts` (Node 22.6+) |
| Java | `javac -d out $(find drivers/java/src/main examples/java -name '*.java') && java -cp out Quickstart` |
| C | `cc -std=c99 -Idrivers/c examples/c/quickstart.c drivers/c/nucleus.c -o quickstart && ./quickstart` |
| C++ | `cc -std=c99 -c drivers/c/nucleus.c -o nucleus.o && c++ -std=c++17 -Idrivers/c -Idrivers/cpp examples/cpp/quickstart.cpp nucleus.o -o quickstart && ./quickstart` |

`NUCLEUS_PORT` points them at another port.  CI runs all of them
(`scripts/driver-tests.sh`).

**A module:** [`extension/`](extension) is a sample nucleus module — a
type, a function and a catalog view added through the extension API — and
the guide for writing one (`cargo run -p nucleus-example-extension --bin
geo-demo -- ./geodb`).
