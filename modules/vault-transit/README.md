# nucleus-vault-transit

The root key of a [nucleus](https://github.com/davidsarosi92/nucleus)
database kept in HashiCorp Vault's Transit secrets engine — or OpenBao's,
its open fork with the same API (PLAN.md §M14.30 K3): the data keys of
the keyring (`<dir>.keyring`) encrypted by a Transit key that never
leaves Vault.  Taking away the token's right to the key, or the key,
makes the database unreadable at its next opening.

```sh
vault secrets enable transit && vault write -f transit/keys/nucleus
export VAULT_ADDR=https://vault.example:8200 VAULT_TOKEN=…
nucleus server ./mydb --root-kms vault-transit:nucleus
# an existing database moved under the Transit key (the server stopped)
nucleus keys ./mydb rotate-root --to-root-kms vault-transit:nucleus
```

- **The key** is `<key>` (the engine at `transit`) or `<mount>/<key>`.
  Opening reads it (`GET <mount>/keys/<key>`) and refuses one that does
  not both encrypt and decrypt or is not an AEAD key (`aes256-gcm96`,
  `aes128-gcm96`, `chacha20-poly1305`).  The keyring records
  `vault-transit:<mount>/<key>` (the namespace first, when one is set).
- **Each data key** is encrypted by one `encrypt` call and decrypted by
  one `decrypt` at opening, with its purpose as associated data (and as
  the context of a key made with derivation): Vault refuses a wrapped
  key offered for another purpose.  The token needs `read` on
  `<mount>/keys/<key>` and `update` on `<mount>/encrypt/<key>` and
  `<mount>/decrypt/<key>`, nothing more.
- **Configured as Vault's CLI is**: `VAULT_ADDR` (default
  `https://127.0.0.1:8200`), `VAULT_TOKEN` (else `~/.vault-token`),
  `VAULT_NAMESPACE`, `VAULT_CACERT` (a private authority, trusted beside
  Mozilla's roots).
- **Over `nucleus-http`**: TLS (rustls) to anything but a loopback
  address; a request retried three times, waits doubling, on a network
  error or HTTP 429, 500, 502 or 503.
- **Rotating the Transit key** (`vault write -f transit/keys/nucleus/rotate`)
  keeps the old versions, so the keyring still opens; its wrapped keys
  stay under the version that wrapped them until they are wrapped again.
- **A provider** (`nucleus_server::keyring::KeyProvider`) registered for
  the `vault-transit` scheme by `nucleus_vault_transit::register()`.  The
  `mock` feature adds `nucleus_vault_transit::mock::Mock`, a stand-in
  engine on a loopback port, for tests.

Licence: AGPL-3.0-only (see `LICENSE` and the repository's `COPYRIGHT`).
