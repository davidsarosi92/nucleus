# nucleus-aws

What [nucleus](https://github.com/davidsarosi92/nucleus)'s AWS modules
share — `nucleus-s3` (backups in object storage) and `nucleus-aws-kms`
(a root key in AWS KMS):

- **AWS Signature Version 4** ([`Signer`]): a request's `Authorization`
  for any service and region, checked against AWS's documented examples.
- **The environment, as the AWS SDKs and CLI read it**
  ([`Credentials::from_env`], [`region`], [`endpoint`]):
  `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`,
  `AWS_REGION` (or `AWS_DEFAULT_REGION`; `us-east-1` without), and
  `AWS_ENDPOINT_URL_<SERVICE>` or `AWS_ENDPOINT_URL` for a service other
  than AWS's own (MinIO, LocalStack, moto, …).

No dependency beyond nucleus's public API (its SHA-256 and HMAC).

Licence: AGPL-3.0-only (see `LICENSE` and the repository's `COPYRIGHT`).
