//! Table statistics and estimates (PLAN.md §M13).  Placeholder until then:
//! `ANALYZE` accepts and does nothing, and plans carry no estimates.

use nucleus_kv::KvTxn;

use crate::error::Result;
use crate::plan::Plan;

/// `ANALYZE [table]`.
pub fn analyze(_txn: &mut dyn KvTxn, _table: Option<&str>) -> Result<()> {
    Ok(())
}

/// Adds row estimates to a plan for `EXPLAIN`.
pub fn annotate(_txn: &dyn KvTxn, plan: Plan) -> Result<Plan> {
    Ok(plan)
}
