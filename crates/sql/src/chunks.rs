//! Chunks of a text (PLAN.md §M14.22 E6): `chunks(text, by, size [,
//! overlap [, model]])`, the table function that cuts a document into
//! the pieces an embedding model takes.
//!
//! A chunk is a run of whole *units*, packed greedily up to `size`:
//!
//! - `sentences`: UAX #29 sentences ([`crate::text::sentences`]);
//! - `tokens`: UAX #29 word segments — a window of `size` tokens;
//! - `markdown`: sections by CommonMark headings (ATX `#`…`######` and
//!   setext underlines; not inside fenced code), a chunk never crossing
//!   one; within a section its blocks (paragraphs between blank lines, a
//!   fenced code block whole).
//!
//! A unit larger than `size` is cut into the next finer units (a block
//! into sentences, a sentence into word segments); a word segment larger
//! than `size` (a model's tokens of one long word) is a chunk alone.
//! Sizes are counted in a model's tokens when one is named (its
//! tokenizer's, without its prefix and special tokens — the provider
//! counts them), else in words.  `overlap`: a chunk starts with the last
//! units of the one before, as many as fit in `overlap` (whole sentences,
//! blocks or segments), and always ahead of where that one started.
//! Chunks are trimmed of white space at both ends; one with nothing else
//! is left out.

use crate::error::{Error, Result};
use crate::text::{sentences, words};

/// How a text is cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// Whole sentences.
    Sentences,
    /// Fixed windows of tokens (words without a model).
    Tokens,
    /// Markdown sections, then blocks, then sentences.
    Markdown,
}

impl By {
    /// `sentences`, `tokens` or `markdown`.
    pub fn parse(s: &str) -> Result<By> {
        match s.to_ascii_lowercase().as_str() {
            "sentences" => Ok(By::Sentences),
            "tokens" => Ok(By::Tokens),
            "markdown" => Ok(By::Markdown),
            other => Err(Error::invalid_parameter(format!(
                "chunks() cuts by sentences, tokens or markdown, not \"{other}\""
            ))),
        }
    }
}

/// A chunk: its byte range in the text, the headings it is under, its
/// size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Where it starts (bytes).
    pub start: usize,
    /// Where it ends (bytes, exclusive).
    pub end: usize,
    /// The headings it is under, outermost first (`markdown`).
    pub headings: Vec<String>,
    /// Its size, in the measure's units.
    pub size: usize,
}

/// What a size is counted in.
pub trait Measure {
    /// The sizes of `pieces`.
    fn sizes(&mut self, pieces: &[&str]) -> Result<Vec<usize>>;

    /// Whether a run's size is its pieces' sum (words: yes; a model's
    /// tokens: nearly — a run is counted again whole).
    fn additive(&self) -> bool;
}

/// Words: UAX #29 segments with a letter or a digit.
pub struct Words;

impl Measure for Words {
    fn sizes(&mut self, pieces: &[&str]) -> Result<Vec<usize>> {
        Ok(pieces.iter().map(|p| words::words(p).count()).collect())
    }

    fn additive(&self) -> bool {
        true
    }
}

/// How finely a unit is cut next.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Level {
    Block,
    Sentence,
    Segment,
}

#[derive(Clone, Copy, Debug)]
struct Unit {
    start: usize,
    end: usize,
    /// The unit it was cut from (0: none) — a chunk packs units of one.
    parent: usize,
    size: usize,
}

/// A text's chunks.
pub fn chunk(
    text: &str,
    by: By,
    size: usize,
    overlap: usize,
    measure: &mut dyn Measure,
) -> Result<Vec<Chunk>> {
    if size == 0 {
        return Err(Error::invalid_parameter("chunks()'s size must be positive"));
    }
    if overlap >= size {
        return Err(Error::invalid_parameter(
            "chunks()'s overlap must be smaller than its size",
        ));
    }
    let mut out = Vec::new();
    let sections = match by {
        By::Markdown => markdown(text),
        _ => vec![Section {
            start: 0,
            end: text.len(),
            headings: Vec::new(),
            blocks: Vec::new(),
        }],
    };
    for s in sections {
        let units: Vec<(usize, usize, Level)> = match by {
            By::Sentences => cut(text, s.start, s.end, Level::Sentence),
            By::Tokens => cut(text, s.start, s.end, Level::Segment),
            By::Markdown => s
                .blocks
                .iter()
                .map(|&(a, b)| (a, b, Level::Block))
                .collect(),
        };
        let units = fit(text, units, size, measure)?;
        pack(text, &units, size, overlap, measure, &s.headings, &mut out)?;
    }
    Ok(out)
}

/// The units of `text[start..end]` at `level`.
fn cut(text: &str, start: usize, end: usize, level: Level) -> Vec<(usize, usize, Level)> {
    let part = &text[start..end];
    let bounds = match level {
        Level::Block => vec![0, part.len()],
        Level::Sentence => sentences::boundaries(part),
        Level::Segment => words::boundaries(part),
    };
    bounds
        .windows(2)
        .map(|w| (start + w[0], start + w[1], level))
        .collect()
}

/// The units measured, those larger than `size` cut finer until they fit
/// or are word segments.
fn fit(
    text: &str,
    units: Vec<(usize, usize, Level)>,
    size: usize,
    measure: &mut dyn Measure,
) -> Result<Vec<Unit>> {
    let mut todo: Vec<(usize, usize, Level, usize)> =
        units.into_iter().map(|(a, b, l)| (a, b, l, 0)).collect();
    let mut done: Vec<Unit> = Vec::new();
    let mut ids = 0;
    while !todo.is_empty() {
        let pieces: Vec<&str> = todo.iter().map(|&(a, b, _, _)| &text[a..b]).collect();
        let sizes = measure.sizes(&pieces)?;
        let mut next = Vec::new();
        for (&(start, end, level, parent), n) in todo.iter().zip(sizes) {
            if n > size && level < Level::Segment {
                let mut parts = cut(text, start, end, Level::Sentence);
                if level == Level::Sentence || parts.len() == 1 {
                    parts = cut(text, start, end, Level::Segment);
                }
                ids += 1;
                next.extend(parts.into_iter().map(|(a, b, l)| (a, b, l, ids)));
            } else {
                done.push(Unit {
                    start,
                    end,
                    parent,
                    size: n,
                });
            }
        }
        todo = next;
    }
    done.sort_by_key(|u| u.start);
    Ok(done)
}

/// Greedy packing of `units` into chunks of at most `size`, each after
/// the first starting `overlap` back.
fn pack(
    text: &str,
    units: &[Unit],
    size: usize,
    overlap: usize,
    measure: &mut dyn Measure,
    headings: &[String],
    out: &mut Vec<Chunk>,
) -> Result<()> {
    let mut a = 0;
    while a < units.len() {
        // As many as fit by the units' sizes.
        let mut b = a + 1;
        let mut total = units[a].size;
        while b < units.len() && units[b].parent == units[a].parent && total + units[b].size <= size
        {
            total += units[b].size;
            b += 1;
        }
        // Units that count nothing (spaces) are not a chunk's start.
        let (start, end) = trimmed(text, units[a].start, units[b - 1].end);
        if start == end {
            a = b;
            continue;
        }
        // A model's count of the run, as the model will see it: fewer
        // units while it is over.
        if !measure.additive() {
            loop {
                let (s, e) = trimmed(text, units[a].start, units[b - 1].end);
                total = measure.sizes(&[&text[s..e]])?[0];
                if total <= size || b == a + 1 {
                    break;
                }
                b -= 1;
            }
        }
        let (start, end) = trimmed(text, units[a].start, units[b - 1].end);
        out.push(Chunk {
            start,
            end,
            headings: headings.to_vec(),
            size: total,
        });
        if b == units.len() {
            break;
        }
        // The next starts with the last units that fit in `overlap` —
        // and leave room for a unit it did not have.
        let mut next = b;
        let mut kept = 0;
        while overlap > 0
            && next > a + 1
            && units[b].parent == units[a].parent
            && kept + units[next - 1].size <= overlap
            && kept + units[next - 1].size + units[b].size <= size
        {
            kept += units[next - 1].size;
            next -= 1;
        }
        a = next;
    }
    Ok(())
}

fn trimmed(text: &str, start: usize, end: usize) -> (usize, usize) {
    let s = &text[start..end];
    let lead = s.len() - s.trim_start().len();
    let t = s.trim();
    (start + lead, start + lead + t.len())
}

// ------------------------------------------------------------ markdown

struct Section {
    start: usize,
    end: usize,
    headings: Vec<String>,
    /// Its blocks' byte ranges.
    blocks: Vec<(usize, usize)>,
}

/// A line's indentation in columns (a tab to the next multiple of 4) and
/// the rest of it.
fn indent(line: &str) -> (usize, &str) {
    let mut cols = 0;
    for (i, c) in line.char_indices() {
        match c {
            ' ' => cols += 1,
            '\t' => cols += 4 - cols % 4,
            _ => return (cols, &line[i..]),
        }
    }
    (cols, "")
}

/// An ATX heading's level and text.
fn atx(line: &str) -> Option<(usize, String)> {
    let (cols, rest) = indent(line);
    if cols > 3 {
        return None;
    }
    let level = rest.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    let after = after.trim_end_matches(['\n', '\r']);
    if !(after.is_empty() || after.starts_with([' ', '\t'])) {
        return None;
    }
    let mut title = after.trim();
    // A closing sequence of #s, after a space (or alone).
    let without = title.trim_end_matches('#');
    if without.is_empty() || without.ends_with([' ', '\t']) {
        title = without.trim_end();
    }
    Some((level, title.to_string()))
}

/// A fence's character and length, if `line` opens or closes one.
fn fence(line: &str) -> Option<(char, usize, &str)> {
    let (cols, rest) = indent(line);
    if cols > 3 {
        return None;
    }
    let c = rest.chars().next().filter(|&c| c == '`' || c == '~')?;
    let n = rest.chars().take_while(|&x| x == c).count();
    (n >= 3).then(|| (c, n, &rest[n..]))
}

/// A setext underline's level.
fn underline(line: &str) -> Option<usize> {
    let (cols, rest) = indent(line);
    let rest = rest.trim_end();
    if cols > 3 || rest.is_empty() {
        return None;
    }
    if rest.bytes().all(|b| b == b'=') {
        Some(1)
    } else if rest.bytes().all(|b| b == b'-') {
        Some(2)
    } else {
        None
    }
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// `text`'s sections, each with its heading path and blocks.
fn markdown(text: &str) -> Vec<Section> {
    let mut sections = vec![Section {
        start: 0,
        end: text.len(),
        headings: Vec::new(),
        blocks: Vec::new(),
    }];
    let mut path: Vec<(usize, String)> = Vec::new();
    // The paragraph being read: its start and its lines' text.
    let mut para: Option<(usize, Vec<&str>)> = None;
    let mut fenced: Option<(char, usize, usize)> = None; // char, length, start
    let mut heading = |sections: &mut Vec<Section>, at: usize, level: usize, title: String| {
        while path.last().is_some_and(|(l, _)| *l >= level) {
            path.pop();
        }
        path.push((level, title));
        let last = sections.last_mut().expect("a section");
        last.end = at;
        sections.push(Section {
            start: at,
            end: text.len(),
            headings: path.iter().map(|(_, t)| t.clone()).collect(),
            blocks: Vec::new(),
        });
    };
    let block = |sections: &mut Vec<Section>, a: usize, b: usize| {
        sections.last_mut().expect("a section").blocks.push((a, b));
    };
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        if let Some((c, n, from)) = fenced {
            if let Some((c2, n2, rest)) = fence(line)
                && c2 == c
                && n2 >= n
                && blank(rest)
            {
                block(&mut sections, from, at);
                fenced = None;
            }
            continue;
        }
        if let Some((c, n, info)) = fence(line)
            && !(c == '`' && info.contains('`'))
        {
            if let Some((p, _)) = para.take() {
                block(&mut sections, p, start);
            }
            fenced = Some((c, n, start));
            continue;
        }
        if blank(line) {
            if let Some((p, _)) = para.take() {
                block(&mut sections, p, start);
            }
            continue;
        }
        if let Some((level, title)) = atx(line) {
            if let Some((p, _)) = para.take() {
                block(&mut sections, p, start);
            }
            heading(&mut sections, start, level, title);
            block(&mut sections, start, at);
            continue;
        }
        if let Some(level) = underline(line)
            && let Some((p, lines)) = para.take()
        {
            // The paragraph above was a setext heading.
            let title = lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
            heading(&mut sections, p, level, title);
            block(&mut sections, p, at);
            continue;
        }
        match &mut para {
            Some((_, lines)) => lines.push(line),
            None => para = Some((start, vec![line])),
        }
    }
    if let Some((_, _, from)) = fenced {
        block(&mut sections, from, text.len());
    }
    if let Some((p, _)) = para {
        block(&mut sections, p, text.len());
    }
    sections.retain(|s| !s.blocks.is_empty());
    sections
}

// ------------------------------------------------------- the function

/// A model's tokens, counted by its provider.
struct ModelTokens<'a> {
    txn: &'a dyn nucleus_kv::KvTxn,
    model: &'a str,
}

impl Measure for ModelTokens<'_> {
    fn sizes(&mut self, pieces: &[&str]) -> Result<Vec<usize>> {
        crate::models::count_tokens(self.txn, self.model, pieces)
    }

    fn additive(&self) -> bool {
        false
    }
}

/// `chunks(text, by, size [, overlap [, model]])`: a row a chunk —
/// `position` (from 1), `chunk`, `start` and `length` (in characters, as
/// `substr` takes them), `headings` (`text[]`), `size` (words, or the
/// model's tokens).
pub(crate) struct Chunks;

impl crate::ext::TableFunction for Chunks {
    fn name(&self) -> &str {
        "chunks"
    }

    fn arity(&self) -> std::ops::RangeInclusive<usize> {
        3..=5
    }

    fn resolve(&self, args: &[crate::ext::ArgInfo]) -> Result<crate::ext::TableSignature> {
        use crate::ext::Arg;
        use crate::types::DataType as T;
        let mut a = vec![
            Arg::Convert(T::Text),
            Arg::Convert(T::Text),
            Arg::Convert(T::BigInt),
        ];
        if args.len() > 3 {
            a.push(Arg::Convert(T::BigInt));
        }
        if args.len() > 4 {
            a.push(Arg::Convert(T::Text));
        }
        Ok(crate::ext::TableSignature {
            args: a,
            columns: vec![
                ("position".into(), Some(T::Integer)),
                ("chunk".into(), Some(T::Text)),
                ("start".into(), Some(T::Integer)),
                ("length".into(), Some(T::Integer)),
                ("headings".into(), T::Text.array_of()),
                ("size".into(), Some(T::Integer)),
            ],
        })
    }

    fn rows(
        &self,
        cx: &crate::ext::CallContext<'_>,
        args: &[crate::types::Value],
    ) -> Result<Vec<crate::types::Row>> {
        use crate::types::Value;
        let text = match &args[0] {
            Value::Null => return Ok(Vec::new()),
            Value::Text(t) => t.as_str(),
            other => return Err(Error::internal(format!("chunks() of {other:?}"))),
        };
        let by = match &args[1] {
            Value::Text(b) => By::parse(b)?,
            _ => return Err(Error::invalid_parameter("chunks()'s by cannot be NULL")),
        };
        let number = |v: Option<&Value>, what: &str, default: i64| -> Result<usize> {
            match v {
                None => Ok(default as usize),
                Some(Value::Int(n)) if *n >= 0 => Ok(*n as usize),
                Some(Value::Null) => Err(Error::invalid_parameter(format!(
                    "chunks()'s {what} cannot be NULL"
                ))),
                Some(_) => Err(Error::invalid_parameter(format!(
                    "chunks()'s {what} cannot be negative"
                ))),
            }
        };
        let size = number(args.get(2), "size", 0)?;
        let overlap = number(args.get(3), "overlap", 0)?;
        let chunks = match args.get(4) {
            None | Some(Value::Null) => chunk(text, by, size, overlap, &mut Words)?,
            Some(Value::Text(m)) => {
                let mut measure = ModelTokens {
                    txn: cx.txn(),
                    model: m,
                };
                chunk(text, by, size, overlap, &mut measure)?
            }
            Some(other) => return Err(Error::internal(format!("chunks() model {other:?}"))),
        };
        // Character offsets, counted once along the text.
        let (mut byte, mut ch) = (0usize, 0usize);
        let mut out = Vec::with_capacity(chunks.len());
        for (i, c) in chunks.into_iter().enumerate() {
            ch += text[byte..c.start].chars().count();
            byte = c.start;
            let piece = &text[c.start..c.end];
            out.push(vec![
                Value::Int(i as i64 + 1),
                Value::Text(piece.to_string()),
                Value::Int(ch as i64 + 1),
                Value::Int(piece.chars().count() as i64),
                Value::Array(c.headings.into_iter().map(Value::Text).collect()),
                Value::Int(c.size as i64),
            ]);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts<'a>(text: &'a str, cs: &[Chunk]) -> Vec<&'a str> {
        cs.iter().map(|c| &text[c.start..c.end]).collect()
    }

    #[test]
    fn sentences_packed_with_overlap() {
        let t = "One two three. Four five. Six seven eight nine. Ten.";
        let cs = chunk(t, By::Sentences, 5, 0, &mut Words).unwrap();
        assert_eq!(
            texts(t, &cs),
            ["One two three. Four five.", "Six seven eight nine. Ten."]
        );
        assert_eq!(cs.iter().map(|c| c.size).collect::<Vec<_>>(), [5, 5]);
        // Overlap only where the next chunk has room for something new.
        let cs = chunk(t, By::Sentences, 5, 2, &mut Words).unwrap();
        assert_eq!(
            texts(t, &cs),
            ["One two three. Four five.", "Six seven eight nine. Ten."]
        );
        let cs = chunk(t, By::Sentences, 6, 2, &mut Words).unwrap();
        assert_eq!(
            texts(t, &cs),
            [
                "One two three. Four five.",
                "Four five. Six seven eight nine.",
                "Ten."
            ]
        );
        // A sentence over the size is cut into words.
        let cs = chunk(t, By::Sentences, 3, 0, &mut Words).unwrap();
        assert_eq!(
            texts(t, &cs),
            [
                "One two three.",
                "Four five.",
                "Six seven eight",
                "nine.",
                "Ten."
            ]
        );
    }

    #[test]
    fn token_windows() {
        let t = "a b c d e f g";
        let cs = chunk(t, By::Tokens, 3, 1, &mut Words).unwrap();
        assert_eq!(texts(t, &cs), ["a b c", "c d e", "e f g"]);
        assert!(chunk(t, By::Tokens, 3, 3, &mut Words).is_err());
        assert!(chunk(t, By::Tokens, 0, 0, &mut Words).is_err());
        assert!(chunk("", By::Tokens, 3, 0, &mut Words).unwrap().is_empty());
        assert!(
            chunk("  \n ", By::Sentences, 3, 0, &mut Words)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn markdown_sections_and_blocks() {
        let t = "Intro line.\n\n# Title\n\nFirst para here.\n\n## Part ##\n\nSecond para.\nStill second.\n\n```\n# not a heading\n```\n\nSetext\n------\nUnder setext.\n";
        let cs = chunk(t, By::Markdown, 100, 0, &mut Words).unwrap();
        let got: Vec<(Vec<String>, &str)> = cs
            .iter()
            .map(|c| (c.headings.clone(), &t[c.start..c.end]))
            .collect();
        let h = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            got,
            [
                (h(&[]), "Intro line."),
                (h(&["Title"]), "# Title\n\nFirst para here."),
                (
                    h(&["Title", "Part"]),
                    "## Part ##\n\nSecond para.\nStill second.\n\n```\n# not a heading\n```"
                ),
                (h(&["Title", "Setext"]), "Setext\n------\nUnder setext."),
            ]
        );
        // Blocks packed apart when they do not fit together.
        let cs = chunk(t, By::Markdown, 3, 0, &mut Words).unwrap();
        assert_eq!(
            texts(t, &cs)[..4],
            [
                "Intro line.",
                "# Title",
                "First para here.",
                // A block cut into sentences is not packed with another.
                "## Part ##"
            ]
        );
    }

    #[test]
    fn atx_headings_as_commonmark_reads_them() {
        assert_eq!(atx("# a"), Some((1, "a".into())));
        assert_eq!(atx("### b ###\n"), Some((3, "b".into())));
        assert_eq!(atx("#"), Some((1, "".into())));
        assert_eq!(atx("## c#"), Some((2, "c#".into())));
        assert_eq!(atx("#a"), None);
        assert_eq!(atx("####### x"), None);
        assert_eq!(atx("    # code"), None);
        assert_eq!(atx("   # three"), Some((1, "three".into())));
    }
}
