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

use std::fmt::Write as _;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nucleus_sql::ext::backups::{Target, register_target};
use nucleus_sql::ext::{hmac_sha256, sha256};

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
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn hex(b: &[u8]) -> String {
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

/// `s` URI-encoded as Signature Version 4 wants: every byte but the
/// unreserved ones (`A-Z a-z 0-9 - _ . ~`) as `%XX`, `/` kept in a path.
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// `YYYYMMDDTHHMMSSZ` of a time.
pub fn amz_date(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// A request's `Authorization` by Signature Version 4: `headers` are the
/// ones signed besides `host` (names in lower case; `x-amz-date` and
/// `x-amz-content-sha256` among them), `uri` the path as sent (encoded),
/// `query` its parameters unencoded.
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
    let date = headers
        .iter()
        .find(|(k, _)| *k == "x-amz-date")
        .map_or("", |(_, v)| *v);
    let mut q: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    q.sort();
    let canonical_query = q
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut h: Vec<(String, String)> = std::iter::once(("host".to_string(), host.to_string()))
        .chain(
            headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())),
        )
        .collect();
    h.sort();
    let signed = h
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let request = format!(
        "{method}\n{uri}\n{canonical_query}\n{canonical_headers}\n{signed}\n{payload_hash}"
    );
    let day = &date[..date.len().min(8)];
    let scope = format!("{day}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
        hex(&sha256(request.as_bytes()))
    );
    let mut key = hmac_sha256(format!("AWS4{secret}").as_bytes(), day.as_bytes());
    for part in [region, "s3", "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    let signature = hex(&hmac_sha256(&key, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed}, Signature={signature}"
    )
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
        let need = |k: &str| {
            env(k)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{url}: {k} is not set (the credentials, as the AWS CLI's)"))
        };
        let key_id = need("AWS_ACCESS_KEY_ID")?;
        let secret = need("AWS_SECRET_ACCESS_KEY")?;
        let region = env("AWS_REGION")
            .or_else(|| env("AWS_DEFAULT_REGION"))
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "us-east-1".into());
        let (endpoint, path_style) = match env("AWS_ENDPOINT_URL_S3")
            .or_else(|| env("AWS_ENDPOINT_URL"))
            .filter(|e| !e.is_empty())
        {
            Some(e) => (e.trim_end_matches('/').to_string(), true),
            None => (format!("https://{bucket}.s3.{region}.amazonaws.com"), false),
        };
        if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
            return Err(format!("{endpoint}: not an http(s) endpoint"));
        }
        Ok(S3 {
            endpoint,
            path_style,
            bucket: bucket.to_string(),
            prefix,
            region,
            key_id,
            secret,
            token: env("AWS_SESSION_TOKEN").filter(|t| !t.is_empty()),
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

    const KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn signature(auth: &str) -> &str {
        auth.rsplit("Signature=").next().unwrap()
    }

    /// The examples of AWS's documentation of Signature Version 4 for S3
    /// ("Signature Calculations for the Authorization Header").
    #[test]
    fn the_documentation_s_examples() {
        let date = ("x-amz-date", "20130524T000000Z");
        // GET Object, a range.
        let auth = sign(
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            &[],
            &[
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", EMPTY),
                date,
            ],
            EMPTY,
            "us-east-1",
            KEY_ID,
            SECRET,
        );
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, "
        ));
        assert_eq!(
            signature(&auth),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        // GET Bucket (list objects).
        let auth = sign(
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/",
            &[("max-keys", "2"), ("prefix", "J")],
            &[("x-amz-content-sha256", EMPTY), date],
            EMPTY,
            "us-east-1",
            KEY_ID,
            SECRET,
        );
        assert_eq!(
            signature(&auth),
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
        // PUT Object, a body.
        let payload = hex(&sha256(b"Welcome to Amazon S3."));
        assert_eq!(
            payload,
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        let auth = sign(
            "PUT",
            "examplebucket.s3.amazonaws.com",
            &format!("/{}", uri_encode("test$file.text", true)),
            &[],
            &[
                ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                ("x-amz-content-sha256", &payload),
                date,
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ],
            &payload,
            "us-east-1",
            KEY_ID,
            SECRET,
        );
        assert_eq!(
            signature(&auth),
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        );
    }

    #[test]
    fn dates_and_encoding() {
        assert_eq!(amz_date(UNIX_EPOCH), "19700101T000000Z");
        assert_eq!(
            amz_date(UNIX_EPOCH + Duration::from_secs(1_369_353_600)),
            "20130524T000000Z"
        );
        assert_eq!(
            amz_date(UNIX_EPOCH + Duration::from_secs(951_825_599)),
            "20000229T115959Z"
        );
        assert_eq!(uri_encode("a b/c~d", true), "a%20b/c~d");
        assert_eq!(uri_encode("a b/c", false), "a%20b%2Fc");
    }

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
