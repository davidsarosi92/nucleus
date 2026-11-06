//! `SUMMARIZE t` and `SUMMARIZE <query>` (DuckDB's statement, PLAN.md
//! §M14.27 S8b): one row a column — its name and type, minimum, maximum,
//! approximate distinct count, mean, standard deviation, quartiles, the
//! row count and the share of `NULL`s — from **one** scan: a single
//! aggregate query over the source, its result turned into rows.  The
//! first thing an agent asks of a table it has not seen.
//!
//! The quartiles are DDSketch's (`approx_percentile`, within 1 % of the
//! value), the distinct count HyperLogLog++'s, as DuckDB's `SUMMARIZE`
//! approximates both — named so (`approx_unique`, `approx_q25` …, where
//! DuckDB says `q25`): approximate only under an `approx_` name.  The rest
//! is exact.  Minimum and maximum are given
//! as text for the types with an order, mean, deviation and quartiles
//! for numbers.

use crate::binder::Binder;
use crate::error::{Error, Result};
use crate::session::{Collect, OutColumn, Output};
use crate::types::{DataType, Value};
use nucleus_kv::KvTxn;

use crate::ast::Statement;
use crate::ext::Registry;

/// The columns of `SUMMARIZE`'s result.
pub fn columns() -> Vec<OutColumn> {
    [
        ("column_name", DataType::Text),
        ("column_type", DataType::Text),
        ("min", DataType::Text),
        ("max", DataType::Text),
        ("approx_unique", DataType::BigInt),
        ("avg", DataType::Float),
        ("std", DataType::Float),
        ("approx_q25", DataType::Float),
        ("approx_q50", DataType::Float),
        ("approx_q75", DataType::Float),
        ("count", DataType::BigInt),
        ("null_percentage", DataType::Float),
    ]
    .into_iter()
    .map(|(name, ty)| OutColumn {
        name: name.into(),
        ty,
    })
    .collect()
}

fn numeric(t: DataType) -> bool {
    t.is_integer() || matches!(t, DataType::Real | DataType::Float | DataType::Numeric)
}

/// Whether `min` and `max` take values of type `t`.
fn ordered(t: DataType) -> bool {
    numeric(t)
        || t.is_temporal()
        || matches!(t, DataType::Text | DataType::Char | DataType::Enum(_))
}

/// Runs `SUMMARIZE` over `source`, a query's text.
pub fn run(txn: &mut dyn KvTxn, reg: &Registry, source: &str, params: &[Value]) -> Result<Output> {
    let Statement::Query(q) = crate::parser::parse_one(source)? else {
        return Err(Error::internal("SUMMARIZE of a statement not a query"));
    };
    let cols: Vec<(String, Option<DataType>)> = {
        let ro: &dyn KvTxn = &*txn;
        let bound = Binder::new(ro, reg).checked().query(&q)?;
        bound
            .scope
            .cols
            .into_iter()
            .map(|c| (c.name, c.ty))
            .collect()
    };
    if cols.is_empty() {
        return Ok(Output::Rows {
            columns: columns(),
            rows: Vec::new(),
        });
    }
    // One aggregate query: the count, then each column's figures.
    let mut items = vec!["count(*)".to_string()];
    for (i, (_, ty)) in cols.iter().enumerate() {
        let c = format!("c{i}");
        let ty = ty.unwrap_or(DataType::Text);
        items.push(format!("count({c})"));
        items.push(format!("approx_count_distinct({c})"));
        if ordered(ty) {
            items.push(format!("min({c})::text"));
            items.push(format!("max({c})::text"));
        } else {
            items.push("NULL::text".into());
            items.push("NULL::text".into());
        }
        if numeric(ty) {
            items.push(format!("avg({c})::float8"));
            items.push(format!("stddev({c})::float8"));
            items.push(format!(
                "approx_percentile({c}::float8, ARRAY[0.25, 0.5, 0.75])"
            ));
        } else {
            items.push("NULL::float8".into());
            items.push("NULL::float8".into());
            items.push("NULL::float8[]".into());
        }
    }
    let names: Vec<String> = (0..cols.len()).map(|i| format!("c{i}")).collect();
    let sql = format!(
        "SELECT {} FROM ({source}) AS summarized({})",
        items.join(", "),
        names.join(", ")
    );
    let stmt = crate::parser::parse_one(&sql)?;
    let mut out = Collect::default();
    crate::session::execute_statement(txn, reg, &stmt, params, &mut out)?;
    let Some(row) = out.rows.pop() else {
        return Err(Error::internal("SUMMARIZE's aggregate gave no row"));
    };
    let total = match row[0] {
        Value::Int(n) => n,
        _ => 0,
    };
    let mut rows = Vec::with_capacity(cols.len());
    for (i, (name, ty)) in cols.into_iter().enumerate() {
        let f = &row[1 + 7 * i..1 + 7 * (i + 1)];
        let non_null = match f[0] {
            Value::Int(n) => n,
            _ => 0,
        };
        let quartile = |k: usize| match &f[6] {
            Value::Array(q) => q.get(k).cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        };
        let nulls = if total == 0 {
            Value::Null
        } else {
            let pct = 100.0 * (total - non_null) as f64 / total as f64;
            Value::Float((pct * 100.0).round() / 100.0)
        };
        rows.push(vec![
            Value::Text(name),
            Value::Text(ty.map_or("unknown".into(), |t| t.to_string())),
            f[2].clone(),
            f[3].clone(),
            f[1].clone(),
            f[4].clone(),
            f[5].clone(),
            quartile(0),
            quartile(1),
            quartile(2),
            Value::Int(total),
            nulls,
        ]);
    }
    Ok(Output::Rows {
        columns: columns(),
        rows,
    })
}
