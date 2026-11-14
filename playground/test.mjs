// The playground in a real browser (PLAN.md §M14.17 E4, E5): serves the
// repository, opens playground/index.html in headless Chrome and goes
// through it — the engine started in the page, a query typed, then every
// step of the tour (docs/tutorial.sql) run with its button, each without
// an error, the plans drawn and the linter's findings shown.
//
//   drivers/js/wasm/build.sh && (cd drivers/js && npx tsc)
//   node playground/test.mjs [--shots <dir>] [--root <site>]
//   node playground/test.mjs --serve      # just the server, to look
//
// CHROME names the browser (default: Chrome's usual place on macOS, else
// google-chrome on the PATH).  No dependencies: the DevTools protocol over
// Node's WebSocket.

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { extname, join, normalize, resolve } from "node:path";

const arg = (name, d) => {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : d;
};
// --root: a site playground/build.sh made, served in place of the repository.
const root = resolve(arg("--root", new URL("..", import.meta.url).pathname));
const types = {
  ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm",
  ".css": "text/css", ".sql": "text/plain; charset=utf-8",
};
const shots = arg("--shots", null);
if (shots) mkdirSync(shots, { recursive: true });

const server = createServer((req, res) => {
  const path = normalize(join(root, decodeURIComponent(new URL(req.url, "http://x").pathname)));
  if (!path.startsWith(root) || !existsSync(path) || statSync(path).isDirectory()) {
    res.writeHead(404).end();
    return;
  }
  res.writeHead(200, { "content-type": types[extname(path)] ?? "application/octet-stream" });
  res.end(readFileSync(path));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const page = `http://127.0.0.1:${server.address().port}/playground/index.html`;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = false;
const check = (ok, what) => {
  console.log(`${ok ? "ok" : "not ok"} - ${what}`);
  if (!ok) failed = true;
};

function chromePath() {
  if (process.env.CHROME) return process.env.CHROME;
  const mac = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
  return existsSync(mac) ? mac : "google-chrome";
}

async function launch(profile) {
  const chrome = spawn(chromePath(), [
    "--headless=new", `--user-data-dir=${profile}`, "--remote-debugging-port=0",
    "--no-first-run", "--no-default-browser-check", "--disable-gpu", "about:blank",
  ], { stdio: ["ignore", "ignore", "pipe"] });
  const port = await new Promise((res, rej) => {
    let text = "";
    chrome.stderr.on("data", (d) => {
      text += d;
      const m = text.match(/DevTools listening on ws:\/\/[^:]+:(\d+)\//);
      if (m) res(Number(m[1]));
    });
    chrome.on("exit", (code) => rej(new Error(`chrome exited (${code}): ${text}`)));
  });
  const target = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: "PUT" })).json();
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  let id = 0;
  const waiting = new Map();
  const errors = [];
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && waiting.has(msg.id)) {
      waiting.get(msg.id)(msg);
      waiting.delete(msg.id);
    } else if (msg.method === "Runtime.exceptionThrown") {
      errors.push(msg.params.exceptionDetails.exception?.description ?? msg.params.exceptionDetails.text);
    } else if (msg.method === "Log.entryAdded" && msg.params.entry.level === "error") {
      errors.push(msg.params.entry.text);
    }
  };
  const send = (method, params = {}) => new Promise((r) => {
    waiting.set(++id, r);
    ws.send(JSON.stringify({ id, method, params }));
  });
  await send("Page.enable");
  await send("Runtime.enable");
  await send("Log.enable");
  await send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
  return { chrome, send, errors, close: () => { ws.close(); chrome.kill(); } };
}

async function evaluate(s, expression) {
  const r = await s.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.result?.exceptionDetails) throw new Error(r.result.exceptionDetails.exception?.description ?? expression);
  return r.result?.result?.value;
}

async function until(s, expression, what, tries = 400) {
  for (let i = 0; i < tries; i++) {
    if (await evaluate(s, `Boolean(${expression})`)) return true;
    await sleep(50);
  }
  check(false, `waited for ${what}`);
  return false;
}

async function shot(s, name) {
  if (!shots) return;
  for (const scheme of ["light", "dark"]) {
    await s.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: scheme }] });
    await sleep(50);
    const r = await s.send("Page.captureScreenshot", { format: "png" });
    writeFileSync(join(shots, `${name}-${scheme}.png`), Buffer.from(r.result.data, "base64"));
  }
  await s.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "light" }] });
}

const button = (scope, text) =>
  `[...document.querySelectorAll(${JSON.stringify(scope + " button")})].find((b) => b.textContent === ${JSON.stringify(text)})`;
const done = `!document.querySelector(".results").textContent.startsWith("Running")`;

async function main() {
  const profile = mkdtempSync(join(tmpdir(), "nucleus-playground-"));
  const s = await launch(profile);
  try {
    await s.send("Page.navigate", { url: page });
    await until(s, `window.playground || window.playgroundError`, "the database in the page");
    const error = await evaluate(s, `window.playgroundError ?? null`);
    check(!error, `the engine starts in the page${error ? `: ${error}` : ""}`);
    if (error) return;
    console.log(`# started in ${await evaluate(s, "window.playground.ready")} ms`);
    check(await evaluate(s, `document.querySelector("header .brand") !== null`), "the console's page is shown");
    check(await evaluate(s, `document.querySelector(".tour h2")?.textContent === "Hello"`), "the tour begins");

    // A query typed and run.
    await evaluate(s, `(() => {
      const e = document.querySelector(".editor textarea");
      e.value = "SELECT 6 * 7 AS answer";
      ${button(".toolbar", "Run")}.click();
    })()`);
    await until(s, done, "the query");
    check(await evaluate(s, `document.querySelector(".grid td")?.textContent === "42"`), "a query typed runs");

    // Every step of the tour, run with its button.
    const steps = await evaluate(s, `Number(document.querySelector(".tour .of").textContent.split(" of ")[1])`);
    check(steps >= 10, `the tour has its steps (${steps})`);
    for (let i = 0; i < steps; i++) {
      const title = await evaluate(s, `document.querySelector(".tour h2").textContent`);
      await evaluate(s, `${button(".tour", "Run this step")}.click()`);
      await sleep(20);
      await until(s, done, `step ${i + 1} to run`);
      const err = await evaluate(s, `document.querySelector(".results .error")?.textContent ?? null`);
      const shown = await evaluate(s, `document.querySelectorAll(".results .result").length`);
      check(!err && shown > 0, `step ${i + 1} (${title}) runs${err ? `: ${err}` : ""}`);
      if (title === "Plans") {
        check(await evaluate(s, `document.querySelectorAll(".plan .node").length >= 3`), "its plan is drawn");
        await shot(s, "playground-plan");
      }
      if (title === "Indexes, found for you") {
        check(await evaluate(s, `[...document.querySelectorAll(".finding .rule")].map((e) => e.textContent).includes("full_scan")`), "the linter's findings are shown");
        await shot(s, "playground-lint");
      }
      if (title === "Time travel") {
        check(await evaluate(s, `[...document.querySelectorAll(".results .result:last-child td")].map((e) => e.textContent).join(",") === "Notebook,0.00,4.20"`), "the past is read beside the present");
      }
      if (i === 0) await shot(s, "playground-start");
      if (i < steps - 1) await evaluate(s, `${button(".tour", "Next")}.click()`);
    }
    await shot(s, "playground-end");
    check(await evaluate(s, `document.querySelectorAll("aside .table").length >= 4`), "the tables the tour made are listed");
    check(s.errors.length === 0, `no errors in the page${s.errors.length ? ": " + s.errors.join("; ") : ""}`);
  } finally {
    s.close();
    await sleep(200);
    rmSync(profile, { recursive: true, force: true });
  }
}

if (process.argv.includes("--serve")) {
  console.log(`open ${page}`);
} else {
  try {
    await main();
  } finally {
    server.close();
  }
  if (failed) process.exit(1);
}
