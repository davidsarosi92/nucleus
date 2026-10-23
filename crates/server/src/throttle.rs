//! Failed logins, slowed and then locked (PLAN.md §M14.25 protection).
//!
//! Each failed login counts against its user name and its client address.
//! From the third failure in a row an answer is delayed — a quarter second,
//! doubling up to four — so guessing gets slow; after the database's
//! `login_lock_failures` (default 10) a role is locked for
//! `login_lock_time` (default 15 minutes): even the right password is
//! refused until then.  A successful login clears its user's count.  Unknown
//! names are counted like known ones, so the answers do not tell them apart.
//! The counts live in the server's memory: a restart forgets them.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

use nucleus_kv::Kv;

#[derive(Clone, Copy, Debug, Default)]
struct Failures {
    count: u32,
    /// The last failure (µs since 2000-01-01 UTC).
    last: i64,
    locked_until: Option<i64>,
}

/// The server's failed logins.
#[derive(Debug, Default)]
pub struct Throttle {
    by_user: Mutex<HashMap<String, Failures>>,
    by_address: Mutex<HashMap<IpAddr, Failures>>,
}

fn now(kv: &dyn Kv) -> i64 {
    nucleus_sql::datetime::from_system(kv.wall_clock())
}

/// The database's lock settings: failures, and time (µs).
fn settings(kv: &dyn Kv) -> (u32, i64) {
    let read = || -> nucleus_sql::Result<(u32, i64)> {
        let txn = kv.begin(nucleus_kv::TxnOptions::read_only())?;
        let failures = nucleus_sql::dbsettings::get(&*txn, "login_lock_failures")?
            .parse()
            .unwrap_or(10);
        let time = nucleus_sql::dbsettings::duration(&nucleus_sql::dbsettings::get(
            &*txn,
            "login_lock_time",
        )?)?;
        Ok((failures, time))
    };
    read().unwrap_or((10, 15 * 60 * 1_000_000))
}

impl Throttle {
    /// `Some(message)` if `user` is locked now.
    pub fn locked(&self, kv: &dyn Kv, user: &str) -> Option<String> {
        let at = now(kv);
        let users = self.by_user.lock().ok()?;
        let until = users.get(user)?.locked_until?;
        (until > at).then(|| {
            format!("role \"{user}\" is locked after too many failed logins; try again later")
        })
    }

    /// A failed login of `user` from `address`: counted, then answered
    /// after the delay its counts call for.
    pub fn failed(&self, kv: &dyn Kv, user: &str, address: Option<IpAddr>) {
        let at = now(kv);
        let (limit, lock_time) = settings(kv);
        let bump = |f: &mut Failures| {
            // A run of failures ends after a lock time without one.
            if at - f.last > lock_time {
                f.count = 0;
            }
            f.count += 1;
            f.last = at;
            f.count
        };
        let mut worst = 0;
        if let Ok(mut users) = self.by_user.lock() {
            let f = users.entry(user.to_string()).or_default();
            let n = bump(f);
            if n >= limit {
                f.locked_until = Some(at + lock_time);
                f.count = 0;
            }
            worst = worst.max(n);
        }
        if let Some(a) = address
            && let Ok(mut addresses) = self.by_address.lock()
        {
            worst = worst.max(bump(addresses.entry(a).or_default()));
        }
        if worst >= 3 {
            let ms = (250u64 << (worst - 3).min(4)).min(4000);
            std::thread::sleep(Duration::from_millis(ms));
        }
    }

    /// A successful login of `user`: its count starts again.
    pub fn succeeded(&self, user: &str) {
        if let Ok(mut users) = self.by_user.lock() {
            users.remove(user);
        }
    }
}
