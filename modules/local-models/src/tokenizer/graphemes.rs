//! Extended grapheme clusters (UAX #29, "Unicode Text Segmentation",
//! section 3), from the tables in [`super::grapheme_breaks`]: the units the
//! SentencePiece normalizer maps.  The rules' numbers are the standard's;
//! checked against its `GraphemeBreakTest.txt`.

/// A code point's grapheme-cluster-break class (UAX #29 table 2); the
/// order is `scripts/gen-tokenizer-tables.py`'s.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Gcb {
    Other,
    Cr,
    Lf,
    Control,
    Extend,
    Zwj,
    RegionalIndicator,
    Prepend,
    SpacingMark,
    L,
    V,
    T,
    Lv,
    Lvt,
}

const CLASSES: [Gcb; 14] = [
    Gcb::Other,
    Gcb::Cr,
    Gcb::Lf,
    Gcb::Control,
    Gcb::Extend,
    Gcb::Zwj,
    Gcb::RegionalIndicator,
    Gcb::Prepend,
    Gcb::SpacingMark,
    Gcb::L,
    Gcb::V,
    Gcb::T,
    Gcb::Lv,
    Gcb::Lvt,
];

/// The Indic conjunct break property (rule GB9c).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InCb {
    None,
    Linker,
    Consonant,
    Extend,
}

#[derive(Clone, Copy, Debug)]
struct Props {
    gcb: Gcb,
    pictographic: bool,
    incb: InCb,
}

fn props(c: char) -> Props {
    let c = c as u32;
    let v = if (0x20..0x7F).contains(&c) {
        0
    } else {
        let table = &super::grapheme_breaks::RANGES;
        let i = table.partition_point(|&(_, hi, _)| hi < c);
        match table.get(i) {
            Some(&(lo, _, v)) if lo <= c => v,
            _ => 0,
        }
    };
    Props {
        gcb: CLASSES[(v & 0x0F) as usize],
        pictographic: v & 0x10 != 0,
        incb: match (v >> 5) & 3 {
            1 => InCb::Linker,
            2 => InCb::Consonant,
            3 => InCb::Extend,
            _ => InCb::None,
        },
    }
}

/// The state the rules that look back need, carried along a cluster.
#[derive(Default)]
struct Back {
    /// GB11: Extended_Pictographic Extend* seen, then maybe a ZWJ.
    emoji: bool,
    emoji_zwj: bool,
    /// GB12/13: regional indicators in a row.
    ri: usize,
    /// GB9c: a consonant, extends and linkers since, and a linker among them.
    consonant: bool,
    linker: bool,
}

/// Whether there is a boundary between `a` and `b`, `back` describing
/// what came before `b`.
fn breaks(a: Props, b: Props, back: &Back) -> bool {
    use Gcb::*;
    match (a.gcb, b.gcb) {
        (Cr, Lf) => false,                                                        // GB3
        (Control | Cr | Lf, _) | (_, Control | Cr | Lf) => true,                  // GB4, GB5
        (L, L | V | Lv | Lvt) => false,                                           // GB6
        (Lv | V, V | T) => false,                                                 // GB7
        (Lvt | T, T) => false,                                                    // GB8
        (_, Extend | Zwj) => false,                                               // GB9
        (_, SpacingMark) => false,                                                // GB9a
        (Prepend, _) => false,                                                    // GB9b
        _ if b.incb == InCb::Consonant && back.consonant && back.linker => false, // GB9c
        (Zwj, _) if b.pictographic && back.emoji_zwj => false,                    // GB11
        (RegionalIndicator, RegionalIndicator) => back.ri.is_multiple_of(2),      // GB12, GB13
        _ => true,                                                                // GB999
    }
}

fn advance(back: &mut Back, p: Props) {
    back.emoji_zwj = back.emoji && p.gcb == Gcb::Zwj;
    back.emoji = p.pictographic || (back.emoji && p.gcb == Gcb::Extend);
    back.ri = if p.gcb == Gcb::RegionalIndicator {
        back.ri + 1
    } else {
        0
    };
    match p.incb {
        InCb::Consonant => {
            back.consonant = true;
            back.linker = false;
        }
        InCb::Linker if back.consonant => back.linker = true,
        InCb::Extend if back.consonant => {}
        _ => {
            back.consonant = false;
            back.linker = false;
        }
    }
}

/// The extended grapheme clusters of `s`, in order.
pub(crate) fn clusters(s: &str) -> impl Iterator<Item = &str> {
    let mut rest = s;
    std::iter::from_fn(move || {
        let mut chars = rest.char_indices();
        let (_, first) = chars.next()?;
        let mut a = props(first);
        let mut back = Back::default();
        advance(&mut back, a);
        let mut end = rest.len();
        for (i, c) in chars {
            let b = props(c);
            if breaks(a, b, &back) {
                end = i;
                break;
            }
            advance(&mut back, b);
            a = b;
        }
        let (cluster, tail) = rest.split_at(end);
        rest = tail;
        Some(cluster)
    })
}

#[cfg(test)]
mod tests {
    use super::clusters;

    #[test]
    fn clusters_hold_together() {
        let c: Vec<&str> = clusters("e\u{301}x\r\n🇭🇺🇭🇺👩\u{200D}🔬각").collect();
        assert_eq!(
            c,
            ["e\u{301}", "x", "\r\n", "🇭🇺", "🇭🇺", "👩\u{200D}🔬", "각"]
        );
        assert_eq!(clusters("").count(), 0);
    }

    /// Every case of Unicode's own test (`GraphemeBreakTest.txt`): `÷` a
    /// boundary, `×` none, between the code points given in hex.
    #[test]
    fn unicode_grapheme_break_test() {
        let cases = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/grapheme_break_test.txt"
        ));
        let mut failed = Vec::new();
        let mut n = 0;
        for line in cases.lines().filter(|l| !l.starts_with('#')) {
            let mut text = String::new();
            let mut want = Vec::new();
            for part in line.split_whitespace() {
                match part {
                    "÷" => want.push(text.len()),
                    "×" => {}
                    hex => {
                        text.push(char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap())
                    }
                }
            }
            let mut got = vec![0];
            for c in clusters(&text) {
                got.push(got[got.len() - 1] + c.len());
            }
            if got != want {
                failed.push(line);
            }
            n += 1;
        }
        assert!(n > 1000);
        assert!(
            failed.is_empty(),
            "{} cases differ: {:?}",
            failed.len(),
            &failed[..failed.len().min(5)]
        );
    }
}
