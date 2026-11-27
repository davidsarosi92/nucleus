//! MongoDB's wire protocol, the part clients speak today: `OP_MSG`
//! (opcode 2013) for every command, and `OP_QUERY` (2004) on a
//! database's `$cmd` for the first `hello` / `isMaster` a driver sends
//! before it knows the server, answered `OP_REPLY` (1).  Every integer is
//! little-endian; a message is a 16-byte header (length, request id, the
//! id it answers, opcode) and its body.

use std::io::{self, Read, Write};

use crate::bson::{self, B, Doc};

pub const OP_REPLY: i32 = 1;
pub const OP_QUERY: i32 = 2004;
pub const OP_MSG: i32 = 2013;

/// The largest message taken (MongoDB's `maxMessageSizeBytes`).
pub const MAX_MESSAGE: usize = 48_000_000;

/// A message read: its header and body.
pub struct Message {
    pub request_id: i32,
    pub opcode: i32,
    pub body: Vec<u8>,
}

fn bad(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

/// The next message, or None when the client has gone.
pub fn read(r: &mut impl Read) -> io::Result<Option<Message>> {
    let mut h = [0u8; 16];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = i32::from_le_bytes(h[0..4].try_into().unwrap());
    let len = usize::try_from(len).map_err(|_| bad("a negative message length"))?;
    if !(16..=MAX_MESSAGE).contains(&len) {
        return Err(bad(format!("a message of {len} bytes")));
    }
    let mut body = vec![0u8; len - 16];
    r.read_exact(&mut body)?;
    Ok(Some(Message {
        request_id: i32::from_le_bytes(h[4..8].try_into().unwrap()),
        opcode: i32::from_le_bytes(h[12..16].try_into().unwrap()),
        body,
    }))
}

/// An `OP_MSG`'s command: its body document with each document
/// sequence (section kind 1 — `insert`'s `documents`, …) added as an
/// array field; and whether the client wants no reply (`moreToCome`).
pub fn op_msg(body: &[u8]) -> Result<(Doc, bool), bson::Bad> {
    if body.len() < 5 {
        return Err(bson::Bad("an OP_MSG without sections".into()));
    }
    let flags = u32::from_le_bytes(body[0..4].try_into().unwrap());
    // Bits 0–15 are required to be understood: 0 checksumPresent,
    // 1 moreToCome; any other is an error.  16 exhaustAllowed is optional.
    if flags & 0xFFFC != 0 {
        return Err(bson::Bad(format!(
            "OP_MSG flags 0x{flags:x} are not supported"
        )));
    }
    let end = if flags & 1 != 0 {
        body.len().saturating_sub(4)
    } else {
        body.len()
    };
    let mut at = 4;
    let mut doc: Option<Doc> = None;
    let mut sequences: Vec<(String, B)> = Vec::new();
    while at < end {
        let kind = body[at];
        at += 1;
        match kind {
            0 => {
                let (d, n) = bson::read_doc(&body[at..end])?;
                if doc.replace(d).is_some() {
                    return Err(bson::Bad("an OP_MSG with two bodies".into()));
                }
                at += n;
            }
            1 => {
                let size = body
                    .get(at..at + 4)
                    .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
                    .and_then(|s| usize::try_from(s).ok())
                    .filter(|&s| s >= 4 && at + s <= end)
                    .ok_or_else(|| bson::Bad("a document sequence's size does not fit".into()))?;
                let section = &body[at + 4..at + size];
                let name_end = section
                    .iter()
                    .position(|&c| c == 0)
                    .ok_or_else(|| bson::Bad("a document sequence without its name".into()))?;
                let name = String::from_utf8_lossy(&section[..name_end]).into_owned();
                let mut docs = Vec::new();
                let mut p = name_end + 1;
                while p < section.len() {
                    let (d, n) = bson::read_doc(&section[p..])?;
                    docs.push(B::Doc(d));
                    p += n;
                }
                sequences.push((name, B::Arr(docs)));
                at += size;
            }
            k => return Err(bson::Bad(format!("OP_MSG section kind {k}"))),
        }
    }
    let mut doc = doc.ok_or_else(|| bson::Bad("an OP_MSG without a body".into()))?;
    doc.extend(sequences);
    Ok((doc, flags & 2 != 0))
}

/// An `OP_QUERY`'s namespace and query document (its `$query` when
/// wrapped).
pub fn op_query(body: &[u8]) -> Result<(String, Doc), bson::Bad> {
    let rest = body
        .get(4..)
        .ok_or_else(|| bson::Bad("a short OP_QUERY".into()))?;
    let end = rest
        .iter()
        .position(|&c| c == 0)
        .ok_or_else(|| bson::Bad("an OP_QUERY without its namespace".into()))?;
    let ns = String::from_utf8_lossy(&rest[..end]).into_owned();
    let q = rest
        .get(end + 9..)
        .ok_or_else(|| bson::Bad("a short OP_QUERY".into()))?;
    let (mut doc, _) = bson::read_doc(q)?;
    if let Some((k, B::Doc(inner))) = doc.first()
        && (k == "$query" || k == "query")
    {
        doc = inner.clone();
    }
    Ok((ns, doc))
}

fn header(len: usize, request_id: i32, response_to: i32, opcode: i32) -> [u8; 16] {
    let mut h = [0u8; 16];
    h[0..4].copy_from_slice(&(len as i32).to_le_bytes());
    h[4..8].copy_from_slice(&request_id.to_le_bytes());
    h[8..12].copy_from_slice(&response_to.to_le_bytes());
    h[12..16].copy_from_slice(&opcode.to_le_bytes());
    h
}

/// Writes `reply` as an `OP_MSG` answering `to`.
pub fn write_msg(w: &mut impl Write, request_id: i32, to: i32, reply: &Doc) -> io::Result<()> {
    let doc = bson::write_doc(reply);
    w.write_all(&header(16 + 5 + doc.len(), request_id, to, OP_MSG))?;
    w.write_all(&0u32.to_le_bytes())?;
    w.write_all(&[0])?;
    w.write_all(&doc)
}

/// Writes `reply` as an `OP_REPLY` answering `to` (`failed`: the
/// QueryFailure flag).
pub fn write_reply(
    w: &mut impl Write,
    request_id: i32,
    to: i32,
    reply: &Doc,
    failed: bool,
) -> io::Result<()> {
    let doc = bson::write_doc(reply);
    w.write_all(&header(16 + 20 + doc.len(), request_id, to, OP_REPLY))?;
    // AwaitCapable (8), QueryFailure (2).
    let flags: i32 = 8 | if failed { 2 } else { 0 };
    w.write_all(&flags.to_le_bytes())?;
    w.write_all(&0i64.to_le_bytes())?;
    w.write_all(&0i32.to_le_bytes())?;
    w.write_all(&1i32.to_le_bytes())?;
    w.write_all(&doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_msg_sections() {
        let body = bson::write_doc(&[
            ("insert".into(), B::Str("c".into())),
            ("$db".into(), B::Str("t".into())),
        ]);
        let d1 = bson::write_doc(&[("a".into(), B::Int32(1))]);
        let mut seq = Vec::new();
        seq.extend_from_slice(&((4 + 10 + d1.len() * 2) as i32).to_le_bytes());
        seq.extend_from_slice(b"documents\0");
        seq.extend_from_slice(&d1);
        seq.extend_from_slice(&d1);
        let mut m = 0u32.to_le_bytes().to_vec();
        m.push(0);
        m.extend_from_slice(&body);
        m.push(1);
        m.extend_from_slice(&seq);
        let (doc, more) = op_msg(&m).unwrap();
        assert!(!more);
        assert_eq!(doc.len(), 3);
        assert!(matches!(bson::get(&doc, "documents"), Some(B::Arr(a)) if a.len() == 2));
        // Unknown required flags, a short sequence: refused.
        let mut bad = m.clone();
        bad[0] = 4;
        assert!(op_msg(&bad).is_err());
        let mut short = m;
        let n = short.len();
        short.truncate(n - 3);
        assert!(op_msg(&short).is_err());
    }
}
