// The browser edition in a real browser (PLAN.md §M14.23): serves the
// repository, opens examples/browser/index.html in headless Chrome, and
// checks that its database persists — across a reload, a reload with the
// database left open, and a restart of the browser on the same profile.
//
//   drivers/js/wasm/build.sh && (cd drivers/js && npx tsc)
//   node drivers/js/test/browser.mjs            # the test
//   node drivers/js/test/browser.mjs --serve    # just the server, to look
//
// CHROME names the browser (default: Chrome's usual place on macOS, else
// google-chrome on the PATH).  No dependencies: the DevTools protocol over
// Node's WebSocket.

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { extname, join, normalize, resolve } from "node:path";

const root = resolve(new URL("../../..", import.meta.url).pathname);
const types = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".css": "text/css" };

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
const page = `http://127.0.0.1:${server.address().port}/examples/browser/index.html`;
if (process.argv.includes("--serve")) {
  console.log(`open ${page}`);
} else {
  try {
    await main();
  } finally {
    server.close();
  }
}

function chromePath() {
  if (process.env.CHROME) return process.env.CHROME;
  const mac = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
  return existsSync(mac) ? mac : "google-chrome";
}

/** Chrome on `profile`, and a DevTools session on a new tab. */
async function launch(profile) {
  const chrome = spawn(chromePath(), [
    "--headless=new", `--user-data-dir=${profile}`, "--remote-debugging-port=0",
    "--no-first-run", "--no-default-browser-check", "--disable-gpu", "about:blank",
  ], { stdio: ["ignore", "ignore", "pipe"] });
  const port = await new Promise((resolve, reject) => {
    let text = "";
    chrome.stderr.on("data", (d) => {
      text += d;
      const m = text.match(/DevTools listening on ws:\/\/[^:]+:(\d+)\//);
      if (m) resolve(Number(m[1]));
    });
    chrome.on("exit", (code) => reject(new Error(`chrome exited (${code}): ${text}`)));
  });
  const target = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: "PUT" })).json();
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  let id = 0;
  const waiting = new Map();
  const events = [];
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && waiting.has(msg.id)) {
      waiting.get(msg.id)(msg);
      waiting.delete(msg.id);
    } else if (msg.method) {
      events.push(msg);
    }
  };
  const send = (method, params = {}) =>
    new Promise((r) => {
      waiting.set(++id, r);
      ws.send(JSON.stringify({ id, method, params }));
    });
  await send("Page.enable");
  await send("Runtime.enable");
  return { chrome, send, events, close: () => { ws.close(); chrome.kill(); } };
}

/** Loads `url` and waits for the page's `done`: the visit number. */
async function visit(s, url) {
  const loaded = s.events.length;
  await s.send("Page.navigate", { url });
  for (let i = 0; i < 200 && !s.events.slice(loaded).some((e) => e.method === "Page.loadEventFired"); i++) {
    await new Promise((r) => setTimeout(r, 50));
  }
  for (let i = 0; i < 200; i++) {
    const r = await s.send("Runtime.evaluate", { expression: "window.done", awaitPromise: true, returnByValue: true });
    if (r.result?.exceptionDetails) {
      const logs = s.events.filter((e) => e.method === "Runtime.consoleAPICalled").map((e) => e.params.args.map((a) => a.value).join(" "));
      throw new Error(`the page failed: ${r.result.exceptionDetails.exception?.description ?? JSON.stringify(r.result.exceptionDetails)}\n${logs.join("\n")}`);
    }
    if (r.result?.result?.type === "number") {
      const ms = await s.send("Runtime.evaluate", { expression: "window.ms", returnByValue: true });
      return [r.result.result.value, ms.result?.result?.value];
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error("the page did not finish");
}

async function main() {
  const profile = mkdtempSync(join(tmpdir(), "nucleus-chrome-"));
  const expect = (got, want, what) => {
    if (got[0] !== want) throw new Error(`${what}: visit ${got[0]}, expected ${want}`);
    console.log(`ok - ${what}: visit ${got[0]} (${got[1]} ms from the page's start)`);
  };
  try {
    let s = await launch(profile);
    expect(await visit(s, page), 1, "a new profile's first visit");
    expect(await visit(s, page), 2, "after a reload");
    expect(await visit(s, `${page}?keep`), 3, "left open");
    expect(await visit(s, page), 4, "after a reload of a page that left it open");
    s.close();
    await new Promise((r) => s.chrome.on("exit", r));
    s = await launch(profile);
    expect(await visit(s, page), 5, "after the browser restarted");
    s.close();
    await new Promise((r) => s.chrome.on("exit", r));
  } finally {
    rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
}
