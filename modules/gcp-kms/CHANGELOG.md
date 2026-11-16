# Changelog

## Unreleased

- The root key of a database's keyring kept in Google Cloud KMS
  (`--root-kms gcp-kms:projects/…/cryptoKeys/<k>`): data keys encrypted
  and decrypted there with their purpose as additional authenticated
  data; tokens from Google's application default credentials (a token
  as it is, a service account's key, gcloud's refresh token, the
  metadata server); a stand-in Cloud KMS for tests behind the `mock`
  feature (PLAN.md §M14.30 K3).
