//! SentencePiece's tokenizer as Hugging Face's `tokenizers` runs it
//! (XLM-RoBERTa: multilingual-e5): the precompiled normalizer, the
//! Metaspace pre-tokenizer and the Unigram model's best segmentation.

use super::graphemes;

/// SentencePiece's precompiled character map: a darts-clone double-array
/// trie over the UTF-8 of what is replaced, its values offsets into a
/// block of NUL-terminated replacements.
pub(crate) struct Precompiled {
    trie: Vec<u32>,
    normalized: Vec<u8>,
}

impl Precompiled {
    pub(crate) fn new(map: &[u8]) -> Result<Precompiled, String> {
        let bad = || "a precompiled character map that is damaged".to_string();
        let size = u32::from_le_bytes(
            map.get(..4)
                .ok_or_else(bad)?
                .try_into()
                .map_err(|_| bad())?,
        ) as usize;
        let trie_bytes = map.get(4..4 + size).ok_or_else(bad)?;
        if !size.is_multiple_of(4) || size == 0 {
            return Err(bad());
        }
        let trie = trie_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect();
        Ok(Precompiled {
            trie,
            normalized: map[4 + size..].to_vec(),
        })
    }

    /// The replacement of the shortest key that starts `chunk`, if any
    /// (what `tokenizers` takes: the first of darts-clone's common-prefix
    /// search).
    fn transform(&self, chunk: &[u8]) -> Option<&[u8]> {
        let unit = |pos: usize| self.trie.get(pos).copied();
        let offset = |u: u32| ((u >> 10) << ((u & (1 << 9)) >> 6)) as usize;
        let mut pos = 0usize;
        let mut u = unit(pos)?;
        pos ^= offset(u);
        for &c in chunk {
            if c == 0 {
                break;
            }
            pos ^= c as usize;
            u = unit(pos)?;
            if u & ((1 << 31) | 0xFF) != c as u32 {
                return None;
            }
            pos ^= offset(u);
            if (u >> 8) & 1 == 1 {
                let value = (unit(pos)? & ((1 << 31) - 1)) as usize;
                let rest = self.normalized.get(value..)?;
                let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
                return Some(&rest[..end]);
            }
        }
        None
    }

    /// Each grapheme cluster of fewer than six bytes replaced whole when
    /// the map has it, else each of its characters that the map has.
    pub(crate) fn apply(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for g in graphemes::clusters(s) {
            if g.len() < 6
                && let Some(r) = self.transform(g.as_bytes())
            {
                out.push_str(&String::from_utf8_lossy(r));
                continue;
            }
            for (i, c) in g.char_indices() {
                match self.transform(&g.as_bytes()[i..i + c.len_utf8()]) {
                    Some(r) => out.push_str(&String::from_utf8_lossy(r)),
                    None => out.push(c),
                }
            }
        }
        out
    }
}

/// Runs of two or more spaces as one (the `Replace(" {2,}", " ")` the
/// XLM-R tokenizers carry).
pub(crate) fn squeeze_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' && prev_space {
            continue;
        }
        prev_space = c == ' ';
        out.push(c);
    }
    out
}

/// Metaspace: spaces as `replacement` (`▁`), one put before the text, and
/// words each starting at one.
pub(crate) fn metaspace(s: &str, replacement: char, prepend: bool) -> Vec<String> {
    let mut t = String::with_capacity(s.len() + 3);
    // After the spaces are replaced: a text that starts with one gets
    // no other.
    if prepend && !s.starts_with([replacement, ' ']) {
        t.push(replacement);
    }
    for c in s.chars() {
        t.push(if c == ' ' { replacement } else { c });
    }
    let mut words = Vec::new();
    let mut start = 0;
    for (i, c) in t.char_indices() {
        if c == replacement && i > start {
            words.push(t[start..i].to_string());
            start = i;
        }
    }
    if start < t.len() {
        words.push(t[start..].to_string());
    }
    words
}

/// A byte trie over the pieces: each node its piece's id, if it ends
/// one, and its children's edges, sorted by byte, in one array.
struct Trie {
    /// (first edge, edges, id or `NONE`).
    nodes: Vec<(u32, u32, u32)>,
    /// (byte, child node).
    edges: Vec<(u8, u32)>,
}

const NONE: u32 = u32::MAX;

impl Trie {
    /// From keys sorted and distinct.
    fn new(keys: &[(Box<[u8]>, u32)]) -> Trie {
        let mut t = Trie {
            nodes: Vec::new(),
            edges: Vec::new(),
        };
        t.build(keys, 0);
        t
    }

    fn build(&mut self, mut keys: &[(Box<[u8]>, u32)], depth: usize) -> u32 {
        let node = self.nodes.len();
        self.nodes.push((0, 0, NONE));
        if let Some((k, id)) = keys.first()
            && k.len() == depth
        {
            self.nodes[node].2 = *id;
            keys = &keys[1..];
        }
        let mut groups = Vec::new();
        let mut i = 0;
        while i < keys.len() {
            let b = keys[i].0[depth];
            let n = keys[i..].partition_point(|k| k.0[depth] == b);
            groups.push((b, i..i + n));
            i += n;
        }
        let first = self.edges.len();
        self.edges.extend(groups.iter().map(|&(b, _)| (b, 0)));
        self.nodes[node].0 = first as u32;
        self.nodes[node].1 = groups.len() as u32;
        for (k, (_, r)) in groups.into_iter().enumerate() {
            let child = self.build(&keys[r], depth + 1);
            self.edges[first + k].1 = child;
        }
        node as u32
    }

    /// The pieces that start `s`, shortest first, as (length, id).
    fn prefixes<'a>(&'a self, s: &'a [u8]) -> impl Iterator<Item = (usize, u32)> + 'a {
        let mut node = 0usize;
        let mut depth = 0;
        std::iter::from_fn(move || {
            while depth < s.len() {
                let (first, n, _) = self.nodes[node];
                let edges = &self.edges[first as usize..(first + n) as usize];
                let b = s[depth];
                node = match edges.binary_search_by_key(&b, |e| e.0) {
                    Ok(i) => edges[i].1 as usize,
                    Err(_) => {
                        depth = s.len();
                        return None;
                    }
                };
                depth += 1;
                let id = self.nodes[node].2;
                if id != NONE {
                    return Some((depth, id));
                }
            }
            None
        })
    }
}

/// The Unigram model: a word as the segmentation into pieces whose
/// scores (log probabilities) sum highest, by Viterbi over the word's
/// bytes; a character no piece starts with is the unknown token at the
/// lowest score less ten, and unknowns in a row are one.
pub(crate) struct Unigram {
    trie: Trie,
    scores: Vec<f64>,
    unk: u32,
    unk_score: f64,
}

impl Unigram {
    pub(crate) fn new(pieces: Vec<(String, f64)>, unk: u32) -> Result<Unigram, String> {
        if unk as usize >= pieces.len() {
            return Err("a Unigram model whose unknown token is not in it".into());
        }
        let min = pieces.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let scores = pieces.iter().map(|p| p.1).collect();
        let mut keys: Vec<(Box<[u8]>, u32)> = pieces
            .into_iter()
            .enumerate()
            .map(|(i, (p, _))| (p.into_bytes().into_boxed_slice(), i as u32))
            .collect();
        // A piece listed twice is its last id (as `tokenizers` maps it).
        keys.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        keys.dedup_by(|a, b| a.0 == b.0);
        Ok(Unigram {
            trie: Trie::new(&keys),
            scores,
            unk,
            unk_score: min - 10.0,
        })
    }

    pub(crate) fn word(&self, w: &str, out: &mut Vec<u32>) {
        let s = w.as_bytes();
        let n = s.len();
        // For each end: the best score to it, where its last piece
        // starts, and that piece.
        let mut best: Vec<(f64, usize, u32)> = vec![(0.0, usize::MAX, 0); n + 1];
        best[0].1 = 0;
        let mut start = 0;
        while start < n {
            let here = best[start].0;
            let ch = w[start..].chars().next().map_or(1, char::len_utf8);
            let mut single = false;
            for (len, id) in self.trie.prefixes(&s[start..]) {
                let end = start + len;
                let score = here + self.scores[id as usize];
                if best[end].1 == usize::MAX || score > best[end].0 {
                    best[end] = (score, start, id);
                }
                single |= len == ch;
            }
            if !single {
                let end = start + ch;
                let score = here + self.unk_score;
                if best[end].1 == usize::MAX || score > best[end].0 {
                    best[end] = (score, start, self.unk);
                }
            }
            start += ch;
        }
        let mark = out.len();
        let mut end = n;
        while end > 0 {
            let (_, from, id) = best[end];
            // Unknowns in a row are one.
            if !(id == self.unk && out.len() > mark && out[out.len() - 1] == self.unk) {
                out.push(id);
            }
            end = from;
        }
        out[mark..].reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_segmentation() {
        let pieces = ["<unk>", "▁", "▁ab", "a", "b", "c", "▁abc", "bc"]
            .iter()
            .zip([0.0, -1.0, -2.0, -3.0, -3.0, -3.0, -10.0, -1.0])
            .map(|(p, s)| (p.to_string(), s))
            .collect();
        let u = Unigram::new(pieces, 0).unwrap();
        let mut out = Vec::new();
        u.word("▁abc", &mut out);
        assert_eq!(out, [1, 3, 7], "▁ a bc beats ▁ab c and ▁abc");
        out.clear();
        u.word("▁xyc", &mut out);
        assert_eq!(out, [1, 0, 5], "unknowns in a row are one");
    }

    #[test]
    fn metaspace_words() {
        assert_eq!(
            metaspace("hello  world", '▁', true),
            ["▁hello", "▁", "▁world"]
        );
        assert_eq!(metaspace("▁x", '▁', true), ["▁x"]);
        assert_eq!(metaspace(" x", '▁', true), ["▁x"]);
        assert_eq!(squeeze_spaces("a   b  c d"), "a b c d");
    }
}
