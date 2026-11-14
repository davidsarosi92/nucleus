// The web console in a real browser (PLAN.md §M14.17 E3): starts
// `nucleus server --console` on a fresh database, opens the page in
// headless Chrome and goes through it as a person would — the login, the
// tables, a query, a plan drawn, the linter's findings, an error, the
// logout — checking what the page shows.
//
//   cargo build -p nucleus-cli
//   node modules/console/test/browser.mjs [--nucleus target/debug/nucleus] [--shots <dir>]
//
// --shots saves a screenshot of each step (light and dark).  CHROME names
// the browser (default: Chrome's usual place on macOS, else google-chrome
// on the PATH).  No dependencies: the DevTools protocol over Node's
// WebSocket.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const arg = (name, d) => {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : d;
};
const root = resolve(new URL("../../..", import.meta.url).pathname);
const nucleus = resolve(arg("--nucleus", join(root, "target/debug/nucleus")));
const shots = arg("--shots", null);
if (shots) mkdirSync(shots, { recursive: true });

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
  await send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 860, deviceScaleFactor: 1, mobile: false });
  return { chrome, send, errors, close: () => { ws.close(); chrome.kill(); } };
}

/** The value of `expression` in the page. */
async function evaluate(s, expression) {
  const r = await s.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.result?.exceptionDetails) throw new Error(r.result.exceptionDetails.exception?.description ?? expression);
  return r.result?.result?.value;
}

/** Waits until `expression` is true in the page. */
async function until(s, expression, what) {
  for (let i = 0; i < 200; i++) {
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

/** Puts `sql` in the editor and presses the button named `button`. */
async function press(s, sql, button) {
  await evaluate(s, `(() => {
    const e = document.querySelector(".editor textarea");
    e.value = ${JSON.stringify(sql)};
    e.setSelectionRange(0, 0);
    [...document.querySelectorAll(".toolbar button")].find((b) => b.textContent === ${JSON.stringify(button)}).click();
  })()`);
  await until(s, `!document.querySelector(".results").textContent.startsWith("Running")`, `${button} to finish`);
}

async function main() {
  const work = mkdtempSync(join(tmpdir(), "nucleus-console-"));
  const db = join(work, "db");
  const setup = spawnSync(nucleus, ["sql", db], {
    input: `CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL, country text);
CREATE TABLE orders (id int PRIMARY KEY, customer_id int REFERENCES customers(id), total numeric(10,2));
INSERT INTO customers SELECT i, 'customer ' || i, CASE i % 3 WHEN 0 THEN 'HU' WHEN 1 THEN 'DE' ELSE NULL END FROM generate_series(1, 3000) i;
INSERT INTO orders SELECT i, i % 3000 + 1, (i % 997) / 10.0 FROM generate_series(1, 20000) i;
ANALYZE;
`,
  });
  if (setup.status !== 0) throw new Error(`setup: ${setup.stderr}`);
  const server = spawn(nucleus, ["server", db, "--listen", "127.0.0.1:0", "--console", "127.0.0.1:0"], { stdio: ["ignore", "ignore", "pipe"] });
  const url = await new Promise((res, rej) => {
    let text = "";
    server.stderr.on("data", (d) => {
      text += d;
      const m = text.match(/(http:\/\/[\d.:]+\/) \(the console\)/);
      if (m) res(m[1]);
    });
    server.on("exit", (code) => rej(new Error(`the server exited (${code}): ${text}`)));
  });
  const profile = mkdtempSync(join(tmpdir(), "nucleus-chrome-"));
  const s = await launch(profile);
  try {
    await s.send("Page.navigate", { url });
    await until(s, `document.querySelector("form.login")`, "the login");
    await shot(s, "1-login");

    // A wrong password, then the right one.
    const login = (password) => evaluate(s, `(() => {
      document.querySelector("input[name=user]").value = "nucleus";
      document.querySelector("input[name=password]").value = ${JSON.stringify(password)};
      document.querySelector("form.login").requestSubmit();
    })()`);
    await login("wrong");
    await until(s, `document.querySelector("form.login .error")`, "the refusal");
    check((await evaluate(s, `document.querySelector("form.login .error").textContent`)).includes("28P01"), "a wrong password is refused (28P01)");
    await login("nucleus");
    await until(s, `document.querySelector("header .brand")`, "the console");
    await until(s, `document.querySelectorAll("aside .table").length === 2`, "the tables");
    check(await evaluate(s, `document.querySelector("header .who").textContent === "nucleus @ nucleus"`), "the header names the user and the database");
    check(await evaluate(s, `[...document.querySelectorAll("aside .table .name")].map((e) => e.textContent).join(",").startsWith("▸customers")`), "the tables are listed");

    // A table opened: its columns; its rows.
    await evaluate(s, `document.querySelector("aside .table .name").click()`);
    check(await evaluate(s, `document.querySelectorAll("aside .col").length === 3`), "a table shows its columns");
    await evaluate(s, `[...document.querySelectorAll("aside .actions button")].find((b) => b.textContent === "Rows").click()`);
    await until(s, `document.querySelector(".grid table")`, "the rows");
    check(await evaluate(s, `document.querySelectorAll(".grid tbody tr").length === 100`), "Rows shows a hundred rows");
    check(await evaluate(s, `document.querySelector(".grid td.null")?.textContent === "NULL"`), "NULL is shown as such");
    await shot(s, "2-rows");

    // A query of several statements: each its result.
    await press(s, "SELECT country, count(*) AS n FROM customers GROUP BY country ORDER BY n DESC; SELECT sum(total) FROM orders", "Run");
    check(await evaluate(s, `document.querySelectorAll(".result").length === 2`), "two statements, two results");
    check(await evaluate(s, `document.querySelector(".grid td.num")?.textContent === "1000"`), "numbers are right-aligned");
    await shot(s, "3-query");

    // A plan, drawn, with each node's own time.
    await press(s, "SELECT c.country, count(*), sum(o.total) FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.name LIKE 'customer 1%' GROUP BY c.country ORDER BY 2 DESC", "Explain analyze");
    await until(s, `document.querySelector(".plan .node")`, "the plan");
    check(await evaluate(s, `document.querySelectorAll(".plan .node").length >= 5`), "the plan's nodes are drawn");
    check(await evaluate(s, `[...document.querySelectorAll(".plan .op")].some((e) => e.textContent === "Hash Join")`), "the join is a node");
    check(await evaluate(s, `document.querySelector(".plan .lines")?.textContent.startsWith("Execution:")`), "the run's total is shown");
    await shot(s, "4-plan");

    // The linter's findings, a fix put in the editor.
    await press(s, "SELECT * FROM orders WHERE total = 5", "Lint");
    await until(s, `document.querySelector(".finding")`, "the findings");
    check(await evaluate(s, `[...document.querySelectorAll(".finding .rule")].map((e) => e.textContent).includes("full_scan")`), "a full scan is found");
    await evaluate(s, `document.querySelector(".finding .fix button").click()`);
    check(await evaluate(s, `document.querySelector(".editor textarea").value.startsWith("CREATE INDEX ON orders")`), "the fix goes into the editor");
    await shot(s, "5-lint");

    // An error, its code and hint.
    await press(s, "SELECT nmae FROM customers", "Run");
    check(await evaluate(s, `document.querySelector(".results .error")?.textContent.includes("42703")`), "an error shows its code");
    check(await evaluate(s, `document.querySelector(".results .error .hint")?.textContent.includes("name")`), "and its hint");
    await shot(s, "6-error");

    // A transaction left open shows in the header.
    await press(s, "BEGIN", "Run");
    check(await evaluate(s, `document.querySelector("header .status").textContent === "in transaction"`), "an open transaction shows");
    await press(s, "ROLLBACK", "Run");

    // A table made appears in the list.
    await press(s, "CREATE TABLE notes (id int PRIMARY KEY, body text)", "Run");
    await until(s, `document.querySelectorAll("aside .table").length === 3`, "the new table in the list");

    // Logged out: back to the login, the session gone.
    await evaluate(s, `[...document.querySelectorAll("header button")].find((b) => b.textContent === "Log out").click()`);
    await until(s, `document.querySelector("form.login")`, "the login after logging out");
    check(await evaluate(s, `fetch("/api/me").then((r) => r.status)`) === 401, "the session is gone");
    // (The refusals asked for above are 401s, which the browser logs.)
    const errors = s.errors.filter((e) => !e.includes("status of 401"));
    check(errors.length === 0, `no errors in the page${errors.length ? ": " + errors.join("; ") : ""}`);
  } finally {
    s.close();
    server.kill();
    await sleep(200);
    rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
    rmSync(work, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
}

await main();
process.exit(failed ? 1 : 0);
