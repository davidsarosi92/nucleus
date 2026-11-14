// The playground (PLAN.md §M14.17 E4): the console's page on the browser
// edition — `connect` opens the engine in WebAssembly in a worker, and a
// backend answers the page as the server's console API would.  The tour
// is the tutorial (§M14.17 E5, docs/tutorial.sql), step by step.
import { connect } from "../drivers/js/dist/browser.js";
import { parseTutorial } from "./tutorial.js";

const app = document.getElementById("app");
const keep = new URLSearchParams(location.search).has("keep");

// PostgreSQL's command tags: the count in place, as the server's console
// shows them.
const tag = (command, count) =>
  command === "INSERT" ? `INSERT 0 ${count}`
    : /^(SELECT|UPDATE|DELETE|MERGE|COPY|FETCH|MOVE)$/.test(command) ? `${command} ${count}`
      : command;

const STATUS = { idle: "idle", transaction: "in transaction", failed: "failed transaction" };

async function main() {
  app.textContent = "Starting the database in this page…";
  const started = performance.now();
  // An hour of history: AS OF and branches of the past work here too.
  const db = await connect(keep ? "file:playground" : ":memory:", { options: "history=1h" });
  const ready = Math.round(performance.now() - started);
  const backend = {
    login: false,
    async query(sql, limit) {
      const t0 = performance.now();
      const { results, error } = await db.queryText(sql);
      const ms = performance.now() - t0;
      return {
        ok: !error,
        results: results.map((r, i) => ({
          command: tag(r.command, r.count),
          columns: r.columns.length ? r.columns.map((c) => ({ name: c.name, type: c.type })) : null,
          rows: r.rows.slice(0, limit),
          truncated: r.rows.length > limit,
          // The page's time is the whole script's: shown on its last.
          ms: i === results.length - 1 ? ms : null,
        })),
        transaction: STATUS[db.status] ?? db.status,
        error: error && { code: error.code, message: error.message, hint: error.hint },
      };
    },
  };
  let tour = null;
  try {
    const r = await fetch("../docs/tutorial.sql");
    if (r.ok) tour = parseTutorial(await r.text());
  } catch {
    /* no tutorial beside it: the console alone */
  }
  window.nucleusConsole.start(backend, {
    title: `playground — a database in this page (${keep ? "kept in this browser" : "in memory"}; started in ${ready} ms)`,
    tour,
  });
  window.playground = { db, ready };
}

main().catch((e) => {
  app.textContent = `The database could not start here: ${e.message}`;
  window.playgroundError = e.message;
});
