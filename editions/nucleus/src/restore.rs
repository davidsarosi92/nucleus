//! `nucleus restore <repository> <dir> [--at <when>]` (PLAN.md §M14.24
//! R6, §M14.18 O3): a new database from the backups `BACKUP TO` wrote —
//! the last, or the last by then.

use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_server::subject_keyfile::SubjectKeyFile;

/// Runs the command.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut keys = None::<String>;
    let mut until = nucleus_sql::backup::Until::Latest;
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--subject-keys" => {
                keys = Some(rest.next().ok_or("--subject-keys needs a file")?.clone());
            }
            "--at" => {
                until = at(rest
                    .next()
                    .ok_or("--at needs a commit, a backup or a time")?)?
            }
            s if s.starts_with("--") => return Err(format!("restore: unknown option {s}")),
            _ => positional.push(a.clone()),
        }
    }
    let [backup, dir] = positional.as_slice() else {
        return Err(
            "usage: nucleus restore <repository> <dir> [--at <commit|backup|time>] \
             [--subject-keys <file>]"
                .into(),
        );
    };
    if std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_some()) {
        return Err(format!(
            "restore: {dir} is not empty: a backup is restored into a new database"
        ));
    }
    let backup = std::fs::canonicalize(backup).map_err(|e| format!("{backup}: {e}"))?;
    // A database made here and not restored is removed again.
    let made = !std::path::Path::new(dir).exists();
    let r = restore_into(dir, &backup, keys, &until);
    if r.is_err() && made {
        let _ = std::fs::remove_dir_all(dir);
    }
    r
}

fn restore_into(
    dir: &str,
    backup: &std::path::Path,
    keys: Option<String>,
    until: &nucleus_sql::backup::Until,
) -> Result<(), String> {
    let db = Database::open(dir, Options::default()).map_err(|e| e.to_string())?;
    let kv: Arc<dyn Kv> = Arc::new(db);
    let path = keys.map_or_else(|| SubjectKeyFile::beside(dir), Into::into);
    if !path.exists() {
        eprintln!(
            "nucleus: no file of subject keys at {}: the rows of subjects are left out \
             (--subject-keys names the file)",
            path.display()
        );
    }
    nucleus_sql::subject_keys::set_subject_keys(Some(Arc::new(SubjectKeyFile::new(
        path,
        Arc::clone(&kv),
    ))));
    let r = nucleus_sql::backup::restore_to(&kv, &crate::sql::registry(), backup, until);
    // The keys' store holds on to the database: let both go.
    nucleus_sql::subject_keys::set_subject_keys(None);
    drop(kv);
    let r = r.map_err(|e| e.message)?;
    println!(
        "restored commit {} from {} backup{}: {} entries ({} opened), {} deleted, \
         {} indexes rebuilt; left out {} entries of forgotten subjects, blanked {} audit \
         details, set {} references",
        r.commit,
        r.backups,
        if r.backups == 1 { "" } else { "s" },
        r.entries,
        r.opened,
        r.deleted,
        r.indexes,
        r.omitted,
        r.blanked,
        r.nulled
    );
    Ok(())
}

/// `--at`'s value: a commit (a number), a backup (its directory's name,
/// `000003-incr`) or a time (`2026-11-15 14:05:00+01`; UTC without a zone).
fn at(s: &str) -> Result<nucleus_sql::backup::Until, String> {
    use nucleus_sql::backup::Until;
    if let Ok(n) = s.parse::<u64>() {
        return Ok(Until::Commit(n));
    }
    if s.len() > 7 && s.as_bytes()[6] == b'-' && s[..6].bytes().all(|b| b.is_ascii_digit()) {
        return Ok(Until::Name(s.to_string()));
    }
    let ts = nucleus_sql::datetime::parse_timestamp(s, true)
        .map_err(|e| format!("--at {s}: not a commit, a backup or a time ({})", e.message))?;
    let unix = ts + nucleus_sql::datetime::UNIX_TO_PG_MICROS;
    u64::try_from(unix)
        .map(Until::Time)
        .map_err(|_| format!("--at {s}: before 1970"))
}
