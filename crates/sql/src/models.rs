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
//! Records are in the catalog, `00 15 <name>`.

use std::collections::HashMap;
use std::ops::RangeInclusive;
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
const VERSION: u8 = 1;

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
}

/// A model provider (principle 12).
pub trait ModelProvider: Send + Sync {
    /// The name `CREATE MODEL … USING` names.
    fn name(&self) -> &str;

    /// Checks a model's options — every one it does not know refused —
    /// and says what the model makes.  Called when a model is made; must
    /// not reach the network.
    fn define(&self, options: &Options) -> Result<Spec>;

    /// The vectors of `request.texts`, in order, each of
    /// `request.dimensions` components.
    fn embed(&self, request: &Request<'_>) -> Result<Vec<Vec<f32>>>;

    /// Whether its calls reach the network (§M14.22 E4: such calls never
    /// run inside a writing transaction).
    fn remote(&self) -> bool {
        true
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
            .u64(self.created as u64);
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        if r.u8()? != VERSION {
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
        Ok(Model {
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
        })
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
    for (k, v) in options {
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
    let spec = p.define(&options)?;
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
    };
    put(txn, &m)
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
    txn.delete(&key_of(name))?;
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

/// Forgets every kept vector (tests).
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
    let m = existing(txn, name)?;
    crate::privileges::Checker::new().require(
        txn,
        crate::privileges::Kind::Model,
        name,
        crate::privileges::USAGE,
        None,
    )?;
    let record = m.encode();
    let prefix = m.prefix(input).unwrap_or("");
    let keys: Vec<[u8; 32]> = texts.iter().map(|t| cache_key(&record, input, t)).collect();
    let mut out: Vec<Option<Arc<[f32]>>> = keys.iter().map(cached).collect();
    let missing: Vec<usize> = (0..texts.len()).filter(|&i| out[i].is_none()).collect();
    count(&m.name, |c| {
        c.cached += (texts.len() - missing.len()) as u64
    });
    if missing.is_empty() {
        return Ok(out.into_iter().map(|v| v.expect("cached")).collect());
    }
    let p = provider(&m.provider)?;
    let secret = m
        .secret
        .as_deref()
        .map(|s| crate::secrets::reveal(txn, s))
        .transpose()?;
    // The same text twice in one call is sent once.
    let mut first: HashMap<&[u8; 32], usize> = HashMap::new();
    let mut send: Vec<usize> = Vec::new();
    for &i in &missing {
        if first.insert(&keys[i], i).is_none() {
            send.push(i);
        }
    }
    for chunk in send.chunks(m.max_batch.max(1) as usize) {
        let batch: Vec<String> = chunk
            .iter()
            .map(|&i| format!("{prefix}{}", texts[i]))
            .collect();
        let started = std::time::Instant::now();
        let r = p.embed(&Request {
            options: &m.options,
            dimensions: m.dimensions,
            secret: secret.as_ref(),
            input,
            texts: &batch,
        });
        let micros = started.elapsed().as_micros() as u64;
        let vectors = match r {
            Ok(v) => v,
            Err(e) => {
                count(&m.name, |c| {
                    c.calls += 1;
                    c.errors += 1;
                    c.micros += micros;
                });
                return Err(e);
            }
        };
        count(&m.name, |c| {
            c.calls += 1;
            c.texts += batch.len() as u64;
            c.micros += micros;
        });
        if vectors.len() != chunk.len() {
            return Err(Error::data_exception(format!(
                "model \"{}\" answered {} vectors for {} texts",
                m.name,
                vectors.len(),
                chunk.len()
            )));
        }
        for (&i, v) in chunk.iter().zip(vectors) {
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
            let v: Arc<[f32]> = v.into();
            keep(keys[i], v.clone());
            out[i] = Some(v);
        }
    }
    for &i in &missing {
        if out[i].is_none() {
            out[i] = out[first[&keys[i]]].clone();
        }
    }
    Ok(out.into_iter().map(|v| v.expect("embedded")).collect())
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

    fn define(&self, options: &Options) -> Result<Spec> {
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
        };
        assert_eq!(Model::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn hashing_overlaps_by_words() {
        crate::text::register_builtins();
        let o = Options(vec![("dimensions".into(), "64".into())]);
        let spec = Hashing.define(&o).unwrap();
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
            })
            .unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!((dot(&v[0], &v[1]) - 1.0).abs() < 1e-6);
        assert!(dot(&v[0], &v[2]).abs() < 0.9);
        assert!(
            Hashing
                .define(&Options(vec![("size".into(), "1".into())]))
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
            })
            .unwrap();
        assert!(v[0].iter().any(|x| *x != 0.0) && v[1].iter().any(|x| *x != 0.0));
        assert!(v[2].iter().all(|x| *x == 0.0));
    }
}
