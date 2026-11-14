# nucleus-console

The web console of [nucleus](https://github.com/davidsarosi92/nucleus)
(PLAN.md §M14.17 E3): a page the server serves to browse the tables, run
SQL, see plans drawn and the linter's findings.

```sh
nucleus server ./mydb --console            # http://127.0.0.1:20036/
```

- **A front end** (`nucleus_server::FrontEnd`, named `console`): HTTP/1.1
  over the standard library, TLS when the server has a certificate (the
  edition refuses a console beyond loopback without one, unless
  `--insecure`).
- **The login is the server's**: the user and password are checked as the
  other front ends check them — the authenticator, the directory, the
  throttle, refusals audited — and the session is a database session
  like any client's, under a random token in an `HttpOnly`,
  `SameSite=Strict` cookie, dropped after 30 minutes unused.
- **Its API** (JSON): `GET /api/me`, `POST /api/login` (`user`,
  `password`, `database` — a branch's name), `POST /api/logout`,
  `POST /api/query` (`sql`, `limit`: each statement's columns, rows as
  text, command tag and time; the first error with its code and hint;
  the transaction's state).  A `POST` needs the header
  `X-Nucleus-Console: 1` and, when the browser names one, the page's own
  origin.
- **The page** (`src/web/`, compiled in; no dependencies): the tables and
  their columns, an editor (Ctrl/⌘+Enter), results, `EXPLAIN` and
  `EXPLAIN ANALYZE` drawn as a tree with each node's own time and the
  estimates far from what happened marked, `ANALYZE QUERY`'s findings
  with their fixes, a history.  `window.nucleusConsole.start(backend)`
  runs it on another backend — the playground's engine in the page.

`node test/browser.mjs` goes through the page in headless Chrome against a
real server.
