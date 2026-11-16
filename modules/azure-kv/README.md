# nucleus-azure-kv

The root key of a [nucleus](https://github.com/davidsarosi92/nucleus)
database kept in Azure Key Vault (PLAN.md §M14.30 K3): the data keys of
the keyring (`<dir>.keyring`) encrypted by a vault's RSA key, whose
private half never leaves Key Vault (or its HSM).  Disabling or deleting
the key, or taking away the right to use it, makes the database
unreadable at its next opening.

```sh
az keyvault key create --vault-name myvault --name nucleus --kty RSA-HSM --ops encrypt decrypt
export AZURE_TENANT_ID=… AZURE_CLIENT_ID=… AZURE_CLIENT_SECRET=…   # or a managed identity
nucleus server ./mydb --root-kms azure-kv:myvault/nucleus
```

- **The key** is `<vault>/<key>` (`https://<vault>.vault.azure.net`) or
  its URL, `https://<host>/keys/<key>` (another cloud's vault).  The
  keyring records `azure-kv:<vault URL>/keys/<key>`.  Opening reads it
  and refuses one not RSA (`RSA`, `RSA-HSM`), disabled, or not allowed
  both `encrypt` and `decrypt`.
- **Each data key** is encrypted by one `encrypt` call (RSA-OAEP-256) and
  decrypted by one `decrypt` at opening.  RSA-OAEP takes no associated
  data, so the purpose is encrypted with the key
  (`nucleus-purpose:<purpose>`, then its 32 bytes) and checked on the
  way back — OAEP is non-malleable, so a wrapped key offered for another
  purpose is refused.  The key's version is kept beside the ciphertext:
  a key rotated in Key Vault still decrypts what an older version
  encrypted.  The caller needs the `Key Vault Crypto User` role (or an
  access policy with get, encrypt and decrypt).
- **Credentials as Azure's SDKs find them**: a service principal's
  secret (`AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`),
  workload identity (`AZURE_FEDERATED_TOKEN_FILE` with the tenant and
  client — AKS), a managed identity's endpoint (`IDENTITY_ENDPOINT`,
  `IDENTITY_HEADER` — App Service, Container Apps, Arc), else the
  instance metadata service (VMs; `AZURE_CLIENT_ID` picks a
  user-assigned identity).  `AZURE_AUTHORITY_HOST` for another cloud's
  sign-in.  A token is kept until a minute before it expires.  Managed
  HSM (`*.managedhsm.azure.net`, another token audience) is not
  supported yet.
- **Over `nucleus-http`**: TLS (rustls); plain HTTP only to a loopback
  or link-local address (the metadata service); a request retried three
  times, waits doubling, on a network error or HTTP 429 or 5xx.
- **A provider** (`nucleus_server::keyring::KeyProvider`) registered for
  the `azure-kv` scheme by `nucleus_azure_kv::register()`.  The `mock`
  feature adds `nucleus_azure_kv::mock::Mock` — a stand-in Key Vault,
  Entra ID token endpoint and managed identity endpoint on a loopback
  port — for tests.

Licence: AGPL-3.0-only (see `LICENSE` and the repository's `COPYRIGHT`).
