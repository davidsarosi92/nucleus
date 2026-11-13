// Runs a wasm32-wasip1 program (a test binary) under Node's WASI, the
// way cargo runs a target's binaries: `.cargo/config` or
// CARGO_TARGET_WASM32_WASIP1_RUNNER="node scripts/wasi-run.mjs" (PLAN.md
// §M14.23).  A fresh temporary directory is the program's /tmp (and its
// current directory); the repository is readable at its own path, so a
// test finds its files.
import { readFileSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { WASI } from "node:wasi";

const [program, ...args] = process.argv.slice(2);
const tmp = mkdtempSync(join(tmpdir(), "nucleus-wasi-"));
const root = resolve(new URL("..", import.meta.url).pathname);
const wasi = new WASI({
  version: "preview1",
  args: [program, ...args],
  // One test at a time: no threads in WebAssembly here.
  env: { ...process.env, TMPDIR: "/tmp", RUST_BACKTRACE: "0", RUST_TEST_THREADS: "1" },
  preopens: { "/tmp": tmp, [root]: root, ".": tmp },
  returnOnExit: true,
});
let code = 1;
try {
  const module = await WebAssembly.compile(readFileSync(program));
  const instance = await WebAssembly.instantiate(module, wasi.getImportObject());
  code = wasi.start(instance);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
process.exit(code);
