# Changelog

## Unreleased

- Stop requests (issue #17): `on_stop` calls the server's shutdown on
  `SIGTERM`, `SIGINT` and `SIGHUP` (Ctrl-C, Ctrl-Break and a console
  closing on Windows); a second request ends the process at once.
