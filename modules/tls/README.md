# nucleus-tls

TLS for [nucleus](https://github.com/davidsarosi92/nucleus), over
[rustls](https://github.com/rustls/rustls): a module, so nucleus's core
carries no TLS library (nucleus PLAN.md §M14.25).

- **`ServerTls`** — a certificate chain and its key (PEM), for
  `nucleus_server::Config::with_tls`.  The front ends offer TLS to clients
  that ask: `ncp` clients by starting a TLS handshake as soon as they are
  connected (nucleus DOCS.md §10), PostgreSQL clients by `SSLRequest`.
  Clients that do not ask still connect in plain text (access rules that
  require TLS are a later step).
- **`ClientTls`** — for the Rust client (`nucleus_client::Config::tls`):
  the server's certificate checked against Mozilla's root certificates
  (`ClientTls::new`) or one's own CA (`with_ca_pem`, `with_ca_file`), and
  that it names the host; or `without_verification` — encrypted, but
  anyone can pose as the server.

TLS 1.2 and 1.3, rustls's safe defaults, ring's cryptography (no OpenSSL).

```rust
use std::sync::Arc;
let tls = nucleus_tls::ServerTls::from_pem_files("cert.pem".as_ref(), "key.pem".as_ref())?;
let config = nucleus_server::Config::default().with_tls(Arc::new(tls));

let client = nucleus_client::Client::connect(nucleus_client::Config {
    host: "db.example.com".into(),
    tls: Some(Arc::new(nucleus_tls::ClientTls::new())),
    ..Default::default()
})?;
```

The nucleus binary includes it: `nucleus server <dir> --tls-cert cert.pem
--tls-key key.pem`, and `ncsql --tls`, `--tls-ca <file>`,
`--tls-no-verify`.

## Built on the public API only

This folder is a module in nucleus's repository, built to leave it: it
depends on nucleus's public API crates (`nucleus-server`,
`nucleus-client`) by version, reads no files outside this folder, and CI
checks both — the boundary check and the detachability test.

## Tests

`tests/tls.rs` — a server with a certificate of a test CA (made with
`rcgen`): a verified client, a client checking the wrong name or the
wrong roots, an unverified one, a plain one, and a server without TLS.

## Licence

MIT — see [LICENSE](LICENSE).
