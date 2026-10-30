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
//!
//! **Intents.**  A large transaction's writes, spilled before it commits,
//! are versions too — *write intents* — whose timestamp is the
//! transaction's id from the top of the range ([`intent_ts`]: `u64::MAX -
//! id`, at or above [`INTENT_FLOOR`]), so a key's intents come before its
//! versions.  A reader resolves one by the transaction's status: its own
//! writes, committed at a timestamp, or neither (running or aborted).
//!
//! **Reserved keys.**  An encoded user key never starts with `00` followed
//! by a byte other than `00` or `FF`, so the store keys from `00 01` up to
//! `00 FF` belong to the engine: a large transaction's status once
//! committed (`00 02 | id -> commit ts`), the next transaction id
//! (`00 03 -> id`), and the side area of earlier versions of the engine,
//! which spilled writes apart (`00 01 | id | user key -> tag | value`),
//! moved into versions when an older database opens.

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
    let ts = decode_key_into(key, &mut user)?;
    Some((user, ts))
}

/// [`decode_key`], the user key into `user` (replacing what it held, its
/// buffer reused).
pub fn decode_key_into(key: &[u8], user: &mut Vec<u8>) -> Option<u64> {
    user.clear();
    decode_key_onto(key, user)
}

/// [`decode_key`], the user key appended to `out`.
pub fn decode_key_onto(key: &[u8], user: &mut Vec<u8>) -> Option<u64> {
    // Byte by byte: user keys hold many short runs between zeros (the
    // big-endian ids of tables and indexes), too short for a search.
    user.reserve(key.len());
    let mut i = 0;
    while i < key.len() {
        let b = key[i];
        if b != 0 {
            user.push(b);
            i += 1;
            continue;
        }
        match key.get(i + 1) {
            Some(0xFF) => {
                user.push(0);
                i += 2;
            }
            Some(0) => {
                let ts = key.get(i + 2..i + 10)?;
                if key.len() != i + 10 {
                    return None;
                }
                return Some(u64::MAX - u64::from_be_bytes(ts.try_into().ok()?));
            }
            _ => return None,
        }
    }
    None
}

/// The lowest timestamp of an intent: commit timestamps stay below it.
pub const INTENT_FLOOR: u64 = 1 << 63;

/// The timestamp intents of transaction `id` carry.
pub fn intent_ts(id: u64) -> u64 {
    u64::MAX - id
}

/// The transaction whose intent a version of timestamp `ts` is, if it is
/// one.
pub fn intent_of(ts: u64) -> Option<u64> {
    (ts >= INTENT_FLOOR).then(|| u64::MAX - ts)
}

/// The record of the next transaction id (above every intent's).
pub const NEXT_ID: [u8; 2] = [0, 3];

/// The first reserved store key: the side area.
pub const SIDE: [u8; 2] = [0, 1];
/// The status records of committed transactions whose spilled writes are
/// still in the side area.
pub const STATUS: [u8; 2] = [0, 2];
/// The first store key past the reserved ones.
pub const RESERVED_END: [u8; 2] = [0, 0xFF];

/// Whether `store_key` is reserved (not a version).
pub fn is_reserved(store_key: &[u8]) -> bool {
    store_key >= &SIDE[..] && store_key < &RESERVED_END[..]
}

/// The side-area key of transaction `id`'s spilled write of `user`.
pub fn side_key(id: u64, user: &[u8]) -> Vec<u8> {
    let mut out = side_prefix(id);
    out.extend_from_slice(user);
    out
}

/// The prefix of transaction `id`'s side area.
pub fn side_prefix(id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    out.extend_from_slice(&SIDE);
    out.extend_from_slice(&id.to_be_bytes());
    out
}

/// Transaction `id`'s status record.
pub fn status_key(id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    out.extend_from_slice(&STATUS);
    out.extend_from_slice(&id.to_be_bytes());
    out
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

/// The value a stored form holds: `Some(None)` is a tombstone, `None` a
/// malformed value.
pub fn decode_value(stored: &[u8]) -> Option<Option<&[u8]>> {
    match stored.split_first() {
        Some((1, v)) => Some(Some(v)),
        Some((0, [])) => Some(None),
        _ => None,
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
    fn reserved_keys_are_no_versions() {
        for user in [&b""[..], b"\x00", b"\x00\x01", b"\xff", b"a"] {
            for ts in [0, 7, u64::MAX] {
                assert!(!is_reserved(&encode_key(user, ts)));
            }
        }
        assert!(is_reserved(&side_key(5, b"\x00\x00")) && is_reserved(&status_key(u64::MAX)));
        assert!(side_key(1, b"\xff\xff") < side_key(2, b""));
        assert_eq!(decode_key(&side_key(1, b"a")), None);
    }

    #[test]
    fn values() {
        assert_eq!(
            decode_value(&encode_value(Some(b"x"))),
            Some(Some(&b"x"[..]))
        );
        assert_eq!(decode_value(&encode_value(None)), Some(None));
        assert!(decode_value(&[]).is_none());
        assert!(decode_value(&[0, 1]).is_none());
    }
}
