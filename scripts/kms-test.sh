#!/usr/bin/env bash
# The root key in a key management service, against the real thing or its
# emulator (PLAN.md §M14.30 K3).  For each service: a database encrypted
# under its key by `nucleus sql --root-kms <scheme>:<key>`, its keyring
# naming that root and no row in the clear; the key taken away — the
# database refused — and given back; a database under a file's root moved
# under the key by `nucleus keys … rotate-root --to-root-kms`.
#
#   scripts/kms-test.sh <nucleus binary> [aws] [vault]   (default: both)
#
# aws: AWS KMS on moto's server, a Python with moto[server] (MOTO_PYTHON,
# default python3); the key disabled and enabled.  vault: Vault's Transit
# engine on `vault server -dev` (VAULT, default vault — or OpenBao's
# bao); the token's policy emptied and restored.  CI installs both.
set -euo pipefail
nucleus=$(cd "$(dirname "${1:?usage: scripts/kms-test.sh <nucleus binary> [aws] [vault]}")" && pwd)/$(basename "$1")
shift
services=("$@")
[ ${#services[@]} -gt 0 ] || services=(aws vault)
work=$(mktemp -d)
pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT
fail() {
  echo "kms test: $*" >&2
  for f in "$work"/*.log; do echo "== $f"; tail -20 "$f"; done >&2
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::error title=kms test::$*"
    for f in "$work"/*.log; do
      [ -s "$f" ] || continue
      echo "::error title=$(basename "$f")::$(tail -15 "$f" | cut -c1-300 | sed 's/%/%25/g' | awk '{printf "%s%%0A", $0}')"
    done
  fi
  exit 1
}
planted=planted-kms-71c04e2d
read_back() { "$nucleus" sql "$1" -c "SELECT v FROM t" "${@:2}" >>"$work/sql.log" 2>&1; }

# scenario <name> <root URI> <root id in the keyring> <take away> <give back> <refusal>
scenario() {
  local name=$1 uri=$2 id=$3 take=$4 give=$5 refusal=$6
  local db=$work/$name root=(--root-kms "$2")
  echo "== $name: a database encrypted under $uri"
  "$nucleus" sql "$db" --encrypt-data yes "${root[@]}" >>"$work/sql.log" 2>&1 <<EOF || fail "$name: the database was not made"
CREATE TABLE t (id INT PRIMARY KEY, v TEXT);
INSERT INTO t SELECT g, '$planted ' || g FROM generate_series(1, 2000) g;
EOF
  grep -q "^root $id " "$db.keyring" || fail "$name: the keyring's root is not $id"
  [ ! -e "$db.root-key" ] || fail "$name: a root key file was made"
  if grep -rq "$planted" "$db"; then fail "$name: rows in the clear in the files"; fi
  read_back "$db" "${root[@]}" || fail "$name: not opened again"

  echo "== $name: the key taken away, then given back"
  $take
  if read_back "$db" "${root[@]}"; then fail "$name: opened without the key"; fi
  tail -1 "$work/sql.log" | grep -q "$refusal" || fail "$name: not refused for the key ($refusal)"
  $give
  read_back "$db" "${root[@]}" || fail "$name: not opened once given back"

  echo "== $name: a file's root moved under the key"
  "$nucleus" sql "$db-moved" --encrypt-data yes >>"$work/sql.log" 2>&1 <<EOF || fail "$name: the second database was not made"
CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('$planted');
EOF
  "$nucleus" keys "$db-moved" rotate-root --to-root-kms "$uri" >>"$work/sql.log" 2>&1 ||
    fail "$name: rotate-root to the key failed"
  grep -q "^root $id " "$db-moved.keyring" || fail "$name: the second keyring's root is not $id"
  : >"$work/read.log"
  "$nucleus" sql "$db-moved" -c "SELECT v FROM t" "${root[@]}" >"$work/read.log" 2>&1 ||
    fail "$name: the second not opened under the key"
  grep -q "$planted" "$work/read.log" || fail "$name: the rows not read back"
}

aws() {
  local py=${MOTO_PYTHON:-python3}
  "$py" -m moto.server -H 127.0.0.1 -p 29100 >"$work/moto.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 60); do curl -s -o /dev/null http://127.0.0.1:29100/ && break; sleep 0.5; done
  export AWS_ACCESS_KEY_ID=test-key AWS_SECRET_ACCESS_KEY=test-secret AWS_REGION=eu-central-1
  export AWS_ENDPOINT_URL_KMS=http://127.0.0.1:29100
  kms() { # create <alias> | enable <alias> | disable <alias>, through boto3
    "$py" - "$@" <<'EOF'
import os, sys, boto3
kms = boto3.client("kms", endpoint_url=os.environ["AWS_ENDPOINT_URL_KMS"], region_name=os.environ["AWS_REGION"])
action, alias = sys.argv[1], sys.argv[2]
if action == "create":
    arn = kms.create_key(Description="nucleus kms test")["KeyMetadata"]["Arn"]
    kms.create_alias(AliasName=alias, TargetKeyId=arn)
    print(arn)
else:
    key = kms.describe_key(KeyId=alias)["KeyMetadata"]["KeyId"]
    (kms.enable_key if action == "enable" else kms.disable_key)(KeyId=key)
EOF
  }
  local arn
  arn=$(kms create alias/nucleus-test) || fail "aws: no key made in moto"
  scenario aws aws-kms:alias/nucleus-test "aws-kms:$arn" \
    "kms disable alias/nucleus-test" "kms enable alias/nucleus-test" Disabled
  # The ARN names the same root as the alias.
  read_back "$work/aws" --root-kms "aws-kms:$arn" || fail "aws: not opened by the ARN"
}

vault() {
  local bin=${VAULT:-vault} a=http://127.0.0.1:28200
  "$bin" server -dev -dev-root-token-id=nucleus-root -dev-listen-address=127.0.0.1:28200 \
    >"$work/vault.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 60); do curl -sf -o /dev/null "$a/v1/sys/health" && break; sleep 0.5; done
  admin() { # <method> <path> [json]
    curl -sf -X "$1" -H "X-Vault-Token: nucleus-root" "$a/v1/$2" ${3:+-d "$3"} >>"$work/vault-admin.log" ||
      fail "vault: $1 $2"
  }
  admin POST sys/mounts/transit '{"type":"transit"}'
  admin POST transit/keys/nucleus '{}'
  local allow='path \"transit/keys/nucleus\" { capabilities = [\"read\"] } path \"transit/encrypt/nucleus\" { capabilities = [\"update\"] } path \"transit/decrypt/nucleus\" { capabilities = [\"update\"] }'
  local deny='path \"transit/*\" { capabilities = [\"deny\"] }'
  allow() { admin PUT sys/policies/acl/nucleus "{\"policy\": \"$allow\"}"; }
  deny() { admin PUT sys/policies/acl/nucleus "{\"policy\": \"$deny\"}"; }
  allow
  # A token of its own, with only that policy.
  VAULT_TOKEN=$(curl -sf -X POST -H "X-Vault-Token: nucleus-root" "$a/v1/auth/token/create" \
    -d '{"policies":["nucleus"],"no_default_policy":true}' | sed -n 's/.*"client_token":"\([^"]*\)".*/\1/p')
  [ -n "$VAULT_TOKEN" ] || fail "vault: no token made"
  export VAULT_ADDR=$a VAULT_TOKEN
  scenario vault vault-transit:nucleus vault-transit:transit/nucleus deny allow "permission denied"
}

for s in "${services[@]}"; do
  case $s in
    aws) aws ;;
    vault) vault ;;
    *) fail "no such service: $s (aws, vault)" ;;
  esac
done
echo "the kms test passed: ${services[*]}"
