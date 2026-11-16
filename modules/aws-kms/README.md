# nucleus-aws-kms

The root key of a [nucleus](https://github.com/davidsarosi92/nucleus)
database kept in AWS KMS (PLAN.md §M14.30 K3): the data keys of the
keyring (`<dir>.keyring`) wrapped by a KMS key that never leaves KMS.
Disabling or deleting that key, or taking away the right to use it,
makes the database unreadable at its next opening.

```sh
export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_REGION=eu-central-1
nucleus server ./mydb --root-kms aws-kms:alias/nucleus-prod
# an existing database moved under the KMS key (the server stopped)
nucleus keys ./mydb rotate-root --root-key ./mydb.root-key \
    --to-root-kms aws-kms:alias/nucleus-prod
```

- **The key** is named as KMS names it: a key id, its ARN, or
  `alias/…`.  At opening a `DescribeKey` finds its ARN — the keyring
  records `aws-kms:<arn>`, so an alias moved to another key is noticed —
  and refuses a key that is not enabled, not for encryption, or not
  symmetric.
- **Each data key** is wrapped by one `Encrypt` call and unwrapped by one
  `Decrypt` at opening, with the encryption context
  `{"nucleus-purpose": "<purpose>"}`: a wrapped key of one purpose does
  not unwrap as another's, and CloudTrail shows which key was used for
  what.  KMS is not called again while the database is open.
- **Configured as the AWS SDKs and CLI are** (through `nucleus-aws`):
  `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`,
  `AWS_REGION` (or `AWS_DEFAULT_REGION`), `AWS_ENDPOINT_URL_KMS` or
  `AWS_ENDPOINT_URL` (an emulator such as moto or LocalStack).
- **Over `nucleus-http`**: TLS (rustls) to anything but a loopback
  address; a request retried three times, waits doubling, on a network
  error, HTTP 500/503 or `ThrottlingException`.
- **A provider** (`nucleus_server::keyring::KeyProvider`) registered for
  the `aws-kms` scheme by `nucleus_aws_kms::register()`.  The `mock`
  feature adds `nucleus_aws_kms::mock::Mock`, a stand-in KMS on a
  loopback port that checks every request's signature, for tests.

Licence: AGPL-3.0-only (see `LICENSE` and the repository's `COPYRIGHT`).
