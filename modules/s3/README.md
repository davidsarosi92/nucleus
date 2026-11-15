# nucleus-s3

Backups of [nucleus](https://github.com/davidsarosi92/nucleus) kept in
S3-compatible object storage — AWS S3, MinIO, Cloudflare R2, Backblaze
B2, … (PLAN.md §M14.18 O3b): a repository of backups at
`s3://bucket/prefix`, each request signed with AWS Signature Version 4.

```sh
export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_REGION=eu-central-1
nucleus sql ./mydb -c "BACKUP TO 's3://my-backups/mydb'"   # full, then what changed
nucleus restore s3://my-backups/mydb ./restored --at '2026-11-15 14:00:00+01'
# MinIO or another store: its endpoint, keys addressed by path
AWS_ENDPOINT_URL=http://127.0.0.1:9000 nucleus sql ./mydb -c "BACKUP TO 's3://bkt/mydb'"
```

- **A repository's files are objects** under the prefix
  (`000001-full/part-000000`, `000001-full/manifest`, …), each written
  whole by one `PUT` and seen only whole; the manifest, written last,
  makes a backup.  No multipart uploads: a part is about 16 MiB.
- **Configured as the AWS SDKs and CLI are**: `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` (or
  `AWS_DEFAULT_REGION`; `us-east-1` without), `AWS_ENDPOINT_URL_S3` or
  `AWS_ENDPOINT_URL` (another store, addressed by path; AWS's own by the
  bucket's virtual host).
- **Over `nucleus-http`**: TLS (rustls) to anything but a loopback
  address; a request retried three times, waits doubling, on a network
  error or HTTP 500/503.
- **A target** (`nucleus_sql::ext::backups::Target`) registered for the
  `s3` scheme by `nucleus_s3::register()`; other stores (GCS, Azure Blob)
  are added the same way.

Licence: AGPL-3.0-only (see `LICENSE` and the repository's `COPYRIGHT`).
