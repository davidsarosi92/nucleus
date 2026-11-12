//! Tokenizers read from Hugging Face's `tokenizer.json` (PLAN.md §M14.22
//! E3a): the two families the BERT-style embedding models use, each
//! exactly as `tokenizers` runs it, so a model sees the ids it was
//! trained and published with.
//!
//! - **WordPiece** behind BERT's normalizer and pre-tokenizer (bge,
//!   MiniLM, GTE, BERT).
//! - **Unigram** behind SentencePiece's precompiled normalizer and
//!   Metaspace (XLM-RoBERTa: multilingual-e5).
//!
//! What else a `tokenizer.json` may hold is refused by name, not
//! approximated.  Checked against `tokenizers` on corpora and on every
//! code point (bench/embed/tokens.py).

mod bert;
mod bert_tables;
mod grapheme_breaks;
pub(crate) mod graphemes;
mod unigram;

use serde_json::Value as Json;

enum Normalizer {
    None,
    Bert(bert::Normalizer),
    Spm {
        map: Option<unigram::Precompiled>,
        squeeze: bool,
    },
}

enum PreTokenizer {
    Bert,
    Metaspace { replacement: char, prepend: Prepend },
}

#[derive(Clone, Copy, PartialEq)]
enum Prepend {
    Always,
    First,
    Never,
}

enum Model {
    WordPiece(bert::WordPiece),
    Unigram(unigram::Unigram),
}

/// A tokenizer: text to the ids a model reads.
pub struct Tokenizer {
    /// Added tokens matched in the text before anything else, longest
    /// first where two start at one place.
    added: Vec<(String, u32)>,
    normalizer: Normalizer,
    pre: PreTokenizer,
    model: Model,
    /// The ids put before and after a text.
    before: Vec<u32>,
    after: Vec<u32>,
}

fn unsupported(what: &str, v: &Json) -> String {
    format!(
        "tokenizer.json: {what} {} is not supported",
        v.get("type").unwrap_or(v)
    )
}

fn decode_base64(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return Err("tokenizer.json: a character map that is not base64".into()),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

fn flag(v: &Json, name: &str, default: bool) -> bool {
    v.get(name).and_then(Json::as_bool).unwrap_or(default)
}

fn normalizer(v: &Json) -> Result<Normalizer, String> {
    if v.is_null() {
        return Ok(Normalizer::None);
    }
    match v["type"].as_str() {
        Some("BertNormalizer") => {
            let lowercase = flag(v, "lowercase", true);
            Ok(Normalizer::Bert(bert::Normalizer {
                clean_text: flag(v, "clean_text", true),
                chinese: flag(v, "handle_chinese_chars", true),
                // Unset, accents go with the case.
                strip_accents: v["strip_accents"].as_bool().unwrap_or(lowercase),
                lowercase,
            }))
        }
        Some("Precompiled") => Ok(Normalizer::Spm {
            map: Some(precompiled(v)?),
            squeeze: false,
        }),
        Some("Sequence") => {
            let (mut map, mut squeeze) = (None, false);
            for n in v["normalizers"].as_array().into_iter().flatten() {
                match n["type"].as_str() {
                    Some("Precompiled") if map.is_none() && !squeeze => map = Some(precompiled(n)?),
                    Some("Replace") if n["pattern"]["Regex"] == " {2,}" && n["content"] == " " => {
                        squeeze = true
                    }
                    _ => return Err(unsupported("the normalizer", n)),
                }
            }
            Ok(Normalizer::Spm { map, squeeze })
        }
        _ => Err(unsupported("the normalizer", v)),
    }
}

fn precompiled(v: &Json) -> Result<unigram::Precompiled, String> {
    let map = v["precompiled_charsmap"]
        .as_str()
        .ok_or("tokenizer.json: a Precompiled normalizer without its map")?;
    unigram::Precompiled::new(&decode_base64(map)?)
}

fn pre_tokenizer(v: &Json) -> Result<PreTokenizer, String> {
    match v["type"].as_str() {
        Some("BertPreTokenizer") => Ok(PreTokenizer::Bert),
        Some("Metaspace") if flag(v, "split", true) => {
            let mut r = v["replacement"].as_str().unwrap_or("▁").chars();
            let replacement = match (r.next(), r.next()) {
                (Some(c), None) => c,
                _ => return Err(unsupported("the pre-tokenizer", v)),
            };
            let prepend = match v["prepend_scheme"].as_str() {
                Some("always") => Prepend::Always,
                Some("first") => Prepend::First,
                Some("never") => Prepend::Never,
                Some(_) => return Err(unsupported("the pre-tokenizer", v)),
                // The older form.
                None if flag(v, "add_prefix_space", true) => Prepend::Always,
                None => Prepend::Never,
            };
            Ok(PreTokenizer::Metaspace {
                replacement,
                prepend,
            })
        }
        _ => Err(unsupported("the pre-tokenizer", v)),
    }
}

fn model(v: &Json) -> Result<Model, String> {
    match v["type"].as_str() {
        Some("WordPiece") => {
            let vocab = v["vocab"]
                .as_object()
                .ok_or("tokenizer.json: a WordPiece model without its vocabulary")?;
            let unk_token = v["unk_token"].as_str().unwrap_or("[UNK]");
            let unk = vocab
                .get(unk_token)
                .and_then(Json::as_u64)
                .ok_or("tokenizer.json: WordPiece's unknown token is not in its vocabulary")?;
            let pieces = vocab
                .iter()
                .map(|(p, id)| {
                    id.as_u64()
                        .map(|id| (p.clone(), id as u32))
                        .ok_or("tokenizer.json: a WordPiece id that is not a number")
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Model::WordPiece(bert::WordPiece::new(
                pieces,
                v["continuing_subword_prefix"].as_str().unwrap_or("##"),
                unk as u32,
                v["max_input_chars_per_word"].as_u64().unwrap_or(100) as usize,
            )))
        }
        Some("Unigram") => {
            if flag(v, "byte_fallback", false) {
                return Err("tokenizer.json: Unigram's byte fallback is not supported".into());
            }
            let pieces = v["vocab"]
                .as_array()
                .ok_or("tokenizer.json: a Unigram model without its vocabulary")?
                .iter()
                .map(|p| match (p[0].as_str(), p[1].as_f64()) {
                    (Some(s), Some(score)) => Ok((s.to_string(), score)),
                    _ => Err("tokenizer.json: a Unigram piece that is not [text, score]"),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let unk = v["unk_id"]
                .as_u64()
                .ok_or("tokenizer.json: a Unigram model without an unknown token")?;
            Ok(Model::Unigram(unigram::Unigram::new(pieces, unk as u32)?))
        }
        _ => Err(unsupported("the model", v)),
    }
}

/// The ids a single text is wrapped in.
fn template(v: &Json, added: &[(String, u32)]) -> Result<(Vec<u32>, Vec<u32>), String> {
    let id_of = |name: &str| {
        added
            .iter()
            .find(|(t, _)| t == name)
            .map(|&(_, id)| id)
            .ok_or_else(|| format!("tokenizer.json: the special token {name} is not defined"))
    };
    match v["type"].as_str() {
        Some("TemplateProcessing") => {
            let (mut before, mut after) = (Vec::new(), Vec::new());
            let mut seen = false;
            for part in v["single"].as_array().into_iter().flatten() {
                if part.get("Sequence").is_some() {
                    if seen {
                        return Err(unsupported("the post-processor", v));
                    }
                    seen = true;
                } else if let Some(name) = part["SpecialToken"]["id"].as_str() {
                    let ids = v["special_tokens"][name]["ids"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Json::as_u64)
                                .map(|i| i as u32)
                                .collect()
                        })
                        .map_or_else(|| id_of(name).map(|i| vec![i]), Ok)?;
                    if seen { &mut after } else { &mut before }.extend(ids);
                } else {
                    return Err(unsupported("the post-processor", v));
                }
            }
            if !seen {
                return Err(unsupported("the post-processor", v));
            }
            Ok((before, after))
        }
        Some("BertProcessing" | "RobertaProcessing") => {
            let id = |k: &str| {
                v[k][1]
                    .as_u64()
                    .map(|i| i as u32)
                    .ok_or_else(|| unsupported("the post-processor", v))
            };
            Ok((vec![id("cls")?], vec![id("sep")?]))
        }
        None if v.is_null() => Ok((Vec::new(), Vec::new())),
        _ => Err(unsupported("the post-processor", v)),
    }
}

impl Tokenizer {
    /// Reads a `tokenizer.json`.
    pub fn from_json(bytes: &[u8]) -> Result<Tokenizer, String> {
        let v: Json = serde_json::from_slice(bytes).map_err(|e| format!("tokenizer.json: {e}"))?;
        let mut added = Vec::new();
        for t in v["added_tokens"].as_array().into_iter().flatten() {
            let (Some(content), Some(id)) = (t["content"].as_str(), t["id"].as_u64()) else {
                return Err("tokenizer.json: an added token without its content or id".into());
            };
            if flag(t, "single_word", false)
                || flag(t, "lstrip", false)
                || flag(t, "rstrip", false)
                || (flag(t, "normalized", false) && !flag(t, "special", false))
            {
                return Err(format!(
                    "tokenizer.json: the added token {content} is matched in a way not supported"
                ));
            }
            if !content.is_empty() {
                added.push((content.to_string(), id as u32));
            }
        }
        let (before, after) = template(&v["post_processor"], &added)?;
        Ok(Tokenizer {
            normalizer: normalizer(&v["normalizer"])?,
            pre: pre_tokenizer(&v["pre_tokenizer"])?,
            model: model(&v["model"])?,
            added,
            before,
            after,
        })
    }

    /// The number of `text`'s ids, unwrapped and uncut.
    pub fn count(&self, text: &str) -> usize {
        self.encode(text, usize::MAX)
            .len()
            .saturating_sub(self.before.len() + self.after.len())
    }

    /// The ids of `text`, wrapped as the model expects and cut to at
    /// most `max` (the wrapping kept, the text's end dropped).
    pub fn encode(&self, text: &str, max: usize) -> Vec<u32> {
        let mut ids = self.before.clone();
        let mut first = true;
        let mut rest = text;
        while !rest.is_empty() {
            let (plain, token) = self.next_added(rest);
            if !plain.is_empty() {
                self.encode_plain(plain, first, &mut ids);
            }
            first = false;
            match token {
                Some((len, id)) => {
                    ids.push(id);
                    rest = &rest[plain.len() + len..];
                }
                None => break,
            }
        }
        let keep = max.saturating_sub(self.after.len()).max(self.before.len());
        ids.truncate(keep);
        ids.extend(&self.after);
        ids
    }

    /// The text before the first added token in `s`, and that token
    /// (its length and id).
    fn next_added<'a>(&self, s: &'a str) -> (&'a str, Option<(usize, u32)>) {
        let mut best: Option<(usize, usize, u32)> = None;
        for (t, id) in &self.added {
            if let Some(at) = s.find(t.as_str()) {
                let better = match best {
                    None => true,
                    Some((b, len, _)) => at < b || (at == b && t.len() > len),
                };
                if better {
                    best = Some((at, t.len(), *id));
                }
            }
        }
        match best {
            Some((at, len, id)) => (&s[..at], Some((len, id))),
            None => (s, None),
        }
    }

    fn encode_plain(&self, s: &str, first: bool, out: &mut Vec<u32>) {
        let normalized = match &self.normalizer {
            Normalizer::None => s.to_string(),
            Normalizer::Bert(n) => n.apply(s),
            Normalizer::Spm { map, squeeze } => {
                let t = map.as_ref().map_or_else(|| s.to_string(), |m| m.apply(s));
                if *squeeze {
                    unigram::squeeze_spaces(&t)
                } else {
                    t
                }
            }
        };
        if normalized.is_empty() {
            return;
        }
        match (&self.pre, &self.model) {
            (PreTokenizer::Bert, model) => {
                for w in bert::words(&normalized) {
                    self.word(model, w, out);
                }
            }
            (
                PreTokenizer::Metaspace {
                    replacement,
                    prepend,
                },
                model,
            ) => {
                let p = match prepend {
                    Prepend::Always => true,
                    Prepend::First => first,
                    Prepend::Never => false,
                };
                for w in unigram::metaspace(&normalized, *replacement, p) {
                    self.word(model, &w, out);
                }
            }
        }
    }

    fn word(&self, model: &Model, w: &str, out: &mut Vec<u32>) {
        match model {
            Model::WordPiece(m) => m.word(w, out),
            Model::Unigram(m) => m.word(w, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::decode_base64;

    #[test]
    fn base64() {
        assert_eq!(decode_base64("bnVjbGV1cw==").unwrap(), b"nucleus");
        assert_eq!(decode_base64("YQ").unwrap(), b"a");
        assert!(decode_base64("a*b").is_err());
    }
}
