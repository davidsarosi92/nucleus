# Changelog

## Unreleased

- The root key of a database's keyring kept in AWS KMS
  (`--root-kms aws-kms:<key>`): data keys wrapped by `Encrypt` and
  unwrapped by `Decrypt` under an encryption context of their purpose;
  a stand-in KMS for tests behind the `mock` feature (PLAN.md §M14.30
  K3).
