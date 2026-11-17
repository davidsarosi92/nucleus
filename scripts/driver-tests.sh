#!/usr/bin/env bash
# Starts `nucleus server` (default credentials) on a free port and runs the
# drivers' test suites and the examples against it (PLAN.md §M14.8).
#
#   scripts/driver-tests.sh <path-to-nucleus-binary> [rust|python|js|java|c|cpp|python-embedded|js-embedded|js-wasm|codegen ...]
#
# Without a list it runs every language whose toolchain is installed.
# python-embedded and js-embedded (PLAN.md §M14.23) run the Python and
# JavaScript suites again on a database in their own process (the
# embedded edition, built from this checkout), and the `anywhere`
# examples both there and against the server, changing only the
# connection string.  js-wasm runs the JavaScript suite on the edition in
# WebAssembly (the browser's), in Node, on files and in memory, then the
# browser example in headless Chrome (when there is one) across reloads,
# and the playground with its tour.  codegen checks `nucleus codegen`'s
# types in TypeScript, Python, Java and Rust against a schema of every type.
set -euo pipefail
bin=$(cd "$(dirname "${1:?usage: scripts/driver-tests.sh <nucleus binary> [langs]}")" && pwd)/$(basename "$1")
shift
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
# With openssl at hand, a CA and a server certificate (for 127.0.0.1 and
# localhost) of its own, so the drivers' TLS tests run (NUCLEUS_TLS_CA).
tls=()
if command -v openssl >/dev/null; then
  (
    cd "$work"
    printf "[req]\ndistinguished_name=dn\nx509_extensions=ca\nprompt=no\n[dn]\nCN=nucleus test CA\n[ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n" >ca.cnf
    openssl req -x509 -config ca.cnf -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 1
    openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj "/CN=localhost"
    printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid\n" >ext.cnf
    openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out server.pem -days 1 -extfile ext.cnf
    # A client certificate for the user nucleus (mutual TLS).
    openssl req -newkey rsa:2048 -nodes -keyout client.key -out client.csr -subj "/CN=nucleus"
    printf "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=clientAuth\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid\n" >client.cnf
    openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out client.pem -days 1 -extfile client.cnf
  ) >/dev/null 2>&1
  tls=(--tls-cert "$work/server.pem" --tls-key "$work/server.key" --tls-client-ca "$work/ca.pem")
  export NUCLEUS_TLS_CA="$work/ca.pem" NUCLEUS_TLS_CERT="$work/client.pem" NUCLEUS_TLS_KEY="$work/client.key"
fi
# A directory (scripts/fake-ldap.py) for drivers_ldap's password, which
# needs TLS.
ldap=()
if [ ${#tls[@]} -gt 0 ]; then
  python3 "$root/scripts/fake-ldap.py" >"$work/ldap.port" &
  ldap_pid=$!
  for _ in $(seq 50); do [ -s "$work/ldap.port" ] && break; sleep 0.1; done
  ldap=(--ldap-url "ldap://127.0.0.1:$(cat "$work/ldap.port")" --ldap-bind-dn "uid={user},ou=people,dc=test")
fi
# The test issuer (scripts/fixtures/oidc): its token logs drivers_oidc in.
oidc=(--oidc-issuer https://issuer.nucleus.test --oidc-audience nucleus-driver-tests
  --oidc-jwks "$root/scripts/fixtures/oidc/jwks.json")
# Its Unix socket too, at a short path (a socket's is bounded: 104 bytes
# on macOS), for the drivers' socket tests.
socket=/tmp/nucleus-drivers-$$.sock
"$bin" server "$work/db" --listen 127.0.0.1:0 --socket "$socket" --history 1h ${tls[@]+"${tls[@]}"} \
  ${ldap[@]+"${ldap[@]}"} "${oidc[@]}" 2>"$work/server.log" &
pid=$!
trap 'kill $pid ${ldap_pid:-} 2>/dev/null; wait $pid ${ldap_pid:-} 2>/dev/null || true; rm -rf "$work"; rm -f "$socket"' EXIT
for _ in $(seq 100); do grep -q "listening on" "$work/server.log" 2>/dev/null && break; sleep 0.1; done
port=$(sed -n 's/.*listening on 127\.0\.0\.1:\([0-9]*\).*/\1/p' "$work/server.log" | head -1)
[ -n "$port" ] || { cat "$work/server.log"; echo "server did not start"; exit 1; }
export NUCLEUS_HOST=127.0.0.1 NUCLEUS_PORT=$port NUCLEUS_SOCKET=$socket
echo "nucleus server on port $port"
"$bin" sql -h "127.0.0.1:$port" -c "CREATE USER drivers_oidc" >/dev/null
if [ ${#ldap[@]} -gt 0 ]; then
  "$bin" sql -h "127.0.0.1:$port" -c "CREATE USER drivers_ldap" >/dev/null
  export NUCLEUS_LDAP_USER=drivers_ldap NUCLEUS_LDAP_PASSWORD=ldap-secret
fi
NUCLEUS_TOKEN=$(cat "$root/scripts/fixtures/oidc/token")
export NUCLEUS_TOKEN
# The encrypted columns' format's test vectors, which every driver checks.
export NUCLEUS_VECTORS="$root/drivers/encrypted-columns.txt"

have() { command -v "$1" >/dev/null; }
langs=("$@")
if [ ${#langs[@]} -eq 0 ]; then
  have cargo && langs+=(rust)
  have python3 && langs+=(python)
  have node && langs+=(js)
  have javac && langs+=(java)
  have cc && langs+=(c)
  have c++ && langs+=(cpp)
  have swift && langs+=(swift)
fi

# The C driver with TLS when OpenSSL is at hand.
ctls=() cflags=() clibs=()
if pkg-config --exists openssl 2>/dev/null; then
  ctls=(TLS=openssl)
  read -r -a cflags <<<"-DNC_WITH_OPENSSL $(pkg-config --cflags openssl)"
  read -r -a clibs <<<"$(pkg-config --libs openssl)"
fi

# A unittest run: its last lines, or, when a test fails, every failure's
# report (CI's log is all there is to go by).
unittest() {
  if "$@" >"$work/unittest.log" 2>&1; then
    tail -3 "$work/unittest.log"
  else
    sed -n '/^======/,$p' "$work/unittest.log"
    return 1
  fi
}

for lang in "${langs[@]}"; do
  echo "== $lang"
  case $lang in
    rust)
      cargo run -q --manifest-path "$root/Cargo.toml" -p nucleus-example ;;
    python)
      unittest python3 "$root/drivers/python/tests/test_nucleus.py" -v
      python3 "$root/examples/python/quickstart.py"
      python3 "$root/examples/python/memory.py"
      python3 "$root/examples/python/structured.py"
      python3 "$root/examples/python/branches.py" ;;
    js)
      node --test "$root/drivers/js/test/nucleus.test.ts"
      node "$root/examples/js/quickstart.ts"
      node "$root/examples/js/memory.ts" ;;
    java)
      out="$work/java"
      javac -Xlint:all -Werror -d "$out" $(find "$root/drivers/java/src" "$root/examples/java" -name '*.java')
      java -cp "$out" io.github.davidsarosi92.nucleus.NucleusTest
      java -cp "$out" Quickstart ;;
    c)
      make -s -C "$root/drivers/c" OUT="$work/c" ${ctls[@]+"${ctls[@]}"} test
      "$work/c/test_nucleus"
      cc -std=c99 -Wall -Wextra -Werror ${cflags[@]+"${cflags[@]}"} -I"$root/drivers/c" -o "$work/c/quickstart" \
        "$root/examples/c/quickstart.c" "$root/drivers/c/nucleus.c" ${clibs[@]+"${clibs[@]}"}
      "$work/c/quickstart" ;;
    cpp)
      mkdir -p "$work/cpp"
      cc -std=c99 -Wall -Wextra -Werror ${cflags[@]+"${cflags[@]}"} -c -o "$work/cpp/nucleus.o" "$root/drivers/c/nucleus.c"
      for src in "$root/drivers/cpp/test/test_nucleus.cpp" "$root/examples/cpp/quickstart.cpp"; do
        exe="$work/cpp/$(basename "$src" .cpp)"
        c++ -std=c++17 -Wall -Wextra -Werror -I"$root/drivers/c" -I"$root/drivers/cpp" \
          -o "$exe" "$src" "$work/cpp/nucleus.o" ${clibs[@]+"${clibs[@]}"}
        "$exe"
      done ;;
    swift)
      # The driver over the C driver; NucleusEmbedded too when its
      # XCFramework is built (drivers/swift/build-xcframework.sh).
      swift test --package-path "$root/drivers/swift" --scratch-path "$work/swift" 2>&1 | grep -E "^Test Suite 'All tests'|Executed|error" | tail -3
      swift run --package-path "$root/examples/swift-cli" --scratch-path "$work/swift-example" quickstart ;;
    python-embedded)
      cargo build -q --release --manifest-path "$root/Cargo.toml" -p nucleus-embedded
      for lib in "$root"/target/release/{libnucleus_embedded.dylib,libnucleus_embedded.so,nucleus_embedded.dll}; do
        [ -f "$lib" ] && export NUCLEUS_EMBEDDED_LIB=$lib && break
      done
      NUCLEUS_URL="file:$work/py-embedded" unittest python3 "$root/drivers/python/tests/test_nucleus.py" -v
      python3 "$root/examples/python/anywhere.py" "file:$work/py-anywhere"
      python3 "$root/examples/python/anywhere.py" ":memory:"
      python3 "$root/examples/python/memory.py" ":memory:"
      python3 "$root/examples/python/structured.py" ":memory:"
      python3 "$root/examples/python/branches.py" "file:$work/py-branches"
      python3 "$root/examples/python/anywhere.py" "nucleus://nucleus:nucleus@127.0.0.1:$port/nucleus" ;;
    js-embedded)
      "$root/drivers/js/native/build.sh"
      NUCLEUS_URL="file:$work/js-embedded" node --test "$root/drivers/js/test/nucleus.test.ts"
      node "$root/examples/js/anywhere.ts" "file:$work/js-anywhere"
      node "$root/examples/js/anywhere.ts" ":memory:"
      node "$root/examples/js/memory.ts" ":memory:"
      node "$root/examples/js/anywhere.ts" "nucleus://nucleus:nucleus@127.0.0.1:$port/nucleus" ;;
    js-wasm)
      "$root/drivers/js/wasm/build.sh"
      (cd "$root/drivers/js" && npm install --no-package-lock --no-audit --no-fund --silent && npx tsc)
      NUCLEUS_POOL="$work/js-wasm-pool" NUCLEUS_URL="wasm:file:suite" node --test "$root/drivers/js/test/nucleus.test.ts"
      NUCLEUS_URL="wasm::memory:" node --test "$root/drivers/js/test/nucleus.test.ts"
      if [ -n "${CHROME:-}" ] || [ -x "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ] || command -v google-chrome >/dev/null; then
        node "$root/drivers/js/test/browser.mjs"
        # The playground (PLAN.md §M14.17 E4, E5): the tour, every step.
        node "$root/playground/test.mjs"
      else
        echo "no Chrome: the browser test is skipped"
      fi ;;
    codegen)
      # nucleus codegen (PLAN.md §M14.16 M5, M6): the types of a schema of
      # every type and the functions of named queries over it, in each
      # language, compiled strictly and reading its rows.
      fx="$root/scripts/fixtures/codegen" cg="$work/codegen"
      mkdir -p "$cg/rust/src"
      "$bin" sql -h "127.0.0.1:$port" <"$fx/schema.sql" >/dev/null
      js="$root/drivers/js/src/index.ts"
      "$bin" codegen ts -h "127.0.0.1:$port" --import "$js" --out "$cg/schema.ts"
      "$bin" codegen ts -h "127.0.0.1:$port" --import "$js" --queries "$fx/queries.sql" --out "$cg/queries.ts"
      sed "s|\"nucleus-db\"|\"$js\"|" "$fx/check.ts" >"$cg/check.ts"
      echo '{"type": "module"}' >"$cg/package.json"
      [ -x "$root/drivers/js/node_modules/.bin/tsc" ] ||
        (cd "$root/drivers/js" && npm install --no-package-lock --no-audit --no-fund --silent)
      printf '{"compilerOptions": {"target": "ES2022", "module": "NodeNext", "moduleResolution": "NodeNext", "strict": true, "noEmit": true, "allowImportingTsExtensions": true, "skipLibCheck": true, "types": ["node"], "typeRoots": ["%s"]}, "files": ["check.ts", "schema.ts", "queries.ts"]}\n' \
        "$root/drivers/js/node_modules/@types" >"$cg/tsconfig.json"
      "$root/drivers/js/node_modules/.bin/tsc" -p "$cg/tsconfig.json"
      node "$cg/check.ts"
      "$bin" codegen python -h "127.0.0.1:$port" --out "$cg/schema.py"
      "$bin" codegen python -h "127.0.0.1:$port" --queries "$fx/queries.sql" --out "$cg/queries.py"
      python3 "$fx/check.py" "$cg"
      "$bin" codegen java -h "127.0.0.1:$port" --out "$cg/Schema.java"
      "$bin" codegen java -h "127.0.0.1:$port" --queries "$fx/queries.sql" --out "$cg/Queries.java"
      javac -Xlint:all -Werror -d "$cg/java" $(find "$root/drivers/java/src/main" -name '*.java') "$cg/Schema.java" "$cg/Queries.java" "$fx/Check.java"
      java -cp "$cg/java" Check
      "$bin" codegen rust -h "127.0.0.1:$port" --out "$cg/rust/src/schema.rs"
      "$bin" codegen rust -h "127.0.0.1:$port" --queries "$fx/queries.sql" --out "$cg/rust/src/queries.rs"
      cp "$fx/check.rs" "$cg/rust/src/main.rs"
      printf '[package]\nname = "codegen-check"\nversion = "0.0.0"\nedition = "2024"\n\n[dependencies]\nnucleus-client = { path = "%s" }\n\n[workspace]\n' \
        "$root/crates/client" >"$cg/rust/Cargo.toml"
      CARGO_TARGET_DIR="$root/target/codegen-check" cargo clippy -q --manifest-path "$cg/rust/Cargo.toml" -- -D warnings
      CARGO_TARGET_DIR="$root/target/codegen-check" cargo run -q --manifest-path "$cg/rust/Cargo.toml"
      # The sample project: migrations, queries, the code made of them.
      "$root/examples/migrations/run.sh" "$bin" ;;
    *) echo "unknown language $lang"; exit 1 ;;
  esac
done
echo "driver tests passed: ${langs[*]}"
