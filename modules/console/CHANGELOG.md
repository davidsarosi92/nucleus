# Changelog

## Unreleased

- The page takes a backend (`nucleusConsole.start(backend, {title, tour})`)
  and a guided tour, so the playground (PLAN.md §M14.17 E4) runs it on
  the engine in WebAssembly; `ANALYZE QUERY`'s findings are shown as
  findings wherever they come back, not only from Lint.
- The web console: the front end `console` (HTTP, TLS with the server's
  certificate), the server's login, sessions under a cookie, the JSON API,
  the page — tables, SQL, plans drawn, the linter's findings, a history.
