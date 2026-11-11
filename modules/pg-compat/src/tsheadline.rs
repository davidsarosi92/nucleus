//! PostgreSQL's `ts_headline([cfg,] document, query [, options])`
//! (PLAN.md §M14.21 T8b), as its default parser's `prsd_headline`
//! chooses and its `generateHeadline` writes: the text as a list of
//! tokens — words, and the runs between them — each word the query's
//! operands it matches; the covers of the query (`hlCover`, checked by
//! `TS_execute` on the words inside); the best one stretched to
//! `MinWords`–`MaxWords` words without ending on a short word
//! (`mark_hl_words`), or the best `MaxFragments` fragments
//! (`mark_hl_fragments`); the matches between `StartSel` and `StopSel`.
//!
//! The words are the `simple` analyzer's (UAX #29 words, every one), the
//! terms and positions the configuration's; the runs between words one
//! token each, as PostgreSQL's parser's blanks.  Departures: HTML tags are
//! text here (PostgreSQL's parser replaces a tag by a space), and URLs,
//! e-mail addresses and hyphenated words are their parts.

use std::ops::RangeInclusive;
use std::sync::Arc;

use nucleus_sql::ext::{
    Arg, ArgInfo, CallContext, DataType, Error, Registry, Result, Returns, ScalarFunction,
    Signature, Value, Volatility,
};
use nucleus_sql::text as nt;

use crate::tsearch::{
    DEFAULT_CONFIG, Phr, Q, Source, Tri, config, execute, output_positions, phrase, query_of,
    tsquery_type,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A word.
    Word,
    /// A number: never a headline's end (PostgreSQL's integers and
    /// decimals).
    Number,
    /// The text between words.
    Space,
}

/// A token of the text (`HeadlineWordEntry`).
#[derive(Clone)]
struct Word<'t, 'q> {
    text: &'t str,
    kind: Kind,
    pos: u32,
    item: Option<&'q Q>,
    repeated: bool,
    selected: bool,
    in_: bool,
}

/// The text's tokens with the operands each word matches (`hlparsetext`).
fn words<'t, 'q>(
    text: &'t str,
    a: &dyn nt::Analyzer,
    items: &[&'q Q],
) -> Result<Vec<Word<'t, 'q>>> {
    let simple = config("simple")?;
    let ws = nt::tokens(simple.as_ref(), text);
    let terms = nt::tokens(a, text);
    let mut out: Vec<Word<'t, 'q>> = Vec::new();
    let space = |s: &'t str| Word {
        text: s,
        kind: Kind::Space,
        pos: 0,
        item: None,
        repeated: false,
        selected: false,
        in_: false,
    };
    let mut at = 0;
    let mut t = 0;
    for w in &ws {
        if w.start < at {
            continue;
        }
        if w.start > at {
            out.push(space(&text[at..w.start]));
        }
        let s = &text[w.start..w.end];
        let number = s.starts_with(|c: char| c.is_ascii_digit())
            && s.bytes()
                .all(|b| b.is_ascii_digit() || b == b'.' || b == b',');
        out.push(Word {
            text: s,
            kind: if number { Kind::Number } else { Kind::Word },
            ..space(s)
        });
        while t < terms.len() && terms[t].start < w.start {
            t += 1;
        }
        // `hlfinditem` for each of the word's terms.
        while t < terms.len() && terms[t].start >= w.start && terms[t].start < w.end {
            let term = &terms[t];
            let last = out.len() - 1;
            out[last].pos = term.position + 1;
            for item in items {
                let Q::Lex { word, prefix, .. } = item else {
                    continue;
                };
                let hit = if *prefix {
                    term.term.starts_with(word.as_str())
                } else {
                    term.term == *word
                };
                if !hit {
                    continue;
                }
                let last = out.len() - 1;
                if out[last].item.is_some() {
                    let mut dup = out[last].clone();
                    dup.item = Some(item);
                    dup.repeated = true;
                    out.push(dup);
                } else {
                    out[last].item = Some(item);
                }
            }
            t += 1;
        }
        at = w.end;
    }
    if at < text.len() {
        out.push(space(&text[at..]));
    }
    Ok(out)
}

/// `checkcondition_HL`: the operand's positions among `words`.
struct HlCheck<'a, 't, 'q>(&'a [Word<'t, 'q>]);

impl Source for HlCheck<'_, '_, '_> {
    fn check(&self, op: &Q, data: Option<&mut Phr>) -> Tri {
        let mut pos: Vec<u32> = Vec::new();
        for w in self.0 {
            if w.item.is_some_and(|i| std::ptr::eq(i, op)) {
                if data.is_none() {
                    return Tri::Yes;
                }
                if pos.last().is_none_or(|&l| l < w.pos) {
                    pos.push(w.pos);
                }
            }
        }
        match data {
            Some(d) if !pos.is_empty() => {
                d.pos = pos;
                Tri::Yes
            }
            _ => Tri::No,
        }
    }
}

/// `TS_execute_locations_recurse`: whether the query matches, and where
/// each of its AND'ed terms and phrases does.
fn locations(q: &Q, src: &dyn Source) -> (bool, Vec<Phr>) {
    match q {
        Q::Lex { .. } => {
            let mut d = Phr::default();
            if src.check(q, Some(&mut d)) == Tri::Yes {
                (true, vec![d])
            } else {
                (false, vec![])
            }
        }
        Q::Stop => (false, vec![]),
        Q::Not(x) => (!locations(x, src).0, vec![]),
        Q::And(l, r) => {
            let (lm, mut ll) = locations(l, src);
            if !lm {
                return (false, vec![]);
            }
            let (rm, rl) = locations(r, src);
            if !rm {
                return (false, vec![]);
            }
            ll.extend(rl);
            (true, ll)
        }
        Q::Or(l, r) => {
            let (lm, ll) = locations(l, src);
            let (rm, rl) = locations(r, src);
            if !(lm || rm) {
                return (false, vec![]);
            }
            if ll.is_empty() {
                return (true, rl);
            }
            if rl.is_empty() {
                return (true, ll);
            }
            let mut out = Vec::new();
            for a in &ll {
                for b in &rl {
                    let mut d = Phr::default();
                    output_positions(a, b, 7, 0, 0, Some(&mut d));
                    d.width = a.width.max(b.width);
                    out.push(d);
                }
            }
            (true, out)
        }
        Q::Phrase(..) => {
            let mut d = Phr::default();
            if phrase(q, src, Some(&mut d)) == Tri::Yes {
                (true, if d.negate { vec![] } else { vec![d] })
            } else {
                (false, vec![])
            }
        }
    }
}

/// The headline's choices (`prsd_headline`'s options).
struct Options {
    min_words: i64,
    max_words: i64,
    short_word: i64,
    max_fragments: i64,
    highlight_all: bool,
    start_sel: String,
    stop_sel: String,
    delimiter: String,
}

/// `deserialize_deflist`: `key = value` pairs, a value bare, `'quoted'`
/// (`''` a quote, `\\` a backslash) or `"quoted"`.
fn deflist(s: &str) -> Result<Vec<(String, String)>> {
    #[derive(PartialEq)]
    enum St {
        WaitKey,
        InKey,
        InQKey,
        WaitEq,
        WaitValue,
        InSq,
        InDq,
        InW,
    }
    let bad = || Error::syntax(format!("invalid parameter list format: \"{s}\""));
    let c: Vec<char> = s.chars().collect();
    let (mut st, mut key, mut val) = (St::WaitKey, String::new(), String::new());
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        match st {
            St::WaitKey => {
                if !(ch.is_ascii_whitespace() || ch == ',') {
                    key.clear();
                    if ch == '"' {
                        st = St::InQKey;
                    } else {
                        key.push(ch);
                        st = St::InKey;
                    }
                }
            }
            St::InKey => {
                if ch.is_ascii_whitespace() {
                    st = St::WaitEq;
                } else if ch == '=' {
                    st = St::WaitValue;
                } else {
                    key.push(ch);
                }
            }
            St::InQKey => {
                if ch == '"' {
                    if c.get(i + 1) == Some(&'"') {
                        key.push('"');
                        i += 1;
                    } else {
                        st = St::WaitEq;
                    }
                } else {
                    key.push(ch);
                }
            }
            St::WaitEq => {
                if ch == '=' {
                    st = St::WaitValue;
                } else if !ch.is_ascii_whitespace() {
                    return Err(bad());
                }
            }
            St::WaitValue => {
                val.clear();
                if ch == '\'' {
                    st = St::InSq;
                } else if ch == 'E' && c.get(i + 1) == Some(&'\'') {
                    i += 1;
                    st = St::InSq;
                } else if ch == '"' {
                    st = St::InDq;
                } else if !ch.is_ascii_whitespace() {
                    val.push(ch);
                    st = St::InW;
                }
            }
            St::InSq => {
                if ch == '\'' {
                    if c.get(i + 1) == Some(&'\'') {
                        val.push('\'');
                        i += 1;
                    } else {
                        out.push((key.clone(), val.clone()));
                        st = St::WaitKey;
                    }
                } else if ch == '\\' && c.get(i + 1) == Some(&'\\') {
                    val.push('\\');
                    i += 1;
                } else {
                    val.push(ch);
                }
            }
            St::InDq => {
                if ch == '"' {
                    if c.get(i + 1) == Some(&'"') {
                        val.push('"');
                        i += 1;
                    } else {
                        out.push((key.clone(), val.clone()));
                        st = St::WaitKey;
                    }
                } else {
                    val.push(ch);
                }
            }
            St::InW => {
                if ch == ',' || ch.is_ascii_whitespace() {
                    out.push((key.clone(), val.clone()));
                    st = St::WaitKey;
                } else {
                    val.push(ch);
                }
            }
        }
        i += 1;
    }
    match st {
        St::InW => out.push((key, val)),
        St::WaitKey => {}
        _ => return Err(bad()),
    }
    Ok(out)
}

impl Options {
    fn of(text: Option<&str>) -> Result<Options> {
        let mut o = Options {
            min_words: 15,
            max_words: 35,
            short_word: 3,
            max_fragments: 0,
            highlight_all: false,
            start_sel: "<b>".into(),
            stop_sel: "</b>".into(),
            delimiter: " ... ".into(),
        };
        let int = |v: &str| {
            v.trim().parse::<i32>().map(i64::from).map_err(|_| {
                Error::invalid_text(format!("invalid input syntax for type integer: \"{v}\""))
            })
        };
        for (k, v) in deflist(text.unwrap_or(""))? {
            match k.to_ascii_lowercase().as_str() {
                "maxwords" => o.max_words = int(&v)?,
                "minwords" => o.min_words = int(&v)?,
                "shortword" => o.short_word = int(&v)?,
                "maxfragments" => o.max_fragments = int(&v)?,
                "startsel" => o.start_sel = v,
                "stopsel" => o.stop_sel = v,
                "fragmentdelimiter" => o.delimiter = v,
                "highlightall" => {
                    o.highlight_all = ["1", "on", "true", "t", "y", "yes"]
                        .iter()
                        .any(|t| v.eq_ignore_ascii_case(t))
                }
                _ => {
                    return Err(Error::invalid_parameter(format!(
                        "unrecognized headline parameter: \"{k}\""
                    )));
                }
            }
        }
        if !o.highlight_all {
            if o.min_words >= o.max_words {
                return Err(Error::invalid_parameter(
                    "MinWords must be less than MaxWords",
                ));
            }
            if o.min_words <= 0 {
                return Err(Error::invalid_parameter("MinWords must be positive"));
            }
            if o.short_word < 0 {
                return Err(Error::invalid_parameter("ShortWord must be >= 0"));
            }
            if o.max_fragments < 0 {
                return Err(Error::invalid_parameter("MaxFragments must be >= 0"));
            }
        }
        Ok(o)
    }
}

/// The selection over the words (`prsd_headline`'s helpers).
struct Hl<'w, 't, 'q> {
    w: &'w mut Vec<Word<'t, 'q>>,
    short_word: i64,
}

impl Hl<'_, '_, '_> {
    fn nonword(&self, j: usize) -> bool {
        self.w[j].kind == Kind::Space
    }
    fn noend(&self, j: usize) -> bool {
        self.w[j].kind != Kind::Word
    }
    fn interesting(&self, j: usize) -> bool {
        self.w[j].item.is_some() && !self.w[j].repeated
    }
    fn bad_end(&self, j: usize) -> bool {
        (self.noend(j) || self.w[j].text.len() as i64 <= self.short_word) && !self.interesting(j)
    }
    fn len(&self) -> usize {
        self.w.len()
    }

    /// `hlCover`.
    fn cover(&self, q: &Q, locs: &[Phr], nextpos: &mut i64) -> Option<(usize, usize)> {
        let mut pos = *nextpos;
        loop {
            let mut pose: i64 = -1;
            for l in locs {
                let first = l.pos.iter().map(|&p| i64::from(p)).find(|&p| p >= pos)?;
                pose = pose.max(first);
            }
            if pose < 0 {
                return None;
            }
            let mut posb = i64::from(i32::MAX) - 1;
            for l in locs {
                let last = l
                    .pos
                    .iter()
                    .rev()
                    .map(|&p| i64::from(p) - i64::from(l.width))
                    .find(|&p| p <= pose)
                    .unwrap_or(-1);
                posb = posb.min(last);
            }
            posb = posb.max(pos);
            if posb <= pose {
                let (mut idxb, mut idxe): (i64, i64) = (-1, -1);
                for (i, w) in self.w.iter().enumerate() {
                    if w.item.is_none() {
                        continue;
                    }
                    if idxb < 0 && i64::from(w.pos) >= posb {
                        idxb = i as i64;
                    }
                    if i64::from(w.pos) <= pose {
                        idxe = i as i64;
                    } else {
                        break;
                    }
                }
                if idxb >= 0 && idxe >= idxb {
                    let (b, e) = (idxb as usize, idxe as usize);
                    if execute(q, &HlCheck(&self.w[b..=e])) != Tri::No {
                        *nextpos = posb + 1;
                        return Some((b, e));
                    }
                }
            }
            pos = posb + 1;
        }
    }

    /// `mark_fragment`.
    fn mark(&mut self, start: i64, end: i64) {
        for i in start.max(0)..=end {
            let w = &mut self.w[i as usize];
            if w.item.is_some() {
                w.selected = true;
            }
            w.in_ = !w.repeated;
        }
    }

    /// `get_next_fragment`.
    fn next_fragment(&self, start: &mut i64, end: &mut i64, max_words: i64) -> (i64, i64) {
        let mut i = *start;
        while i <= *end {
            *start = i;
            if self.interesting(i as usize) {
                break;
            }
            i += 1;
        }
        let (mut curlen, mut poslen) = (0, 0);
        i = *start;
        while i <= *end && curlen < max_words {
            if !self.nonword(i as usize) {
                curlen += 1;
            }
            if self.interesting(i as usize) {
                poslen += 1;
            }
            i += 1;
        }
        if *end > i {
            *end = i;
            while i >= *start {
                *end = i;
                if self.interesting(i as usize) {
                    break;
                }
                if !self.nonword(i as usize) {
                    curlen -= 1;
                }
                i -= 1;
            }
        }
        (curlen, poslen)
    }

    /// `mark_hl_fragments`.
    fn fragments(&mut self, q: Option<&Q>, locs: &[Phr], o: &Options) {
        struct Cover {
            start: i64,
            end: i64,
            poslen: i64,
            curlen: i64,
            chosen: bool,
            excluded: bool,
        }
        let mut covers: Vec<Cover> = Vec::new();
        let mut nextpos = 0;
        if let Some(q) = q {
            while let Some((p, qq)) = self.cover(q, locs, &mut nextpos) {
                let (mut start, mut end) = (p as i64, qq as i64);
                while start <= end {
                    let (c, pl) = self.next_fragment(&mut start, &mut end, o.max_words);
                    covers.push(Cover {
                        start,
                        end,
                        curlen: c,
                        poslen: pl,
                        chosen: false,
                        excluded: false,
                    });
                    start = end + 1;
                    end = qq as i64;
                }
            }
        }
        let mut num_f = 0;
        for _ in 0..o.max_fragments {
            let (mut maxitems, mut minwords, mut min_i) = (0, i64::from(i32::MAX), -1i64);
            for (i, c) in covers.iter().enumerate() {
                if !c.chosen
                    && !c.excluded
                    && (maxitems < c.poslen || (maxitems == c.poslen && minwords > c.curlen))
                {
                    maxitems = c.poslen;
                    minwords = c.curlen;
                    min_i = i as i64;
                }
            }
            if min_i < 0 {
                break;
            }
            let m = min_i as usize;
            covers[m].chosen = true;
            let (mut start, mut end) = (covers[m].start, covers[m].end);
            let mut curlen = covers[m].curlen;
            if curlen < o.max_words {
                let maxstretch = (o.max_words - curlen) / 2;
                let mut stretch = 0;
                let mut posmarker = start;
                let mut i = start - 1;
                while i >= 0 && stretch < maxstretch && !self.w[i as usize].in_ {
                    if !self.nonword(i as usize) {
                        curlen += 1;
                        stretch += 1;
                    }
                    posmarker = i;
                    i -= 1;
                }
                i = posmarker;
                while i < start && self.bad_end(i as usize) {
                    if !self.nonword(i as usize) {
                        curlen -= 1;
                    }
                    i += 1;
                }
                start = i;
                posmarker = end;
                i = end + 1;
                while (i as usize) < self.len() && curlen < o.max_words && !self.w[i as usize].in_ {
                    if !self.nonword(i as usize) {
                        curlen += 1;
                    }
                    posmarker = i;
                    i += 1;
                }
                i = posmarker;
                while i > end && self.bad_end(i as usize) {
                    if !self.nonword(i as usize) {
                        curlen -= 1;
                    }
                    i -= 1;
                }
                end = i;
            }
            covers[m].start = start;
            covers[m].end = end;
            covers[m].curlen = curlen;
            self.mark(start, end);
            num_f += 1;
            for (i, c) in covers.iter_mut().enumerate() {
                if i != m
                    && ((c.start >= start && c.start <= end)
                        || (c.end >= start && c.end <= end)
                        || (c.start < start && c.end > end))
                {
                    c.excluded = true;
                }
            }
        }
        if num_f == 0 {
            let (mut curlen, mut end) = (0, -1i64);
            let mut i = 0;
            while i < self.len() && curlen < o.min_words {
                if !self.nonword(i) {
                    curlen += 1;
                }
                end = i as i64;
                i += 1;
            }
            self.mark(0, end);
        }
    }

    /// `mark_hl_words`.
    fn words(&mut self, q: Option<&Q>, locs: &[Phr], o: &Options) {
        let (mut bestb, mut beste, mut bestlen, mut bestcover) = (-1i64, -1i64, -1i64, false);
        let n = self.len() as i64;
        if !o.highlight_all {
            let mut nextpos = 0;
            while let Some((p, qq)) = q.and_then(|q| self.cover(q, locs, &mut nextpos)) {
                let (p, qq) = (p as i64, qq as i64);
                let (mut curlen, mut poslen) = (0, 0);
                let (mut posb, mut pose) = (p, p);
                let mut i = p;
                while i <= qq && curlen < o.max_words {
                    if !self.nonword(i as usize) {
                        curlen += 1;
                    }
                    if self.interesting(i as usize) {
                        poslen += 1;
                    }
                    pose = i;
                    i += 1;
                }
                if curlen < o.max_words {
                    i -= 1;
                    while i < n && curlen < o.max_words {
                        if i > qq {
                            if !self.nonword(i as usize) {
                                curlen += 1;
                            }
                            if self.interesting(i as usize) {
                                poslen += 1;
                            }
                        }
                        pose = i;
                        if self.bad_end(i as usize) {
                            i += 1;
                            continue;
                        }
                        if curlen >= o.min_words {
                            break;
                        }
                        i += 1;
                    }
                    if curlen < o.min_words {
                        i = p - 1;
                        while i >= 0 {
                            if !self.nonword(i as usize) {
                                curlen += 1;
                            }
                            if self.interesting(i as usize) {
                                poslen += 1;
                            }
                            if curlen >= o.max_words {
                                break;
                            }
                            if self.bad_end(i as usize) {
                                i -= 1;
                                continue;
                            }
                            if curlen >= o.min_words {
                                break;
                            }
                            i -= 1;
                        }
                        posb = i.max(0);
                    }
                } else {
                    if i > qq {
                        i = qq;
                    }
                    while curlen > o.min_words {
                        if !self.bad_end(i as usize) {
                            break;
                        }
                        if !self.nonword(i as usize) {
                            curlen -= 1;
                        }
                        if self.interesting(i as usize) {
                            poslen -= 1;
                        }
                        pose = i - 1;
                        i -= 1;
                    }
                }
                let poscover = posb <= p && pose >= qq;
                if (poscover && !bestcover)
                    || (poscover == bestcover && poslen > bestlen)
                    || (poscover == bestcover
                        && poslen == bestlen
                        && !self.bad_end(pose as usize)
                        && self.bad_end(beste as usize))
                {
                    bestb = posb;
                    beste = pose;
                    bestlen = poslen;
                    bestcover = poscover;
                }
            }
            if bestlen < 0 {
                let (mut curlen, mut pose) = (0, -1i64);
                let mut i = 0;
                while i < n && curlen < o.min_words {
                    if !self.nonword(i as usize) {
                        curlen += 1;
                    }
                    pose = i;
                    i += 1;
                }
                bestb = 0;
                beste = pose;
            }
        } else {
            bestb = 0;
            beste = n - 1;
        }
        self.mark(bestb, beste);
    }
}

/// `generateHeadline`.
fn generate(w: &[Word<'_, '_>], o: &Options) -> String {
    let mut out = String::new();
    let (mut infrag, mut fragments) = (false, 0);
    for x in w {
        if x.in_ && !x.repeated {
            if !infrag {
                infrag = true;
                fragments += 1;
                if fragments > 1 {
                    out.push_str(&o.delimiter);
                }
            }
            if x.selected {
                out.push_str(&o.start_sel);
            }
            out.push_str(x.text);
            if x.selected {
                out.push_str(&o.stop_sel);
            }
        } else if !x.repeated {
            infrag = false;
        }
    }
    out
}

/// The query's operands in PostgreSQL's array order.
fn items<'q>(q: &'q Q, out: &mut Vec<&'q Q>) {
    match q {
        Q::Lex { .. } => out.push(q),
        Q::Stop => {}
        Q::Not(x) => items(x, out),
        Q::And(l, r) | Q::Or(l, r) | Q::Phrase(l, r, _) => {
            items(r, out);
            items(l, out);
        }
    }
}

/// The headline of `doc` for `q` by the analyzer `cfg`.
fn headline(cfg: &str, doc: &str, q: Option<&Q>, opts: Option<&str>) -> Result<String> {
    let a = config(cfg)?;
    let mut ops = Vec::new();
    if let Some(q) = q {
        items(q, &mut ops);
    }
    let mut w = words(doc, a.as_ref(), &ops)?;
    let o = Options::of(opts)?;
    let locs = match q {
        Some(q) => locations(q, &HlCheck(&w)).1,
        None => Vec::new(),
    };
    let mut hl = Hl {
        w: &mut w,
        short_word: o.short_word,
    };
    if o.max_fragments == 0 {
        hl.words(q, &locs, &o);
    } else {
        hl.fragments(q, &locs, &o);
    }
    Ok(generate(&w, &o))
}

/// `ts_headline([cfg,] document, query [, options])`.
struct Headline;

impl ScalarFunction for Headline {
    fn name(&self) -> &str {
        "ts_headline"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=4
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let q = DataType::Ext(tsquery_type());
        let configured = match args.len() {
            4 => true,
            3 => args[1].ty != Some(q),
            _ => false,
        };
        let mut sig = Vec::new();
        if configured {
            sig.push(Arg::Convert(DataType::Text));
        }
        sig.push(Arg::Convert(DataType::Text));
        sig.push(Arg::Untyped(q));
        if sig.len() < args.len() {
            sig.push(Arg::Convert(DataType::Text));
        }
        Ok(Signature::new(sig, Returns::Type(Some(DataType::Text))))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let text = |v: &Value| match v {
            Value::Text(t) => Ok(t.clone()),
            _ => Err(Error::internal("ts_headline of texts")),
        };
        let configured = args.len() == 4 || (args.len() == 3 && matches!(args[1], Value::Text(_)));
        let (cfg, rest) = if configured {
            (text(&args[0])?, &args[1..])
        } else {
            (DEFAULT_CONFIG.to_string(), args)
        };
        let doc = text(&rest[0])?;
        let q = query_of(&rest[1])?;
        let opts = rest.get(2).map(text).transpose()?;
        Ok(Value::Text(headline(
            &cfg,
            &doc,
            q.as_ref(),
            opts.as_deref(),
        )?))
    }
}

/// Registers `ts_headline`.
pub(crate) fn register(r: &mut Registry) -> Result<()> {
    r.add_function(&["ts_headline"], Arc::new(Headline))
}
