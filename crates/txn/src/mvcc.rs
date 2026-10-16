//! How versions are laid out in the store.
//!
//! Every version of a user key is its own store entry:
//!
//! ```text
//! escape(user_key) 00 00 | (u64::MAX - ts) big-endian   ->   tag | value
//! ```
//!
//! `escape` doubles every `00` byte into `00 FF`, so `00 00` can only be the
//! terminator, and the encoding is prefix-free: bytewise order of encoded
//! keys is the order of user keys, then of timestamps **newest first**.
//! Reading key `k` as of `ts` is one seek to `encode(k, ts)`; the first entry
//! at or after it, if it belongs to `k`, is the newest version `≤ ts`.
//!
//! The value tag is 1 for a value, 0 for a tombstone (a delete).

/// Bytes added by the encoding besides escaping: terminator and timestamp.
pub const OVERHEAD: usize = 10;

/// `user` with every `00` doubled to `00 FF`.
pub fn escape(user: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(user.len() + OVERHEAD);
    for &b in user {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out
}

/// The store key of `user`'s version at `ts`.
pub fn encode_key(user: &[u8], ts: u64) -> Vec<u8> {
    let mut out = escape(user);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&(u64::MAX - ts).to_be_bytes());
    out
}

/// The smallest store key of any version of `user` (its newest version).
pub fn first_version(user: &[u8]) -> Vec<u8> {
    encode_key(user, u64::MAX)
}

/// A store key above every version of `user` and below every version of
/// any greater user key.
pub fn after_versions(user: &[u8]) -> Vec<u8> {
    let mut out = escape(user);
    out.extend_from_slice(&[0, 1]);
    out
}

/// Splits a store key into user key and timestamp.
pub fn decode_key(key: &[u8]) -> Option<(Vec<u8>, u64)> {
    let mut user = Vec::with_capacity(key.len());
    let mut i = 0;
    while i < key.len() {
        match (key[i], key.get(i + 1)) {
            (0, Some(0xFF)) => {
                user.push(0);
                i += 2;
            }
            (0, Some(0)) => {
                let ts = key.get(i + 2..i + 10)?;
                if key.len() != i + 10 {
                    return None;
                }
                return Some((user, u64::MAX - u64::from_be_bytes(ts.try_into().ok()?)));
            }
            (0, _) => return None,
            (b, _) => {
                user.push(b);
                i += 1;
            }
        }
    }
    None
}

/// The stored form of a value (`None` = tombstone).
pub fn encode_value(value: Option<&[u8]>) -> Vec<u8> {
    match value {
        None => vec![0],
        Some(v) => {
            let mut out = Vec::with_capacity(v.len() + 1);
            out.push(1);
            out.extend_from_slice(v);
            out
        }
    }
}

/// The value a stored form holds (`None` = tombstone); `Err` if malformed.
pub fn decode_value(stored: &[u8]) -> Result<Option<&[u8]>, ()> {
    match stored.split_first() {
        Some((1, v)) => Ok(Some(v)),
        Some((0, [])) => Ok(None),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for user in [&b""[..], b"a", b"\x00", b"a\x00b", b"\x00\x00\xff"] {
            for ts in [0, 1, 77, u64::MAX] {
                assert_eq!(decode_key(&encode_key(user, ts)), Some((user.to_vec(), ts)));
            }
        }
        assert_eq!(decode_key(b"abc"), None);
    }

    #[test]
    fn order_is_user_key_then_newest_first() {
        let users: [&[u8]; 6] = [b"", b"\x00", b"\x00\x00", b"\x00\x01", b"a", b"ab"];
        let mut all = Vec::new();
        for (i, u) in users.iter().enumerate() {
            for ts in [9u64, 5, 1] {
                all.push((i, ts, encode_key(u, ts)));
            }
        }
        let mut sorted = all.clone();
        sorted.sort_by(|a, b| a.2.cmp(&b.2));
        assert_eq!(sorted, all, "encoded order must be (user key asc, ts desc)");
        for (i, u) in users.iter().enumerate() {
            let after = after_versions(u);
            for (j, _, k) in &all {
                assert_eq!(*k < after, *j <= i, "after_versions({u:?}) vs {k:?}");
            }
            assert!(first_version(u) <= encode_key(u, 9));
        }
    }

    #[test]
    fn values() {
        assert_eq!(decode_value(&encode_value(Some(b"x"))), Ok(Some(&b"x"[..])));
        assert_eq!(decode_value(&encode_value(None)), Ok(None));
        assert!(decode_value(&[]).is_err());
        assert!(decode_value(&[0, 1]).is_err());
    }
}
