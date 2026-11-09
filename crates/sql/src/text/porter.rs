//! The Porter stemmer (M. F. Porter, "An algorithm for suffix
//! stripping", Program 14(3), 1980) as its author's reference
//! implementation has it — with that implementation's three departures
//! from the paper (`bli` → `ble`, `logi` → `log`, words of one or two
//! letters left alone), which Lucene's `PorterStemFilter` and therefore
//! Anserini's BM25 baselines share.  Checked against Snowball's Porter
//! vocabulary (`crates/sql/tests/text.rs`).

/// `word`'s stem as Lucene has it; `word` is expected in lower case.
pub(crate) fn stem(word: &str) -> String {
    stem_as(word, true)
}

/// `word`'s stem, with the reference implementation's departures from
/// the paper (`lucene`) or without them (Snowball's `porter`).
fn stem_as(word: &str, lucene: bool) -> String {
    let b: Vec<char> = word.chars().collect();
    if b.len() <= 2 && lucene || b.len() <= 1 {
        // The paper's step 1a reaches a lone `s` too.
        return if word == "s" && !lucene {
            String::new()
        } else {
            word.to_string()
        };
    }
    let mut s = Stemmer {
        k: b.len() as isize - 1,
        j: 0,
        b,
        lucene,
    };
    s.step1ab();
    if s.k > 0 {
        s.step1c();
        s.step2();
        s.step3();
        s.step4();
        s.step5();
    }
    s.b[..=s.k as usize].iter().collect()
}

struct Stemmer {
    b: Vec<char>,
    /// The end of the word being stemmed (inclusive).
    k: isize,
    /// The end of the stem before a suffix [`Stemmer::ends`] matched.
    j: isize,
    /// The reference implementation's departures from the paper.
    lucene: bool,
}

impl Stemmer {
    fn at(&self, i: isize) -> char {
        self.b[i as usize]
    }

    fn cons(&self, i: isize) -> bool {
        match self.at(i) {
            'a' | 'e' | 'i' | 'o' | 'u' => false,
            'y' => i == 0 || !self.cons(i - 1),
            _ => true,
        }
    }

    /// The number of vowel–consonant sequences in `b[..=j]`.
    fn m(&self) -> usize {
        let (mut n, mut i, j) = (0, 0, self.j);
        loop {
            if i > j {
                return n;
            }
            if !self.cons(i) {
                break;
            }
            i += 1;
        }
        i += 1;
        loop {
            loop {
                if i > j {
                    return n;
                }
                if self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
            n += 1;
            loop {
                if i > j {
                    return n;
                }
                if !self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
        }
    }

    fn vowel_in_stem(&self) -> bool {
        (0..=self.j).any(|i| !self.cons(i))
    }

    fn double_c(&self, j: isize) -> bool {
        j >= 1 && self.at(j) == self.at(j - 1) && self.cons(j)
    }

    /// Consonant–vowel–consonant ending at `i`, the last not w, x or y.
    fn cvc(&self, i: isize) -> bool {
        i >= 2
            && self.cons(i)
            && !self.cons(i - 1)
            && self.cons(i - 2)
            && !matches!(self.at(i), 'w' | 'x' | 'y')
    }

    fn ends(&mut self, s: &str) -> bool {
        let n = s.chars().count() as isize;
        if n > self.k + 1 {
            return false;
        }
        let start = self.k - n + 1;
        if !s.chars().zip(start..).all(|(c, i)| self.at(i) == c) {
            return false;
        }
        self.j = self.k - n;
        true
    }

    fn set_to(&mut self, s: &str) {
        let at = (self.j + 1) as usize;
        self.b.truncate(at);
        self.b.extend(s.chars());
        self.k = self.b.len() as isize - 1;
    }

    fn r(&mut self, s: &str) {
        if self.m() > 0 {
            self.set_to(s);
        }
    }

    fn step1ab(&mut self) {
        if self.at(self.k) == 's' {
            if self.ends("sses") {
                self.k -= 2;
            } else if self.ends("ies") {
                self.set_to("i");
            } else if self.at(self.k - 1) != 's' {
                self.k -= 1;
            }
        }
        if self.ends("eed") {
            if self.m() > 0 {
                self.k -= 1;
            }
        } else if (self.ends("ed") || self.ends("ing")) && self.vowel_in_stem() {
            self.k = self.j;
            if self.ends("at") {
                self.set_to("ate");
            } else if self.ends("bl") {
                self.set_to("ble");
            } else if self.ends("iz") {
                self.set_to("ize");
            } else if self.double_c(self.k) {
                self.k -= 1;
                if matches!(self.at(self.k), 'l' | 's' | 'z') {
                    self.k += 1;
                }
            } else if self.m() == 1 && self.cvc(self.k) {
                self.set_to("e");
            }
        }
    }

    fn step1c(&mut self) {
        if self.ends("y") && self.vowel_in_stem() {
            let k = self.k as usize;
            self.b[k] = 'i';
        }
    }

    /// The first of `rules` (suffix, replacement) that ends the word,
    /// replaced where the stem before it has a vowel–consonant sequence.
    fn first(&mut self, rules: &[(&str, &str)]) {
        for (suffix, to) in rules {
            if self.ends(suffix) {
                self.r(to);
                return;
            }
        }
    }

    fn step2(&mut self) {
        let rules: &[(&str, &str)] = match self.at(self.k - 1) {
            'a' => &[("ational", "ate"), ("tional", "tion")],
            'c' => &[("enci", "ence"), ("anci", "ance")],
            'e' => &[("izer", "ize")],
            'l' if !self.lucene => &[
                ("abli", "able"),
                ("alli", "al"),
                ("entli", "ent"),
                ("eli", "e"),
                ("ousli", "ous"),
            ],
            'l' => &[
                ("bli", "ble"),
                ("alli", "al"),
                ("entli", "ent"),
                ("eli", "e"),
                ("ousli", "ous"),
            ],
            'o' => &[("ization", "ize"), ("ation", "ate"), ("ator", "ate")],
            's' => &[
                ("alism", "al"),
                ("iveness", "ive"),
                ("fulness", "ful"),
                ("ousness", "ous"),
            ],
            't' => &[("aliti", "al"), ("iviti", "ive"), ("biliti", "ble")],
            'g' if self.lucene => &[("logi", "log")],
            _ => &[],
        };
        self.first(rules);
    }

    fn step3(&mut self) {
        let rules: &[(&str, &str)] = match self.at(self.k) {
            'e' => &[("icate", "ic"), ("ative", ""), ("alize", "al")],
            'i' => &[("iciti", "ic")],
            'l' => &[("ical", "ic"), ("ful", "")],
            's' => &[("ness", "")],
            _ => &[],
        };
        self.first(rules);
    }

    fn step4(&mut self) {
        let suffixes: &[&str] = match self.at(self.k - 1) {
            'a' => &["al"],
            'c' => &["ance", "ence"],
            'e' => &["er"],
            'i' => &["ic"],
            'l' => &["able", "ible"],
            'n' => &["ant", "ement", "ment", "ent"],
            'o' => {
                if self.ends("ion") && self.j >= 0 && matches!(self.at(self.j), 's' | 't') {
                    if self.m() > 1 {
                        self.k = self.j;
                    }
                    return;
                }
                &["ou"]
            }
            's' => &["ism"],
            't' => &["ate", "iti"],
            'u' => &["ous"],
            'v' => &["ive"],
            'z' => &["ize"],
            _ => &[],
        };
        if suffixes.iter().any(|s| self.ends(s)) && self.m() > 1 {
            self.k = self.j;
        }
    }

    fn step5(&mut self) {
        self.j = self.k;
        if self.at(self.k) == 'e' {
            let a = self.m();
            if a > 1 || a == 1 && !self.cvc(self.k - 1) {
                self.k -= 1;
            }
        }
        if self.at(self.k) == 'l' && self.double_c(self.k) && self.m() > 1 {
            self.k -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{stem, stem_as};

    /// Snowball's `porter` is the paper's algorithm: the departures off.
    fn check(pairs: &str) {
        let mut failed = Vec::new();
        for line in pairs.lines().filter(|l| !l.starts_with('#')) {
            let (word, want) = line.split_once(' ').unwrap();
            let got = stem_as(word, false);
            if got != want {
                failed.push((word.to_string(), got, want.to_string()));
            }
        }
        assert!(
            failed.is_empty(),
            "{} stems differ: {:?}",
            failed.len(),
            &failed[..failed.len().min(10)]
        );
    }

    /// A fortieth of Snowball's Porter vocabulary, always; the whole of it
    /// where `target/text-data` holds it (`bench/text/data.py`).
    #[test]
    fn snowball_porter_vocabulary() {
        check(include_str!("../../tests/data/porter_stems.txt"));
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/text-data/snowball-data/porter"
        );
        if let (Ok(voc), Ok(out)) = (
            std::fs::read_to_string(format!("{dir}/voc.txt")),
            std::fs::read_to_string(format!("{dir}/output.txt")),
        ) {
            let pairs: String = voc
                .lines()
                .zip(out.lines())
                .map(|(v, o)| format!("{v} {o}\n"))
                .collect();
            check(&pairs);
        }
    }

    /// Lucene's (the reference implementation's) three departures.
    #[test]
    fn lucene_departures() {
        for (word, lucene, paper) in [
            ("superbly", "superbl", "superbli"),
            ("analogies", "analog", "analogi"),
            ("ay", "ay", "ai"),
            ("generalizations", "gener", "gener"),
        ] {
            assert_eq!(stem(word), lucene, "{word}");
            assert_eq!(stem_as(word, false), paper, "{word}");
        }
    }
}
