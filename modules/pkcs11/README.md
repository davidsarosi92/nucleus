# nucleus-pkcs11

The root key of a [nucleus](https://github.com/davidsarosi92/nucleus)
database kept in a hardware security module, or any PKCS#11 token (PLAN.md
§M14.30 K3): the data keys of the keyring (`<dir>.keyring`) encrypted by
an AES-256 key that never leaves the token.  Taking the token away, or
its PIN, or the key makes the database unreadable at its next opening.

```sh
softhsm2-util --init-token --free --label nucleus --so-pin … --pin …
pkcs11-tool --module /usr/lib/softhsm/libsofthsm2.so --login --keygen --key-type AES:32 --label root
nucleus server ./mydb --root-kms 'pkcs11:token=nucleus;object=root?module-path=/usr/lib/softhsm/libsofthsm2.so&pin-source=file:/etc/nucleus/pin'
```

- **The key** is named by an RFC 7512 URI: the token by `token` (its
  label), `serial`, `manufacturer`, `model` or `slot-id` (none when the
  module has one initialized token), the key by `object` (its label)
  and/or `id`.  The module's library by `module-path` (else
  `NUCLEUS_PKCS11_MODULE`), the user PIN by `pin-source` (a file),
  `pin-value` or `NUCLEUS_PKCS11_PIN` — or none, for a token without a
  login.  An attribute not known is refused, not ignored.  The keyring
  records `pkcs11:token=<label>;object=<label>[;id=<id>]`, read from the
  token: the same root however the URI names the key, wherever the
  library lives.
- **Opening** finds the one secret key so named and refuses one that is
  not AES-256 or cannot both encrypt and decrypt.
- **Each data key** is encrypted in the token with AES-GCM
  (`CKM_AES_GCM`), a 12-byte IV from the token's generator, and its
  purpose as the additional authenticated data — the token itself
  refuses to decrypt it for another purpose.  A blob is
  `1 ‖ IV ‖ ciphertext ‖ tag`.
- **One session** is opened, logged in, and kept; one the token dropped
  (removed and put back, a handle gone stale) is opened again once before
  an error is reported.  The library is loaded and initialized once a
  process and never finalized.
- **Through [cryptoki](https://crates.io/crates/cryptoki)**, the Parsec
  project's safe wrapper of the PKCS#11 interface — the module's own code
  stays free of `unsafe`.
- **A provider** (`nucleus_server::keyring::KeyProvider`) registered for
  the `pkcs11` scheme by `nucleus_pkcs11::register()`.  Its tests run
  against SoftHSM2 when it is installed (`brew install softhsm`,
  `apt install softhsm2`) and are skipped otherwise, unless
  `NUCLEUS_TEST_SOFTHSM_REQUIRED` is set.

Licence: AGPL-3.0-only (see `LICENSE` and the repository's `COPYRIGHT`).
