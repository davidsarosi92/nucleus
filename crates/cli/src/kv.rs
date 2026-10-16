//! `nucleus kv`: one-shot commands and an interactive shell over the KV
//! contract.

use std::io::{BufRead, IsTerminal, Write};
use std::ops::Bound;

use nucleus_kv::{Database, Kv, KvTxn, Options, TxnOptions, prefix_end, transact};

type Result<T> = std::result::Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Turns `\xNN` and `\\` escapes into bytes.
pub fn unescape(s: &str) -> Result<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        match b.get(i + 1) {
            Some(b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            Some(b'x') => {
                let hex = s.get(i + 2..i + 4).ok_or("incomplete \\x escape")?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| format!("bad escape \\x{hex}"))?);
                i += 4;
            }
            _ => return Err("a backslash must start \\\\ or \\xNN".into()),
        }
    }
    Ok(out)
}

/// Bytes as text: printable ASCII as is, everything else as `\xNN`.
pub fn escape(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7E => out.push(c as char),
            _ => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out
}

pub fn run(args: &[String]) -> Result<()> {
    let Some(dir) = args.first() else {
        return Err("kv: missing <dir>".into());
    };
    let db = Database::open(dir, Options::default()).map_err(err)?;
    let result = match args.get(1) {
        None => shell(&db),
        Some(_) => {
            let words: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            let mut out = std::io::stdout().lock();
            let mut session = Session { db: &db, txn: None };
            session.command(&words, &mut out).and_then(|done| {
                if done {
                    Ok(())
                } else {
                    Err("kv: unknown command".into())
                }
            })
        }
    };
    let closed = db.close().map_err(err);
    result.and(closed)
}

struct Session<'a> {
    db: &'a Database,
    /// The open explicit transaction (`begin` … `commit`), if any.
    txn: Option<Box<dyn KvTxn>>,
}

const HELP: &str = "\
get <key>              value of a key
put <key> <value>      set (the value is the rest of the line)
delete <key>           delete
scan [<prefix>]        list keys and values
begin                  start a transaction (otherwise each command commits)
commit | rollback      end it
checkpoint | vacuum    maintenance
quit
";

impl Session<'_> {
    /// Runs one command; returns `Ok(false)` if it is unknown.
    fn command(&mut self, words: &[&str], out: &mut dyn Write) -> Result<bool> {
        let w = |out: &mut dyn Write, s: String| writeln!(out, "{s}").map_err(err);
        match words {
            ["get", key] => {
                let key = unescape(key)?;
                let value = self.read(|t| t.get(&key))?;
                w(out, value.map_or("(not found)".into(), |v| escape(&v)))?;
            }
            ["put", key, value @ ..] if !value.is_empty() => {
                let (key, value) = (unescape(key)?, unescape(&value.join(" "))?);
                self.write(|t| t.put(&key, &value))?;
            }
            ["delete", key] => {
                let key = unescape(key)?;
                self.write(|t| t.delete(&key))?;
            }
            ["scan"] | ["scan", _] => {
                let prefix = words
                    .get(1)
                    .map(|p| unescape(p))
                    .transpose()?
                    .unwrap_or_default();
                let end = prefix_end(&prefix);
                let rows = self.read(|t| {
                    let end = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
                    t.scan(Bound::Included(&prefix), end)?
                        .collect::<nucleus_kv::Result<Vec<_>>>()
                })?;
                for (k, v) in &rows {
                    w(out, format!("{} = {}", escape(k), escape(v)))?;
                }
                w(out, format!("({} rows)", rows.len()))?;
            }
            ["begin"] => {
                if self.txn.is_some() {
                    return Err("a transaction is already open".into());
                }
                self.txn = Some(self.db.begin(TxnOptions::default()).map_err(err)?);
            }
            ["commit"] => {
                let txn = self.txn.take().ok_or("no transaction is open")?;
                let ts = txn.commit().map_err(err)?;
                w(out, format!("committed at {}", ts.0))?;
            }
            ["rollback"] => {
                self.txn.take().ok_or("no transaction is open")?.rollback();
            }
            ["checkpoint"] => self.db.checkpoint().map_err(err)?,
            ["vacuum"] => {
                let n = self.db.vacuum().map_err(err)?;
                w(out, format!("removed {n} old versions"))?;
            }
            ["help"] => write!(out, "{HELP}").map_err(err)?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn read<T>(&self, f: impl FnOnce(&dyn KvTxn) -> nucleus_kv::Result<T>) -> Result<T> {
        match &self.txn {
            Some(t) => f(t.as_ref()).map_err(err),
            None => {
                let t = self.db.begin(TxnOptions::read_only()).map_err(err)?;
                f(t.as_ref()).map_err(err)
            }
        }
    }

    fn write(&mut self, mut f: impl FnMut(&mut dyn KvTxn) -> nucleus_kv::Result<()>) -> Result<()> {
        match &mut self.txn {
            Some(t) => f(t.as_mut()).map_err(err),
            None => transact(self.db, TxnOptions::default(), f).map_err(err),
        }
    }
}

fn shell(db: &Database) -> Result<()> {
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    let mut out = std::io::stdout().lock();
    let mut session = Session { db, txn: None };
    if interactive {
        writeln!(out, "nucleus {} — type `help`", env!("CARGO_PKG_VERSION")).map_err(err)?;
    }
    let mut line = String::new();
    loop {
        if interactive {
            let prompt = if session.txn.is_some() {
                "nucleus*> "
            } else {
                "nucleus> "
            };
            write!(out, "{prompt}").map_err(err)?;
            out.flush().map_err(err)?;
        }
        line.clear();
        if stdin.lock().read_line(&mut line).map_err(err)? == 0 {
            break;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [] => continue,
            ["quit" | "exit"] => break,
            _ => match session.command(&words, &mut out) {
                Ok(true) => {}
                Ok(false) => writeln!(out, "unknown command; type `help`").map_err(err)?,
                Err(e) => writeln!(out, "error: {e}").map_err(err)?,
            },
        }
    }
    if let Some(t) = session.txn.take() {
        t.rollback();
        writeln!(out, "(open transaction rolled back)").map_err(err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_roundtrip() {
        for b in [&b""[..], b"plain", b"\x00\xff\\", b"a b\n"] {
            assert_eq!(unescape(&escape(b)).unwrap(), b);
        }
        assert!(unescape("\\x1").is_err());
        assert!(unescape("\\q").is_err());
    }
}
