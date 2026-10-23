#!/usr/bin/env bash
# Starts `nucleus server` (default credentials) on a free port and runs the
# drivers' test suites and the examples against it (PLAN.md §M14.8).
#
#   scripts/driver-tests.sh <path-to-nucleus-binary> [rust|python|js|java|c|cpp ...]
#
# Without a list it runs every language whose toolchain is installed.
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
"$bin" server "$work/db" --listen 127.0.0.1:0 ${tls[@]+"${tls[@]}"} 2>"$work/server.log" &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null || true; rm -rf "$work"' EXIT
for _ in $(seq 100); do grep -q "listening on" "$work/server.log" 2>/dev/null && break; sleep 0.1; done
port=$(sed -n 's/.*listening on 127\.0\.0\.1:\([0-9]*\).*/\1/p' "$work/server.log" | head -1)
[ -n "$port" ] || { cat "$work/server.log"; echo "server did not start"; exit 1; }
export NUCLEUS_HOST=127.0.0.1 NUCLEUS_PORT=$port
echo "nucleus server on port $port"

have() { command -v "$1" >/dev/null; }
langs=("$@")
if [ ${#langs[@]} -eq 0 ]; then
  have cargo && langs+=(rust)
  have python3 && langs+=(python)
  have node && langs+=(js)
  have javac && langs+=(java)
  have cc && langs+=(c)
  have c++ && langs+=(cpp)
fi

# The C driver with TLS when OpenSSL is at hand.
ctls=() cflags=() clibs=()
if pkg-config --exists openssl 2>/dev/null; then
  ctls=(TLS=openssl)
  read -r -a cflags <<<"-DNC_WITH_OPENSSL $(pkg-config --cflags openssl)"
  read -r -a clibs <<<"$(pkg-config --libs openssl)"
fi

for lang in "${langs[@]}"; do
  echo "== $lang"
  case $lang in
    rust)
      cargo run -q --manifest-path "$root/Cargo.toml" -p nucleus-example ;;
    python)
      python3 "$root/drivers/python/tests/test_nucleus.py" -v 2>&1 | tail -3
      python3 "$root/examples/python/quickstart.py" ;;
    js)
      node --test "$root/drivers/js/test/nucleus.test.ts"
      node "$root/examples/js/quickstart.ts" ;;
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
    *) echo "unknown language $lang"; exit 1 ;;
  esac
done
echo "driver tests passed: ${langs[*]}"
