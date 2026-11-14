//! `nucleus restore <backup> <dir>` (PLAN.md §M14.24 R6): a new database
//! from a backup `BACKUP TO` wrote.

use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_server::subject_keyfile::SubjectKeyFile;

/// Runs the command.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut keys = None::<String>;
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--subject-keys" => {
                keys = Some(rest.next().ok_or("--subject-keys needs a file")?.clone());
            }
            s if s.starts_with("--") => return Err(format!("restore: unknown option {s}")),
            _ => positional.push(a.clone()),
        }
    }
    let [backup, dir] = positional.as_slice() else {
        return Err("usage: nucleus restore <backup> <dir> [--subject-keys <file>]".into());
    };
    if std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_some()) {
        return Err(format!(
            "restore: {dir} is not empty: a backup is restored into a new database"
        ));
    }
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
    let backup = std::fs::canonicalize(backup).map_err(|e| format!("{backup}: {e}"))?;
    let r = nucleus_sql::backup::restore(&kv, &crate::sql::registry(), &backup)
        .map_err(|e| e.message)?;
    println!(
        "restored commit {}: {} entries ({} opened), {} indexes rebuilt; \
         left out {} entries of forgotten subjects, blanked {} audit details, \
         set {} references",
        r.commit, r.entries, r.opened, r.indexes, r.omitted, r.blanked, r.nulled
    );
    Ok(())
}
