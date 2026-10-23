# nucleus-ldap

LDAP for [nucleus](https://github.com/davidsarosi92/nucleus): the passwords
of roles that have none of their own are checked by a directory (nucleus
PLAN.md §M14.25).  A module, with its own small LDAPv3 client (RFC 4511:
bind, search, StartTLS), so nucleus's core carries no LDAP library.

An **`Ldap`** is a `nucleus_server::Directory`.  For a role that may log in
and has no password in the catalog (`CREATE USER alice`), the server asks
the client for the password itself — over TLS only (`PLAIN` on `ncp`, the
cleartext method on the PostgreSQL protocol; without TLS the login is
refused before any password is sent) — and the directory checks it with a
simple bind:

- `Ldap::bind(url, "uid={user},ou=people,dc=example,dc=com")` — as the DN
  the template makes of the user name, escaped as RFC 4514 says;
- `Ldap::search(url, "dc=example,dc=com", "uid")` — find the one entry
  whose attribute is the user name (anonymously, or
  `with_search_account(dn, password)`), then bind as it; none or more than
  one: no login.  The name is an equality match's value, never filter
  text.

An empty password is never sent (a bind with one is anonymous, and
succeeds).  `ldaps://` speaks TLS from the start; `starttls()` upgrades an
`ldap://` connection; the directory's certificate is checked against
Mozilla's roots or `with_ca_file`'s.  Roles with a password of their own
keep SCRAM.

The nucleus binary includes it: `nucleus server <dir> --ldap-url <url>
(--ldap-bind-dn <template> | --ldap-search-base <dn>
[--ldap-search-attribute <attr>] [--ldap-search-dn <dn>
--ldap-search-password <pw>]) [--ldap-starttls] [--ldap-ca <pem>]`, with
TLS (`--tls-cert`, `--tls-key`).

## Built on the public API only

This folder is a module in nucleus's repository, built to leave it: it
depends on nucleus's public API crates (`nucleus-server`) by version,
reads no files outside this folder, and CI checks both — the boundary
check and the detachability test.

## Tests

`tests/ldap.rs` — a small directory in the test: binds, wrong and empty
passwords, names that try to change the DN or the search, search with and
without an account, ambiguous entries; and logins through the server, the
password only over TLS.  Also checked by hand against OpenLDAP 2.6 (bind,
search as an account, anonymous search) and with `psql` (`sslmode=require`).

## Licence

MIT — see [LICENSE](LICENSE).
