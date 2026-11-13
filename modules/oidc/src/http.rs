//! The issuer's metadata and keys: `GET` as JSON, through `nucleus-http`
//! (TLS; plain HTTP only to a loopback address).

/// GETs `url` and parses it as JSON.
pub(crate) fn get_json(url: &str) -> Result<serde_json::Value, String> {
    let r = nucleus_http::send(&nucleus_http::Request {
        url,
        headers: &[("Accept", "application/json")],
        timeout: std::time::Duration::from_secs(10),
        // A login waits for it.
        total: Some(std::time::Duration::from_secs(20)),
        limit: 1 << 20,
        agent: "nucleus-oidc",
        ..Default::default()
    })
    .map_err(|e| e.to_string())?;
    if r.status != 200 {
        return Err(format!("{url}: the server answered {}", r.status));
    }
    serde_json::from_slice(&r.body).map_err(|e| format!("{url}: not JSON: {e}"))
}
