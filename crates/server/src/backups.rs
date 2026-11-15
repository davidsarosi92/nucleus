//! Scheduled backups (PLAN.md §M14.18 O3c): `nucleus server --backup-to
//! <repository> --backup-every <interval>` — a backup each interval after
//! the repository's last (at once if that is overdue), every
//! `full_every`-th a full one and the others incremental, the older
//! chains removed past `keep` full ones.  A failed one is tried again
//! within five minutes.  Each is counted in the metrics
//! (`nucleus_backup_*`), the schedule's interval too, and the repository's
//! last backup's time is read at the start: the alerts
//! `NucleusBackupOverdue` and `NucleusBackupFailing` watch them.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nucleus_kv::Kv;

use crate::Stop;

/// A schedule of backups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// The repository (`BACKUP TO`'s).
    pub to: String,
    /// The time between backups.
    pub every: Duration,
    /// Every this many backups, a full one (1: every one).
    pub full_every: u64,
    /// The full backups kept (`KEEP`), with those after them.
    pub keep: Option<u64>,
}

/// How soon a failed backup is tried again (at most).
const RETRY: Duration = Duration::from_secs(300);

fn unix_micros(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// Starts the schedule on a thread of its own, until `stop`.
pub fn start(kv: Arc<dyn Kv>, s: Schedule, stop: Stop) -> std::thread::JoinHandle<()> {
    nucleus_sql::metrics::BACKUP_INTERVAL.set(s.every.as_secs() as i64);
    std::thread::spawn(move || {
        let every = s.every.as_micros() as u64;
        // The repository's last backup: when the next is due, and how many
        // incremental ones since a full one.
        let (mut due, mut since_full) = match nucleus_sql::backup::last_backup(&kv, &s.to) {
            Ok(Some(last)) => {
                nucleus_sql::metrics::BACKUP_LAST.set((last.time / 1_000_000) as i64);
                (last.time.saturating_add(every), Some(last.since_full))
            }
            Ok(None) => (0, None),
            Err(e) => {
                nucleus_platform::log!("nucleus: backups to {}: {}", s.to, e.message);
                (0, None)
            }
        };
        loop {
            let now = unix_micros(kv.wall_clock());
            if now < due {
                // Woken now and then: the wall clock may jump.
                let wait = Duration::from_micros(due - now).min(Duration::from_secs(60));
                if stop.wait(wait) {
                    return;
                }
                continue;
            }
            let full = since_full.is_none_or(|n| n + 1 >= s.full_every);
            match nucleus_sql::backup::backup(&kv, &s.to, full, s.keep) {
                Ok(b) => {
                    nucleus_platform::log!(
                        "nucleus: backup {} to {} ({}, commit {}, {} entries, {} bytes{})",
                        b.name,
                        s.to,
                        b.kind,
                        b.commit,
                        b.entries,
                        b.bytes,
                        if b.removed > 0 {
                            format!("; {} older removed", b.removed)
                        } else {
                            String::new()
                        }
                    );
                    since_full = Some(if b.kind == "full" {
                        0
                    } else {
                        since_full.unwrap_or(0) + 1
                    });
                    due = now.saturating_add(every);
                }
                Err(e) => {
                    nucleus_platform::log!("nucleus: backup to {} failed: {}", s.to, e.message);
                    due = now.saturating_add(every.min(RETRY.as_micros() as u64));
                }
            }
            if stop.stopped() {
                return;
            }
        }
    })
}

/// Sets the key backups are made and restored with (§M14.18 O3c): from
/// `file` (`--backup-key`), else from the file `NUCLEUS_BACKUP_KEY_FILE`
/// names, else none.  The file holds 64 hexadecimal digits
/// (`openssl rand -hex 32`); what it names: its id.
pub fn load_key(file: Option<&str>) -> Result<Option<String>, String> {
    let path = match file {
        Some(f) => f.to_string(),
        None => match std::env::var("NUCLEUS_BACKUP_KEY_FILE") {
            Ok(f) if !f.is_empty() => f,
            _ => return Ok(None),
        },
    };
    let key = nucleus_sql::backup::BackupKey::from_file(path.as_ref())
        .map_err(|e| format!("backup key: {e}"))?;
    let id = key.id().to_string();
    nucleus_sql::backup::set_backup_key(Some(key));
    Ok(Some(id))
}

/// A schedule of restore tests (PLAN.md §M14.18 O4): `nucleus server
/// --restore-test-every <interval>` with `--backup-to`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreTests {
    /// The repository (the backups').
    pub to: String,
    /// The time between tests.
    pub every: Duration,
    /// A directory of the test's own (`--restore-test-dir`, by default
    /// `<dir>.restore-test`): the database restored is made in it and
    /// removed after.
    pub scratch: std::path::PathBuf,
}

/// The file that marks a directory as restore tests' (one holding
/// anything else is not used: its files are not thrown away).
const MARK: &str = ".nucleus-restore-test";

/// Makes `scratch` restore tests' (or checks it is): empty or marked.
pub fn claim_scratch(scratch: &std::path::Path) -> Result<(), String> {
    let shown = scratch.display();
    if let Ok(mut d) = std::fs::read_dir(scratch)
        && d.next().is_some()
        && !scratch.join(MARK).exists()
    {
        return Err(format!(
            "{shown} holds files not restore tests': give an empty directory (--restore-test-dir)"
        ));
    }
    std::fs::create_dir_all(scratch).map_err(|e| format!("{shown}: {e}"))?;
    std::fs::write(
        scratch.join(MARK),
        "nucleus restore tests: a database is restored here and removed after\n",
    )
    .map_err(|e| format!("{shown}: {e}"))
}

/// One restore test: the repository's last backup restored into a new
/// database in `scratch` (claimed), checked, removed.
pub fn test_once(
    kv: &Arc<dyn Kv>,
    registry: &nucleus_sql::ext::Registry,
    to: &str,
    scratch: &std::path::Path,
) -> Result<Option<nucleus_sql::backup::Tested>, String> {
    let db = scratch.join("db");
    let _ = std::fs::remove_dir_all(&db);
    let r = (|| {
        let restored: Arc<dyn Kv> = Arc::new(
            nucleus_kv::Database::open(&db, nucleus_kv::Options::default())
                .map_err(|e| format!("{}: {e}", db.display()))?,
        );
        nucleus_sql::backup::restore_test(kv, &restored, registry, to).map_err(|e| e.message)
    })();
    let _ = std::fs::remove_dir_all(&db);
    r
}

/// Starts the restore tests on a thread of their own, until `stop`: due
/// an interval after the repository's last test (at once if there was
/// none, once it holds a backup).
pub fn start_restore_tests(
    kv: Arc<dyn Kv>,
    registry: Arc<nucleus_sql::ext::Registry>,
    s: RestoreTests,
    stop: Stop,
) -> std::thread::JoinHandle<()> {
    nucleus_sql::metrics::RESTORE_TEST_INTERVAL.set(s.every.as_secs() as i64);
    std::thread::spawn(move || {
        let every = s.every.as_micros() as u64;
        let mut due = match nucleus_sql::backup::last_restore_test(&kv, &s.to) {
            Ok(Some((_, time, held))) => {
                if held {
                    nucleus_sql::metrics::RESTORE_TEST_LAST.set((time / 1_000_000) as i64);
                }
                time.saturating_add(every)
            }
            Ok(None) => 0,
            Err(e) => {
                nucleus_platform::log!("nucleus: restore tests of {}: {}", s.to, e.message);
                0
            }
        };
        loop {
            let now = unix_micros(kv.wall_clock());
            if now < due {
                let wait = Duration::from_micros(due - now).min(Duration::from_secs(60));
                if stop.wait(wait) {
                    return;
                }
                continue;
            }
            due = match test_once(&kv, &registry, &s.to, &s.scratch) {
                Ok(None) => now.saturating_add(every.min(60_000_000)),
                Ok(Some(t)) => {
                    if t.problems.is_empty() {
                        nucleus_platform::log!(
                            "nucleus: restore test of {} in {}: it holds (commit {}, {} entries, \
                             {} indexes rebuilt)",
                            t.backup,
                            s.to,
                            t.restored.commit,
                            t.restored.entries,
                            t.restored.indexes
                        );
                    } else {
                        nucleus_platform::log!(
                            "nucleus: restore test of {} in {} FAILED: {}",
                            t.backup,
                            s.to,
                            t.problems.join("; ")
                        );
                    }
                    now.saturating_add(every)
                }
                Err(e) => {
                    nucleus_platform::log!("nucleus: restore test of {}: {e}", s.to);
                    now.saturating_add(every.min(RETRY.as_micros() as u64))
                }
            };
            if stop.stopped() {
                return;
            }
        }
    })
}
