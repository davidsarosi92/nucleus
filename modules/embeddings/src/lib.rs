//! Embedding model providers for nucleus (PLAN.md §M14.22 E2b): what
//! `CREATE MODEL m USING openai | cohere | voyage WITH (…)` calls.
//!
//! - **`openai`** — any OpenAI-compatible `POST {url}/embeddings`: OpenAI
//!   itself (the default `url`), and Ollama, vLLM, LM Studio, Hugging
//!   Face's text-embeddings-inference, Mistral, Together and the rest
//!   that speak it.  Options: `model` (required), `url`, `dimensions`
//!   (required unless the model is one this module knows; for OpenAI's
//!   `text-embedding-3-*` it is also sent, shortening the vectors),
//!   `batch` (texts a call, default 512).
//! - **`cohere`** — Cohere's `POST /v2/embed`, `input_type`
//!   `search_query` or `search_document` as the call is a query or a
//!   document.  Options: `model`, `url`, `dimensions` (`embed-v4.0`'s
//!   `output_dimension`), `batch` (at most 96).
//! - **`voyage`** — Voyage AI's `POST /v1/embeddings`, `input_type`
//!   `query` or `document`.  Options: `model`, `url`, `dimensions`
//!   (`output_dimension`), `batch` (at most 1,000; default 128).
//!
//! The key is the model's `secret` (nucleus's `CREATE SECRET`), sent as
//! the API asks (`Authorization: Bearer …`) and never put in an error.  A
//! call throttled (429) or failed by the server (5xx) or the network is
//! tried once more after `Retry-After` (at most 10 s) or half a second;
//! then it fails with `58030` (try again later: §M14.22 E4's queue
//! retries those, after the `Retry-After` it carries in its hint), while a refused key or a bad request fails with
//! `55000` (it will not succeed as defined).  A `url` of a model made by
//! a role other than an administrator must be in the database's
//! `model_urls` (nucleus checks it when the model is made).  The tokens each call used,
//! as the answer reports them, go to the statement's costs (§M14.22 E8).

use std::sync::Arc;
use std::time::Duration;

use nucleus_sql::ext::models::{Files, Input, ModelProvider, Options, Request, Spec};
use nucleus_sql::ext::{Error, Module, Registry, Result};
use serde_json::{Value as Json, json};

/// The module: registers the three providers.
pub struct Embeddings;

impl Module for Embeddings {
    fn name(&self) -> &str {
        "embeddings"
    }

    fn register(&self, r: &mut Registry) -> Result<()> {
        r.add_model_provider(Arc::new(OpenAi))?;
        r.add_model_provider(Arc::new(Cohere))?;
        r.add_model_provider(Arc::new(Voyage))
    }
}

/// The longest one wait for `Retry-After`.
const MAX_WAIT: Duration = Duration::from_secs(10);

/// The longest a call may take, answer included.
const MAX_CALL: Duration = Duration::from_secs(300);

/// POSTs `body` as JSON, once more when throttled or failed in passing,
/// all within the time the statement has `left`; the answer's JSON.
fn post(
    provider: &str,
    url: &str,
    key: Option<&str>,
    body: &Json,
    left: Option<Duration>,
) -> Result<Json> {
    let deadline = std::time::Instant::now() + left.map_or(MAX_CALL, |l| l.min(MAX_CALL));
    let bytes = serde_json::to_vec(body).map_err(|e| Error::internal(e.to_string()))?;
    let auth = key.map(|k| format!("Bearer {k}"));
    let mut headers = vec![
        ("Content-Type", "application/json"),
        ("Accept", "application/json"),
    ];
    if let Some(a) = &auth {
        headers.push(("Authorization", a.as_str()));
    }
    let mut attempt = 0;
    loop {
        attempt += 1;
        let r = nucleus_http::send(&nucleus_http::Request {
            method: "POST",
            url,
            headers: &headers,
            body: Some(&bytes),
            timeout: Duration::from_secs(60),
            total: Some(deadline.saturating_duration_since(std::time::Instant::now())),
            limit: 256 << 20,
            agent: "nucleus-embeddings",
        });
        let (wait, error) = match r {
            Ok(r) if r.status == 200 => {
                return serde_json::from_slice(&r.body).map_err(|e| {
                    Error::data_exception(format!("{provider} at {url}: an answer not JSON: {e}"))
                });
            }
            Ok(r) if r.status == 429 || r.status >= 500 => (
                r.retry_after(),
                Error::io(format!(
                    "{provider} at {url} answered {}: {}",
                    r.status,
                    r.body_text(300)
                )),
            ),
            Ok(r) => {
                return Err(Error::prerequisite(format!(
                    "{provider} at {url} refused the call ({}): {}",
                    r.status,
                    r.body_text(300)
                )));
            }
            Err(e) if e.transient => (None, Error::io(format!("{provider}: {e}"))),
            Err(e) => return Err(Error::prerequisite(format!("{provider}: {e}"))),
        };
        let pause = wait.unwrap_or(Duration::from_millis(500)).min(MAX_WAIT);
        if attempt >= 2 || std::time::Instant::now() + pause >= deadline {
            // The embedding columns' queue waits as long as asked.
            return Err(match wait {
                Some(w) => error.with_hint(nucleus_sql::ext::models::retry_hint(w)),
                None => error,
            });
        }
        std::thread::sleep(pause);
    }
}

fn floats(provider: &str, v: &Json) -> Result<Vec<f32>> {
    v.as_array()
        .and_then(|a| {
            a.iter()
                .map(|x| x.as_f64().map(|f| f as f32))
                .collect::<Option<Vec<f32>>>()
        })
        .ok_or_else(|| Error::data_exception(format!("{provider}: a vector that is not numbers")))
}

fn required<'a>(provider: &str, o: &'a Options, name: &str) -> Result<&'a str> {
    o.get(name).ok_or_else(|| {
        Error::invalid_parameter(format!("model provider \"{provider}\" needs option {name}"))
    })
}

/// `dimensions` as given, else the known model's, else an error.
fn dimensions(provider: &str, o: &Options, known: &[(&str, u32)]) -> Result<u32> {
    let model = required(provider, o, "model")?;
    if let Some(d) = o.number("dimensions", 1..=16_000)? {
        return Ok(d as u32);
    }
    known
        .iter()
        .find(|(m, _)| *m == model)
        .map(|(_, d)| *d)
        .ok_or_else(|| {
            Error::invalid_parameter(format!(
                "model provider \"{provider}\" does not know {model}'s dimensions: give option dimensions"
            ))
        })
}

fn url(o: &Options, default: &str) -> String {
    o.get("url")
        .unwrap_or(default)
        .trim_end_matches('/')
        .to_string()
}

/// The tokens an answer says the call used (OpenAI's and Voyage's
/// `usage.total_tokens`, Cohere's `meta.billed_units.input_tokens`), for
/// the statement's costs.
fn usage(r: &Request<'_>, tokens: &Json) {
    if let Some(n) = tokens.as_u64() {
        r.report_tokens(n);
    }
}

/// Answers in `data[]`, each with its `index` (OpenAI's and Voyage's
/// form), in the order of the texts.
fn by_index(provider: &str, answer: &Json, n: usize) -> Result<Vec<Vec<f32>>> {
    let data = answer["data"]
        .as_array()
        .ok_or_else(|| Error::data_exception(format!("{provider}: an answer without data")))?;
    let mut out: Vec<Option<Vec<f32>>> = vec![None; n];
    for (pos, d) in data.iter().enumerate() {
        let i = d["index"].as_u64().map_or(pos, |i| i as usize);
        let slot = out.get_mut(i).ok_or_else(|| {
            Error::data_exception(format!("{provider}: an answer for text {i} of {n}"))
        })?;
        *slot = Some(floats(provider, &d["embedding"])?);
    }
    out.into_iter()
        .map(|v| {
            v.ok_or_else(|| Error::data_exception(format!("{provider}: a text left unanswered")))
        })
        .collect()
}

/// OpenAI-compatible endpoints.
pub struct OpenAi;

const OPENAI_KNOWN: [(&str, u32); 3] = [
    ("text-embedding-3-small", 1536),
    ("text-embedding-3-large", 3072),
    ("text-embedding-ada-002", 1536),
];

impl ModelProvider for OpenAi {
    fn name(&self) -> &str {
        "openai"
    }

    fn urls(&self, o: &Options) -> Vec<String> {
        o.get("url").map(str::to_string).into_iter().collect()
    }

    fn define(&self, o: &Options, _files: &Files<'_>) -> Result<Spec> {
        o.only("openai", &["model", "url", "dimensions", "batch"])?;
        let dimensions = dimensions("openai", o, &OPENAI_KNOWN)?;
        let batch = o.number("batch", 1..=2048)?.unwrap_or(512) as usize;
        Ok(Spec {
            dimensions,
            max_batch: batch,
        })
    }

    fn embed(&self, r: &Request<'_>) -> Result<Vec<Vec<f32>>> {
        let model = required("openai", r.options, "model")?;
        let mut body = json!({
            "model": model,
            "input": r.texts,
            "encoding_format": "float",
        });
        // Only OpenAI's own v3 models take `dimensions`; other servers
        // may refuse a field they do not know.
        if model.starts_with("text-embedding-3") && r.options.get("dimensions").is_some() {
            body["dimensions"] = json!(r.dimensions);
        }
        let u = format!("{}/embeddings", url(r.options, "https://api.openai.com/v1"));
        let answer = post(
            "openai",
            &u,
            r.secret.map(|s| s.expose()),
            &body,
            r.time_left(),
        )?;
        usage(r, &answer["usage"]["total_tokens"]);
        by_index("openai", &answer, r.texts.len())
    }
}

/// Cohere's v2 `embed`.
pub struct Cohere;

const COHERE_KNOWN: [(&str, u32); 5] = [
    ("embed-v4.0", 1536),
    ("embed-english-v3.0", 1024),
    ("embed-multilingual-v3.0", 1024),
    ("embed-english-light-v3.0", 384),
    ("embed-multilingual-light-v3.0", 384),
];

impl ModelProvider for Cohere {
    fn name(&self) -> &str {
        "cohere"
    }

    fn urls(&self, o: &Options) -> Vec<String> {
        o.get("url").map(str::to_string).into_iter().collect()
    }

    fn define(&self, o: &Options, _files: &Files<'_>) -> Result<Spec> {
        o.only("cohere", &["model", "url", "dimensions", "batch"])?;
        let dimensions = dimensions("cohere", o, &COHERE_KNOWN)?;
        let batch = o.number("batch", 1..=96)?.unwrap_or(96) as usize;
        Ok(Spec {
            dimensions,
            max_batch: batch,
        })
    }

    fn embed(&self, r: &Request<'_>) -> Result<Vec<Vec<f32>>> {
        let model = required("cohere", r.options, "model")?;
        let mut body = json!({
            "model": model,
            "texts": r.texts,
            "input_type": match r.input {
                Input::Query => "search_query",
                Input::Document => "search_document",
            },
            "embedding_types": ["float"],
        });
        if r.options.get("dimensions").is_some() {
            body["output_dimension"] = json!(r.dimensions);
        }
        let u = format!("{}/v2/embed", url(r.options, "https://api.cohere.com"));
        let answer = post(
            "cohere",
            &u,
            r.secret.map(|s| s.expose()),
            &body,
            r.time_left(),
        )?;
        usage(r, &answer["meta"]["billed_units"]["input_tokens"]);
        let rows = answer["embeddings"]["float"]
            .as_array()
            .ok_or_else(|| Error::data_exception("cohere: an answer without embeddings.float"))?;
        if rows.len() != r.texts.len() {
            return Err(Error::data_exception(format!(
                "cohere: {} vectors for {} texts",
                rows.len(),
                r.texts.len()
            )));
        }
        rows.iter().map(|v| floats("cohere", v)).collect()
    }
}

/// Voyage AI.
pub struct Voyage;

const VOYAGE_KNOWN: [(&str, u32); 7] = [
    ("voyage-3.5", 1024),
    ("voyage-3.5-lite", 1024),
    ("voyage-3-large", 1024),
    ("voyage-3", 1024),
    ("voyage-3-lite", 512),
    ("voyage-code-3", 1024),
    ("voyage-multilingual-2", 1024),
];

impl ModelProvider for Voyage {
    fn name(&self) -> &str {
        "voyage"
    }

    fn urls(&self, o: &Options) -> Vec<String> {
        o.get("url").map(str::to_string).into_iter().collect()
    }

    fn define(&self, o: &Options, _files: &Files<'_>) -> Result<Spec> {
        o.only("voyage", &["model", "url", "dimensions", "batch"])?;
        let dimensions = dimensions("voyage", o, &VOYAGE_KNOWN)?;
        let batch = o.number("batch", 1..=1000)?.unwrap_or(128) as usize;
        Ok(Spec {
            dimensions,
            max_batch: batch,
        })
    }

    fn embed(&self, r: &Request<'_>) -> Result<Vec<Vec<f32>>> {
        let model = required("voyage", r.options, "model")?;
        let mut body = json!({
            "model": model,
            "input": r.texts,
            "input_type": r.input.word(),
        });
        if r.options.get("dimensions").is_some() {
            body["output_dimension"] = json!(r.dimensions);
        }
        let u = format!(
            "{}/v1/embeddings",
            url(r.options, "https://api.voyageai.com")
        );
        let answer = post(
            "voyage",
            &u,
            r.secret.map(|s| s.expose()),
            &body,
            r.time_left(),
        )?;
        usage(r, &answer["usage"]["total_tokens"]);
        by_index("voyage", &answer, r.texts.len())
    }
}
