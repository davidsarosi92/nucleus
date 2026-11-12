//! The `local` model provider (PLAN.md §M14.22 E3c): `CREATE MODEL m
//! USING local WITH (huggingface = 'BAAI/bge-small-en-v1.5')` — or
//! `path = '/models/bge'` — fetches the model's files once, the core
//! keeps them in the database (by content), and `embed()` runs the model
//! in the process: no network after it is made, and a backup or a
//! replica carries the model with the data.
//!
//! Options:
//!
//! - `huggingface` — a repository (`org/name`), its files from `revision`
//!   (a branch, a tag or a commit; `main` by default) at `endpoint`
//!   (`https://huggingface.co`; another — a mirror — only an
//!   administrator may name).  A file Hugging Face serves from its
//!   large-file store is checked against the SHA-256 it announces.
//! - `path` — a directory on the server holding the same files; only an
//!   administrator may name one (the server's files are theirs).
//! - `pooling` — `cls` or `mean`, where the model has no
//!   `1_Pooling/config.json` or should be pooled otherwise.
//! - `threads` — threads a call runs on (the machine's cores by default).
//! - `batch` — texts a call (256 by default).
//!
//! The files: `tokenizer.json`, `config.json`, `model.safetensors`, and
//! `1_Pooling/config.json` where there is one.  A model loaded is kept
//! for the process (up to four, the least recently used dropped), by its
//! files' identity — two models made from the same files share it.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use nucleus_sql::ext::models::{Files as ModelFiles, ModelProvider, Options, Request, Spec};
use nucleus_sql::ext::{Error, Result, current_who};

use crate::encoder::Pooling;
use crate::{Files, LocalModel};

/// The files a model is made from: required, then optional.
const REQUIRED: [&str; 3] = ["tokenizer.json", "config.json", "model.safetensors"];
const POOLING: &str = "1_Pooling/config.json";
/// Models kept loaded.
const KEPT: usize = 4;

/// The provider.
pub struct Local;

fn pooling(o: &Options) -> Result<Option<Pooling>> {
    match o.get("pooling") {
        None => Ok(None),
        Some("cls") => Ok(Some(Pooling::Cls)),
        Some("mean") => Ok(Some(Pooling::Mean)),
        Some(other) => Err(Error::invalid_parameter(format!(
            "option pooling is cls or mean, not {other}"
        ))),
    }
}

fn admin(what: &str) -> Result<()> {
    if current_who().superuser {
        Ok(())
    } else {
        Err(Error::insufficient_privilege(format!(
            "only an administrator may {what}"
        )))
    }
}

/// A repository's name or a revision: letters, digits, `-`, `_`, `.`,
/// and one `/` in a repository.
fn safe(s: &str, slashes: usize) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.contains("..")
        && s.matches('/').count() <= slashes
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        && !s.starts_with(['/', '.'])
}

/// GETs `url`, following up to five redirects (to `https://` only): the
/// body, and the SHA-256 Hugging Face announces for a large file.
fn download(url: &str) -> Result<(u16, Vec<u8>, Option<String>)> {
    let mut url = url.to_string();
    let mut announced = None;
    for _ in 0..6 {
        let r = nucleus_http::send(&nucleus_http::Request {
            url: &url,
            timeout: Duration::from_secs(60),
            limit: 8 << 30,
            agent: "nucleus-local-models",
            ..Default::default()
        })
        .map_err(|e| {
            if e.transient {
                Error::io(format!("local: {e}"))
            } else {
                Error::prerequisite(format!("local: {e}"))
            }
        })?;
        if let Some(etag) = r.header("x-linked-etag") {
            announced = Some(etag.trim_matches('"').to_ascii_lowercase());
        }
        match r.status {
            301 | 302 | 303 | 307 | 308 => {
                let next = r
                    .header("location")
                    .ok_or_else(|| Error::prerequisite("local: a redirect without a place"))?;
                url = if next.starts_with('/') {
                    // Relative to the same host.
                    let host_end = url
                        .find("://")
                        .and_then(|i| url[i + 3..].find('/').map(|j| i + 3 + j))
                        .unwrap_or(url.len());
                    format!("{}{next}", &url[..host_end])
                } else {
                    next.to_string()
                };
            }
            status => return Ok((status, r.body, announced)),
        }
    }
    Err(Error::prerequisite("local: more than five redirects"))
}

fn fetch_hub(o: &Options, repo: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let revision = o.get("revision").unwrap_or("main");
    if !safe(repo, 1) || !repo.contains('/') {
        return Err(Error::invalid_parameter(format!(
            "option huggingface is a repository, organization/name: not {repo}"
        )));
    }
    if !safe(revision, 0) {
        return Err(Error::invalid_parameter(format!(
            "option revision is a branch, a tag or a commit: not {revision}"
        )));
    }
    let endpoint = match o.get("endpoint") {
        Some(e) => {
            admin("name another endpoint")?;
            e.trim_end_matches('/')
        }
        None => "https://huggingface.co",
    };
    let mut out = Vec::new();
    for name in REQUIRED.iter().chain([&POOLING]) {
        let url = format!("{endpoint}/{repo}/resolve/{revision}/{name}");
        let (status, body, announced) = download(&url)?;
        match status {
            200 => {}
            404 if *name == POOLING => continue,
            401 | 403 | 404 => {
                return Err(Error::prerequisite(format!(
                    "local: {repo} at {revision} has no {name} that may be read ({status})"
                )));
            }
            s if s == 429 || s >= 500 => {
                return Err(Error::io(format!("local: {url} answered {s}")));
            }
            s => {
                return Err(Error::prerequisite(format!("local: {url} answered {s}")));
            }
        }
        if let Some(want) = announced {
            let got: String = nucleus_sql::ext::sha256(&body)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if want.len() == 64 && got != want {
                return Err(Error::io(format!(
                    "local: {name} of {repo} is not what Hugging Face announced (SHA-256 {got}, not {want})"
                )));
            }
        }
        out.push((name.to_string(), body));
    }
    Ok(out)
}

fn fetch_path(dir: &str) -> Result<Vec<(String, Vec<u8>)>> {
    admin("read a model from the server's files (option path)")?;
    let dir = std::path::Path::new(dir);
    let mut out = Vec::new();
    for name in REQUIRED.iter().chain([&POOLING]) {
        match std::fs::read(dir.join(name)) {
            Ok(b) => out.push((name.to_string(), b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && *name == POOLING => {}
            Err(e) => {
                return Err(Error::prerequisite(format!(
                    "local: {}: {e}",
                    dir.join(name).display()
                )));
            }
        }
    }
    Ok(out)
}

/// The model the files make, loaded once.
fn loaded(files: &ModelFiles<'_>, pooling: Option<Pooling>) -> Result<Arc<LocalModel>> {
    type Kept = Vec<([u8; 32], Option<Pooling>, Arc<LocalModel>)>;
    static KEPT_MODELS: OnceLock<Mutex<Kept>> = OnceLock::new();
    let id = files.id();
    let kept = KEPT_MODELS.get_or_init(|| Mutex::new(Vec::new()));
    let mut kept = kept.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = kept.iter().position(|(k, p, _)| *k == id && *p == pooling) {
        // The most recently used last.
        let e = kept.remove(i);
        let m = e.2.clone();
        kept.push(e);
        return Ok(m);
    }
    let read = |name: &str| files.read(name);
    let need = |name: &str| -> Result<Vec<u8>> {
        read(name)?.ok_or_else(|| Error::prerequisite(format!("local: the model has no {name}")))
    };
    let (tokenizer, config, weights) = (
        need("tokenizer.json")?,
        need("config.json")?,
        need("model.safetensors")?,
    );
    let pool = read(POOLING)?;
    let m = LocalModel::new(
        &Files {
            tokenizer: &tokenizer,
            config: &config,
            pooling: pool.as_deref(),
            weights: &weights,
        },
        pooling,
    )
    .map_err(|e| Error::prerequisite(format!("local: {e}")))?;
    let m = Arc::new(m);
    if kept.len() >= KEPT {
        kept.remove(0);
    }
    kept.push((id, pooling, m.clone()));
    Ok(m)
}

fn threads(o: &Options) -> Result<usize> {
    Ok(match o.number("threads", 1..=1024)? {
        Some(n) => n as usize,
        None => std::thread::available_parallelism().map_or(1, |n| n.get()),
    })
}

impl ModelProvider for Local {
    fn name(&self) -> &str {
        "local"
    }

    fn fetch(&self, o: &Options) -> Result<Vec<(String, Vec<u8>)>> {
        o.only(
            "local",
            &[
                "huggingface",
                "revision",
                "endpoint",
                "path",
                "pooling",
                "threads",
                "batch",
            ],
        )?;
        match (o.get("huggingface"), o.get("path")) {
            (Some(repo), None) => fetch_hub(o, repo),
            (None, Some(dir)) => {
                if o.get("revision").is_some() || o.get("endpoint").is_some() {
                    return Err(Error::invalid_parameter(
                        "options revision and endpoint go with huggingface, not path",
                    ));
                }
                fetch_path(dir)
            }
            _ => Err(Error::invalid_parameter(
                "model provider \"local\" needs option huggingface (a repository) or path (a directory)",
            )),
        }
    }

    fn define(&self, o: &Options, files: &ModelFiles<'_>) -> Result<Spec> {
        threads(o)?;
        let batch = o.number("batch", 1..=65_536)?.unwrap_or(256) as usize;
        // Loaded now, so a model that cannot run is refused when made —
        // and its first call finds it ready.
        let m = loaded(files, pooling(o)?)?;
        Ok(Spec {
            dimensions: m.dimensions() as u32,
            max_batch: batch,
        })
    }

    fn embed(&self, r: &Request<'_>) -> Result<Vec<Vec<f32>>> {
        let m = loaded(r.files, pooling(r.options)?)?;
        let texts: Vec<&str> = r.texts.iter().map(String::as_str).collect();
        let (v, tokens) = m
            .embed_counted(&texts, threads(r.options)?)
            .map_err(|e| Error::data_exception(format!("local: {e}")))?;
        // The tokens it ran, for the statement's costs.
        r.report_tokens(tokens as u64);
        Ok(v)
    }

    fn remote(&self) -> bool {
        false
    }

    fn count_tokens(&self, r: &Request<'_>) -> Result<Option<Vec<usize>>> {
        let m = loaded(r.files, pooling(r.options)?)?;
        Ok(Some(r.texts.iter().map(|t| m.count(t)).collect()))
    }
}
