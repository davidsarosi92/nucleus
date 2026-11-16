//! What nucleus's AWS modules share (PLAN.md §M14.30 K3; taken out of
//! `nucleus-s3`, §M14.18 O3b): requests signed with AWS Signature
//! Version 4 for any service, and the credentials, region and endpoints
//! of the environment, read as the AWS SDKs and CLI read them.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use nucleus_sql::ext::{hmac_sha256, sha256};

/// The SHA-256 of nothing, hex: an empty body's.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// `b` in lower-case hex.
pub fn hex(b: &[u8]) -> String {
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

/// The SHA-256 of `b`, hex: a payload's hash as Signature Version 4
/// wants it.
pub fn payload_hash(b: &[u8]) -> String {
    hex(&sha256(b))
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

/// The host of `url` (`https://host[:port]/…`), port included.
pub fn host_of(url: &str) -> &str {
    url.split_once("://")
        .map_or(url, |(_, h)| h)
        .split('/')
        .next()
        .unwrap_or("")
}

/// Who signs: a service's name (`s3`, `kms`, …), a region, the
/// credentials.
#[derive(Clone, Copy)]
pub struct Signer<'a> {
    /// The service's name in the credential scope.
    pub service: &'a str,
    /// The region.
    pub region: &'a str,
    /// The access key's id.
    pub key_id: &'a str,
    /// The secret access key.
    pub secret: &'a str,
}

impl std::fmt::Debug for Signer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("service", &self.service)
            .field("region", &self.region)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl Signer<'_> {
    /// A request's `Authorization`: `headers` are the ones signed besides
    /// `host` (names in lower case; `x-amz-date` among them), `uri` the
    /// path as sent (encoded), `query` its parameters unencoded,
    /// `payload_hash` the body's SHA-256 in hex.
    pub fn authorization(
        &self,
        method: &str,
        host: &str,
        uri: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        payload_hash: &str,
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
        let scope = format!("{day}/{}/{}/aws4_request", self.region, self.service);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
            hex(&sha256(request.as_bytes()))
        );
        let mut key = hmac_sha256(format!("AWS4{}", self.secret).as_bytes(), day.as_bytes());
        for part in [self.region, self.service, "aws4_request"] {
            key = hmac_sha256(&key, part.as_bytes());
        }
        let signature = hex(&hmac_sha256(&key, to_sign.as_bytes()));
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            self.key_id
        )
    }
}

/// Credentials, as the AWS SDKs read them from the environment.
#[derive(Clone)]
pub struct Credentials {
    /// `AWS_ACCESS_KEY_ID`.
    pub key_id: String,
    /// `AWS_SECRET_ACCESS_KEY`.
    pub secret: String,
    /// `AWS_SESSION_TOKEN`, for temporary credentials.
    pub token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// The credentials `env` gives (the process's environment, or a
    /// test's); an error naming what is missing, `what` saying for what.
    pub fn from_env(env: &impl Fn(&str) -> Option<String>, what: &str) -> Result<Self, String> {
        let need = |k: &str| {
            env(k).filter(|v| !v.is_empty()).ok_or_else(|| {
                format!("{what}: {k} is not set (the credentials, as the AWS CLI's)")
            })
        };
        Ok(Self {
            key_id: need("AWS_ACCESS_KEY_ID")?,
            secret: need("AWS_SECRET_ACCESS_KEY")?,
            token: env("AWS_SESSION_TOKEN").filter(|t| !t.is_empty()),
        })
    }

    /// A signer for `service` in `region` under these credentials.
    pub fn signer<'a>(&'a self, service: &'a str, region: &'a str) -> Signer<'a> {
        Signer {
            service,
            region,
            key_id: &self.key_id,
            secret: &self.secret,
        }
    }
}

/// The region: `AWS_REGION`, else `AWS_DEFAULT_REGION`, else `us-east-1`.
pub fn region(env: &impl Fn(&str) -> Option<String>) -> String {
    env("AWS_REGION")
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "us-east-1".into())
}

/// The endpoint set for service `id` (`S3`, `KMS`: the suffix of
/// `AWS_ENDPOINT_URL_<id>`), else `AWS_ENDPOINT_URL`, without a trailing
/// `/`; `None` for AWS's own.  An error for one not `http(s)://`.
pub fn endpoint(env: &impl Fn(&str) -> Option<String>, id: &str) -> Result<Option<String>, String> {
    let Some(e) = env(&format!("AWS_ENDPOINT_URL_{id}"))
        .or_else(|| env("AWS_ENDPOINT_URL"))
        .filter(|e| !e.is_empty())
    else {
        return Ok(None);
    };
    if !e.starts_with("https://") && !e.starts_with("http://") {
        return Err(format!("{e}: not an http(s) endpoint"));
    }
    Ok(Some(e.trim_end_matches('/').to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn signature(auth: &str) -> &str {
        auth.rsplit("Signature=").next().unwrap()
    }

    /// The examples of AWS's documentation of Signature Version 4 for S3
    /// ("Signature Calculations for the Authorization Header").
    #[test]
    fn the_documentation_s_examples() {
        let s3 = Signer {
            service: "s3",
            region: "us-east-1",
            key_id: KEY_ID,
            secret: SECRET,
        };
        let date = ("x-amz-date", "20130524T000000Z");
        let host = "examplebucket.s3.amazonaws.com";
        // GET Object, a range.
        let auth = s3.authorization(
            "GET",
            host,
            "/test.txt",
            &[],
            &[
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", EMPTY_SHA256),
                date,
            ],
            EMPTY_SHA256,
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
        let auth = s3.authorization(
            "GET",
            host,
            "/",
            &[("max-keys", "2"), ("prefix", "J")],
            &[("x-amz-content-sha256", EMPTY_SHA256), date],
            EMPTY_SHA256,
        );
        assert_eq!(
            signature(&auth),
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
        // PUT Object, a body.
        let payload = payload_hash(b"Welcome to Amazon S3.");
        assert_eq!(
            payload,
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        let auth = s3.authorization(
            "PUT",
            host,
            &format!("/{}", uri_encode("test$file.text", true)),
            &[],
            &[
                ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                ("x-amz-content-sha256", &payload),
                date,
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ],
            &payload,
        );
        assert_eq!(
            signature(&auth),
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        );
    }

    /// AWS's Signature Version 4 test suite, `get-vanilla`: another
    /// service's name in the scope.
    #[test]
    fn the_test_suite_s_vanilla_request() {
        let s = Signer {
            service: "service",
            region: "us-east-1",
            key_id: "AKIDEXAMPLE",
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        };
        let auth = s.authorization(
            "GET",
            "example.amazonaws.com",
            "/",
            &[],
            &[("x-amz-date", "20150830T123600Z")],
            EMPTY_SHA256,
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
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
        assert_eq!(host_of("http://127.0.0.1:9000/x/y"), "127.0.0.1:9000");
        assert_eq!(
            host_of("https://kms.eu-central-1.amazonaws.com"),
            "kms.eu-central-1.amazonaws.com"
        );
    }

    #[test]
    fn the_environment_read() {
        let env = |k: &str| match k {
            "AWS_ACCESS_KEY_ID" => Some("id".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            "AWS_ENDPOINT_URL_KMS" => Some("http://127.0.0.1:4566/".to_string()),
            "AWS_ENDPOINT_URL" => Some("http://127.0.0.1:9000".to_string()),
            "AWS_DEFAULT_REGION" => Some("eu-west-1".to_string()),
            _ => None,
        };
        let c = Credentials::from_env(&env, "test").unwrap();
        assert_eq!((c.key_id.as_str(), c.token.as_deref()), ("id", None));
        assert!(!format!("{c:?}").contains("secret"));
        assert_eq!(region(&env), "eu-west-1");
        assert_eq!(region(&|_: &str| None), "us-east-1");
        assert_eq!(
            endpoint(&env, "KMS").unwrap().as_deref(),
            Some("http://127.0.0.1:4566")
        );
        assert_eq!(
            endpoint(&env, "S3").unwrap().as_deref(),
            Some("http://127.0.0.1:9000")
        );
        assert_eq!(endpoint(&|_: &str| None, "S3").unwrap(), None);
        let bad = |k: &str| (k == "AWS_ENDPOINT_URL").then(|| "ftp://x".to_string());
        assert!(endpoint(&bad, "S3").is_err());
        let e = Credentials::from_env(&|_: &str| None, "aws-kms:alias/x").unwrap_err();
        assert!(
            e.contains("AWS_ACCESS_KEY_ID") && e.starts_with("aws-kms:alias/x"),
            "{e}"
        );
    }
}
