// The nucleus console (PLAN.md §M14.17 E3, E4): one page for a server
// (its HTTP API, below) and for the playground (the engine in the page,
// which hands `start` a backend of its own).  No dependencies; every value
// shown goes in as text, never as markup.
"use strict";
(function () {
  // An element: attributes (`class`, `on…` listeners) and children.
  function h(tag, attrs, ...kids) {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === undefined || v === null || v === false) continue;
      if (k === "class") e.className = v;
      else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
      else if (k === "value") e.value = v;
      else e.setAttribute(k, v === true ? "" : v);
    }
    for (const k of kids.flat(Infinity)) {
      if (k === undefined || k === null || k === false) continue;
      e.append(k instanceof Node ? k : String(k));
    }
    return e;
  }

  const store = {
    get(k, d) {
      try {
        const v = localStorage.getItem("nucleus.console." + k);
        return v === null ? d : JSON.parse(v);
      } catch {
        return d;
      }
    },
    set(k, v) {
      try {
        localStorage.setItem("nucleus.console." + k, JSON.stringify(v));
      } catch {
        /* private windows, full storage: kept for this page only */
      }
    },
  };

  // The server's console API.  Every backend answers the same shapes:
  // me/login → {ok, user, database}; query → {results: [{command,
  // columns: [{name, type}] | null, rows, truncated, ms}], transaction,
  // error?: {code, message, hint}}.
  const server = {
    login: true,
    async call(path, body) {
      const post = body !== undefined;
      let r;
      try {
        r = await fetch(path, {
          method: post ? "POST" : "GET",
          headers: post ? { "Content-Type": "application/json", "X-Nucleus-Console": "1" } : {},
          body: post ? JSON.stringify(body) : undefined,
          credentials: "same-origin",
        });
      } catch (e) {
        return { ok: false, error: { code: "08006", message: "the server cannot be reached: " + e.message } };
      }
      let j;
      try {
        j = await r.json();
      } catch {
        j = { error: { code: String(r.status), message: r.statusText || "an answer that is not JSON" } };
      }
      return Object.assign({ ok: r.ok && !j.error }, j);
    },
    me() { return this.call("/api/me"); },
    signIn(user, password, database) { return this.call("/api/login", { user, password, database }); },
    signOut() { return this.call("/api/logout", {}); },
    query(sql, limit) { return this.call("/api/query", { sql, limit }); },
  };

  const NUMERIC = /^(smallint|integer|bigint|real|double precision|numeric|decimal)/;
  let app, backend, options, me;

  function start(b, opts) {
    backend = b;
    options = opts || {};
    app = document.getElementById("app");
    app.classList.remove("loading");
    if (!backend.login) return main({ user: options.user || "", database: options.database || "" });
    backend.me().then((r) => (r.ok ? main(r) : login()));
  }

  // ------------------------------------------------------------- login

  function login(error) {
    const user = h("input", { name: "user", autocomplete: "username", value: store.get("user", "nucleus") });
    const password = h("input", { name: "password", type: "password", autocomplete: "current-password" });
    const database = h("input", { name: "database", placeholder: "nucleus (or a branch's name)",
      value: store.get("database", "") });
    const button = h("button", { class: "btn primary", type: "submit" }, "Log in");
    const form = h("form", { class: "login", onsubmit: async (ev) => {
      ev.preventDefault();
      button.disabled = true;
      const r = await backend.signIn(user.value, password.value, database.value.trim());
      button.disabled = false;
      if (!r.ok) return login(r.error);
      store.set("user", user.value);
      store.set("database", database.value.trim());
      main(r);
    } },
      h("h1", {}, "nucleus"),
      h("p", {}, "Log in to the database."),
      h("label", { for: "user" }, "User"), user,
      h("label", { for: "password" }, "Password"), password,
      h("label", { for: "database" }, "Database"), database,
      error && h("div", { class: "error" }, error.message, h("span", { class: "code" }, error.code)),
      button);
    app.replaceChildren(form);
    (user.value ? password : user).focus();
  }

  // -------------------------------------------------------------- main

  let editor, results, status, tables, filter, history, tourBox;

  function main(who) {
    me = who;
    status = h("span", { class: "status" }, "idle");
    const header = h("header", {},
      h("span", { class: "brand" }, "nucleus"),
      h("span", { class: "who" }, [who.user, who.database].filter(Boolean).join(" @ ") || options.title || ""),
      status,
      h("span", { class: "spacer" }),
      backend.login && h("button", { class: "btn small", onclick: async () => { await backend.signOut(); login(); } },
        "Log out"));
    filter = h("input", { placeholder: "Filter tables", oninput: drawTables });
    tables = h("div", {});
    const aside = h("aside", {}, filter, tables);

    editor = h("textarea", { spellcheck: "false", "aria-label": "SQL",
      value: store.get("sql", "SELECT 'hello, nucleus' AS greeting;") });
    editor.addEventListener("keydown", (ev) => {
      if ((ev.metaKey || ev.ctrlKey) && ev.key === "Enter") {
        ev.preventDefault();
        run(ev.shiftKey ? "analyze" : "run");
      } else if (ev.key === "Tab" && !ev.shiftKey) {
        ev.preventDefault();
        editor.setRangeText("  ", editor.selectionStart, editor.selectionEnd, "end");
      }
    });
    editor.addEventListener("input", () => store.set("sql", editor.value));
    history = h("select", { "aria-label": "History", onchange: () => {
      if (history.value) {
        editor.value = history.value;
        store.set("sql", editor.value);
      }
      history.selectedIndex = 0;
    } });
    drawHistory();
    const mac = /Mac|iP/.test(navigator.platform);
    const toolbar = h("div", { class: "toolbar" },
      h("button", { class: "btn primary", onclick: () => run("run") }, "Run"),
      h("button", { class: "btn", onclick: () => run("explain") }, "Explain"),
      h("button", { class: "btn", onclick: () => run("analyze") }, "Explain analyze"),
      h("button", { class: "btn", onclick: () => run("lint") }, "Lint"),
      history,
      h("span", { class: "hint" }, (mac ? "⌘" : "Ctrl") + "+Enter runs the selection, or all; with Shift, explain analyze"));
    tourBox = h("div", {});
    results = h("div", { class: "results" });
    const work = h("section", { class: "work" }, tourBox, h("div", { class: "editor" }, editor, toolbar), results);
    app.replaceChildren(header, h("main", {}, aside, work));
    if (options.tour) tour(store.get("step", 0));
    loadTables();
    editor.focus();
  }

  function drawHistory() {
    const items = store.get("history", []);
    history.replaceChildren(h("option", { value: "" }, "History"),
      items.map((sql) => h("option", { value: sql }, sql.replace(/\s+/g, " ").slice(0, 80))));
  }

  // ------------------------------------------------------------ tables

  let known = [];

  async function loadTables() {
    const r = await backend.query(
      "SELECT name, analyzed_rows FROM nc_catalog.tables ORDER BY name;" +
      "SELECT table_name, name, type, in_primary_key FROM nc_catalog.columns ORDER BY table_name, position", 10000);
    if (!r.ok || r.results.length < 2) {
      known = [];
      tables.replaceChildren(h("div", { class: "empty" }, r.error ? r.error.message : "No tables."));
      return;
    }
    const cols = {};
    for (const [t, name, type, key] of r.results[1].rows) (cols[t] = cols[t] || []).push({ name, type, key: key === "t" });
    known = r.results[0].rows.map(([name, rows]) => ({ name, rows, cols: cols[name] || [] }));
    drawTables();
  }

  const open = new Set();

  function drawTables() {
    const f = filter.value.trim().toLowerCase();
    const shown = known.filter((t) => !f || t.name.toLowerCase().includes(f));
    if (!shown.length) {
      tables.replaceChildren(h("div", { class: "empty" }, known.length ? "No table matches." : "No tables yet."));
      return;
    }
    tables.replaceChildren(...shown.map((t) => {
      const isOpen = open.has(t.name);
      const quoted = /^[a-z_][a-z0-9_]*$/.test(t.name) ? t.name : '"' + t.name.replace(/"/g, '""') + '"';
      return h("div", { class: "table" },
        h("div", { class: "name", onclick: () => { isOpen ? open.delete(t.name) : open.add(t.name); drawTables(); } },
          h("span", {}, isOpen ? "▾" : "▸"), h("span", {}, t.name),
          t.rows !== null && h("span", { class: "count", title: "rows when last analyzed" }, Number(t.rows).toLocaleString())),
        isOpen && h("div", { class: "cols" },
          t.cols.map((c) => h("div", { class: "col" + (c.key ? " key" : "") },
            h("span", { class: "cname" }, c.name), h("span", { class: "type" }, c.type))),
          h("div", { class: "actions" },
            h("button", { class: "btn small", onclick: () => put(`SELECT * FROM ${quoted} LIMIT 100;`, true) }, "Rows"),
            h("button", { class: "btn small", onclick: () => put(`SELECT count(*) FROM ${quoted};`, true) }, "Count"))));
    }));
  }

  function put(sql, go) {
    editor.value = sql;
    store.set("sql", sql);
    editor.focus();
    if (go) run("run");
  }

  // --------------------------------------------------------------- run

  let running = false;

  async function run(mode) {
    if (running) return;
    const selected = editor.value.slice(editor.selectionStart, editor.selectionEnd);
    let sql = (selected.trim() ? selected : editor.value).trim();
    if (!sql) return;
    const one = sql.replace(/;\s*$/, "");
    if (mode === "explain") sql = "EXPLAIN " + one;
    else if (mode === "analyze") sql = "EXPLAIN ANALYZE " + one;
    else if (mode === "lint") sql = "ANALYZE QUERY " + one;
    else remember(sql);
    running = true;
    results.replaceChildren(h("div", { class: "empty" }, "Running…"));
    const r = await backend.query(sql, 1000);
    running = false;
    show(r, mode);
    if (r.transaction) {
      status.textContent = r.transaction;
      status.className = "status" + (r.transaction === "in transaction" ? " in" : r.transaction === "failed transaction" ? " failed" : "");
    }
    // A statement may have made, dropped or changed a table.
    if (mode === "run" && /\b(create|drop|alter|import|restore)\b/i.test(sql)) loadTables();
  }

  function remember(sql) {
    const items = store.get("history", []).filter((s) => s !== sql);
    items.unshift(sql);
    store.set("history", items.slice(0, 50));
    drawHistory();
  }

  function show(r, mode) {
    const out = [];
    for (const res of r.results || []) {
      const names = (res.columns || []).map((c) => c.name);
      if (names.length === 1 && names[0] === "QUERY PLAN") out.push(planView(res));
      else if (mode === "lint" && names.includes("rule") && names.includes("fix")) out.push(findingsView(res));
      else out.push(gridView(res));
    }
    if (r.error) {
      out.push(h("div", { class: "error" },
        h("div", {}, r.error.message, h("span", { class: "code" }, r.error.code)),
        r.error.hint && h("div", { class: "hint" }, "Hint: " + r.error.hint)));
    }
    if (!out.length) out.push(h("div", { class: "empty" }, "Nothing to show."));
    results.replaceChildren(...out);
  }

  function meta(res, extra, noCount) {
    return h("div", { class: "meta" },
      h("span", { class: "tag" }, res.command),
      res.columns && !noCount && h("span", {}, res.rows.length.toLocaleString() + (res.rows.length === 1 ? " row" : " rows")),
      h("span", {}, res.ms.toFixed(res.ms < 10 ? 2 : 0) + " ms"),
      res.truncated && h("span", { class: "note" }, "the first " + res.rows.length.toLocaleString() + " rows only"),
      extra);
  }

  function gridView(res) {
    if (!res.columns) return h("div", { class: "result" }, meta(res));
    const numeric = res.columns.map((c) => NUMERIC.test(c.type));
    const table = h("table", {},
      h("thead", {}, h("tr", {}, res.columns.map((c, i) => h("th", { class: numeric[i] ? "num" : undefined }, c.name, h("span", { class: "ty" }, c.type))))),
      h("tbody", {}, res.rows.map((row) => h("tr", {}, row.map((v, i) =>
        v === null ? h("td", { class: "null" }, "NULL")
          : h("td", { class: numeric[i] ? "num" : undefined, title: v.length > 60 ? v : undefined }, v))))));
    return h("div", { class: "result" }, meta(res), h("div", { class: "grid" }, table));
  }

  // -------------------------------------------------------------- plans

  // EXPLAIN's text, read back into its tree: two spaces a level, each
  // node's estimate `(rows=n)` and, with ANALYZE, `(actual rows=n loops=n
  // time=t ms)` or `(never executed)`; the last lines (`Execution: …`,
  // `Models: …`) are about the whole.
  function parsePlan(lines) {
    const roots = [], stack = [], notes = [];
    for (const raw of lines) {
      const indent = raw.length - raw.trimStart().length;
      let text = raw.trim();
      if (!text) continue;
      if (indent === 0 && /^(Execution|Models):/.test(text)) { notes.push(text); continue; }
      const node = { children: [] };
      const act = /\(actual rows=(\d+) loops=(\d+)(?: time=([\d.]+) ms)?\)/.exec(text);
      if (act) {
        node.actual = +act[1];
        node.loops = +act[2];
        node.time = act[3] === undefined ? null : +act[3];
        text = text.replace(act[0], "");
      }
      if (text.includes("(never executed)")) {
        node.never = true;
        text = text.replace("(never executed)", "");
      }
      const est = /\(rows=(\d+)\)/.exec(text);
      if (est) {
        node.est = +est[1];
        text = text.replace(est[0], "");
      }
      text = text.replace(/\s+/g, " ").trim();
      const op = /^[A-Z][A-Za-z]*(?: [A-Z][A-Za-z]*)*/.exec(text);
      node.op = op ? op[0] : text;
      node.detail = text.slice(node.op.length).replace(/^:\s*/, "").trim();
      const depth = Math.floor(indent / 2);
      while (stack.length > depth) stack.pop();
      (stack.length ? stack[stack.length - 1].children : roots).push(node);
      stack.push(node);
    }
    return { roots, notes };
  }

  function planView(res) {
    const { roots, notes } = parsePlan(res.rows.map((r) => r[0] || ""));
    const timed = roots.length && roots[0].time !== undefined && roots[0].time !== null;
    const total = timed ? roots.reduce((s, n) => s + n.time, 0) : 0;
    const draw = (n) => {
      const own = timed && n.time !== null ? Math.max(0, n.time - n.children.reduce((s, c) => s + (c.time || 0), 0)) : 0;
      let off = null;
      if (n.est !== undefined && n.actual !== undefined && !n.never) {
        const actual = n.actual / Math.max(1, n.loops);
        const ratio = Math.max(n.est, actual) / Math.max(1, Math.min(n.est, actual));
        if (ratio >= 10 && Math.max(n.est, actual) >= 100) {
          off = h("span", { class: "off", title: "the estimate is far from what happened: ANALYZE the table" },
            "estimate " + (n.est > actual ? "÷" : "×") + Math.round(ratio));
        }
      }
      const fill = h("div", { class: "fill" });
      if (timed && total > 0) fill.style.width = (100 * own / total).toFixed(1) + "%";
      return h("div", {},
        h("div", { class: "node" + (n.never ? " never" : "") }, fill,
          h("div", { class: "body" },
            h("span", { class: "op" }, n.op),
            n.detail && h("span", { class: "detail" }, n.detail),
            off,
            h("span", { class: "nums" },
              n.est !== undefined && ["est ", h("b", {}, n.est.toLocaleString())],
              n.actual !== undefined && [" · rows ", h("b", {}, n.actual.toLocaleString())],
              n.loops > 1 && [" · loops ", h("b", {}, n.loops.toLocaleString())],
              timed && n.time !== null && [" · ", h("b", {}, own.toFixed(2)), " ms own"],
              n.never && " · never executed"))),
        n.children.length > 0 && h("div", { class: "children" }, n.children.map(draw)));
    };
    return h("div", { class: "result" }, meta(res, timed && h("span", {}, "bars: each node's own time"), true),
      h("div", { class: "plan" }, roots.map(draw), notes.length > 0 && h("div", { class: "lines" }, notes.map((t) => h("div", {}, t)))));
  }

  // ----------------------------------------------------------- findings

  function findingsView(res) {
    const at = Object.fromEntries(res.columns.map((c, i) => [c.name, i]));
    if (!res.rows.length) return h("div", { class: "result" }, meta(res), h("div", { class: "none" }, "No findings."));
    return h("div", { class: "result" }, meta(res),
      h("div", { class: "findings" }, res.rows.map((row) => {
        const fix = row[at.fix];
        const sqlFix = fix && /^(CREATE|ALTER|DROP|ANALYZE)\b/i.test(fix);
        return h("div", { class: "finding" },
          h("div", { class: "rule" }, row[at.rule]),
          h("div", { class: "place" }, row[at.place]),
          h("div", {}, row[at.finding]),
          fix && h("div", { class: "fix" }, "Fix: ", h("code", {}, fix),
            sqlFix && h("button", { class: "btn small", onclick: () => put(fix.replace(/;?$/, ";"), false) }, "Put in the editor")));
      })));
  }

  // --------------------------------------------------------------- tour

  // A guided tour (§M14.17 E5): steps of a title, a text and SQL.
  function tour(i) {
    const steps = options.tour;
    i = Math.min(Math.max(0, i), steps.length - 1);
    store.set("step", i);
    const s = steps[i];
    tourBox.replaceChildren(h("div", { class: "tour" },
      h("h2", {}, s.title),
      s.text.split("\n\n").map((p) => h("p", {}, p)),
      h("div", { class: "steps" },
        h("button", { class: "btn small", disabled: i === 0, onclick: () => tour(i - 1) }, "Back"),
        h("button", { class: "btn small primary", onclick: () => put(s.sql, true) }, "Run this step"),
        h("button", { class: "btn small", disabled: i === steps.length - 1, onclick: () => tour(i + 1) }, "Next"),
        h("span", { class: "of" }, `${i + 1} of ${steps.length}`))));
  }

  window.nucleusConsole = { start, parsePlan, server };
  // A page that brings its own backend (the playground) starts it itself.
  if (!window.nucleusConsoleManual) {
    if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", () => start(server, {}));
    else start(server, {});
  }
})();
