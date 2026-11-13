# nucleus-http

Just enough HTTP/1.1 for the [nucleus](https://github.com/davidsarosi92/nucleus)
modules that call out — the OIDC module fetching an issuer's keys, the
embeddings module calling a provider (nucleus PLAN.md §M14.22).  A
module, so nucleus's core carries no TLS or HTTP library.

```rust
let r = nucleus_http::send(&nucleus_http::Request {
    method: "POST",
    url: "https://api.example.com/v1/embeddings",
    headers: &[("Authorization", "Bearer …"), ("Content-Type", "application/json")],
    body: Some(br#"{"input":["hello"]}"#),
    ..Default::default()
})?;
assert_eq!(r.status, 200);
```

- TLS by rustls with Mozilla's root certificates (`webpki-roots`); plain
  `http://` only to a loopback address (a local model server, tests), so
  a credential never crosses a network in the clear.
- One request a connection (`Connection: close`); a response read to its
  length or its last chunk, refused past a size limit (`limit`, 16 MB by
  default); connect and read time out (`timeout`, 30 s by default), and the whole
  exchange (`total`, five minutes by default).
- Any status is returned — the caller decides; `Error::transient` says
  whether trying again may help (no connection, a time-out, a cut
  response), `Response::retry_after` reads a `Retry-After` in seconds.

Not here: connection reuse, redirects, proxies, compression, HTTP/2.
