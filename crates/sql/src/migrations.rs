//! The migrations applied to a database (PLAN.md §M14.16 M2): their
//! records in the catalog, `00 1B <version>`, written by `RECORD
//! MIGRATION` in the transaction that applies one and removed by `FORGET
//! MIGRATION` in the one that reverts it — so the record and the schema
//! change commit together or not at all, travel with the database (its
//! backups, its branches) and are read as `nc_catalog.migrations`.
//!
//! `nucleus migrate` writes them; any client may, with the right to
//! create in the database.  Two migrators applying the same migration
//! write the same key: one of them commits, the other's transaction
//! conflicts (`40001`) or, starting later, finds the record (`42710`).

use nucleus_kv::KvTxn;

use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};

const MIGRATIONS: [u8; 2] = [0x00, 0x1B];
const VERSION: u8 = 1;

/// A migration's record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    /// Its version: the number its file's name begins with.
    pub version: u64,
    /// Its name: the rest of the file's name.
    pub name: String,
    /// Its file's checksum when it was applied (SHA-256, hex).
    pub checksum: String,
    /// When it was applied (µs since 2000-01-01 UTC).
    pub applied_at: i64,
    /// The role that applied it.
    pub applied_by: String,
}

fn key(version: u64) -> Vec<u8> {
    let mut k = MIGRATIONS.to_vec();
    k.extend_from_slice(&version.to_be_bytes());
    k
}

impl Migration {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(VERSION)
            .u64(self.version)
            .str(&self.name)
            .str(&self.checksum)
            .u64(self.applied_at as u64)
            .str(&self.applied_by);
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        if r.u8()? != VERSION {
            return Err(Error::internal("unknown migration record version"));
        }
        Ok(Self {
            version: r.u64()?,
            name: r.str()?,
            checksum: r.str()?,
            applied_at: r.u64()? as i64,
            applied_by: r.str()?,
        })
    }
}

/// Every migration recorded, by version.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Migration>> {
    let end = nucleus_kv::prefix_end(&MIGRATIONS).expect("migration prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&MIGRATIONS[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        out.push(Migration::decode(&e?.1)?);
    }
    Ok(out)
}

/// `RECORD MIGRATION version NAME name CHECKSUM checksum`.
pub fn record(txn: &mut dyn KvTxn, version: u64, name: &str, checksum: &str) -> Result<()> {
    if let Some(b) = txn.get(&key(version))? {
        let m = Migration::decode(&b)?;
        return Err(Error::duplicate_object(format!(
            "migration {version} ({}) is already recorded",
            m.name
        )));
    }
    let m = Migration {
        version,
        name: name.to_string(),
        checksum: checksum.to_string(),
        applied_at: crate::datetime::now()?,
        applied_by: crate::access::current().user.clone(),
    };
    txn.put(&key(version), &m.encode())?;
    Ok(())
}

/// `FORGET MIGRATION version`.
pub fn forget(txn: &mut dyn KvTxn, version: u64) -> Result<()> {
    if txn.get(&key(version))?.is_none() {
        return Err(Error::undefined_object(format!(
            "migration {version} is not recorded"
        )));
    }
    txn.delete(&key(version))?;
    Ok(())
}
