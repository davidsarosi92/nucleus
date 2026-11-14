//! `nucleus migrate` (PLAN.md §M14.16 M2): versioned migration files,
//! applied in order, each in one transaction with its record
//! (`RECORD MIGRATION`, read back as `nc_catalog.migrations`).
//!
//! A migration is a file `<version>_<name>.sql` in the migrations
//! directory (`migrations` by default): `<version>` a whole number (`new`
//! makes it the time, `YYYYMMDDHHMMSS`), the file both directions, as
//! dbmate's —
//!
//! ```sql
//! -- migrate:up
//! CREATE TABLE notes (id int PRIMARY KEY, body text);
//! -- migrate:down
//! DROP TABLE notes;
//! ```
//!
//! — `transaction:false` after a direction's marker for statements that
//! cannot run in a block (`CREATE BRANCH`, `ALTER TABLE … SET STORAGE`).
//! Applied files are checked against their checksums (SHA-256 of the
//! file), so one changed after it was applied is refused, as is one older
//! than the newest applied (out of order) and a record without its file.
//!
//! `--dry-run` (M4) applies on a branch of the database made for it and
//! dropped after, and shows how the schema would change; `diff` compares
//! what the files make (replayed into a database in memory) with the
//! database's schema and writes the statements between them
//! ([`nucleus_sql::schema_diff`]) — `--new` as the next migration.

use std::path::{Path, PathBuf};

use std::sync::Arc;

use crate::dump::rows;
use crate::sql::{Backend, MEMORY, Res, Target, open_local, registry};

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: nucleus migrate <command> [<dir> | :memory:] [connection options of nucleus sql]
                       [--dir <migrations directory, default migrations>]
  new <name>               a new migration file, its version the time (UTC)
  status                   each migration: applied, pending, changed or missing
  up [--to <version>]      the pending ones, in order (up to and with <version>)
  down [--to <version> | --all]
                           the newest applied one reverted (or each above
                           <version>, or all)
      --dry-run            up or down on a branch made for it, dropped after:
                           each one's time and how the schema would change
  diff [--new <name>]      the statements from the schema the files make to
                           the database's; --new <name>: as a new migration,
                           recorded as applied (the database has it already)
Each migration runs in one transaction with its record (nc_catalog.migrations):
one that fails leaves nothing behind.
";

/// A migration file.
#[derive(Debug, Clone)]
pub(crate) struct File {
    pub(crate) version: u64,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) checksum: String,
    pub(crate) up: Section,
    pub(crate) down: Section,
}

/// One direction of a migration.
#[derive(Debug, Clone, Default)]
pub(crate) struct Section {
    pub(crate) sql: String,
    /// `transaction:false`: its statements one by one, not in a block.
    pub(crate) no_transaction: bool,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Parses a migration file's text.
pub(crate) fn parse(text: &str) -> Result<(Section, Section)> {
    let (mut up, mut down) = (None::<Section>, None::<Section>);
    let mut current: Option<&mut Section> = None;
    let mut before = String::new();
    for line in text.lines() {
        let marker = line.trim_end();
        let which = if let Some(rest) = marker.strip_prefix("-- migrate:up") {
            Some((true, rest))
        } else {
            marker
                .strip_prefix("-- migrate:down")
                .map(|rest| (false, rest))
        };
        if let Some((is_up, rest)) = which {
            let options: Vec<&str> = rest.split_whitespace().collect();
            let no_transaction = match options.as_slice() {
                [] => false,
                ["transaction:false"] => true,
                ["transaction:true"] => false,
                _ => {
                    return Err(format!(
                        "unknown option in {marker:?} (there is: transaction:false)"
                    ));
                }
            };
            let slot = if is_up { &mut up } else { &mut down };
            if slot.is_some() {
                return Err(format!("{marker:?} twice"));
            }
            current = Some(slot.insert(Section {
                sql: String::new(),
                no_transaction,
            }));
            continue;
        }
        match current.as_deref_mut() {
            Some(s) => {
                s.sql.push_str(line);
                s.sql.push('\n');
            }
            None => {
                before.push_str(line);
                before.push('\n');
            }
        }
    }
    let Some(up) = up else {
        return Err("no `-- migrate:up` line".into());
    };
    if before.lines().any(|l| {
        let l = l.trim();
        !l.is_empty() && !l.starts_with("--")
    }) {
        return Err("SQL before `-- migrate:up`".into());
    }
    Ok((up, down.unwrap_or_default()))
}

/// The migration files of `dir`, by version.
pub(crate) fn files(dir: &Path) -> Result<Vec<File>> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        format!(
            "{}: {e} (nucleus migrate new <name> makes one)",
            dir.display()
        )
    })?;
    let mut out: Vec<File> = Vec::new();
    for e in entries {
        let path = e.map_err(|e| e.to_string())?.path();
        let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        let Some(stem) = file.strip_suffix(".sql") else {
            continue;
        };
        let Some((v, name)) = stem.split_once('_') else {
            return Err(format!("{file}: not <version>_<name>.sql"));
        };
        let version: u64 = v
            .parse()
            .ok()
            .filter(|_| v.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| format!("{file}: its version {v:?} is not a whole number"))?;
        let bytes = std::fs::read(&path).map_err(|e| format!("{file}: {e}"))?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{file}: not UTF-8"))?;
        // The same checksum on any platform: line ends as \n.
        let text = text.replace("\r\n", "\n");
        let (up, down) = parse(&text).map_err(|e| format!("{file}: {e}"))?;
        if let Some(other) = out.iter().find(|f| f.version == version) {
            return Err(format!(
                "{file} and {}: the same version",
                other.path.display()
            ));
        }
        out.push(File {
            version,
            name: name.to_string(),
            checksum: hex(&nucleus_crypto::sha256(text.as_bytes())),
            path,
            up,
            down,
        });
    }
    out.sort_by_key(|f| f.version);
    Ok(out)
}

/// A migration's record.
#[derive(Debug, Clone)]
pub(crate) struct Record {
    pub(crate) version: u64,
    pub(crate) name: String,
    pub(crate) checksum: String,
    pub(crate) applied_at: String,
}

pub(crate) fn records(b: &mut Backend) -> Result<Vec<Record>> {
    rows(
        b,
        "SELECT version, name, checksum, applied_at FROM nc_catalog.migrations ORDER BY version",
    )?
    .into_iter()
    .map(|r| {
        let t = |i: usize| r[i].clone().unwrap_or_default();
        Ok(Record {
            version: t(0)
                .parse()
                .map_err(|_| "a migration's version".to_string())?,
            name: t(1),
            checksum: t(2),
            applied_at: t(3),
        })
    })
    .collect()
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A section's SQL as a script that ends its last statement.
fn script(sql: &str) -> String {
    let t = sql.trim_end();
    if t.is_empty() || t.ends_with(';') {
        format!("{t}\n")
    } else {
        format!("{t};\n")
    }
}

/// Whether a section has a statement (not only comments and blanks).
fn has_sql(sql: &str) -> bool {
    sql.lines().any(|l| {
        let l = l.trim();
        !l.is_empty() && !l.starts_with("--")
    })
}

/// How a run of a migration ended.
pub(crate) enum Outcome {
    Done,
    /// Another migrator did it first (its record is there, or the commit
    /// conflicted with its).
    Raced,
}

/// Runs `body` then `record` (a `RECORD` or `FORGET MIGRATION`), in one
/// transaction unless the section says not to.
pub(crate) fn apply(b: &mut Backend, label: &str, s: &Section, record: &str) -> Result<Outcome> {
    let text = if s.no_transaction {
        format!("{}{record};\n", script(&s.sql))
    } else {
        format!("BEGIN;\n{}{record};\nCOMMIT;\n", script(&s.sql))
    };
    let results = b.execute(&text)?;
    if let Some(Err(f)) = results.iter().find(|r| r.is_err()) {
        // A block left open by the failure ends here.
        let _ = b.execute("ROLLBACK");
        if f.code == "40001" || f.code == "42710" {
            return Ok(Outcome::Raced);
        }
        return Err(format!("{label}: {f}"));
    }
    // A COMMIT that could not commit says so as its tag.
    if let Some(Ok(Res::Command(tag))) = results.last()
        && tag == "ROLLBACK"
    {
        return Err(format!("{label}: the transaction was rolled back"));
    }
    Ok(Outcome::Done)
}

/// Every applied migration's file unchanged, none missing, none pending
/// older than the newest applied.
pub(crate) fn validate(files: &[File], records: &[Record]) -> Result<()> {
    for r in records {
        match files.iter().find(|f| f.version == r.version) {
            None => {
                return Err(format!(
                    "migration {}_{} is applied but its file is missing",
                    r.version, r.name
                ));
            }
            Some(f) if f.checksum != r.checksum => {
                return Err(format!(
                    "{}: changed since it was applied ({}): a change goes into a new migration",
                    f.path.display(),
                    r.applied_at
                ));
            }
            Some(_) => {}
        }
    }
    if let Some(newest) = records.last()
        && let Some(f) = files
            .iter()
            .find(|f| f.version < newest.version && !records.iter().any(|r| r.version == f.version))
    {
        return Err(format!(
            "{}: older than the newest applied migration ({}_{}) but not applied — give it a newer version",
            f.path.display(),
            newest.version,
            newest.name
        ));
    }
    Ok(())
}

/// The UTC time as `YYYYMMDDHHMMSS`.
fn now_version() -> u64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let (h, mi, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    format!("{y:04}{m:02}{d:02}{h:02}{mi:02}{s:02}")
        .parse()
        .expect("digits")
}

/// A new migration file named `name`, holding `text`: its path.
fn new_file(dir: &Path, name: &str, text: &str) -> Result<PathBuf> {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_matches('_');
    if clean.is_empty() {
        return Err("a migration's name has letters or digits".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut version = now_version();
    // Two made in the same second: the next number.
    if let Ok(existing) = files(dir)
        && let Some(last) = existing.last()
    {
        version = version.max(last.version + 1);
    }
    let path = dir.join(format!("{version}_{clean}.sql"));
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Runs `sql`, every statement of which must succeed.
fn run_ok(b: &mut Backend, sql: &str) -> Result<()> {
    if let Some(Err(f)) = b.execute(sql)?.into_iter().find(|r| r.is_err()) {
        let _ = b.execute("ROLLBACK");
        return Err(format!("{}: {f}", sql.trim()));
    }
    Ok(())
}

/// The schema as `nc_catalog.definitions` has it.
fn definitions(b: &mut Backend) -> Result<Vec<String>> {
    Ok(rows(
        b,
        "SELECT definition FROM nc_catalog.definitions ORDER BY position",
    )?
    .into_iter()
    .map(|r| r[0].clone().unwrap_or_default())
    .collect())
}

/// The pending migrations applied (up to and with `to`).
fn up(b: &mut Backend, files: &[File], records: &[Record], to: Option<u64>) -> Result<()> {
    let pending: Vec<&File> = files
        .iter()
        .filter(|f| !records.iter().any(|r| r.version == f.version))
        .filter(|f| to.is_none_or(|t| f.version <= t))
        .collect();
    if pending.is_empty() {
        println!("up to date");
    }
    for f in pending {
        let label = format!("{}_{}", f.version, f.name);
        let started = std::time::Instant::now();
        let record = format!(
            "RECORD MIGRATION {} NAME {} CHECKSUM {}",
            f.version,
            literal(&f.name),
            literal(&f.checksum)
        );
        match apply(b, &label, &f.up, &record)? {
            Outcome::Done => println!("applied {label} ({} ms)", started.elapsed().as_millis()),
            Outcome::Raced => {
                // Applied meanwhile by another: the same file?
                let now = self::records(b)?;
                match now.iter().find(|r| r.version == f.version) {
                    Some(r) if r.checksum == f.checksum => {
                        println!("{label}: applied by another migrator");
                    }
                    _ => {
                        return Err(format!(
                            "{label}: a conflict with another transaction; run it again"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// The newest applied migration reverted — or each above `to`, or all.
fn down(
    b: &mut Backend,
    files: &[File],
    records: &[Record],
    to: Option<u64>,
    all: bool,
) -> Result<()> {
    if to.is_some() && all {
        return Err("give --to or --all, not both".into());
    }
    let undo: Vec<&Record> = if all {
        records.iter().rev().collect()
    } else if let Some(t) = to {
        records.iter().rev().filter(|r| r.version > t).collect()
    } else {
        records.iter().rev().take(1).collect()
    };
    if undo.is_empty() {
        println!("nothing to revert");
    }
    for r in undo {
        let f = files
            .iter()
            .find(|f| f.version == r.version)
            .expect("validated");
        let label = format!("{}_{}", f.version, f.name);
        if !has_sql(&f.down.sql) {
            return Err(format!("{label}: its `-- migrate:down` is empty"));
        }
        let started = std::time::Instant::now();
        match apply(
            b,
            &label,
            &f.down,
            &format!("FORGET MIGRATION {}", f.version),
        )? {
            Outcome::Done => println!("reverted {label} ({} ms)", started.elapsed().as_millis()),
            Outcome::Raced => {
                return Err(format!(
                    "{label}: a conflict with another transaction; run it again"
                ));
            }
        }
    }
    Ok(())
}

/// `work` run on a branch of `b`'s database made for it (PLAN.md
/// §M14.16 M4), the branch dropped after however it went: what `work`
/// printed, then how the schema would change.
fn dry_run(b: &mut Backend, work: impl FnOnce(&mut Backend) -> Result<()>) -> Result<()> {
    let branch = format!("migrate_dry_run_{}", std::process::id());
    run_ok(b, &format!("CREATE BRANCH {branch}"))?;
    let result = match b.on_branch(&branch) {
        Ok(mut on) => {
            let ran = definitions(&mut on).and_then(|before| {
                work(&mut on)?;
                Ok((before, definitions(&mut on)?))
            });
            let released = on.release();
            ran.and_then(|r| released.map(|()| r))
        }
        Err(e) => Err(e),
    };
    let dropped = run_ok(b, &format!("DROP BRANCH {branch}"));
    let (before, after) = result?;
    dropped?;
    let gone: Vec<&String> = before.iter().filter(|d| !after.contains(d)).collect();
    let made: Vec<&String> = after.iter().filter(|d| !before.contains(d)).collect();
    if gone.is_empty() && made.is_empty() {
        println!("the schema would not change");
    } else {
        println!("the schema would change:");
        for d in gone {
            println!("- {d}");
        }
        for d in made {
            println!("+ {d}");
        }
    }
    println!("dry run: nothing was changed (branch {branch} dropped)");
    Ok(())
}

/// A database in memory, and its store.
fn memory() -> Result<(Backend, Arc<dyn nucleus_kv::Kv>)> {
    let db = open_local(MEMORY)?;
    let kv: Arc<dyn nucleus_kv::Kv> = Arc::new(db.clone());
    let session = nucleus_sql::Session::with_registry(Arc::clone(&kv), registry());
    Ok((
        Backend::Local {
            session,
            db,
            dir: MEMORY.into(),
        },
        kv,
    ))
}

/// `diff [--new name]`: from what the files make to the database's
/// schema.
fn diff(
    b: &mut Backend,
    dir: &Path,
    files: &[File],
    records: &[Record],
    new: Option<&str>,
) -> Result<()> {
    if let Some(f) = files
        .iter()
        .find(|f| !records.iter().any(|r| r.version == f.version))
    {
        return Err(format!(
            "{} is pending: apply it first (nucleus migrate up) — a diff is from what the applied files make to the database",
            f.path.display()
        ));
    }
    let (mut made, made_kv) = memory()?;
    for f in files {
        run_ok(&mut made, &script(&f.up.sql)).map_err(|e| format!("{}: {e}", f.path.display()))?;
    }
    let (mut target, target_kv) = memory()?;
    for d in definitions(b)? {
        run_ok(&mut target, &d)?;
    }
    let schema_diff = |from: &Arc<dyn nucleus_kv::Kv>, to: &Arc<dyn nucleus_kv::Kv>| {
        nucleus_sql::schema_diff::between(&**from, &**to).map_err(|e| e.message)
    };
    let up = schema_diff(&made_kv, &target_kv)?;
    let down = schema_diff(&target_kv, &made_kv)?;
    made.close()?;
    target.close()?;
    if up.statements.is_empty() {
        for n in &up.notes {
            println!("note: {n}");
        }
        println!("no difference: the files make the database's schema");
        return Ok(());
    }
    let Some(name) = new else {
        print!("{}", up.script());
        return Ok(());
    };
    let text = format!(
        "-- migrate:up\n{}\n-- migrate:down\n{}",
        up.script(),
        down.script()
    );
    let path = new_file(dir, name, &text)?;
    let file = self::files(dir)?
        .into_iter()
        .find(|f| f.path == path)
        .ok_or("the new migration")?;
    // The database has the change already: the migration is applied there.
    run_ok(
        b,
        &format!(
            "RECORD MIGRATION {} NAME {} CHECKSUM {}",
            file.version,
            literal(&file.name),
            literal(&file.checksum)
        ),
    )?;
    println!("{}", path.display());
    println!("recorded as applied: the database has its change");
    for n in &up.notes {
        println!("note: {n}");
    }
    Ok(())
}

fn status(files: &[File], records: &[Record]) {
    let mut lines: Vec<(u64, String, String)> = Vec::new();
    for f in files {
        let state = match records.iter().find(|r| r.version == f.version) {
            Some(r) if r.checksum == f.checksum => format!("applied {}", r.applied_at),
            Some(r) => format!("CHANGED since applied {}", r.applied_at),
            None => "pending".to_string(),
        };
        lines.push((f.version, f.name.clone(), state));
    }
    for r in records {
        if !files.iter().any(|f| f.version == r.version) {
            lines.push((r.version, r.name.clone(), "MISSING its file".into()));
        }
    }
    lines.sort_by_key(|l| l.0);
    if lines.is_empty() {
        println!("no migrations");
    }
    for (v, name, state) in lines {
        println!("{v}  {name:<32}  {state}");
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let mut target = Target::new("nucleus migrate");
    let mut dir = PathBuf::from("migrations");
    let mut to: Option<u64> = None;
    let mut all = false;
    let mut dry = false;
    let mut new: Option<String> = None;
    let mut words: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{a} needs {what}"));
        match a.as_str() {
            "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--dir" => dir = PathBuf::from(value("a directory")?),
            "--to" => {
                to = Some(
                    value("a version")?
                        .parse()
                        .map_err(|_| "--to needs a version (a whole number)".to_string())?,
                );
            }
            "--all" => all = true,
            "--dry-run" => dry = true,
            "--new" => new = Some(value("a name")?),
            // The command and its argument come first; then the database.
            s if words.is_empty() || (words[0] == "new" && words.len() == 1) => {
                if s.starts_with('-') {
                    return Err(format!("unknown option {s}\n{USAGE}"));
                }
                words.push(s.to_string());
            }
            _ => {
                if !target.option(a, &mut value)? {
                    return Err(format!("unknown option {a}\n{USAGE}"));
                }
            }
        }
    }
    let command = words.first().map(String::as_str).ok_or(USAGE)?;
    if command == "new" {
        let name = words.get(1).ok_or("nucleus migrate new <name>")?;
        let path = new_file(&dir, name, "-- migrate:up\n\n\n-- migrate:down\n\n")?;
        println!("{}", path.display());
        return Ok(());
    }
    if !matches!(command, "status" | "up" | "down" | "diff") {
        return Err(format!("unknown command {command}\n{USAGE}"));
    }
    if dry && !matches!(command, "up" | "down") {
        return Err("--dry-run is for up and down".into());
    }
    if new.is_some() && command != "diff" {
        return Err("--new is for diff".into());
    }
    let files = files(&dir)?;
    let mut b = target.open()?;
    let result = (|| -> Result<()> {
        let records = records(&mut b)?;
        if command == "status" {
            status(&files, &records);
            return Ok(());
        }
        validate(&files, &records)?;
        let work = |b: &mut Backend| match command {
            "up" => up(b, &files, &records, to),
            _ => down(b, &files, &records, to, all),
        };
        match command {
            "diff" => diff(&mut b, &dir, &files, &records, new.as_deref()),
            _ if dry => dry_run(&mut b, work),
            _ => work(&mut b),
        }
    })();
    let closed = b.close();
    result.and(closed)
}
