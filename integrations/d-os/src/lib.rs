//! The d-os settings store on nucleus (PLAN.md §M6.5).
//!
//! d-os keeps its settings in a key-value cache inside the kernel, with one
//! layer for the machine and one per signed-in account.  This crate is the
//! persistence behind that cache: [`Settings`] over any [`Kv`], and the
//! `nucleus-settings` service that speaks a line protocol on stdin/stdout
//! (see `README.md`).  It uses only the public KV contract (principle 9).
//!
//! **Key layout:** `cfg/machine/<key>` and `cfg/user/<uid>/<key>`.  A layer
//! is named `machine` or `user/<uid>`.
//!
//! **Atomicity:** changes are staged and [`Settings::commit`] writes them in
//! one transaction — a settings panel that changes several keys commits all
//! or none, and a crash leaves the complete old or the complete new set.
//!
//! **History:** every commit returns a timestamp; [`Settings::dump_at`]
//! reads a layer as it was then, for as long as the database keeps the
//! versions (`Options::history`).

use std::collections::BTreeMap;
use std::ops::Bound;

use nucleus_kv::{Error, Kv, Result, Timestamp, TxnOptions, prefix_end, transact};

/// A settings layer.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// The machine-wide layer.
    Machine,
    /// One account's layer.
    User(u32),
}

impl Layer {
    /// Parses `machine` or `user/<uid>`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "machine" => Some(Layer::Machine),
            _ => s.strip_prefix("user/")?.parse().ok().map(Layer::User),
        }
    }

    fn prefix(&self) -> Vec<u8> {
        match self {
            Layer::Machine => b"cfg/machine/".to_vec(),
            Layer::User(uid) => format!("cfg/user/{uid}/").into_bytes(),
        }
    }

    fn key(&self, key: &str) -> Vec<u8> {
        let mut k = self.prefix();
        k.extend_from_slice(key.as_bytes());
        k
    }
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Layer::Machine => f.write_str("machine"),
            Layer::User(uid) => write!(f, "user/{uid}"),
        }
    }
}

/// Settings over a [`Kv`], with staged changes.
pub struct Settings<'a> {
    kv: &'a dyn Kv,
    /// Staged changes: `None` removes the key.
    staged: BTreeMap<(Layer, String), Option<String>>,
}

impl<'a> Settings<'a> {
    /// Settings stored in `kv`.
    pub fn new(kv: &'a dyn Kv) -> Self {
        Self {
            kv,
            staged: BTreeMap::new(),
        }
    }

    /// The committed value of `key` in `layer` (staged changes not included).
    pub fn get(&self, layer: &Layer, key: &str) -> Result<Option<String>> {
        let t = self.kv.begin(TxnOptions::read_only())?;
        t.get(&layer.key(key))?.map(text).transpose()
    }

    /// Stages `key = value`.
    pub fn set(&mut self, layer: Layer, key: &str, value: &str) {
        self.staged
            .insert((layer, key.to_string()), Some(value.to_string()));
    }

    /// Stages removing `key`.
    pub fn unset(&mut self, layer: Layer, key: &str) {
        self.staged.insert((layer, key.to_string()), None);
    }

    /// Staged changes not yet committed.
    pub fn staged(&self) -> usize {
        self.staged.len()
    }

    /// Discards the staged changes.
    pub fn discard(&mut self) {
        self.staged.clear();
    }

    /// Writes every staged change in one transaction; returns its
    /// timestamp.  On error nothing was written and the changes stay staged.
    pub fn commit(&mut self) -> Result<Timestamp> {
        let staged = &self.staged;
        let ts = transact(self.kv, TxnOptions::default(), |t| {
            for ((layer, key), value) in staged {
                match value {
                    Some(v) => t.put(&layer.key(key), v.as_bytes())?,
                    None => t.delete(&layer.key(key))?,
                }
            }
            Ok(t.read_ts())
        })?;
        self.staged.clear();
        Ok(ts)
    }

    /// The current timestamp: [`Settings::dump_at`] it later to see today's
    /// settings.
    pub fn snapshot(&self) -> Result<Timestamp> {
        Ok(self.kv.begin(TxnOptions::read_only())?.read_ts())
    }

    /// Every committed key and value of `layer`, sorted.
    pub fn dump(&self, layer: &Layer) -> Result<Vec<(String, String)>> {
        self.dump_with(layer, TxnOptions::read_only())
    }

    /// `layer` as it was at `ts`.
    pub fn dump_at(&self, layer: &Layer, ts: Timestamp) -> Result<Vec<(String, String)>> {
        self.dump_with(
            layer,
            TxnOptions {
                at: Some(ts),
                ..TxnOptions::default()
            },
        )
    }

    fn dump_with(&self, layer: &Layer, opts: TxnOptions) -> Result<Vec<(String, String)>> {
        let t = self.kv.begin(opts)?;
        let prefix = layer.prefix();
        let end = prefix_end(&prefix).expect("a layer prefix ends in '/'");
        let mut out = Vec::new();
        for entry in t.scan(Bound::Included(&prefix), Bound::Excluded(&end))? {
            let (k, v) = entry?;
            out.push((text(k[prefix.len()..].to_vec())?, text(v)?));
        }
        Ok(out)
    }

    /// The layers that hold at least one key.
    pub fn layers(&self) -> Result<Vec<Layer>> {
        let t = self.kv.begin(TxnOptions::read_only())?;
        let mut out: Vec<Layer> = Vec::new();
        for entry in t.scan(Bound::Included(b"cfg/"), Bound::Excluded(b"cfg0"))? {
            let (k, _) = entry?;
            let rest = std::str::from_utf8(&k[4..]).map_err(corrupt)?;
            let layer = if rest.starts_with("machine/") {
                Some(Layer::Machine)
            } else {
                rest.strip_prefix("user/")
                    .and_then(|r| r.split('/').next())
                    .and_then(|u| u.parse().ok())
                    .map(Layer::User)
            };
            if let Some(l) = layer.filter(|l| out.last() != Some(l)) {
                out.push(l);
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Stages the entries of a `d-os.conf` file (`key = value` lines, `#`
    /// comments, blank lines) into `layer`; returns how many.
    pub fn import(&mut self, layer: &Layer, conf: &str) -> usize {
        let mut n = 0;
        for line in conf.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                if !k.is_empty() {
                    self.set(layer.clone(), k, v.trim());
                    n += 1;
                }
            }
        }
        n
    }

    /// `layer` in the `d-os.conf` format, readable by d-os and by people.
    pub fn export(&self, layer: &Layer) -> Result<String> {
        let mut out = format!("# d-os settings, layer {layer}, exported by nucleus-settings\n");
        for (k, v) in self.dump(layer)? {
            out.push_str(&format!("{k} = {v}\n"));
        }
        Ok(out)
    }
}

fn corrupt(_: impl std::fmt::Display) -> Error {
    Error::Storage("a settings key or value is not UTF-8".into())
}

fn text(b: Vec<u8>) -> Result<String> {
    String::from_utf8(b).map_err(corrupt)
}
