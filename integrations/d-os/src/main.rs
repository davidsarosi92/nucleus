//! `nucleus-settings <dir>`: the d-os settings service.  Reads one command
//! per line on stdin, answers on stdout (protocol in `README.md`).

use std::io::{BufRead, Write};
use std::process::ExitCode;

use nucleus_dos_settings::{Layer, Settings};
use nucleus_kv::{Database, Options, Timestamp};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir] = args.as_slice() else {
        eprintln!("usage: nucleus-settings <dir>");
        return ExitCode::from(2);
    };
    // Keep old versions for a while, so earlier settings can be read back.
    let opts = Options {
        cache_bytes: 1 << 20,
        history: 10_000,
        ..Options::default()
    };
    let db = match Database::open(dir, opts) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("nucleus-settings: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut settings = Settings::new(&db);
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "READY nucleus-settings {}", env!("CARGO_PKG_VERSION"));
    let _ = out.flush();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let reply = handle(&mut settings, &line);
        if reply.as_deref() == Some("BYE") {
            let _ = writeln!(out, "BYE");
            break;
        }
        if let Some(r) = reply {
            let _ = writeln!(out, "{r}");
        }
        let _ = out.flush();
    }
    if settings.staged() > 0 {
        eprintln!(
            "nucleus-settings: {} staged changes discarded (no COMMIT)",
            settings.staged()
        );
    }
    drop(settings);
    match db.close() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nucleus-settings: {e}");
            ExitCode::FAILURE
        }
    }
}

fn layer(s: &str) -> Result<Layer, String> {
    Layer::parse(s).ok_or_else(|| format!("ERR bad layer {s:?} (machine or user/<uid>)"))
}

/// One command → one reply (several lines for listings; `None` for a blank
/// line).
fn handle(s: &mut Settings, line: &str) -> Option<String> {
    let mut parts = line.trim_start().splitn(4, ' ');
    let cmd = parts.next().unwrap_or("");
    let args: Vec<&str> = parts.collect();
    let r: Result<String, String> = (|| {
        let e = |e: nucleus_kv::Error| format!("ERR {e}");
        match (cmd, args.as_slice()) {
            ("", _) => Err(String::new()),
            ("GET", [l, k]) => Ok(match s.get(&layer(l)?, k).map_err(e)? {
                Some(v) => format!("OK {v}"),
                None => "NONE".into(),
            }),
            ("SET", [l, k, v]) => {
                s.set(layer(l)?, k, v);
                Ok("OK".into())
            }
            ("SET", [l, k]) => {
                s.set(layer(l)?, k, "");
                Ok("OK".into())
            }
            ("UNSET", [l, k]) => {
                s.unset(layer(l)?, k);
                Ok("OK".into())
            }
            ("COMMIT", []) => Ok(format!("OK {}", s.commit().map_err(e)?)),
            ("DISCARD", []) => {
                s.discard();
                Ok("OK".into())
            }
            ("SNAPSHOT", []) => Ok(format!("OK {}", s.snapshot().map_err(e)?)),
            ("DUMP", [l]) => Ok(listing(s.dump(&layer(l)?).map_err(e)?)),
            ("DUMP", [l, "AT", ts]) => {
                let ts: Timestamp = ts.parse().map_err(|_| "ERR bad timestamp".to_string())?;
                Ok(listing(s.dump_at(&layer(l)?, ts).map_err(e)?))
            }
            ("LAYERS", []) => {
                let mut out = String::new();
                for l in s.layers().map_err(e)? {
                    out.push_str(&format!("{l}\n"));
                }
                Ok(out + "END")
            }
            ("IMPORT", [l, path]) => {
                let conf =
                    std::fs::read_to_string(path).map_err(|err| format!("ERR {path}: {err}"))?;
                let n = s.import(&layer(l)?, &conf);
                Ok(format!("OK {n}"))
            }
            ("EXPORT", [l, path]) => {
                let text = s.export(&layer(l)?).map_err(e)?;
                std::fs::write(path, text).map_err(|err| format!("ERR {path}: {err}"))?;
                Ok("OK".into())
            }
            ("QUIT", []) => Ok("BYE".into()),
            _ => Err(format!("ERR unknown command or wrong arguments: {line:?}")),
        }
    })();
    match r {
        Ok(reply) => Some(reply),
        Err(e) if e.is_empty() => None,
        Err(e) => Some(e),
    }
}

fn listing(rows: Vec<(String, String)>) -> String {
    let mut out = String::new();
    for (k, v) in rows {
        out.push_str(&format!("{k} = {v}\n"));
    }
    out + "END"
}
