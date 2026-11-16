# Changelog

## Unreleased

- The root key of a database's keyring kept in Azure Key Vault
  (`--root-kms azure-kv:<vault>/<key>`): data keys encrypted by the
  vault's RSA key (RSA-OAEP-256) with their purpose bound in, decrypted
  by the version that encrypted them; tokens from a service principal's
  secret, workload identity, or a managed identity; a stand-in Key Vault
  for tests behind the `mock` feature (PLAN.md §M14.30 K3).
