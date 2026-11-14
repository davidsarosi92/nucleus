#!/usr/bin/env bash
# The playground as a site (PLAN.md §M14.17 E4): copies what its page
# loads into <out>, in the repository's layout, so the page's relative
# paths hold — the console's page and style, the JavaScript driver's
# browser build and the engine in WebAssembly, the tutorial.  Any static
# host serves it (application/wasm for .wasm); open <out>/playground/.
#
#   drivers/js/wasm/build.sh && (cd drivers/js && npx tsc)
#   playground/build.sh <out>
set -euo pipefail
out=${1:?usage: playground/build.sh <out>}
root=$(cd "$(dirname "$0")/.." && pwd)
[ -f "$root/drivers/js/wasm/nucleus.wasm" ] || { echo "no drivers/js/wasm/nucleus.wasm: run drivers/js/wasm/build.sh" >&2; exit 1; }
[ -f "$root/drivers/js/dist/browser.js" ] || { echo "no drivers/js/dist: run npx tsc in drivers/js" >&2; exit 1; }
mkdir -p "$out/playground" "$out/modules/console/src/web" "$out/drivers/js/dist" "$out/drivers/js/wasm" "$out/docs"
cp "$root"/playground/{index.html,playground.js,start.js,tutorial.js} "$out/playground/"
cp "$root"/modules/console/src/web/{app.js,app.css} "$out/modules/console/src/web/"
cp "$root"/drivers/js/dist/*.js "$out/drivers/js/dist/"
cp "$root/drivers/js/wasm/nucleus.wasm" "$out/drivers/js/wasm/"
cp "$root/docs/tutorial.sql" "$out/docs/"
# The site's root opens the playground.
printf '<!doctype html><meta charset="utf-8"><meta http-equiv="refresh" content="0; url=playground/"><a href="playground/">the nucleus playground</a>\n' >"$out/index.html"
echo "the playground: $out/playground/ ($(du -sh "$out" | cut -f1))"
