//! A BERT-family encoder's forward pass (PLAN.md §M14.22 E3b): BERT and
//! XLM-RoBERTa — bge, e5, MiniLM, GTE — from their `config.json` and
//! safetensors, on nucleus's kernels (`ext::kernels`).
//!
//! **Packed, not padded.**  A batch's texts are one matrix of their
//! tokens, end to end: the linear layers run over every token at once
//! and attention over each text's own, so no work is spent on padding
//! and no mask is needed.  **Each text's vector is independent of the
//! batch it came in and of the threads** — a row of a matrix product
//! depends on that row only, every output is one chain of fused
//! multiply-adds (`ext::kernels::matmul`), and threads take whole texts
//! — so a vector computed once is the vector, wherever it is computed
//! again on the same build and CPU.

use nucleus_sql::ext::kernels::{PackedB, add_assign, gelu, layer_norm, matmul, softmax_rows};
use serde_json::Value as Json;

use crate::safetensors::SafeTensors;

/// How a text's token vectors become its vector.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pooling {
    /// The first token's (`[CLS]`, `<s>`): bge.
    Cls,
    /// The mean of all of them: e5, MiniLM, GTE.
    Mean,
}

impl Pooling {
    /// From sentence-transformers' `1_Pooling/config.json`.
    pub fn from_json(bytes: &[u8]) -> Result<Pooling, String> {
        let v: Json = serde_json::from_slice(bytes).map_err(|e| format!("pooling config: {e}"))?;
        let on = |k: &str| v[k].as_bool().unwrap_or(false);
        match (
            on("pooling_mode_cls_token"),
            on("pooling_mode_mean_tokens"),
            on("pooling_mode_max_tokens") || on("pooling_mode_mean_sqrt_len_tokens"),
        ) {
            (true, false, false) => Ok(Pooling::Cls),
            (false, true, false) => Ok(Pooling::Mean),
            _ => Err("pooling config: only CLS or mean pooling is supported".into()),
        }
    }

    /// Its name, as `CREATE MODEL`'s option gives it.
    pub fn name(self) -> &'static str {
        match self {
            Pooling::Cls => "cls",
            Pooling::Mean => "mean",
        }
    }
}

/// What `config.json` says of the encoder.
#[derive(Clone, Debug)]
pub struct Config {
    /// The width of a token's vector.
    pub hidden: usize,
    /// Encoder layers.
    pub layers: usize,
    /// Attention heads a layer.
    pub heads: usize,
    /// The feed-forward layer's width.
    pub intermediate: usize,
    /// Tokens of the vocabulary.
    pub vocab: usize,
    /// Positions there are vectors for.
    pub positions: usize,
    /// The first position's index (RoBERTa's start after the padding
    /// token's: `pad_token_id + 1`; BERT's at 0).
    pub position_offset: usize,
    /// Token types there are vectors for.
    pub types: usize,
    /// Layer normalization's epsilon.
    pub eps: f32,
}

impl Config {
    /// Reads `config.json`.
    pub fn from_json(bytes: &[u8]) -> Result<Config, String> {
        let v: Json = serde_json::from_slice(bytes).map_err(|e| format!("config.json: {e}"))?;
        let n = |k: &str| {
            v[k].as_u64()
                .map(|x| x as usize)
                .ok_or_else(|| format!("config.json: no {k}"))
        };
        let kind = v["model_type"].as_str().unwrap_or("");
        let offset = match kind {
            "bert" => 0,
            "roberta" | "xlm-roberta" => n("pad_token_id")? + 1,
            other => {
                return Err(format!(
                    "config.json: model type {other:?} is not supported"
                ));
            }
        };
        match v["hidden_act"].as_str() {
            Some("gelu") => {}
            other => {
                return Err(format!(
                    "config.json: activation {other:?} is not supported"
                ));
            }
        }
        match v["position_embedding_type"].as_str() {
            None | Some("absolute") => {}
            Some(other) => {
                return Err(format!(
                    "config.json: position embeddings {other:?} are not supported"
                ));
            }
        }
        let c = Config {
            hidden: n("hidden_size")?,
            layers: n("num_hidden_layers")?,
            heads: n("num_attention_heads")?,
            intermediate: n("intermediate_size")?,
            vocab: n("vocab_size")?,
            positions: n("max_position_embeddings")?,
            position_offset: offset,
            types: n("type_vocab_size").unwrap_or(1).max(1),
            eps: v["layer_norm_eps"].as_f64().unwrap_or(1e-12) as f32,
        };
        // Each within what a sentence encoder could be (the largest
        // published are a few times smaller), so a hostile file cannot ask
        // for an allocation the weights do not back.
        for (what, value, most) in [
            ("hidden_size", c.hidden, 1 << 14),
            ("num_hidden_layers", c.layers, 256),
            ("num_attention_heads", c.heads, 256),
            ("intermediate_size", c.intermediate, 1 << 16),
            ("vocab_size", c.vocab, 1 << 22),
            ("max_position_embeddings", c.positions, 1 << 16),
            ("type_vocab_size", c.types, 1 << 10),
        ] {
            if value == 0 || value > most {
                return Err(format!("config.json: {what} {value} (1 to {most})"));
            }
        }
        if !c.hidden.is_multiple_of(c.heads) || c.positions <= offset {
            return Err("config.json: dimensions that do not fit together".into());
        }
        Ok(c)
    }

    /// The most tokens a text may have, its special tokens included.
    pub fn max_tokens(&self) -> usize {
        self.positions - self.position_offset
    }
}

struct Norm {
    gamma: Vec<f32>,
    beta: Vec<f32>,
}

struct Layer {
    qkv: PackedB,
    qkv_bias: Vec<f32>,
    out: PackedB,
    out_bias: Vec<f32>,
    attention_norm: Norm,
    up: PackedB,
    up_bias: Vec<f32>,
    down: PackedB,
    down_bias: Vec<f32>,
    output_norm: Norm,
}

/// An encoder ready to run: its weights, the linear layers' packed for
/// the matrix product.
pub struct Encoder {
    config: Config,
    pooling: Pooling,
    words: Vec<f32>,
    positions: Vec<f32>,
    /// Token type 0's vector (a single text is all type 0).
    type0: Vec<f32>,
    embedding_norm: Norm,
    layers: Vec<Layer>,
}

/// Texts a thread runs through the layers at once, by tokens: the
/// buffers of 2,048 tokens of a 384-wide model are 30 MB.
const GROUP_TOKENS: usize = 2048;

impl Encoder {
    /// From the configuration and the weights.
    pub fn new(config: Config, pooling: Pooling, weights: &SafeTensors) -> Result<Encoder, String> {
        let h = config.hidden;
        // The weights' names may carry the model's prefix.
        let prefix = ["", "bert.", "roberta.", "model."]
            .into_iter()
            .find(|p| {
                weights
                    .names()
                    .any(|n| n == format!("{p}embeddings.word_embeddings.weight"))
            })
            .ok_or("the weights have no embeddings.word_embeddings")?;
        let t = |name: &str, shape: &[usize]| weights.f32(&format!("{prefix}{name}"), shape);
        let norm = |name: &str| -> Result<Norm, String> {
            Ok(Norm {
                gamma: t(&format!("{name}.weight"), &[h])?,
                beta: t(&format!("{name}.bias"), &[h])?,
            })
        };
        let linear = |name: &str, out: usize, inp: usize| -> Result<(PackedB, Vec<f32>), String> {
            let w = t(&format!("{name}.weight"), &[out, inp])?;
            Ok((
                PackedB::from_transposed(&w, inp, out, inp),
                t(&format!("{name}.bias"), &[out])?,
            ))
        };
        // Grown as the weights are read, never sized by the configuration
        // alone.
        let mut layers = Vec::new();
        for l in 0..config.layers {
            let p = format!("encoder.layer.{l}.");
            // Query, key and value as one product of three times the width.
            let mut w = Vec::new();
            let mut b = Vec::new();
            for part in ["query", "key", "value"] {
                w.extend(t(&format!("{p}attention.self.{part}.weight"), &[h, h])?);
                b.extend(t(&format!("{p}attention.self.{part}.bias"), &[h])?);
            }
            let (out, out_bias) = linear(&format!("{p}attention.output.dense"), h, h)?;
            let (up, up_bias) = linear(&format!("{p}intermediate.dense"), config.intermediate, h)?;
            let (down, down_bias) = linear(&format!("{p}output.dense"), h, config.intermediate)?;
            layers.push(Layer {
                qkv: PackedB::from_transposed(&w, h, 3 * h, h),
                qkv_bias: b,
                out,
                out_bias,
                attention_norm: norm(&format!("{p}attention.output.LayerNorm"))?,
                up,
                up_bias,
                down,
                down_bias,
                output_norm: norm(&format!("{p}output.LayerNorm"))?,
            });
        }
        let types = t(
            "embeddings.token_type_embeddings.weight",
            &[config.types, h],
        )?;
        Ok(Encoder {
            words: t("embeddings.word_embeddings.weight", &[config.vocab, h])?,
            positions: t(
                "embeddings.position_embeddings.weight",
                &[config.positions, h],
            )?,
            type0: types[..h].to_vec(),
            embedding_norm: norm("embeddings.LayerNorm")?,
            layers,
            pooling,
            config,
        })
    }

    /// The configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The pooling.
    pub fn pooling(&self) -> Pooling {
        self.pooling
    }

    /// The vectors of `texts` (each its token ids, at most
    /// [`Config::max_tokens`]), scaled to length 1 when `normalize`, on
    /// up to `threads` threads.
    pub fn encode(
        &self,
        texts: &[Vec<u32>],
        normalize: bool,
        threads: usize,
    ) -> Result<Vec<Vec<f32>>, String> {
        for t in texts {
            if t.is_empty() || t.len() > self.config.max_tokens() {
                return Err(format!(
                    "a text of {} tokens (1 to {} are taken)",
                    t.len(),
                    self.config.max_tokens()
                ));
            }
            if let Some(id) = t.iter().find(|&&id| id as usize >= self.config.vocab) {
                return Err(format!("token {id} is not in the model's vocabulary"));
            }
        }
        // Whole texts to each thread, contiguous, by about equal work
        // (tokens, and attention's square).
        let cost = |t: &Vec<u32>| t.len() * (self.config.hidden * 8) + t.len() * t.len();
        let total: usize = texts.iter().map(cost).sum();
        let threads = threads.clamp(1, texts.len().max(1));
        let mut ranges = Vec::with_capacity(threads);
        let (mut start, mut acc) = (0, 0);
        for (i, t) in texts.iter().enumerate() {
            acc += cost(t);
            if acc * threads >= total * (ranges.len() + 1) && ranges.len() + 1 < threads {
                ranges.push(start..i + 1);
                start = i + 1;
            }
        }
        ranges.push(start..texts.len());
        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        if ranges.len() == 1 {
            self.run(texts, &mut out, normalize);
        } else {
            std::thread::scope(|s| {
                let mut rest: &mut [Vec<f32>] = &mut out;
                for r in &ranges {
                    let (mine, tail) = rest.split_at_mut(r.len());
                    rest = tail;
                    let texts = &texts[r.clone()];
                    s.spawn(move || self.run(texts, mine, normalize));
                }
            });
        }
        Ok(out)
    }

    /// `texts` on this thread, in groups of about [`GROUP_TOKENS`].
    fn run(&self, texts: &[Vec<u32>], out: &mut [Vec<f32>], normalize: bool) {
        let mut scratch = Scratch::default();
        let mut start = 0;
        while start < texts.len() {
            let mut end = start;
            let mut tokens = 0;
            while end < texts.len() && (end == start || tokens + texts[end].len() <= GROUP_TOKENS) {
                tokens += texts[end].len();
                end += 1;
            }
            self.forward(&texts[start..end], &mut out[start..end], &mut scratch);
            if normalize {
                for v in &mut out[start..end] {
                    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
                    v.iter_mut().for_each(|x| *x /= norm);
                }
            }
            start = end;
        }
    }

    fn forward(&self, texts: &[Vec<u32>], out: &mut [Vec<f32>], s: &mut Scratch) {
        let c = &self.config;
        let h = c.hidden;
        let d = h / c.heads;
        let n: usize = texts.iter().map(Vec::len).sum();
        let starts: Vec<usize> = texts
            .iter()
            .scan(0, |o, t| {
                let at = *o;
                *o += t.len();
                Some(at)
            })
            .collect();
        s.size(n, h, c.intermediate);
        // Embeddings: the word's, plus the type's, plus the position's.
        for (t, &o) in texts.iter().zip(&starts) {
            for (p, &id) in t.iter().enumerate() {
                let row = &mut s.x[(o + p) * h..][..h];
                let w = &self.words[id as usize * h..][..h];
                let pos = &self.positions[(p + c.position_offset) * h..][..h];
                for k in 0..h {
                    row[k] = (w[k] + self.type0[k]) + pos[k];
                }
            }
        }
        let en = &self.embedding_norm;
        layer_norm(&mut s.x, n, h, h, &en.gamma, &en.beta, c.eps);
        let scale = 1.0 / (d as f32).sqrt();
        for l in &self.layers {
            matmul(
                &s.x,
                h,
                n,
                &l.qkv,
                Some(&l.qkv_bias),
                &mut s.qkv,
                3 * h,
                &mut s.pack,
            );
            for (t, &o) in texts.iter().zip(&starts) {
                let len = t.len();
                s.scores.resize(len * len, 0.0);
                for head in 0..c.heads {
                    let q = &s.qkv[o * 3 * h + head * d..];
                    let k = &s.qkv[o * 3 * h + h + head * d..];
                    let v = &s.qkv[o * 3 * h + 2 * h + head * d..];
                    let kt = PackedB::from_transposed(k, 3 * h, len, d);
                    matmul(q, 3 * h, len, &kt, None, &mut s.scores, len, &mut s.pack);
                    softmax_rows(&mut s.scores, len, len, len, scale);
                    let vp = PackedB::from_rows(v, 3 * h, len, d);
                    let ctx = &mut s.ctx[o * h + head * d..];
                    matmul(&s.scores, len, len, &vp, None, ctx, h, &mut s.pack);
                }
            }
            matmul(
                &s.ctx,
                h,
                n,
                &l.out,
                Some(&l.out_bias),
                &mut s.tmp,
                h,
                &mut s.pack,
            );
            add_assign(&mut s.tmp, &s.x);
            let an = &l.attention_norm;
            layer_norm(&mut s.tmp, n, h, h, &an.gamma, &an.beta, c.eps);
            std::mem::swap(&mut s.x, &mut s.tmp);
            let i = c.intermediate;
            matmul(
                &s.x,
                h,
                n,
                &l.up,
                Some(&l.up_bias),
                &mut s.inter,
                i,
                &mut s.pack,
            );
            gelu(&mut s.inter);
            matmul(
                &s.inter,
                i,
                n,
                &l.down,
                Some(&l.down_bias),
                &mut s.tmp,
                h,
                &mut s.pack,
            );
            add_assign(&mut s.tmp, &s.x);
            let on = &l.output_norm;
            layer_norm(&mut s.tmp, n, h, h, &on.gamma, &on.beta, c.eps);
            std::mem::swap(&mut s.x, &mut s.tmp);
        }
        for ((t, &o), v) in texts.iter().zip(&starts).zip(out.iter_mut()) {
            *v = match self.pooling {
                Pooling::Cls => s.x[o * h..][..h].to_vec(),
                Pooling::Mean => {
                    let mut sum = vec![0f32; h];
                    for p in 0..t.len() {
                        add_assign(&mut sum, &s.x[(o + p) * h..][..h]);
                    }
                    let inv = t.len() as f32;
                    sum.iter_mut().for_each(|x| *x /= inv);
                    sum
                }
            };
        }
    }
}

/// A thread's buffers, kept from group to group.
#[derive(Default)]
struct Scratch {
    x: Vec<f32>,
    tmp: Vec<f32>,
    qkv: Vec<f32>,
    ctx: Vec<f32>,
    inter: Vec<f32>,
    scores: Vec<f32>,
    pack: Vec<[f32; nucleus_sql::ext::kernels::MR]>,
}

impl Scratch {
    fn size(&mut self, n: usize, h: usize, i: usize) {
        for (b, len) in [
            (&mut self.x, n * h),
            (&mut self.tmp, n * h),
            (&mut self.qkv, n * 3 * h),
            (&mut self.ctx, n * h),
            (&mut self.inter, n * i),
        ] {
            b.clear();
            b.resize(len, 0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration's numbers are bounded before anything is sized by
    /// them.
    #[test]
    fn configurations_out_of_bounds() {
        let config = |layers: &str, hidden: &str| {
            format!(
                r#"{{"model_type":"bert","hidden_act":"gelu","hidden_size":{hidden},"num_hidden_layers":{layers},"num_attention_heads":4,"intermediate_size":64,"vocab_size":100,"max_position_embeddings":512}}"#
            )
        };
        assert!(Config::from_json(config("2", "32").as_bytes()).is_ok());
        for (layers, hidden) in [("1000000000000000", "32"), ("2", "0"), ("2", "1048576")] {
            let e = Config::from_json(config(layers, hidden).as_bytes()).unwrap_err();
            assert!(e.contains("(1 to "), "{e}");
        }
    }
}
