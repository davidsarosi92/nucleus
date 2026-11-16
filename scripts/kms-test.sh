#!/usr/bin/env bash
# The root key in AWS KMS against an emulator (PLAN.md §M14.30 K3): moto's
# server, a symmetric key with an alias made there, a database encrypted
# under it by `nucleus sql --root-kms aws-kms:alias/…`, opened again by
# the alias and by the ARN; the key disabled — the database refused —
# and enabled again; a database under a file's root moved under the KMS
# key by `nucleus keys … rotate-root --to-root-kms`.
#
#   scripts/kms-test.sh <nucleus binary>
#
# Needs a Python with moto[server] (MOTO_PYTHON, default python3); CI
# installs it.
set -euo pipefail
nucleus=$(cd "$(dirname "${1:?usage: scripts/kms-test.sh <nucleus binary>}")" && pwd)/$(basename "$1")
py=${MOTO_PYTHON:-python3}
work=$(mktemp -d)
pid=
cleanup() { [ -n "$pid" ] && kill "$pid" 2>/dev/null; wait 2>/dev/null || true; rm -rf "$work"; }
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

"$py" -m moto.server -H 127.0.0.1 -p 29100 >"$work/moto.log" 2>&1 &
pid=$!
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
arn=$(kms create alias/nucleus-test) || fail "no key made in moto"
planted=planted-kms-71c04e2d
root=(--root-kms aws-kms:alias/nucleus-test)
read_back() { "$nucleus" sql "$1" -c "SELECT v FROM t" "${@:2}" >>"$work/sql.log" 2>&1; }

echo "== a database encrypted under $arn"
"$nucleus" sql "$work/db" --encrypt-data yes "${root[@]}" >>"$work/sql.log" 2>&1 <<EOF || fail "the database was not made"
CREATE TABLE t (id INT PRIMARY KEY, v TEXT);
INSERT INTO t SELECT g, '$planted ' || g FROM generate_series(1, 2000) g;
EOF
grep -q "^root aws-kms:$arn " "$work/db.keyring" || fail "the keyring's root is not aws-kms:$arn"
[ ! -e "$work/db.root-key" ] || fail "a root key file was made"
if grep -rq "$planted" "$work/db"; then fail "rows in the clear in the files"; fi
read_back "$work/db" "${root[@]}" || fail "not opened by the alias"
read_back "$work/db" --root-kms "aws-kms:$arn" || fail "not opened by the ARN"

echo "== the key disabled, then enabled"
kms disable alias/nucleus-test
if read_back "$work/db" "${root[@]}"; then fail "opened under a disabled key"; fi
tail -1 "$work/sql.log" | grep -q "Disabled" || fail "not refused for the disabled key"
kms enable alias/nucleus-test
read_back "$work/db" "${root[@]}" || fail "not opened once enabled"

echo "== a file's root moved under the KMS key"
"$nucleus" sql "$work/db2" --encrypt-data yes >>"$work/sql.log" 2>&1 <<EOF || fail "the second database was not made"
CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('$planted');
EOF
"$nucleus" keys "$work/db2" rotate-root --to-root-kms aws-kms:alias/nucleus-test >>"$work/sql.log" 2>&1 ||
  fail "rotate-root to KMS failed"
grep -q "^root aws-kms:$arn " "$work/db2.keyring" || fail "the second keyring's root is not the KMS key"
read_back "$work/db2" "${root[@]}" || fail "the second not opened under KMS"
grep -q "$planted" "$work/sql.log" || fail "the rows not read back"
echo "the kms test passed: encrypted under $arn, refused while disabled, a root moved there"
