# Changelog

## Unreleased

- The root key of a database's keyring kept in Vault's (or OpenBao's)
  Transit engine (`--root-kms vault-transit:[<mount>/]<key>`): data keys
  encrypted and decrypted there with their purpose as associated data; a
  stand-in engine for tests behind the `mock` feature (PLAN.md §M14.30
  K3).
