//! Just enough BER (X.690) for LDAP's messages (RFC 4511): definite
//! lengths, the universal types LDAP uses, and its tags.

/// An element being built.
pub(crate) fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = contents.len();
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let bytes = n.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(contents);
    out
}

pub(crate) fn integer(tag: u8, v: i64) -> Vec<u8> {
    let bytes = v.to_be_bytes();
    // The shortest two's complement form.
    let mut i = 0;
    while i < 7
        && ((bytes[i] == 0 && bytes[i + 1] & 0x80 == 0)
            || (bytes[i] == 0xff && bytes[i + 1] & 0x80 != 0))
    {
        i += 1;
    }
    tlv(tag, &bytes[i..])
}

pub(crate) fn octets(tag: u8, b: &[u8]) -> Vec<u8> {
    tlv(tag, b)
}

pub(crate) fn sequence(tag: u8, parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(tag, &parts.concat())
}

pub(crate) const INTEGER: u8 = 0x02;
pub(crate) const OCTET_STRING: u8 = 0x04;
pub(crate) const BOOLEAN: u8 = 0x01;
pub(crate) const ENUMERATED: u8 = 0x0a;
pub(crate) const SEQUENCE: u8 = 0x30;

/// A parsed element: its tag, its contents.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Element<'a> {
    pub tag: u8,
    pub contents: &'a [u8],
}

/// The elements one after another in `b`.
pub(crate) struct Elements<'a>(pub &'a [u8]);

impl<'a> Iterator for Elements<'a> {
    type Item = Result<Element<'a>, String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        Some(match split(self.0) {
            Ok((e, rest)) => {
                self.0 = rest;
                Ok(e)
            }
            Err(e) => {
                self.0 = &[];
                Err(e)
            }
        })
    }
}

/// The first element of `b`, and what follows it.
pub(crate) fn split(b: &[u8]) -> Result<(Element<'_>, &[u8]), String> {
    let bad = || "a malformed LDAP message".to_string();
    let (&tag, rest) = b.split_first().ok_or_else(bad)?;
    let (len, rest) = length(rest).ok_or_else(bad)?;
    if rest.len() < len {
        return Err(bad());
    }
    let (contents, rest) = rest.split_at(len);
    Ok((Element { tag, contents }, rest))
}

/// A definite length, and what follows it.
pub(crate) fn length(b: &[u8]) -> Option<(usize, &[u8])> {
    let (&first, rest) = b.split_first()?;
    if first < 0x80 {
        return Some((usize::from(first), rest));
    }
    let n = usize::from(first & 0x7f);
    if n == 0 || n > 4 || rest.len() < n {
        return None;
    }
    let (l, rest) = rest.split_at(n);
    Some((l.iter().fold(0, |a, &x| (a << 8) | usize::from(x)), rest))
}

pub(crate) fn int(e: Element<'_>) -> Result<i64, String> {
    if e.contents.is_empty() || e.contents.len() > 8 {
        return Err("a malformed LDAP integer".into());
    }
    let neg = e.contents[0] & 0x80 != 0;
    Ok(e.contents
        .iter()
        .fold(if neg { -1i64 } else { 0 }, |a, &x| (a << 8) | i64::from(x)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for v in [0i64, 1, 127, 128, 255, 256, -1, -128, -129, 1 << 40] {
            let b = integer(INTEGER, v);
            let (e, rest) = split(&b).unwrap();
            assert!(rest.is_empty());
            assert_eq!(int(e).unwrap(), v, "{v}");
        }
        assert_eq!(integer(INTEGER, 128), [2, 2, 0, 128]);
        let long = vec![7u8; 300];
        let b = octets(OCTET_STRING, &long);
        assert_eq!(&b[..4], [4, 0x82, 1, 44]);
        assert_eq!(split(&b).unwrap().0.contents, &long[..]);
        assert!(split(&[4, 5, 1]).is_err());
    }
}
