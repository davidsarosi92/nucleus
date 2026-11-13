//! Models (PLAN.md §M14.22 E2b): text to vectors, by the database.
//!
//! `CREATE MODEL [IF NOT EXISTS] m USING provider WITH (option = value,
//! …)` defines one; `embed(text, m [, 'query' | 'document'])` calls it.
//! A *provider* is registered for the process ([`ModelProvider`],
//! `Registry::add_model_provider`, principle 12): built in, `hashing` (a
//! lexical vector by feature hashing, no network — tests, and a baseline
//! that needs nothing); the `embeddings` module adds OpenAI-compatible
//! endpoints, Cohere and Voyage; §M14.22 E3 a local encoder.
//!
//! The provider checks its options and says what the model makes (its
//! dimensions, its batch); three options are the core's, for every
//! provider: `secret` (a secret, §[`crate::secrets`], whose value the
//! provider gets when it calls — naming one needs `USAGE` on it, so a
//! model cannot carry a secret its maker may not use to an address of
//! their choosing), `query_prefix` and `document_prefix` (put before
//! the text of a query or of a document: e5's `query: ` and `passage: `,
//! bge's instruction — a model knows how its queries differ, so callers
//! cannot forget).  A provider whose API distinguishes the two itself
//! (Cohere's and Voyage's `input_type`) is told which a call is.
//!
//! Calling a model needs `USAGE` on it (its owner, an administrator,
//! whom they grant: `GRANT USAGE ON MODEL m TO r`).  Vectors are kept by
//! what made them — the model's definition, query or document, the text
//! (SHA-256 of them) — in a bounded cache for the process, so a query's
//! vector in `ORDER BY v <=> embed($1, m)` is computed once, not a row at a
//! time, and the same text is never sent twice while it is kept.
//!
//! A provider may *fetch* files when a model is made — a local model's
//! tokenizer and weights, from a directory or a repository — the only
//! time it reads files or the network outside a call.  They are kept in
//! the database with the model, by content: `00 16 <SHA-256> <chunk>`, in
//! chunks of 32 KiB, so a backup or a replica has them, two models made
//! from the same file store it once, and a call never needs the network
//! again.  A call reads them through [`Files`] (a provider keeps what it
//! built from them by [`Files::id`]).
//!
//! What calls cost is bounded and seen (§M14.22 E8).  Four more core
//! options, for any provider: `batch_tokens` (the most tokens a call
//! sends — texts are packed by both it and the provider's batch),
//! `requests_per_minute` and `tokens_per_minute` (a token bucket for the
//! process, per model, as full as the minute's limit and refilled
//! steadily: a statement waits for it, within its deadline, while the
//! embedding columns' workers are told when to come back — `53400`, its
//! hint the wait), and `parallel` (calls a request may make at once, for
//! a remote model: a local one uses the threads it is given).  Tokens are
//! the provider's count where it reports one (the local model's own
//! tokenizer, OpenAI's and Voyage's `usage`, Cohere's billed units),
//! else estimated as a UTF-8 byte in three.  Each statement is charged
//! for its calls — calls, texts, cache hits, tokens, errors, time — in
//! `EXPLAIN ANALYZE` and the statement statistics.
//!
//! Records are in the catalog, `00 15 <name>`.

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use nucleus_kv::KvTxn;

use crate::access;
use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};
use crate::ext::{ArgInfo, CallContext, Registry, Returns, ScalarFunction, Signature, Volatility};
use crate::secrets::Revealed;
use crate::types::{DataType, Value};
use crate::vector::{Sel, Vector};

const MODELS: [u8; 2] = [0x00, 0x15];
const FILES: [u8; 2] = [0x00, 0x16];
/// 2: with the model's files.
const VERSION: u8 = 3;
/// A stored file's chunk.
const FILE_CHUNK: usize = 32 * 1024;

/// The most components a model's vectors may have (pgvector's limit for
/// `vector`).
pub const MAX_DIMENSIONS: u32 = 16_000;

/// Whether a text is a query or a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Input {
    /// A query: what is searched for.
    Query,
    /// A document: what is searched.
    Document,
}

impl Input {
    /// Its word.
    pub fn word(self) -> &'static str {
        match self {
            Input::Query => "query",
            Input::Document => "document",
        }
    }

    fn parse(s: &str) -> Result<Input> {
        match s.to_ascii_lowercase().as_str() {
            "query" => Ok(Input::Query),
            "document" | "passage" => Ok(Input::Document),
            _ => Err(Error::invalid_parameter(format!(
                "an embedding's input is 'query' or 'document', not '{s}'"
            ))),
        }
    }
}

/// A model's options as written (`WITH (…)`), without the core's own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options(pub Vec<(String, String)>);

impl Options {
    /// The value of `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The value of `name`, a whole number in `range`.
    pub fn number(&self, name: &str, range: RangeInclusive<u64>) -> Result<Option<u64>> {
        let Some(v) = self.get(name) else {
            return Ok(None);
        };
        match v.parse::<u64>() {
            Ok(n) if range.contains(&n) => Ok(Some(n)),
            _ => Err(Error::invalid_parameter(format!(
                "option {name} must be a whole number from {} to {}, not {v}",
                range.start(),
                range.end()
            ))),
        }
    }

    /// `42601`-like refusal of options a provider does not know.
    pub fn only(&self, provider: &str, known: &[&str]) -> Result<()> {
        if let Some((k, _)) = self.0.iter().find(|(k, _)| !known.contains(&k.as_str())) {
            return Err(Error::invalid_parameter(format!(
                "model provider \"{provider}\" has no option {k} (it has: {})",
                known.join(", ")
            )));
        }
        Ok(())
    }
}

/// What a model makes, as its provider says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    /// The components of its vectors.
    pub dimensions: u32,
    /// The most texts one call takes.
    pub max_batch: usize,
}

/// One call: texts of one kind, for one model.
pub struct Request<'a> {
    /// The model's provider options.
    pub options: &'a Options,
    /// The model's dimensions.
    pub dimensions: u32,
    /// The secret its definition names, revealed.
    pub secret: Option<&'a Revealed>,
    /// Query or document (the core has put the model's prefix before
    /// each text already).
    pub input: Input,
    /// The texts, at most the model's batch.
    pub texts: &'a [String],
    /// The files the model was made from (none for most providers).
    pub files: &'a Files<'a>,
    /// The tokens the provider reported ([`Request::report_tokens`]).
    tokens: AtomicU64,
    /// When the statement that makes the call must end, if it must.
    deadline: Option<std::time::Instant>,
}

impl Request<'_> {
    /// The tokens the call used, as the provider counts or bills them: in
    /// the statement's costs and the model's statistics, in place of the
    /// core's estimate.
    pub fn report_tokens(&self, n: u64) {
        self.tokens.fetch_add(n, Ordering::Relaxed);
    }

    /// The time the statement making the call has left (`None`: it has
    /// no time limit — a background worker's call, or a statement
    /// without `statement_timeout`): a remote call should give up by
    /// then, as the statement will.
    pub fn time_left(&self) -> Option<std::time::Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(std::time::Instant::now()))
    }
}

/// A file a model keeps: its name, the SHA-256 of its bytes, their
/// number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRef {
    /// Its name (`tokenizer.json`, `model.safetensors`, …).
    pub name: String,
    /// The SHA-256 of its bytes: where it is stored.
    pub digest: [u8; 32],
    /// Its length.
    pub len: u64,
}

enum Source<'a> {
    Memory(&'a [(String, Vec<u8>)]),
    Kv(&'a dyn KvTxn),
}

/// A model's files, by name: their bytes in hand while it is made, read
/// from the database when a call needs them.
pub struct Files<'a> {
    list: Vec<FileRef>,
    source: Source<'a>,
}

impl<'a> Files<'a> {
    /// No files.
    pub fn none() -> Files<'static> {
        Files {
            list: Vec::new(),
            source: Source::Memory(&[]),
        }
    }

    /// Files in hand (as a provider fetched them).
    pub fn in_memory(files: &'a [(String, Vec<u8>)]) -> Files<'a> {
        Files {
            list: files
                .iter()
                .map(|(name, bytes)| FileRef {
                    name: name.clone(),
                    digest: nucleus_crypto::sha256(bytes),
                    len: bytes.len() as u64,
                })
                .collect(),
            source: Source::Memory(files),
        }
    }

    fn stored(list: Vec<FileRef>, txn: &'a dyn KvTxn) -> Files<'a> {
        Files {
            list,
            source: Source::Kv(txn),
        }
    }

    /// The files, in the order they were fetched.
    pub fn list(&self) -> &[FileRef] {
        &self.list
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// What identifies these files together: the SHA-256 of their names
    /// and digests (what a provider keeps a loaded model by).
    pub fn id(&self) -> [u8; 32] {
        let mut b = Vec::new();
        for f in &self.list {
            b.extend_from_slice(&(f.name.len() as u32).to_le_bytes());
            b.extend_from_slice(f.name.as_bytes());
            b.extend_from_slice(&f.digest);
        }
        nucleus_crypto::sha256(&b)
    }

    /// The bytes of file `name`, if the model has it; read from the
    /// database, and their digest checked, unless they are in hand.
    pub fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(f) = self.list.iter().find(|f| f.name == name) else {
            return Ok(None);
        };
        match self.source {
            Source::Memory(m) => Ok(m.iter().find(|(n, _)| n == name).map(|(_, b)| b.clone())),
            Source::Kv(txn) => {
                let (start, end) = file_range(&f.digest);
                let mut out = Vec::with_capacity(f.len as usize);
                for e in txn.scan(
                    std::ops::Bound::Included(&start[..]),
                    std::ops::Bound::Excluded(&end[..]),
                )? {
                    out.extend_from_slice(&e?.1);
                }
                if out.len() as u64 != f.len || nucleus_crypto::sha256(&out) != f.digest {
                    return Err(Error::internal(format!(
                        "a model's file {name} is damaged in the database"
                    )));
                }
                Ok(Some(out))
            }
        }
    }
}

fn file_key(digest: &[u8; 32], chunk: u32) -> Vec<u8> {
    let mut k = FILES.to_vec();
    k.extend_from_slice(digest);
    k.extend_from_slice(&chunk.to_be_bytes());
    k
}

/// The keys of a stored file's chunks.
fn file_range(digest: &[u8; 32]) -> (Vec<u8>, Vec<u8>) {
    let mut p = FILES.to_vec();
    p.extend_from_slice(digest);
    let end = nucleus_kv::prefix_end(&p).expect("file prefix");
    (p, end)
}

/// Stores `bytes` under its digest, unless a model stored it already.
fn store_file(txn: &mut dyn KvTxn, digest: &[u8; 32], bytes: &[u8]) -> Result<()> {
    if txn.get(&file_key(digest, 0))?.is_some() {
        return Ok(());
    }
    if bytes.is_empty() {
        txn.put(&file_key(digest, 0), &[])?;
    }
    for (i, c) in bytes.chunks(FILE_CHUNK).enumerate() {
        txn.put(&file_key(digest, i as u32), c)?;
    }
    Ok(())
}

/// A model provider (principle 12).
pub trait ModelProvider: Send + Sync {
    /// The name `CREATE MODEL … USING` names.
    fn name(&self) -> &str;

    /// The files a model is made from and keeps (a local model's
    /// tokenizer and weights), fetched when it is made — the only time a
    /// provider may read files or the network outside a call.  None by
    /// default.
    fn fetch(&self, options: &Options) -> Result<Vec<(String, Vec<u8>)>> {
        let _ = options;
        Ok(Vec::new())
    }

    /// Checks a model's options — every one it does not know refused —
    /// and its fetched files, and says what the model makes.  Called when
    /// a model is made; must not reach the network.
    fn define(&self, options: &Options, files: &Files<'_>) -> Result<Spec>;

    /// The vectors of `request.texts`, in order, each of
    /// `request.dimensions` components.
    fn embed(&self, request: &Request<'_>) -> Result<Vec<Vec<f32>>>;

    /// The addresses `options` point a model at, beside the provider's
    /// own (an OpenAI-compatible server's `url`): a model made by a role
    /// other than an administrator may name only those in the database's
    /// `model_urls`, so it cannot reach the server's own network.
    fn urls(&self, options: &Options) -> Vec<String> {
        let _ = options;
        Vec::new()
    }

    /// Whether its calls reach the network (§M14.22 E4: such calls never
    /// run inside a writing transaction).
    fn remote(&self) -> bool {
        true
    }

    /// The model's tokens in each of `request.texts` — its tokenizer's,
    /// without the prefix and the special tokens — for chunks sized in
    /// them (§M14.22 E6), or `None`: it cannot count them (a remote
    /// model's tokenizer is its server's).  Must not reach the network.
    fn count_tokens(&self, request: &Request<'_>) -> Result<Option<Vec<usize>>> {
        let _ = request;
        Ok(None)
    }
}

fn providers() -> &'static RwLock<HashMap<String, Arc<dyn ModelProvider>>> {
    static P: OnceLock<RwLock<HashMap<String, Arc<dyn ModelProvider>>>> = OnceLock::new();
    P.get_or_init(|| {
        let mut m: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        m.insert("hashing".into(), Arc::new(Hashing));
        RwLock::new(m)
    })
}

/// Registers a provider for the process (`Registry::add_model_provider`).
/// The same one again (by name) is no error: registries are built more
/// than once a process.
pub fn register(p: Arc<dyn ModelProvider>) -> Result<()> {
    providers()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(p.name().to_string(), p);
    Ok(())
}

/// The provider called `name`.
pub fn provider(name: &str) -> Result<Arc<dyn ModelProvider>> {
    let all = providers().read().unwrap_or_else(|e| e.into_inner());
    all.get(name).cloned().ok_or_else(|| {
        let mut names: Vec<&str> = all.keys().map(String::as_str).collect();
        names.sort_unstable();
        Error::undefined_object(format!(
            "model provider \"{name}\" does not exist (there are: {})",
            names.join(", ")
        ))
    })
}

/// The registered providers' names, sorted.
pub fn provider_names() -> Vec<String> {
    let mut v: Vec<String> = providers()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    v.sort_unstable();
    v
}

/// A model, as the catalog keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    /// Its name.
    pub name: String,
    /// The role that made it.
    pub owner: String,
    /// Its provider.
    pub provider: String,
    /// The provider's options.
    pub options: Options,
    /// The secret it calls with.
    pub secret: Option<String>,
    /// Put before a query's text.
    pub query_prefix: Option<String>,
    /// Put before a document's text.
    pub document_prefix: Option<String>,
    /// Its vectors' components.
    pub dimensions: u32,
    /// The most texts a call.
    pub max_batch: u32,
    /// When it was made (µs since 2000-01-01 UTC).
    pub created: i64,
    /// The files it was made from, stored by content.
    pub files: Vec<FileRef>,
    /// What its calls may spend.
    pub limits: Limits,
}

/// What a model's calls may spend: the core's options `batch_tokens`,
/// `requests_per_minute`, `tokens_per_minute` and `parallel` (0: no limit;
/// 1 call at a time).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    /// The most tokens a call sends.
    pub batch_tokens: u32,
    /// The most calls a minute.
    pub requests_per_minute: u32,
    /// The most tokens a minute.
    pub tokens_per_minute: u32,
    /// The most calls at once (a remote model's).
    pub parallel: u32,
}

/// The core's limit options, by name.
const LIMITS: [&str; 4] = [
    "batch_tokens",
    "requests_per_minute",
    "tokens_per_minute",
    "parallel",
];

impl Limits {
    fn set(&mut self, name: &str, v: u32) {
        match name {
            "batch_tokens" => self.batch_tokens = v,
            "requests_per_minute" => self.requests_per_minute = v,
            "tokens_per_minute" => self.tokens_per_minute = v,
            _ => self.parallel = v,
        }
    }

    /// Each limit set, by name (`nc_catalog.models`).
    pub fn get(&self, name: &str) -> u32 {
        match name {
            "batch_tokens" => self.batch_tokens,
            "requests_per_minute" => self.requests_per_minute,
            "tokens_per_minute" => self.tokens_per_minute,
            _ => self.parallel,
        }
    }
}

fn key_of(name: &str) -> Vec<u8> {
    let mut k = MODELS.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

impl Model {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(VERSION)
            .str(&self.name)
            .str(&self.owner)
            .str(&self.provider)
            .u32(self.options.0.len() as u32);
        for (k, v) in &self.options.0 {
            w.str(k).str(v);
        }
        w.opt_str(self.secret.as_deref())
            .opt_str(self.query_prefix.as_deref())
            .opt_str(self.document_prefix.as_deref())
            .u32(self.dimensions)
            .u32(self.max_batch)
            .u64(self.created as u64)
            .u32(self.files.len() as u32);
        for f in &self.files {
            w.str(&f.name);
            w.0.extend_from_slice(&f.digest);
            w.u64(f.len);
        }
        for l in LIMITS {
            w.u32(self.limits.get(l));
        }
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let version = r.u8()?;
        if !(1..=VERSION).contains(&version) {
            return Err(Error::internal("unknown model record version"));
        }
        let name = r.str()?;
        let owner = r.str()?;
        let provider = r.str()?;
        let n = r.u32()? as usize;
        let mut options = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            options.push((r.str()?, r.str()?));
        }
        let mut m = Model {
            name,
            owner,
            provider,
            options: Options(options),
            secret: r.opt_str()?,
            query_prefix: r.opt_str()?,
            document_prefix: r.opt_str()?,
            dimensions: r.u32()?,
            max_batch: r.u32()?,
            created: r.u64()? as i64,
            files: Vec::new(),
            limits: Limits::default(),
        };
        if version >= 2 {
            let n = r.u32()? as usize;
            for _ in 0..n {
                let name = r.str()?;
                let digest: [u8; 32] = r.take(32)?.try_into().expect("32 bytes");
                m.files.push(FileRef {
                    name,
                    digest,
                    len: r.u64()?,
                });
            }
        }
        if version >= 3 {
            for l in LIMITS {
                let v = r.u32()?;
                m.limits.set(l, v);
            }
        }
        Ok(m)
    }

    fn prefix(&self, input: Input) -> Option<&str> {
        match input {
            Input::Query => self.query_prefix.as_deref(),
            Input::Document => self.document_prefix.as_deref(),
        }
    }
}

/// The model called `name`.
pub fn get(txn: &dyn KvTxn, name: &str) -> Result<Option<Model>> {
    txn.get(&key_of(name))?
        .map(|b| Model::decode(&b))
        .transpose()
}

/// Every model, by name.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Model>> {
    let end = nucleus_kv::prefix_end(&MODELS).expect("model prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&MODELS[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        out.push(Model::decode(&e?.1)?);
    }
    Ok(out)
}

fn put(txn: &mut dyn KvTxn, m: &Model) -> Result<()> {
    txn.put(&key_of(&m.name), &m.encode())?;
    Ok(())
}

fn existing(txn: &dyn KvTxn, name: &str) -> Result<Model> {
    crate::catalog_cache::memo(txn, crate::catalog_cache::Part::Model, name, || {
        get(txn, name)
    })?
    .ok_or_else(|| Error::undefined_object(format!("model \"{name}\" does not exist")))
}

/// `CREATE MODEL [IF NOT EXISTS] name USING provider WITH (…)`.
pub fn create(
    txn: &mut dyn KvTxn,
    name: &str,
    provider_name: &str,
    options: &[(String, String)],
    if_not_exists: bool,
) -> Result<()> {
    if get(&*txn, name)?.is_some() {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::duplicate_object(format!(
            "model \"{name}\" already exists"
        )));
    }
    let p = provider(provider_name)?;
    let mut rest = Vec::new();
    let (mut secret, mut query_prefix, mut document_prefix) = (None, None, None);
    let mut limits = Limits::default();
    let mut seen_limits = Vec::new();
    for (k, v) in options {
        if LIMITS.contains(&k.as_str()) {
            if seen_limits.contains(k) {
                return Err(Error::invalid_parameter(format!("option {k} given twice")));
            }
            seen_limits.push(k.clone());
            let n = Options(vec![(k.clone(), v.clone())])
                .number(k, 1..=u64::from(u32::MAX))?
                .expect("given");
            limits.set(k, n as u32);
            continue;
        }
        let slot = match k.as_str() {
            "secret" => &mut secret,
            "query_prefix" => &mut query_prefix,
            "document_prefix" => &mut document_prefix,
            _ => {
                if rest.iter().any(|(r, _): &(String, String)| r == k) {
                    return Err(Error::invalid_parameter(format!("option {k} given twice")));
                }
                rest.push((k.clone(), v.clone()));
                continue;
            }
        };
        if slot.replace(v.clone()).is_some() {
            return Err(Error::invalid_parameter(format!("option {k} given twice")));
        }
    }
    if let Some(s) = &secret {
        if crate::secrets::get(&*txn, s)?.is_none() {
            return Err(Error::undefined_object(format!(
                "secret \"{s}\" does not exist"
            )));
        }
        crate::privileges::Checker::new().require(
            &*txn,
            crate::privileges::Kind::Secret,
            s,
            crate::privileges::USAGE,
            None,
        )?;
    }
    let options = Options(rest);
    allowed_urls(&*txn, &*p, &options, access::current().superuser)?;
    let fetched = p.fetch(&options)?;
    let files = Files::in_memory(&fetched);
    let spec = p.define(&options, &files)?;
    if spec.dimensions == 0 || spec.dimensions > MAX_DIMENSIONS {
        return Err(Error::invalid_parameter(format!(
            "a model's vectors have 1 to {MAX_DIMENSIONS} components, not {}",
            spec.dimensions
        )));
    }
    let m = Model {
        name: name.to_string(),
        owner: access::current().user.clone(),
        provider: provider_name.to_string(),
        options,
        secret,
        query_prefix,
        document_prefix,
        dimensions: spec.dimensions,
        max_batch: spec.max_batch.clamp(1, u32::MAX as usize) as u32,
        created: crate::datetime::now()?,
        files: files.list().to_vec(),
        limits,
    };
    for (f, (_, bytes)) in m.files.iter().zip(&fetched) {
        store_file(txn, &f.digest, bytes)?;
    }
    put(txn, &m)
}

/// A URL's scheme, host (with its port; both in lower case) and path —
/// `None` for one that is not `http://` or `https://`.
pub fn split_url(url: &str) -> Option<(String, String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (host, path) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    Some((scheme, host.to_ascii_lowercase(), path.to_string()))
}

/// Whether `url` is under one of `prefixes`: the same scheme and host,
/// and a path that starts with the prefix's at a `/` (so
/// `https://api.example.com` does not admit `https://api.example.com.evil`,
/// nor `…/v1` admit `…/v10`).
pub fn url_under(url: &str, prefixes: &str) -> bool {
    let Some((scheme, host, path)) = split_url(url) else {
        return false;
    };
    prefixes
        .split(',')
        .map(str::trim)
        .filter_map(split_url)
        .any(|(s, h, p)| {
            let p = p.trim_end_matches('/');
            s == scheme
                && h == host
                && path.starts_with(p)
                && matches!(
                    path.as_bytes().get(p.len()),
                    None | Some(b'/' | b'?' | b'#')
                )
        })
}

/// The addresses a model's options name ([`ModelProvider::urls`]): an
/// administrator's may be any (`admin`: whoever makes it, or owns it
/// when it is called); anyone else's must be in the database's
/// `model_urls` (`42501`) — when it is made, and at every call, so a
/// URL taken off the list is not called again.
fn allowed_urls(
    txn: &dyn KvTxn,
    p: &dyn ModelProvider,
    options: &Options,
    admin: bool,
) -> Result<()> {
    let urls = p.urls(options);
    if urls.is_empty() || admin {
        return Ok(());
    }
    let allowed = crate::dbsettings::get(txn, "model_urls")?;
    match urls.iter().find(|u| !url_under(u, &allowed)) {
        None => Ok(()),
        Some(u) => Err(Error::insufficient_privilege(format!(
            "a model may call {u} only if it is in the database's model_urls"
        ))
        .with_hint("an administrator adds it: ALTER DATABASE nucleus SET model_urls = '<url>, …'")),
    }
}

/// `DROP MODEL [IF EXISTS] name`; its grants go with it.
pub fn drop(txn: &mut dyn KvTxn, name: &str, if_exists: bool) -> Result<()> {
    if get(&*txn, name)?.is_none() {
        if if_exists {
            return Ok(());
        }
        return Err(Error::undefined_object(format!(
            "model \"{name}\" does not exist"
        )));
    }
    let users = crate::embedding::using_model(&*txn, name)?;
    if !users.is_empty() {
        return Err(Error::dependent_objects(format!(
            "cannot drop model \"{name}\": embedding columns or chunk tables of {} use it",
            users.join(", ")
        )));
    }
    let gone = get(&*txn, name)?.map(|m| m.files).unwrap_or_default();
    txn.delete(&key_of(name))?;
    // Its files, unless another model keeps them too.
    let kept: Vec<[u8; 32]> = all(&*txn)?
        .into_iter()
        .flat_map(|m| m.files.into_iter().map(|f| f.digest))
        .collect();
    for f in gone {
        if !kept.contains(&f.digest) {
            let (start, end) = file_range(&f.digest);
            crate::catalog::delete_range(txn, &start, Some(&end))?;
        }
    }
    crate::privileges::forget(txn, crate::privileges::Kind::Model, name)
}

/// The models that name secret `secret` (it is not dropped under them).
pub fn using_secret(txn: &dyn KvTxn, secret: &str) -> Result<Vec<String>> {
    Ok(all(txn)?
        .into_iter()
        .filter(|m| m.secret.as_deref() == Some(secret))
        .map(|m| m.name)
        .collect())
}

/// A role renamed: the models it owns follow.
pub fn rename_owner(txn: &mut dyn KvTxn, from: &str, to: &str) -> Result<()> {
    for mut m in all(&*txn)? {
        if m.owner == from {
            m.owner = to.to_string();
            put(txn, &m)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ cache

/// The most components the cache keeps (32 MB of `f32`).
const CACHE_FLOATS: usize = 8 << 20;

struct Cache {
    map: HashMap<[u8; 32], (Arc<[f32]>, u64)>,
    floats: usize,
    tick: u64,
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(|| {
        Mutex::new(Cache {
            map: HashMap::new(),
            floats: 0,
            tick: 0,
        })
    })
}

fn cache_key(model: &[u8], input: Input, text: &str) -> [u8; 32] {
    let mut m = Vec::with_capacity(model.len() + text.len() + 16);
    m.extend_from_slice(&(model.len() as u64).to_le_bytes());
    m.extend_from_slice(model);
    m.push(input as u8);
    m.extend_from_slice(text.as_bytes());
    nucleus_crypto::sha256(&m)
}

fn cached(key: &[u8; 32]) -> Option<Arc<[f32]>> {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    c.tick += 1;
    let tick = c.tick;
    let e = c.map.get_mut(key)?;
    e.1 = tick;
    Some(e.0.clone())
}

fn keep(key: [u8; 32], v: Arc<[f32]>) {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if v.len() > CACHE_FLOATS / 4 {
        return;
    }
    c.tick += 1;
    let tick = c.tick;
    c.floats += v.len();
    if let Some((old, _)) = c.map.insert(key, (v, tick)) {
        c.floats -= old.len();
    }
    if c.floats > CACHE_FLOATS {
        // The older half goes.
        let mut ticks: Vec<u64> = c.map.values().map(|(_, t)| *t).collect();
        ticks.sort_unstable();
        let cut = ticks[ticks.len() / 2];
        c.map.retain(|_, (_, t)| *t > cut);
        c.floats = c.map.values().map(|(v, _)| v.len()).sum();
    }
}

/// Forgets every kept vector (tests; an erasure, PLAN.md §M14.24 R2b: the
/// cache is keyed by a hash of each text, so a deleted row's cannot be
/// picked out).
pub fn clear_cache() {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    c.map.clear();
    c.floats = 0;
}

// ------------------------------------------------------------------ calls

/// Statistics of the process's model calls (`nc_catalog.models`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Calls {
    /// Calls to the provider.
    pub calls: u64,
    /// Texts sent.
    pub texts: u64,
    /// Texts answered from the cache.
    pub cached: u64,
    /// Calls that failed.
    pub errors: u64,
    /// Time in the provider's calls (µs).
    pub micros: u64,
    /// Tokens sent ([`Request::report_tokens`], else estimated).
    pub tokens: u64,
    /// Time waited for the model's rate limits (µs).
    pub throttled: u64,
}

fn calls() -> &'static Mutex<HashMap<String, Calls>> {
    static C: OnceLock<Mutex<HashMap<String, Calls>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The calls made to model `name` since the process started.
pub fn calls_of(name: &str) -> Calls {
    calls()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .copied()
        .unwrap_or_default()
}

fn count(name: &str, f: impl FnOnce(&mut Calls)) {
    f(calls()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(name.to_string())
        .or_default());
}

/// The vectors of `texts` by model `name`, in order: `USAGE` on it
/// checked, the cache read first, the rest sent in batches.
pub fn embed(txn: &dyn KvTxn, name: &str, texts: &[&str], input: Input) -> Result<Vec<Arc<[f32]>>> {
    existing(txn, name)?;
    crate::privileges::Checker::new().require(
        txn,
        crate::privileges::Kind::Model,
        name,
        crate::privileges::USAGE,
        None,
    )?;
    embed_unchecked(txn, name, texts, input, true)
}

/// The hint of an error a provider's `Retry-After` came with: when to
/// call again (what the embedding queue obeys, §[`crate::embedding`]).
pub fn retry_hint(after: std::time::Duration) -> String {
    format!(
        "try again in {} ms (the provider's Retry-After)",
        after.as_millis()
    )
}

/// The wait an error's [`retry_hint`] asks for.
pub fn retry_after(e: &Error) -> Option<std::time::Duration> {
    let h = e.hint.as_deref()?.strip_prefix("try again in ")?;
    let ms = h.split(' ').next()?.parse().ok()?;
    Some(std::time::Duration::from_millis(ms))
}

/// Whether `e` is a model's own rate limit refusing a worker's call (not
/// its provider failing).
pub fn throttled(e: &Error) -> bool {
    e.code == "53400"
        && e.hint
            .as_deref()
            .is_some_and(|h| h.ends_with("(the model's rate limit)"))
}

/// The tokens of `texts` by model `name` (`USAGE` on it checked), as
/// its provider counts them ([`ModelProvider::count_tokens`]).
pub fn count_tokens(txn: &dyn KvTxn, name: &str, texts: &[&str]) -> Result<Vec<usize>> {
    existing(txn, name)?;
    crate::privileges::Checker::new().require(
        txn,
        crate::privileges::Kind::Model,
        name,
        crate::privileges::USAGE,
        None,
    )?;
    count_tokens_unchecked(txn, name, texts)
}

/// [`count_tokens`] without the privilege check: for a chunk table's
/// writes, whose maker was checked when they made it (its writers need
/// not be granted the model).
pub(crate) fn count_tokens_unchecked(
    txn: &dyn KvTxn,
    name: &str,
    texts: &[&str],
) -> Result<Vec<usize>> {
    let m = existing(txn, name)?;
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let p = provider(&m.provider)?;
    let files = Files::stored(m.files.clone(), txn);
    let batch: Vec<String> = texts.iter().map(|t| t.to_string()).collect();
    let counted = p.count_tokens(&Request {
        options: &m.options,
        dimensions: m.dimensions,
        secret: None,
        input: Input::Document,
        texts: &batch,
        files: &files,
        tokens: AtomicU64::new(0),
        deadline: None,
    })?;
    match counted {
        Some(n) if n.len() == texts.len() => Ok(n),
        Some(n) => Err(Error::data_exception(format!(
            "model \"{name}\" counted {} texts of {}",
            n.len(),
            texts.len()
        ))),
        None => Err(Error::feature(format!(
            "model \"{name}\" (provider {}) cannot count its tokens here; give chunks() no model to count words",
            m.provider
        ))),
    }
}

/// [`embed`] without the privilege check: for the embedding columns'
/// workers, whose columns' makers were checked when they made them.  A
/// call over the model's rate limits waits (`wait`: a statement's, up to
/// its deadline) or fails at once with `53400`, its hint the wait (a
/// worker's: the queue comes back then).
pub(crate) fn embed_unchecked(
    txn: &dyn KvTxn,
    name: &str,
    texts: &[&str],
    input: Input,
    wait: bool,
) -> Result<Vec<Arc<[f32]>>> {
    let m = existing(txn, name)?;
    let record = m.encode();
    let prefix = m.prefix(input).unwrap_or("");
    let keys: Vec<[u8; 32]> = texts.iter().map(|t| cache_key(&record, input, t)).collect();
    let mut out: Vec<Option<Arc<[f32]>>> = keys.iter().map(cached).collect();
    let missing: Vec<usize> = (0..texts.len()).filter(|&i| out[i].is_none()).collect();
    let hits = (texts.len() - missing.len()) as u64;
    count(&m.name, |c| c.cached += hits);
    let mut cost = crate::budget::ModelCost {
        cached: hits,
        ..Default::default()
    };
    let r = (|| {
        if missing.is_empty() {
            return Ok(());
        }
        let p = provider(&m.provider)?;
        // An owner without a role record is the unclaimed database's
        // administrator (a role that owns a model is not dropped).
        let admin = || -> Result<bool> {
            Ok(crate::roles::get(txn, &m.owner)?.is_none_or(|r| r.superuser))
        };
        if !p.urls(&m.options).is_empty() {
            allowed_urls(txn, &*p, &m.options, admin()?)?;
        }
        let secret = m
            .secret
            .as_deref()
            .map(|s| crate::secrets::reveal(txn, s))
            .transpose()?;
        let files = Files::stored(m.files.clone(), txn);
        // The same text twice in one call is sent once: the first of it.
        let mut first: HashMap<&[u8; 32], usize> = HashMap::new();
        let mut send: Vec<usize> = Vec::new();
        for &i in &missing {
            if let std::collections::hash_map::Entry::Vacant(e) = first.entry(&keys[i]) {
                e.insert(i);
                send.push(i);
            }
        }
        let sent: Vec<String> = send
            .iter()
            .map(|&i| format!("{prefix}{}", texts[i]))
            .collect();
        let tokens = estimate(&*p, &m, &sent, &files)?;
        let batches = pack(&tokens, m.max_batch.max(1) as usize, m.limits.batch_tokens);
        let throttle = Throttle {
            clock: crate::datetime::current_kv(),
            deadline: if wait {
                Some(crate::budget::deadline())
            } else {
                None
            },
        };
        // The statement's time, read here: the calls may run on threads of
        // their own, outside it.
        let deadline = crate::budget::time_left().map(|t| std::time::Instant::now() + t);
        let call = |b: &std::ops::Range<usize>, files: &Files<'_>| {
            let est: u64 = tokens[b.clone()].iter().sum();
            let mut cost = crate::budget::ModelCost::default();
            let r = (|| {
                cost.micros += throttle.acquire(&m, est)?;
                let request = Request {
                    options: &m.options,
                    dimensions: m.dimensions,
                    secret: secret.as_ref(),
                    input,
                    texts: &sent[b.clone()],
                    files,
                    tokens: AtomicU64::new(0),
                    deadline,
                };
                let started = std::time::Instant::now();
                let r = p.embed(&request);
                let micros = started.elapsed().as_micros() as u64;
                cost.calls += 1;
                cost.micros += micros;
                match &r {
                    Ok(_) => {
                        cost.texts += b.len() as u64;
                        cost.tokens += match request.tokens.load(Ordering::Relaxed) {
                            0 => est,
                            n => n,
                        };
                    }
                    Err(_) => cost.errors += 1,
                }
                r
            })();
            (r, cost)
        };
        let parallel = (m.limits.parallel as usize).min(batches.len());
        let answers: Vec<(Result<Vec<Vec<f32>>>, crate::budget::ModelCost)> =
            if parallel > 1 && m.files.is_empty() && !cfg!(target_os = "wasi") {
                // A remote model's calls at once: each thread takes the
                // next batch; the answers kept in order.
                let next = AtomicUsize::new(0);
                let slots: Vec<Mutex<Option<_>>> =
                    (0..batches.len()).map(|_| Mutex::new(None)).collect();
                std::thread::scope(|sc| {
                    for _ in 0..parallel {
                        sc.spawn(|| {
                            let files = Files::none();
                            loop {
                                let i = next.fetch_add(1, Ordering::Relaxed);
                                let Some(b) = batches.get(i) else { break };
                                let a = call(b, &files);
                                *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(a);
                            }
                        });
                    }
                });
                slots
                    .into_iter()
                    .map(|s| {
                        s.into_inner()
                            .unwrap_or_else(|e| e.into_inner())
                            .expect("every batch called")
                    })
                    .collect()
            } else {
                let mut answers = Vec::with_capacity(batches.len());
                for b in &batches {
                    let a = call(b, &files);
                    let failed = a.0.is_err();
                    answers.push(a);
                    if failed {
                        break;
                    }
                }
                answers
            };
        let mut failure = None;
        for (b, (r, c)) in batches.iter().zip(answers) {
            cost.add(&c);
            count(&m.name, |k| {
                k.calls += c.calls;
                k.texts += c.texts;
                k.errors += c.errors;
                k.micros += c.micros;
                k.tokens += c.tokens;
            });
            // Every batch answered is kept, whatever another's fate: the
            // call is paid for, and a retry finds it in the cache.
            let checked = r.and_then(|vectors| {
                if vectors.len() != b.len() {
                    return Err(Error::data_exception(format!(
                        "model \"{}\" answered {} vectors for {} texts",
                        m.name,
                        vectors.len(),
                        b.len()
                    )));
                }
                for v in &vectors {
                    if v.len() != m.dimensions as usize {
                        return Err(Error::data_exception(format!(
                            "model \"{}\" answered a vector of {} components, not {}",
                            m.name,
                            v.len(),
                            m.dimensions
                        )));
                    }
                    if v.iter().any(|x| !x.is_finite()) {
                        return Err(Error::data_exception(format!(
                            "model \"{}\" answered a component that is not a finite number",
                            m.name
                        )));
                    }
                }
                Ok(vectors)
            });
            match checked {
                Ok(vectors) => {
                    for (&i, v) in send[b.clone()].iter().zip(vectors) {
                        let v: Arc<[f32]> = v.into();
                        keep(keys[i], v.clone());
                        out[i] = Some(v);
                    }
                }
                Err(e) => {
                    failure.get_or_insert(e);
                }
            }
        }
        if let Some(e) = failure {
            return Err(e);
        }
        for &i in &missing {
            if out[i].is_none() {
                out[i] = out[first[&keys[i]]].clone();
            }
        }
        Ok(())
    })();
    crate::budget::charge_model(&cost);
    r?;
    Ok(out.into_iter().map(|v| v.expect("embedded")).collect())
}

/// The tokens of each of `texts` (prefixed, as sent): the provider's
/// count where the limits need one and it counts here (a local model),
/// else a UTF-8 byte in three — about what BPE vocabularies make of
/// English, and more than they make of most text, so a limit holds.
fn estimate(
    p: &dyn ModelProvider,
    m: &Model,
    texts: &[String],
    files: &Files<'_>,
) -> Result<Vec<u64>> {
    let rough = || texts.iter().map(|t| t.len().div_ceil(3) as u64).collect();
    let l = &m.limits;
    if (l.batch_tokens == 0 && l.tokens_per_minute == 0) || m.files.is_empty() {
        return Ok(rough());
    }
    let counted = p.count_tokens(&Request {
        options: &m.options,
        dimensions: m.dimensions,
        secret: None,
        input: Input::Document,
        texts,
        files,
        tokens: AtomicU64::new(0),
        deadline: None,
    })?;
    Ok(match counted {
        Some(n) if n.len() == texts.len() => n.into_iter().map(|n| n as u64).collect(),
        _ => rough(),
    })
}

/// `tokens` (a text's each) in batches, in order: at most `max_texts`
/// texts and `max_tokens` tokens a batch (0: any), a text over that
/// alone (the provider says whether it takes it).
fn pack(tokens: &[u64], max_texts: usize, max_tokens: u32) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut sum) = (0, 0u64);
    for (i, &n) in tokens.iter().enumerate() {
        let full = i - start >= max_texts
            || (max_tokens > 0 && i > start && sum + n > u64::from(max_tokens));
        if full {
            out.push(start..i);
            (start, sum) = (i, 0);
        }
        sum += n;
    }
    if start < tokens.len() {
        out.push(start..tokens.len());
    }
    out
}

// ------------------------------------------------------------ rate limits

/// A model's rate limits, for a database in the process (each has its
/// clock): two token buckets (calls and tokens), each holding up to a
/// minute's limit and refilled at it.
#[derive(Default)]
struct Bucket {
    calls: f64,
    tokens: f64,
    /// When they were last filled (µs, by the database's clock).
    at: i64,
    /// The model's definition they were filled for.
    limits: Limits,
}

type Buckets = HashMap<(usize, String), Bucket>;

fn buckets() -> &'static Mutex<Buckets> {
    static B: OnceLock<Mutex<Buckets>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How a call waits for a model's rate limits.
struct Throttle {
    /// The database's clock (the host's outside a statement).
    clock: Option<Arc<dyn nucleus_kv::Kv>>,
    /// `Some`: a statement's call, which waits until its deadline (if it
    /// has one); `None`: a worker's, which is told when to come back.
    deadline: Option<Option<i64>>,
}

impl Throttle {
    fn now(&self) -> i64 {
        match &self.clock {
            Some(kv) => crate::datetime::from_system(kv.wall_clock()),
            None => crate::datetime::from_system(std::time::SystemTime::now()),
        }
    }

    fn sleep(&self, micros: i64) {
        let d = std::time::Duration::from_micros(micros.max(0) as u64);
        match &self.clock {
            Some(kv) => kv.sleep(d),
            None => std::thread::sleep(d),
        }
    }

    /// Takes a call of `tokens` tokens from `m`'s buckets, waiting for
    /// them as the caller may; the µs waited.
    fn acquire(&self, m: &Model, tokens: u64) -> Result<u64> {
        let l = m.limits;
        if l.requests_per_minute == 0 && l.tokens_per_minute == 0 {
            return Ok(0);
        }
        let mut waited = 0i64;
        loop {
            let now = self.now();
            let wait = {
                let mut all = buckets().lock().unwrap_or_else(|e| e.into_inner());
                let database = self
                    .clock
                    .as_ref()
                    .map_or(0, |kv| Arc::as_ptr(kv) as *const () as usize);
                let b = all.entry((database, m.name.clone())).or_default();
                let (rpm, tpm) = (
                    f64::from(l.requests_per_minute),
                    f64::from(l.tokens_per_minute),
                );
                if b.limits != l {
                    // New, or redefined: full.
                    *b = Bucket {
                        calls: rpm,
                        tokens: tpm,
                        at: now,
                        limits: l,
                    };
                }
                let elapsed = (now - b.at).max(0) as f64 / 60e6;
                b.calls = (b.calls + elapsed * rpm).min(rpm);
                b.tokens = (b.tokens + elapsed * tpm).min(tpm);
                b.at = now.max(b.at);
                // A call bigger than a minute's tokens waits for a full
                // bucket, not for ever.
                let need = (tokens as f64).min(tpm);
                let short = |have: f64, want: f64, per_minute: f64| {
                    if per_minute == 0.0 || have >= want {
                        0.0
                    } else {
                        (want - have) / per_minute * 60e6
                    }
                };
                let wait = short(b.calls, 1.0, rpm).max(short(b.tokens, need, tpm));
                if wait == 0.0 {
                    if rpm > 0.0 {
                        b.calls -= 1.0;
                    }
                    if tpm > 0.0 {
                        b.tokens -= need;
                    }
                    None
                } else {
                    Some(wait.ceil() as i64)
                }
            };
            let Some(wait) = wait else {
                if waited > 0 {
                    count(&m.name, |c| c.throttled += waited as u64);
                }
                return Ok(waited as u64);
            };
            let refused = match self.deadline {
                None => true,
                Some(Some(d)) => now + wait > d,
                Some(None) => false,
            };
            if refused {
                return Err(Error::configuration_limit(format!(
                    "model \"{}\" is at its rate limit ({} calls, {} tokens a minute)",
                    m.name,
                    fmt_limit(l.requests_per_minute),
                    fmt_limit(l.tokens_per_minute)
                ))
                .with_hint(format!(
                    "try again in {} ms (the model's rate limit)",
                    (wait as u64).div_ceil(1000)
                )));
            }
            self.sleep(wait);
            waited += wait;
        }
    }
}

fn fmt_limit(n: u32) -> String {
    if n == 0 { "any".into() } else { n.to_string() }
}

// --------------------------------------------------------------- embed()

/// `embed(text, model [, 'query' | 'document'])` — a document by default.
struct Embed;

fn text_arg(v: &Value, what: &str) -> Result<String> {
    match v {
        Value::Text(s) => Ok(s.to_string()),
        other => Err(Error::type_mismatch(format!(
            "embed()'s {what} is text, not {other:?}"
        ))),
    }
}

impl ScalarFunction for Embed {
    fn name(&self) -> &str {
        "embed"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        2..=3
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        for (i, a) in args.iter().enumerate() {
            if a.ty.as_ref().is_some_and(|t| *t != DataType::Text) && !a.text_literal {
                return Err(Error::undefined_function(format!(
                    "embed()'s argument {} must be text",
                    i + 1
                )));
            }
        }
        Ok(Signature {
            args: vec![crate::ext::Arg::Untyped(DataType::Text); args.len()],
            returns: Returns::Type(Some(crate::vectors::data_type())),
        })
    }

    fn volatility(&self) -> Volatility {
        // The same within a statement: a model's answer for a text is
        // kept (the cache), and a model is redefined only by DDL.
        Volatility::Stable
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn call(&self, cx: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let text = text_arg(&args[0], "text")?;
        let model = text_arg(&args[1], "model")?;
        let input = match args.get(2) {
            Some(v) => Input::parse(&text_arg(v, "input")?)?,
            None => Input::Document,
        };
        let v = embed(cx.txn(), &model, &[&text], input)?;
        crate::vectors::checked(&v[0])
    }

    /// A batch's texts in as few calls as the model's batch allows.
    fn call_batch(
        &self,
        cx: &CallContext<'_>,
        args: &[&Vector],
        len: usize,
        sel: &Sel,
    ) -> Option<Result<Vector>> {
        let consts: Vec<&Value> = args[1..]
            .iter()
            .map(|v| match v {
                Vector::Const(c) => Some(c),
                _ => None,
            })
            .collect::<Option<_>>()?;
        if consts.iter().any(|c| matches!(c, Value::Null)) {
            return Some(Ok(Vector::Const(Value::Null)));
        }
        let run = || -> Result<Vector> {
            let model = text_arg(consts[0], "model")?;
            let input = match consts.get(1) {
                Some(v) => Input::parse(&text_arg(v, "input")?)?,
                None => Input::Document,
            };
            let mut at = Vec::new();
            let mut texts = Vec::new();
            for i in sel.iter() {
                match args[0].value(i) {
                    Value::Text(s) => {
                        at.push(i);
                        texts.push(s.to_string());
                    }
                    Value::Null => {}
                    other => {
                        return Err(Error::type_mismatch(format!(
                            "embed()'s text is text, not {other:?}"
                        )));
                    }
                }
            }
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            let vectors = embed(cx.txn(), &model, &refs, input)?;
            let mut out = vec![Value::Null; len];
            for (i, v) in at.into_iter().zip(vectors) {
                out[i] = crate::vectors::checked(&v)?;
            }
            Ok(Vector::typed(out))
        };
        Some(run())
    }
}

// --------------------------------------------------------------- hashing

/// The built-in provider `hashing`: a text's terms (an analyzer's,
/// `simple` by default) hashed into `dimensions` buckets (feature
/// hashing, Weinberger et al. 2009), counted, and the vector scaled to
/// length 1 — so the cosine of two is about the overlap of their words.
/// Without the paper's random signs: they keep inner products unbiased
/// but let two words cancel into a zero vector, which has no cosine; only
/// a text without terms is zero here.  No model and no network: tests,
/// and a lexical baseline.
struct Hashing;

impl ModelProvider for Hashing {
    fn name(&self) -> &str {
        "hashing"
    }

    fn define(&self, options: &Options, _files: &Files<'_>) -> Result<Spec> {
        options.only("hashing", &["dimensions", "analyzer"])?;
        let dimensions = options
            .number("dimensions", 1..=u64::from(MAX_DIMENSIONS))?
            .unwrap_or(256) as u32;
        crate::text::named(options.get("analyzer").unwrap_or("simple"))?;
        Ok(Spec {
            dimensions,
            max_batch: 1024,
        })
    }

    fn embed(&self, r: &Request<'_>) -> Result<Vec<Vec<f32>>> {
        let a = crate::text::named(r.options.get("analyzer").unwrap_or("simple"))?;
        let d = r.dimensions as u64;
        Ok(r.texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; d as usize];
                a.analyze(t, &mut |tok| {
                    let h = crate::stablehash::xxh64(tok.term.as_bytes(), 0x6e75_636c);
                    v[(h % d) as usize] += 1.0;
                });
                let norm = v
                    .iter()
                    .map(|x| f64::from(*x) * f64::from(*x))
                    .sum::<f64>()
                    .sqrt();
                if norm > 0.0 {
                    for x in &mut v {
                        *x = (f64::from(*x) / norm) as f32;
                    }
                }
                v
            })
            .collect())
    }

    fn remote(&self) -> bool {
        false
    }
}

/// Registers `embed()`.
pub fn register_functions(r: &mut Registry) -> Result<()> {
    r.add_function(&["embed"], Arc::new(Embed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let m = Model {
            name: "m".into(),
            owner: "o".into(),
            provider: "openai".into(),
            options: Options(vec![("model".into(), "text-embedding-3-small".into())]),
            secret: Some("k".into()),
            query_prefix: Some("query: ".into()),
            document_prefix: None,
            dimensions: 1536,
            max_batch: 2048,
            created: 9,
            files: vec![FileRef {
                name: "model.safetensors".into(),
                digest: [7; 32],
                len: 1 << 33,
            }],
            limits: Limits {
                batch_tokens: 8192,
                requests_per_minute: 3000,
                tokens_per_minute: 0,
                parallel: 4,
            },
        };
        assert_eq!(Model::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn hashing_overlaps_by_words() {
        crate::text::register_builtins();
        let o = Options(vec![("dimensions".into(), "64".into())]);
        let spec = Hashing.define(&o, &Files::none()).unwrap();
        let texts = [
            "red apple pie".to_string(),
            "Apple pie, red!".to_string(),
            "blue sky".to_string(),
        ];
        let v = Hashing
            .embed(&Request {
                options: &o,
                dimensions: spec.dimensions,
                secret: None,
                input: Input::Document,
                texts: &texts,
                files: &Files::none(),
                tokens: AtomicU64::new(0),
                deadline: None,
            })
            .unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!((dot(&v[0], &v[1]) - 1.0).abs() < 1e-6);
        assert!(dot(&v[0], &v[2]).abs() < 0.9);
        assert!(
            Hashing
                .define(&Options(vec![("size".into(), "1".into())]), &Files::none())
                .is_err()
        );
        // Words never cancel: no zero vector but for a text without terms.
        let o4 = Options(vec![("dimensions".into(), "4".into())]);
        let few: Vec<String> = ["hello world", "a b c d e f g", ""]
            .map(String::from)
            .to_vec();
        let v = Hashing
            .embed(&Request {
                options: &o4,
                dimensions: 4,
                secret: None,
                input: Input::Document,
                texts: &few,
                files: &Files::none(),
                tokens: AtomicU64::new(0),
                deadline: None,
            })
            .unwrap();
        assert!(v[0].iter().any(|x| *x != 0.0) && v[1].iter().any(|x| *x != 0.0));
        assert!(v[2].iter().all(|x| *x == 0.0));
    }
}
