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
| Swift | `swift run --package-path examples/swift-cli quickstart` |
| C++ | `cc -std=c99 -c drivers/c/nucleus.c -o nucleus.o && c++ -std=c++17 -Idrivers/c -Idrivers/cpp examples/cpp/quickstart.cpp nucleus.o -o quickstart && ./quickstart` |

`NUCLEUS_PORT` points them at another port.  CI runs all of them
(`scripts/driver-tests.sh`).

**Memory for an AI application:** `memory` (Python and JavaScript) is an
assistant's data end to end — a knowledge base it cites (a citation read
back `AS OF COMMIT` after the document changed), a memory per user,
what each reply read (`SET request_id`), and a user erased on request
(`FORGET SUBJECT`); [`docs/memory.md`](../docs/memory.md) explains it.
Against the server (started with `--history 1h` for the citation) or in
the process:

```sh
python3 examples/python/memory.py
node examples/js/memory.ts :memory:
```

**Without a server:** `anywhere` (Python and JavaScript) runs one program
on a database inside the process — in a directory or in memory — or on a
server, the connection string its only difference (PLAN.md §M14.23):

```sh
cargo build --release -p nucleus-embedded && drivers/js/native/build.sh
NUCLEUS_EMBEDDED_LIB=target/release/libnucleus_embedded.dylib python3 examples/python/anywhere.py file:notes.db
python3 examples/python/anywhere.py nucleus://nucleus:nucleus@127.0.0.1:20035/nucleus
node examples/js/anywhere.ts :memory:
```

(`.so` on Linux; the platform wheel and addon package carry the library.)

**In a browser:** [`browser/index.html`](browser/index.html) is a page with
a database in it — the engine in WebAssembly, its files in OPFS — that
counts its visits across reloads:

```sh
drivers/js/wasm/build.sh && (cd drivers/js && npm install && npx tsc)
node drivers/js/test/browser.mjs --serve   # prints the page's address
```

**On phones:** [`ios/Notes.swiftpm`](ios/Notes.swiftpm) (SwiftUI) and
[`android/notes`](android/notes) keep notes in a database inside the app;
each `run.sh` builds the app, runs it on a simulator or emulator twice
and checks that the second launch finds the first one's notes.
[`swift-cli/`](swift-cli) has the quickstart in Swift (`swift run --package-path
examples/swift-cli quickstart`) and `anywhere` (a server or a database in the
process).

```sh
drivers/swift/build-xcframework.sh && examples/ios/run.sh
drivers/android/build-native.sh && examples/android/run.sh   # an emulator running
```

**A module:** [`extension/`](extension) is a sample nucleus module — a
type, a function and a catalog view added through the extension API — and
the guide for writing one (`cargo run -p nucleus-example-extension --bin
geo-demo -- ./geodb`).
