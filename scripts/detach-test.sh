#!/usr/bin/env bash
# The detachability test (PLAN.md §M14.19): every module under modules/ is
# packaged as it would be published, unpacked into an empty directory and
# built and tested there from versioned dependencies alone — the core
# crates as packaged too, never the repository's files.
#
#   scripts/detach-test.sh [<module package> ...]   (default: every module)
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD

# The core crates a module may depend on, and theirs.
core=(nucleus-platform nucleus-storage nucleus-txn nucleus-kv nucleus-sql nucleus-server nucleus-testkit)
if [ $# -gt 0 ]; then
    modules=("$@")
else
    modules=()
    for m in modules/*/Cargo.toml; do
        modules+=("$(sed -n 's/^name = "\(.*\)"/\1/p' "$m" | head -1)")
    done
fi

args=()
for p in "${core[@]}" "${modules[@]}"; do args+=(-p "$p"); done
# Packages and verifies (builds each from its packaged form, against the
# others' packages in a temporary registry).
cargo package --allow-dirty "${args[@]}"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for m in "${modules[@]}"; do
    crate=$(ls target/package/"$m"-*.crate)
    dir="$work/$m"
    mkdir -p "$dir/.cargo"
    tar -xzf "$crate" -C "$dir" --strip-components=1
    # The core crates as packaged, standing in for the published ones.
    {
        echo "[patch.crates-io]"
        for p in "${core[@]}"; do
            echo "$p = { path = \"$(ls -d "$root"/target/package/"$p"-*/ | head -1)\" }"
        done
    } > "$dir/.cargo/config.toml"
    cp rust-toolchain.toml "$dir/"
    echo "== $m, detached in $dir"
    (cd "$dir" && CARGO_TARGET_DIR="$root/target/detach" cargo test)
done
echo "detachability test passed: ${modules[*]}"
