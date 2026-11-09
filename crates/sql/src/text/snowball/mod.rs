//! Snowball stemmers (https://snowballstem.org): the runtime and the
//! algorithms as Snowball 3.0.1's compiler generates them for Rust
//! (`snowball algorithms/<language>.sbl -rust`), with only their `use`
//! paths changed to this module's.  Checked against Snowball's own
//! vocabularies (`crates/sql/tests/data/*_stems.txt`).
//!
//! Copyright (c) 2001, Dr Martin Porter; Copyright (c) 2004,2005, Richard
//! Boulton; Copyright (c) 2013, Yoshiki Shibukawa; Copyright (c)
//! 2006,2007,2009,2010,2011,2014-2019, Olly Betts.  All rights reserved.
//!
//! Redistribution and use in source and binary forms, with or without
//! modification, are permitted provided that the following conditions are
//! met:
//!
//! 1. Redistributions of source code must retain the above copyright
//!    notice, this list of conditions and the following disclaimer.
//! 2. Redistributions in binary form must reproduce the above copyright
//!    notice, this list of conditions and the following disclaimer in the
//!    documentation and/or other materials provided with the distribution.
//! 3. Neither the name of the Snowball project nor the names of its
//!    contributors may be used to endorse or promote products derived from
//!    this software without specific prior written permission.
//!
//! THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
//! "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
//! LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A
//! PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER
//! OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
//! EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
//! PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
//! PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
//! LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
//! NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
//! SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

#![allow(
    missing_docs,
    clippy::all,
    clippy::pedantic,
    unused_imports,
    unused_assignments,
    dead_code
)]

// Generated code and its runtime stay as Snowball wrote them.
#[rustfmt::skip]
mod among;
#[rustfmt::skip]
mod snowball_env;

pub(crate) use among::Among;
pub(crate) use snowball_env::SnowballEnv;

pub(crate) mod algorithms {
    #[rustfmt::skip]
    pub(crate) mod danish_stemmer;
    #[rustfmt::skip]
    pub(crate) mod dutch_stemmer;
    #[rustfmt::skip]
    pub(crate) mod english_stemmer;
    #[rustfmt::skip]
    pub(crate) mod finnish_stemmer;
    #[rustfmt::skip]
    pub(crate) mod french_stemmer;
    #[rustfmt::skip]
    pub(crate) mod german_stemmer;
    #[rustfmt::skip]
    pub(crate) mod hungarian_stemmer;
    #[rustfmt::skip]
    pub(crate) mod italian_stemmer;
    #[rustfmt::skip]
    pub(crate) mod norwegian_stemmer;
    #[rustfmt::skip]
    pub(crate) mod portuguese_stemmer;
    #[rustfmt::skip]
    pub(crate) mod russian_stemmer;
    #[rustfmt::skip]
    pub(crate) mod spanish_stemmer;
    #[rustfmt::skip]
    pub(crate) mod swedish_stemmer;
}

/// The languages, each with its algorithm.
pub(crate) const LANGUAGES: [(&str, fn(&mut SnowballEnv) -> bool); 13] = {
    use algorithms::*;
    [
        ("danish", danish_stemmer::stem),
        ("dutch", dutch_stemmer::stem),
        ("english", english_stemmer::stem),
        ("finnish", finnish_stemmer::stem),
        ("french", french_stemmer::stem),
        ("german", german_stemmer::stem),
        ("hungarian", hungarian_stemmer::stem),
        ("italian", italian_stemmer::stem),
        ("norwegian", norwegian_stemmer::stem),
        ("portuguese", portuguese_stemmer::stem),
        ("russian", russian_stemmer::stem),
        ("spanish", spanish_stemmer::stem),
        ("swedish", swedish_stemmer::stem),
    ]
};

/// `word` stemmed by `algorithm`; `word` is expected in lower case.
pub(crate) fn stem(algorithm: fn(&mut SnowballEnv) -> bool, word: &str) -> String {
    let mut env = SnowballEnv::create(word);
    algorithm(&mut env);
    env.get_current().into_owned()
}

#[cfg(test)]
mod tests {
    use super::{LANGUAGES, stem};

    /// A fortieth of each language's vocabulary, always; the whole of it
    /// where `target/text-data` holds it (`bench/text/data.py`).
    #[test]
    fn snowball_vocabularies() {
        let samples: [(&str, &str); 13] = [
            (
                "danish",
                include_str!("../../../tests/data/danish_stems.txt"),
            ),
            ("dutch", include_str!("../../../tests/data/dutch_stems.txt")),
            (
                "english",
                include_str!("../../../tests/data/english_stems.txt"),
            ),
            (
                "finnish",
                include_str!("../../../tests/data/finnish_stems.txt"),
            ),
            (
                "french",
                include_str!("../../../tests/data/french_stems.txt"),
            ),
            (
                "german",
                include_str!("../../../tests/data/german_stems.txt"),
            ),
            (
                "hungarian",
                include_str!("../../../tests/data/hungarian_stems.txt"),
            ),
            (
                "italian",
                include_str!("../../../tests/data/italian_stems.txt"),
            ),
            (
                "norwegian",
                include_str!("../../../tests/data/norwegian_stems.txt"),
            ),
            (
                "portuguese",
                include_str!("../../../tests/data/portuguese_stems.txt"),
            ),
            (
                "russian",
                include_str!("../../../tests/data/russian_stems.txt"),
            ),
            (
                "spanish",
                include_str!("../../../tests/data/spanish_stems.txt"),
            ),
            (
                "swedish",
                include_str!("../../../tests/data/swedish_stems.txt"),
            ),
        ];
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/text-data/snowball-data"
        );
        for ((language, sample), (name, algorithm)) in samples.into_iter().zip(LANGUAGES) {
            assert_eq!(language, name);
            let mut pairs: Vec<(String, String)> = sample
                .lines()
                .filter(|l| !l.starts_with('#'))
                .map(|l| {
                    let (w, s) = l.split_once(' ').unwrap();
                    (w.to_string(), s.to_string())
                })
                .collect();
            if let (Ok(voc), Ok(out)) = (
                std::fs::read_to_string(format!("{dir}/{language}/voc.txt")),
                std::fs::read_to_string(format!("{dir}/{language}/output.txt")),
            ) {
                pairs = voc
                    .lines()
                    .zip(out.lines())
                    .map(|(v, o)| (v.to_string(), o.to_string()))
                    .collect();
            }
            let failed: Vec<_> = pairs
                .iter()
                .filter(|(w, s)| stem(algorithm, w) != *s)
                .take(5)
                .collect();
            assert!(failed.is_empty(), "{language}: {failed:?}");
        }
    }
}
