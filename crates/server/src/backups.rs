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
