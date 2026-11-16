# Changelog

## Unreleased

- The root key of a database's keyring kept in a PKCS#11 token
  (`--root-kms 'pkcs11:token=<t>;object=<k>?module-path=<lib>&pin-source=<file>'`):
  data keys encrypted in the token by an AES-256 key with AES-GCM, their
  purpose as the additional authenticated data; RFC 7512 URIs; a session
  kept and opened again once when it goes stale (PLAN.md §M14.30 K3).
