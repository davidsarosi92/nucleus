# nucleus-gcp-kms

The root key of a [nucleus](https://github.com/davidsarosi92/nucleus)
database kept in Google Cloud KMS (PLAN.md §M14.30 K3): the data keys of
the keyring (`<dir>.keyring`) encrypted by a symmetric Cloud KMS key that
never leaves it.  Disabling or destroying the key's version, or taking
away the right to use it, makes the database unreadable at its next
opening.

```sh
gcloud kms keys create nucleus --keyring ring --location europe-west3 --purpose encryption
gcloud auth application-default login     # or a service account, or the VM's own
nucleus server ./mydb \
  --root-kms gcp-kms:projects/my-project/locations/europe-west3/keyRings/ring/cryptoKeys/nucleus
```

- **The key** is its resource name,
  `projects/<p>/locations/<l>/keyRings/<r>/cryptoKeys/<k>` (the keyring
  records `gcp-kms:<name>`).  Opening reads it and refuses one not for
  `ENCRYPT_DECRYPT` or whose primary version is not `ENABLED`.
- **Each data key** is encrypted by one `encrypt` call and decrypted by
  one `decrypt` at opening, with its purpose as additional authenticated
  data: Cloud KMS refuses a wrapped key offered for another purpose.
  The caller needs `roles/cloudkms.cryptoKeyEncrypterDecrypter` and
  `cloudkms.cryptoKeys.get` (`roles/cloudkms.viewer`) on the key.
- **Credentials as Google's client libraries find them**:
  `GOOGLE_OAUTH_ACCESS_TOKEN` (a token as it is); else the file
  `GOOGLE_APPLICATION_CREDENTIALS` names, or gcloud's
  `~/.config/gcloud/application_default_credentials.json` — a service
  account's key (a JWT signed RS256 with it, exchanged for a token) or a
  user's refresh token; else the machine's metadata server (Compute
  Engine, GKE workload identity, Cloud Run; `GCE_METADATA_HOST` for
  another).  A token is kept until a minute before it expires.
  Workload identity federation's `external_account` files are not read
  yet.  `CLOUDSDK_API_ENDPOINT_OVERRIDES_CLOUDKMS` names another
  endpoint.
- **Over `nucleus-http`**: TLS (rustls); plain HTTP only to a loopback
  or link-local address (the metadata server); a request retried three
  times, waits doubling, on a network error or HTTP 429 or 5xx.
- **A provider** (`nucleus_server::keyring::KeyProvider`) registered for
  the `gcp-kms` scheme by `nucleus_gcp_kms::register()`.  The `mock`
  feature adds `nucleus_gcp_kms::mock::Mock` — a stand-in Cloud KMS,
  token endpoint (checking a service account's signature) and metadata
  server on a loopback port — for tests; Google publishes no Cloud KMS
  emulator.

Licence: AGPL-3.0-only (see `LICENSE` and the repository's `COPYRIGHT`).
