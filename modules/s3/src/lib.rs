//! Backups in S3-compatible object storage (PLAN.md §M14.18 O3b): a
//! repository of backups at `s3://bucket/prefix`, its files objects under
//! the prefix — AWS S3, MinIO, Cloudflare R2, Backblaze B2, … — over
//! `nucleus-http`, each request signed with AWS Signature Version 4.
//!
//! Configured as the AWS SDKs and CLI are, by environment variables:
//! `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` (for
//! temporary credentials), `AWS_REGION` (or `AWS_DEFAULT_REGION`;
//! `us-east-1` without), and `AWS_ENDPOINT_URL_S3` (or
//! `AWS_ENDPOINT_URL`) for a store other than AWS's — addressed by path
//! (`<endpoint>/<bucket>/<key>`), as MinIO and most others want; AWS's own
//! by its virtual host (`<bucket>.s3.<region>.amazonaws.com`).
//!
//! An object is written whole by one `PUT` and seen only whole: the
//! manifest, written last, makes a backup.  No multipart uploads: a part
//! is about 16 MiB, far below a `PUT`'s 5 GiB.

use std::io;
use std::time::{Duration, SystemTime};

use nucleus_sql::ext::backups::{Target, register_target};
use nucleus_sql::ext::sha256;

/// Lets `BACKUP TO 's3://…'` and `nucleus restore s3://…` use this module.
pub fn register() {
    register_target("s3", open);
}

/// The repository at `url` (`s3://bucket/prefix`), with the credentials
/// and endpoint of the environment.
pub fn open(url: &str) -> Result<Box<dyn Target>, String> {
    S3::new(url, |k| std::env::var(k).ok()).map(|s| Box::new(s) as Box<dyn Target>)
}

/// A repository in a bucket.
#[derive(Clone)]
pub struct S3 {
    /// `https://host[:port]` (or `http://` to a loopback address).
    endpoint: String,
    /// Keys under `<endpoint>/<bucket>/`, else under the endpoint (the
    /// bucket's virtual host).
    path_style: bool,
    bucket: String,
    /// `""`, or the keys' prefix ending in `/`.
    prefix: String,
    region: String,
    key_id: String,
    secret: String,
    token: Option<String>,
}

impl std::fmt::Debug for S3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

/// The SHA-256 of nothing, hex: an empty body's.
const EMPTY: &str = nucleus_aws::EMPTY_SHA256;

pub use nucleus_aws::{amz_date, hex, uri_encode};

/// A request's `Authorization` by Signature Version 4 for S3: `headers`
/// are the ones signed besides `host` (names in lower case; `x-amz-date`
/// and `x-amz-content-sha256` among them), `uri` the path as sent
/// (encoded), `query` its parameters unencoded ([`nucleus_aws::Signer`]).
#[allow(clippy::too_many_arguments)]
pub fn sign(
    method: &str,
    host: &str,
    uri: &str,
    query: &[(&str, &str)],
    headers: &[(&str, &str)],
    payload_hash: &str,
    region: &str,
    key_id: &str,
    secret: &str,
) -> String {
    nucleus_aws::Signer {
        service: "s3",
        region,
        key_id,
        secret,
    }
    .authorization(method, host, uri, query, headers, payload_hash)
}

/// XML's five entities, undone.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The texts of every `<tag>…</tag>` in `xml`.
fn texts<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(&after[..j]);
        rest = &after[j + close.len()..];
    }
    out
}

impl S3 {
    /// The repository at `url` (`s3://bucket[/prefix]`), its settings read
    /// by `env` (the process's environment, or a test's).
    pub fn new(url: &str, env: impl Fn(&str) -> Option<String>) -> Result<S3, String> {
        let rest = url
            .strip_prefix("s3://")
            .ok_or_else(|| format!("{url}: not an s3:// URL"))?;
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty()
            || !bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'))
        {
            return Err(format!("{url}: not a bucket's name: {bucket:?}"));
        }
        let prefix = prefix.trim_matches('/');
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        let what = url.to_string();
        let creds = nucleus_aws::Credentials::from_env(&env, &what)?;
        let region = nucleus_aws::region(&env);
        let (endpoint, path_style) = match nucleus_aws::endpoint(&env, "S3")? {
            Some(e) => (e, true),
            None => (format!("https://{bucket}.s3.{region}.amazonaws.com"), false),
        };
        Ok(S3 {
            endpoint,
            path_style,
            bucket: bucket.to_string(),
            prefix,
            region,
            key_id: creds.key_id,
            secret: creds.secret,
            token: creds.token,
        })
    }

    /// The path a key is at (encoded), from the endpoint.
    fn path(&self, key: &str) -> String {
        let key = uri_encode(key, true);
        if self.path_style {
            format!("/{}/{key}", uri_encode(&self.bucket, false))
        } else {
            format!("/{key}")
        }
    }

    /// One request, signed; retried a few times when the store or the
    /// network says to try again.
    fn request(
        &self,
        method: &str,
        key: &str,
        query: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> io::Result<nucleus_http::Response> {
        let host = self
            .endpoint
            .split_once("://")
            .map_or(self.endpoint.as_str(), |(_, h)| h)
            .split('/')
            .next()
            .unwrap_or("");
        let uri = self.path(key);
        let payload = body.map_or_else(|| EMPTY.to_string(), |b| hex(&sha256(b)));
        let mut url = format!("{}{uri}", self.endpoint);
        if !query.is_empty() {
            url.push('?');
            url.push_str(
                &query
                    .iter()
                    .map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false)))
                    .collect::<Vec<_>>()
                    .join("&"),
            );
        }
        let mut wait = Duration::from_millis(250);
        let mut tries = 0;
        loop {
            tries += 1;
            let date = amz_date(SystemTime::now());
            let mut headers: Vec<(&str, &str)> =
                vec![("x-amz-content-sha256", &payload), ("x-amz-date", &date)];
            if let Some(t) = &self.token {
                headers.push(("x-amz-security-token", t));
            }
            let auth = sign(
                method,
                host,
                &uri,
                query,
                &headers,
                &payload,
                &self.region,
                &self.key_id,
                &self.secret,
            );
            headers.push(("Authorization", &auth));
            let r = nucleus_http::send(&nucleus_http::Request {
                method,
                url: &url,
                headers: &headers,
                body,
                limit: 64 << 20,
                agent: "nucleus-s3",
                ..nucleus_http::Request::default()
            });
            let again = match &r {
                Err(e) => e.transient,
                Ok(r) => r.status == 500 || r.status == 503,
            };
            if !again || tries == 4 {
                return r.map_err(|e| io::Error::other(e.message));
            }
            std::thread::sleep(wait);
            wait *= 2;
        }
    }

    /// An error for a response the store refused.
    fn refused(&self, what: &str, key: &str, r: &nucleus_http::Response) -> io::Error {
        let body = r.body_text(4096);
        let code = texts(&body, "Code")
            .first()
            .copied()
            .unwrap_or("")
            .to_string();
        let message = texts(&body, "Message")
            .first()
            .copied()
            .unwrap_or("")
            .to_string();
        io::Error::other(format!(
            "{what} s3://{}/{key}: HTTP {} {code} {message}",
            self.bucket, r.status
        ))
    }
}

impl Target for S3 {
    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        let mut prefix = format!("{}{}", self.prefix, dir.trim_matches('/'));
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type", "2"),
                ("delimiter", "/"),
                ("prefix", prefix.as_str()),
            ];
            if let Some(t) = &token {
                query.push(("continuation-token", t));
            }
            let r = self.request("GET", "", &query, None)?;
            if r.status != 200 {
                return Err(self.refused("list", &prefix, &r));
            }
            let body = String::from_utf8_lossy(&r.body).into_owned();
            // Objects right under the prefix, and the "directories".
            for c in texts(&body, "Contents") {
                if let Some(k) = texts(c, "Key").first() {
                    let k = unescape(k);
                    if let Some(name) = k.strip_prefix(&prefix).filter(|n| !n.is_empty()) {
                        out.push(name.to_string());
                    }
                }
            }
            for c in texts(&body, "CommonPrefixes") {
                if let Some(p) = texts(c, "Prefix").first() {
                    let p = unescape(p);
                    if let Some(name) = p.strip_prefix(&prefix) {
                        let name = name.trim_end_matches('/');
                        if !name.is_empty() {
                            out.push(name.to_string());
                        }
                    }
                }
            }
            let truncated = texts(&body, "IsTruncated").first() == Some(&"true");
            token = texts(&body, "NextContinuationToken")
                .first()
                .map(|t| unescape(t));
            if !truncated || token.is_none() {
                break;
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let key = format!("{}{name}", self.prefix);
        let r = self.request("GET", &key, &[], None)?;
        match r.status {
            200 => Ok(Some(r.body)),
            404 => Ok(None),
            _ => Err(self.refused("read", &key, &r)),
        }
    }

    fn put(&self, name: &str, data: &[u8]) -> io::Result<()> {
        let key = format!("{}{name}", self.prefix);
        let r = self.request("PUT", &key, &[], Some(data))?;
        if r.status == 200 {
            Ok(())
        } else {
            Err(self.refused("write", &key, &r))
        }
    }

    fn delete(&self, name: &str) -> io::Result<()> {
        let key = format!("{}{name}", self.prefix);
        let r = self.request("DELETE", &key, &[], None)?;
        if matches!(r.status, 200 | 204 | 404) {
            Ok(())
        } else {
            Err(self.refused("delete", &key, &r))
        }
    }

    fn describe(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_settings() {
        let env = |k: &str| match k {
            "AWS_ACCESS_KEY_ID" => Some("id".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        };
        let s = S3::new("s3://bkt/backups/db/", env).unwrap();
        assert_eq!(s.endpoint, "https://bkt.s3.us-east-1.amazonaws.com");
        assert_eq!(
            (s.prefix.as_str(), s.path("x/y")),
            ("backups/db/", "/x/y".into())
        );
        assert_eq!(s.describe(), "s3://bkt/backups/db/");
        assert!(!format!("{s:?}").contains("secret"));
        let local = |k: &str| match k {
            "AWS_ENDPOINT_URL" => Some("http://127.0.0.1:9000/".to_string()),
            "AWS_REGION" => Some("eu-central-1".to_string()),
            _ => env(k),
        };
        let s = S3::new("s3://bkt", local).unwrap();
        assert_eq!(
            (s.endpoint.as_str(), s.prefix.as_str(), s.path("a")),
            ("http://127.0.0.1:9000", "", "/bkt/a".into())
        );
        assert_eq!(s.region, "eu-central-1");
        assert!(S3::new("s3://Bad_Bucket/x", env).is_err());
        assert!(
            S3::new("s3://bkt", |_| None)
                .unwrap_err()
                .contains("AWS_ACCESS_KEY_ID")
        );
        assert!(S3::new("gs://bkt", env).is_err());
    }

    #[test]
    fn listings_read() {
        let xml = "<ListBucketResult><Prefix>r/</Prefix><Contents><Key>r/a&amp;b</Key></Contents>\
                   <CommonPrefixes><Prefix>r/000001-full/</Prefix></CommonPrefixes></ListBucketResult>";
        assert_eq!(texts(xml, "Prefix"), ["r/", "r/000001-full/"]);
        let c: Vec<String> = texts(xml, "Contents")
            .iter()
            .flat_map(|c| texts(c, "Key"))
            .map(unescape)
            .collect();
        assert_eq!(c, ["r/a&b"]);
    }
}
