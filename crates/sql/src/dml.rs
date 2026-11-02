//! Writes: INSERT, UPDATE, DELETE — constraints and index maintenance.

use std::collections::HashSet;
use std::rc::Rc;

use nucleus_kv::KvTxn;

use crate::ast::{self, SelectItem};
use crate::binder::{Binder, Scope, ScopeCol, conjunction, conjuncts};
use crate::catalog::{self, Table};
use crate::error::{Error, Result};
use crate::exec::{self, index_key, pk_suffix, row_key_of};
use crate::expr::{Ctx, Env, Expr};
use crate::ext::Registry;
use crate::optimizer::choose_access;
use crate::parser::parse_expr;
use crate::plan::{Access, Plan};
use crate::privileges::{DELETE, INSERT, Kind, UPDATE};
use crate::types::{DataType, Row, Value};

/// The scope of a table's own columns.
pub fn table_scope(table: &Table) -> Scope {
    Scope {
        cols: table
            .columns
            .iter()
            .map(|c| ScopeCol {
                table: Some(table.name.clone()),
                name: c.name.clone(),
                ty: Some(c.ty),
                hidden: false,
                origin: Some((table.name.clone(), c.name.clone())),
            })
            .collect(),
    }
}

/// How a table's index entries are made: per index, per column, the
/// column's value or (a column indexed in a taught ordering) its mapping,
/// bound once.
pub struct IndexKeys {
    maps: Vec<Vec<Option<Expr>>>,
}

impl IndexKeys {
    /// Binds the mappings of `table`'s indexes.
    pub fn new(txn: &dyn KvTxn, reg: &Registry, table: &Table) -> Result<Self> {
        let mut binder = Binder::new(txn, reg);
        let mut maps = Vec::with_capacity(table.indexes.len());
        for ix in &table.indexes {
            let mut per = Vec::with_capacity(ix.columns.len());
            for (&c, o) in ix.columns.iter().zip(&ix.orderings) {
                per.push(match o {
                    None => None,
                    Some(name) => match binder.ordering(Some(table.columns[c].ty), name)? {
                        crate::binder::OrderingUse::Native => None,
                        crate::binder::OrderingUse::Mapped { mapping, .. } => {
                            Some(crate::binder::apply(&mapping, &Expr::Column(c)))
                        }
                    },
                });
            }
            maps.push(per);
        }
        Ok(Self { maps })
    }

    /// Every index entry of `row` (primary key bytes `pk`): key and whether
    /// it takes part in uniqueness, one per index.
    pub fn entries(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        row: &Row,
        pk: &[u8],
    ) -> Result<Vec<(Vec<u8>, bool)>> {
        // Mappings are immutable: they need nothing of the session.
        static NONE: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
        let ctx = Ctx::new(txn, NONE.get_or_init(Registry::empty), &[]);
        let env = Env {
            row,
            outer: None,
            ctx: &ctx,
        };
        (0..self.maps.len())
            .map(|i| self.entry_in(&env, t, row, pk, i))
            .collect()
    }

    /// Index `i`'s entry of `row` alone ([`entries`](Self::entries)'
    /// `i`-th): what an index build needs, not every index's.
    pub fn entry(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        row: &Row,
        pk: &[u8],
        i: usize,
    ) -> Result<(Vec<u8>, bool)> {
        static NONE: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
        let ctx = Ctx::new(txn, NONE.get_or_init(Registry::empty), &[]);
        let env = Env {
            row,
            outer: None,
            ctx: &ctx,
        };
        self.entry_in(&env, t, row, pk, i)
    }

    fn entry_in(
        &self,
        env: &Env,
        t: &Table,
        row: &Row,
        pk: &[u8],
        i: usize,
    ) -> Result<(Vec<u8>, bool)> {
        let maps = &self.maps[i];
        let mut vals = Vec::with_capacity(maps.len());
        for (&c, m) in t.indexes[i].columns.iter().zip(maps) {
            vals.push(match m {
                None => row[c].clone(),
                Some(e) => e.eval(env)?,
            });
        }
        Ok(index_key(t, i, vals, pk))
    }
}

/// Index entries gathered for a build (PLAN.md §M14.20 W2) — of a new
/// index, or of every index of a table whose rows are rewritten — then
/// written in key order: sorted in memory up to the statement's
/// `work_mem` (shared by the indexes built at once), past it as sorted
/// runs in scratch space merged.  In key order the writes are appends —
/// the write set keeps them as a sorted vector and its spills append to
/// the tree, which packs its leaves full (as PostgreSQL's `nbtsort` builds
/// a B-tree from a sorted run) —, and a unique index's duplicates are
/// neighbours, found without a look at the tree.
pub struct IndexBuild {
    indexes: Vec<usize>,
    sorters: Vec<EntrySorter>,
}

impl IndexBuild {
    /// A build of the indexes `indexes` (positions in the table's list).
    pub fn new(indexes: Vec<usize>) -> Self {
        let budget = crate::budget::work_mem() / indexes.len().max(1);
        let sorters = indexes.iter().map(|_| EntrySorter::new(budget)).collect();
        Self { indexes, sorters }
    }

    /// The entries of `row` (primary key bytes `pk`) in each index built.
    pub fn add(
        &mut self,
        keys: &IndexKeys,
        txn: &dyn KvTxn,
        t: &Table,
        row: &Row,
        pk: &[u8],
    ) -> Result<()> {
        for (s, &i) in self.sorters.iter_mut().zip(&self.indexes) {
            let (k, unique) = keys.entry(txn, t, row, pk, i)?;
            s.entries.push(&k, pk, unique);
            s.spill_past_budget()?;
        }
        Ok(())
    }

    /// Entries of the `n`-th index built, made elsewhere (on a worker).
    fn extend(&mut self, n: usize, entries: Entries) -> Result<()> {
        self.sorters[n].entries.append(entries);
        self.sorters[n].spill_past_budget()
    }

    /// Writes every index's entries in key order; a unique index's
    /// duplicate is `duplicate(index name)`.
    pub fn write(
        self,
        txn: &mut dyn KvTxn,
        t: &Table,
        duplicate: &dyn Fn(&str) -> Error,
    ) -> Result<()> {
        for (s, i) in self.sorters.into_iter().zip(self.indexes) {
            let mut last_unique: Option<Vec<u8>> = None;
            s.each(&mut |k, pk, unique| {
                if unique {
                    if last_unique.as_deref() == Some(k) {
                        return Err(duplicate(&t.indexes[i].name));
                    }
                    let last = last_unique.get_or_insert_with(Vec::new);
                    last.clear();
                    last.extend_from_slice(k);
                }
                Ok(txn.put(k, pk)?)
            })?;
        }
        Ok(())
    }
}

/// Index entries in one buffer — keys and primary keys side by side, no
/// allocation of their own — and, per entry, its place and its key's first
/// eight bytes as a number: most comparisons of a sort end there, without
/// a look at the bytes (PostgreSQL's abbreviated keys, the normalized key
/// prefix of DuckDB's sort).
#[derive(Default)]
struct Entries {
    bytes: Vec<u8>,
    items: Vec<Item>,
}

#[derive(Clone, Copy)]
struct Item {
    /// The key's first eight bytes, big-endian, zeros past its end.
    prefix: u64,
    /// Where the key starts in the buffer, the primary key right after.
    at: usize,
    key: u32,
    pk: u32,
    unique: bool,
}

impl Entries {
    fn push(&mut self, k: &[u8], pk: &[u8], unique: bool) {
        let mut prefix = [0u8; 8];
        let n = k.len().min(8);
        prefix[..n].copy_from_slice(&k[..n]);
        self.items.push(Item {
            prefix: u64::from_be_bytes(prefix),
            at: self.bytes.len(),
            key: k.len() as u32,
            pk: pk.len() as u32,
            unique,
        });
        self.bytes.extend_from_slice(k);
        self.bytes.extend_from_slice(pk);
    }

    fn append(&mut self, other: Entries) {
        if self.items.is_empty() {
            *self = other;
            return;
        }
        let shift = self.bytes.len();
        self.bytes.extend_from_slice(&other.bytes);
        self.items.extend(other.items.into_iter().map(|it| Item {
            at: it.at + shift,
            ..it
        }));
    }

    /// The memory held.
    fn size(&self) -> usize {
        self.bytes.len() + self.items.len() * std::mem::size_of::<Item>()
    }

    fn key(&self, it: &Item) -> &[u8] {
        key_in(&self.bytes, it)
    }

    fn pk(&self, it: &Item) -> &[u8] {
        let from = it.at + it.key as usize;
        &self.bytes[from..from + it.pk as usize]
    }

    /// Sorts the entries by key: cut in parts sorted on threads of their
    /// own, the parts merged.
    fn sort(&mut self) {
        let threads = std::thread::available_parallelism().map_or(1, usize::from);
        let chunk = self.items.len().div_ceil(threads).max(SORT_CHUNK);
        let bytes = &self.bytes;
        let items = &mut self.items;
        std::thread::scope(|sc| {
            for part in items.chunks_mut(chunk) {
                sc.spawn(|| part.sort_unstable_by(|a, b| cmp_in(bytes, a, b)));
            }
        });
        if items.len() > chunk {
            let parts: Vec<&[Item]> = items.chunks(chunk).collect();
            let at: Vec<std::cell::Cell<usize>> =
                parts.iter().map(|_| Default::default()).collect();
            let mut merged = Vec::with_capacity(items.len());
            merge(
                parts.len(),
                |p| at[p].get() < parts[p].len(),
                |a, b| cmp_in(bytes, &parts[a][at[a].get()], &parts[b][at[b].get()]).is_lt(),
                |p| {
                    merged.push(parts[p][at[p].get()]);
                    at[p].set(at[p].get() + 1);
                    Ok(())
                },
            )
            .expect("an in-memory merge does not fail");
            *items = merged;
        }
    }
}

fn key_in<'a>(bytes: &'a [u8], it: &Item) -> &'a [u8] {
    &bytes[it.at..it.at + it.key as usize]
}

/// Entries' order: by the keys' first bytes as numbers, the bytes past
/// them only where those are equal.
fn cmp_in(bytes: &[u8], a: &Item, b: &Item) -> std::cmp::Ordering {
    a.prefix
        .cmp(&b.prefix)
        .then_with(|| key_in(bytes, a).cmp(key_in(bytes, b)))
}

/// A k-way merge of `n` sorted sources: `live(i)` whether source `i` has
/// an entry, `less(i, j)` whether its entry comes before `j`'s, `take(i)`
/// consumes its entry (and moves to its next) — a binary heap of the
/// sources, the entries where they are.
fn merge(
    n: usize,
    mut live: impl FnMut(usize) -> bool,
    mut less: impl FnMut(usize, usize) -> bool,
    mut take: impl FnMut(usize) -> Result<()>,
) -> Result<()> {
    let mut heap: Vec<usize> = (0..n).filter(|&i| live(i)).collect();
    let sift = |heap: &mut Vec<usize>, less: &mut dyn FnMut(usize, usize) -> bool, mut i: usize| loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < heap.len() && less(heap[l], heap[m]) {
            m = l;
        }
        if r < heap.len() && less(heap[r], heap[m]) {
            m = r;
        }
        if m == i {
            break;
        }
        heap.swap(i, m);
        i = m;
    };
    for i in (0..heap.len() / 2).rev() {
        sift(&mut heap, &mut less, i);
    }
    while let Some(&top) = heap.first() {
        take(top)?;
        if !live(top) {
            let last = heap.pop().expect("a source");
            if heap.is_empty() {
                break;
            }
            heap[0] = last;
        }
        sift(&mut heap, &mut less, 0);
    }
    Ok(())
}

/// Entries a sort thread takes at least (fewer are sorted on one).
const SORT_CHUNK: usize = 1 << 16;

/// One index's entries in key order (bytewise, as the tree orders them),
/// in bounded memory.
struct EntrySorter {
    budget: usize,
    entries: Entries,
    runs: Vec<Rc<crate::spill::Sealed>>,
}

impl EntrySorter {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            entries: Entries::default(),
            runs: Vec::new(),
        }
    }

    fn spill_past_budget(&mut self) -> Result<()> {
        if self.entries.size() > self.budget {
            self.spill()?;
        }
        Ok(())
    }

    /// The entries, sorted, as a run in scratch space: records of key
    /// length, key, primary key and the uniqueness flag.
    fn spill(&mut self) -> Result<()> {
        let mut e = std::mem::take(&mut self.entries);
        e.sort();
        let mut run = crate::spill::Run::new()?;
        for it in &e.items {
            run.push_record(&[
                &it.key.to_le_bytes(),
                e.key(it),
                e.pk(it),
                &[u8::from(it.unique)],
            ])?;
        }
        self.runs.push(run.seal()?);
        Ok(())
    }

    /// Calls `f(key, primary key, unique)` for every entry in key order
    /// (equal keys in no particular order: only a unique index has any, and
    /// they are an error).
    fn each(mut self, f: &mut dyn FnMut(&[u8], &[u8], bool) -> Result<()>) -> Result<()> {
        if self.runs.is_empty() {
            self.entries.sort();
            let e = &self.entries;
            return e
                .items
                .iter()
                .try_for_each(|it| f(e.key(it), e.pk(it), it.unique));
        }
        if !self.entries.items.is_empty() {
            self.spill()?;
        }
        // Each run's next record, copied (into a buffer of its own, reused)
        // out of the run's block.
        let mut runs: Vec<crate::spill::RunIter> =
            self.runs.iter().map(crate::spill::Sealed::iter).collect();
        let mut heads: Vec<Option<Vec<u8>>> = vec![None; runs.len()];
        let bad = || Error::internal("a malformed index entry in scratch space");
        let advance = |run: &mut crate::spill::RunIter, head: &mut Option<Vec<u8>>| -> Result<()> {
            match run.next_record()? {
                Some(rec) => {
                    let buf = head.get_or_insert_with(Vec::new);
                    buf.clear();
                    buf.extend_from_slice(rec);
                }
                None => *head = None,
            }
            Ok(())
        };
        let split = |rec: &[u8]| -> Result<(usize, usize)> {
            let n = u32::from_le_bytes(rec.get(..4).ok_or_else(bad)?.try_into().unwrap()) as usize;
            if rec.len() < 5 + n {
                return Err(bad());
            }
            Ok((4 + n, rec.len() - 1))
        };
        for (run, head) in runs.iter_mut().zip(&mut heads) {
            advance(run, head)?;
        }
        let heads = std::cell::RefCell::new(heads);
        merge(
            runs.len(),
            |i| heads.borrow()[i].is_some(),
            |a, b| {
                let h = heads.borrow();
                let key = |i: usize| {
                    let rec = h[i].as_deref().unwrap_or(&[]);
                    split(rec).map_or(&rec[..0], |(k, _)| &rec[4..k])
                };
                key(a) < key(b)
            },
            |i| {
                let mut h = heads.borrow_mut();
                let rec = h[i].as_deref().ok_or_else(bad)?;
                let (k, end) = split(rec)?;
                f(&rec[4..k], &rec[k..end], rec[end] != 0)?;
                advance(&mut runs[i], &mut h[i])
            },
        )
    }
}

/// What writes to one table need, bound once per statement.
pub struct Writer {
    /// The table.
    pub table: Table,
    checks: Vec<(String, Expr)>,
    defaults: Vec<Option<Expr>>,
    /// Its index entries.
    pub keys: IndexKeys,
    /// How its rows are stored.
    pub storage: std::sync::Arc<dyn crate::storage::TableStorage>,
    /// A partitioned table: how, and its partitions' writers (rows go to
    /// them).
    partitions: Option<(crate::partition::Spec, Vec<Writer>)>,
    /// A partition: the key a row must have — its partitioning, its bound,
    /// and (a default partition) its siblings' bounds.
    bound: Option<(
        crate::partition::Spec,
        crate::partition::PartBound,
        Vec<crate::partition::PartBound>,
    )>,
}

impl Writer {
    /// Binds the table's `CHECK` and `DEFAULT` expressions.
    pub fn new(txn: &dyn KvTxn, reg: &Registry, table: Table) -> Result<Self> {
        let scope = table_scope(&table);
        let mut binder = Binder::new(txn, reg);
        let mut checks = Vec::new();
        for (name, text) in &table.checks {
            checks.push((name.clone(), binder.condition(&parse_expr(text)?, &scope)?));
        }
        let empty = Scope::default();
        let mut defaults = Vec::new();
        for c in &table.columns {
            defaults.push(match &c.default {
                None => None,
                Some(text) => Some(binder.expr(&parse_expr(text)?, &empty)?.0),
            });
        }
        let keys = IndexKeys::new(txn, reg, &table)?;
        let storage = crate::storage::of(&table)?;
        catalog::validate_table(txn, &table.name)?;
        let partitions = match &table.partition_by {
            None => None,
            Some(spec) => Some((
                spec.clone(),
                crate::partition::partitions(txn, &table)?
                    .into_iter()
                    .map(|p| Writer::new(txn, reg, p))
                    .collect::<Result<_>>()?,
            )),
        };
        let bound = match &table.partition_of {
            None => None,
            Some((parent, b)) => {
                let parent = catalog::table(txn, parent)?;
                let spec = parent
                    .partition_by
                    .clone()
                    .ok_or_else(|| Error::internal("a partition of a table not partitioned"))?;
                let others = crate::partition::partitions(txn, &parent)?
                    .into_iter()
                    .filter(|p| p.name != table.name)
                    .filter_map(|p| p.partition_of.map(|(_, b)| b))
                    .collect();
                Some((spec, b.clone(), others))
            }
        };
        Ok(Self {
            table,
            checks,
            defaults,
            keys,
            storage,
            partitions,
            bound,
        })
    }

    /// The default value of column `c`.
    pub fn default_of(&self, c: usize, ctx: &Ctx) -> Result<Value> {
        match &self.defaults[c] {
            None => Ok(Value::Null),
            Some(e) => assign(
                ctx.txn,
                e.eval(&Env {
                    row: &[],
                    outer: None,
                    ctx,
                })?,
                &self.table,
                c,
            ),
        }
    }

    /// Checks NOT NULL, lengths and CHECK constraints of a full row.
    pub fn validate(&self, row: &Row, ctx: &Ctx) -> Result<()> {
        let t = &self.table;
        for (i, c) in t.columns.iter().enumerate() {
            if row[i].is_null() && (!c.nullable || t.primary_key.contains(&i)) {
                return Err(Error::not_null_violation(format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                    c.name, t.name
                )));
            }
        }
        let env = Env {
            row,
            outer: None,
            ctx,
        };
        for (name, check) in &self.checks {
            if check.eval(&env)?.truth()? == Some(false) {
                return Err(Error::check_violation(format!(
                    "new row for relation \"{}\" violates check constraint \"{name}\"",
                    t.name
                )));
            }
        }
        if let Some((spec, bound, others)) = &self.bound {
            let key: Vec<Value> = spec.columns.iter().map(|&c| row[c].clone()).collect();
            let fits = match bound {
                crate::partition::PartBound::Default => {
                    !others.iter().any(|b| crate::partition::contains(b, &key))
                }
                b => crate::partition::contains(b, &key),
            };
            if !fits {
                return Err(Error::check_violation(format!(
                    "new row for relation \"{}\" violates partition constraint",
                    t.name
                )));
            }
        }
        Ok(())
    }

    /// The tables its rows are in: its partitions', or its own.
    pub fn targets(&self) -> Vec<&Table> {
        match &self.partitions {
            Some((_, parts)) => parts.iter().map(|w| &w.table).collect(),
            None => vec![&self.table],
        }
    }

    /// A partitioned table's partition (its writer) a row goes to.
    fn partition_of(&self, row: &Row) -> Result<Option<usize>> {
        let Some((spec, parts)) = &self.partitions else {
            return Ok(None);
        };
        let tables: Vec<Table> = parts.iter().map(|w| w.table.clone()).collect();
        let to = crate::partition::route(spec, &tables, row).map_err(|_| {
            Error::check_violation(format!(
                "no partition of relation \"{}\" found for row",
                self.table.name
            ))
        })?;
        Ok(tables.iter().position(|t| t.name == to.name))
    }
}

/// The row-level security condition `command` must pass on existing
/// rows of `t` (`None`: none applies).
fn row_policy(
    binder: &Binder,
    t: &Table,
    command: crate::policies::Command,
) -> Result<Option<Expr>> {
    if binder.checker().is_none() {
        return Ok(None);
    }
    crate::policies::applied(binder.txn(), t, command)?
        .map(|a| binder.policy_condition(t, &a.using))
        .transpose()
}

/// The row-level security condition `command`'s new rows must pass.
pub(crate) fn new_row_check(
    binder: &Binder,
    t: &Table,
    command: crate::policies::Command,
) -> Result<Option<Expr>> {
    if binder.checker().is_none() {
        return Ok(None);
    }
    crate::policies::applied(binder.txn(), t, command)?
        .map(|a| binder.policy_condition(t, &a.check))
        .transpose()
}

/// `42501` unless `row` passes the policies' check.
pub(crate) fn admit(check: &Option<Expr>, t: &Table, row: &[Value], ctx: &Ctx) -> Result<()> {
    if let Some(c) = check
        && !c.holds(&exec::env(row, ctx, None))?
    {
        return Err(crate::policies::violated(&t.name));
    }
    Ok(())
}

/// The rows an `UPDATE` or `DELETE` touches: those the policy lets through
/// and, then — never on a hidden row — the statement's condition holds for.
/// Keyed ([`exec::keyed`]) and stashed: collected before anything is
/// written, in bounded memory.
fn secure_rows(
    t: &Table,
    access: &Access,
    policy: Option<&Expr>,
    pred: Option<&Expr>,
    ctx: &Ctx,
) -> Result<crate::spill::Stored> {
    let Some(policy) = policy else {
        return exec::matching_stash(t, access, pred, ctx);
    };
    let mut out = crate::spill::Stash::new();
    for r in exec::matching(t, access, Some(policy), ctx)? {
        let (k, row) = r?;
        if pred.map_or(Ok(true), |p| p.holds(&exec::env(&row, ctx, None)))? {
            out.push(exec::keyed(k, row))?;
        }
    }
    out.seal()
}

/// Rows a write applies at a time (their foreign keys checked together).
const CHUNK: usize = 4096;

/// Converts a value for storing in column `c` (assignment cast; an enum's
/// label read through `txn`'s catalog).
pub fn assign(txn: &dyn KvTxn, v: Value, table: &Table, c: usize) -> Result<Value> {
    let col = &table.columns[c];
    // Assignment casts: within numbers, within dates and timestamps, and
    // from or to text (text is parsed, as an untyped literal would be).
    // (A value's own type is approximate: an integer reads as `bigint`, an
    // array's as its first element's.)
    let from = v.data_type();
    fn assignable(f: DataType, to: DataType) -> bool {
        match (f.element(), to.element()) {
            (Some(fe), Some(te)) => assignable(fe, te),
            _ => {
                f == to
                    || f == DataType::Text
                    || to == DataType::Text
                    || (f.is_number() && to.is_number())
                    || (f.is_datetime() && to.is_datetime())
            }
        }
    }
    let compatible = |f: DataType| assignable(f, col.ty);
    let v = match (&v, col.ty) {
        (Value::Null, _) => return Ok(v),
        _ if !from.is_some_and(compatible) => {
            return Err(Error::type_mismatch(format!(
                "column \"{}\" is of type {} but expression is of type {}",
                col.name,
                col.ty,
                crate::types::type_name(&v)
            )));
        }
        _ => crate::enums::cast(txn, v, col.ty)?,
    };
    if let (Some(max), Value::Text(s)) = (col.max_len, &v)
        && s.chars().count() > max as usize
    {
        return Err(Error::string_too_long(format!(
            "value too long for type character varying({max})"
        )));
    }
    if let Value::Char(s) = &v {
        return Ok(match col.max_len {
            Some(n) => Value::Char(crate::types::fit_char(s, n, false)?),
            None => v,
        });
    }
    if let (Some((p, s)), Value::Numeric(d)) = (col.numeric, &v) {
        return Ok(Value::Numeric(d.fit(p, s)?));
    }
    Ok(v)
}

fn duplicate(name: &str) -> Error {
    Error::unique_violation(format!(
        "duplicate key value violates unique constraint \"{name}\""
    ))
}

/// Stores a new row and its index entries.
pub(crate) fn insert_row(txn: &mut dyn KvTxn, w: &Writer, row: &Row) -> Result<()> {
    let t = &w.table;
    let rowid = if t.primary_key.is_empty() {
        Some(catalog::next_rowid(txn, t)?)
    } else {
        None
    };
    let key = row_key_of(t, row, rowid);
    if rowid.is_some() {
        w.storage.write(txn, t, &key, row, None)?;
    } else if !w.storage.insert(txn, t, &key, row)? {
        return Err(duplicate(&format!("{}_pkey", t.name)));
    }
    let pk = pk_suffix(t, &key);
    for (i, (k, unique)) in w.keys.entries(&*txn, t, row, pk)?.into_iter().enumerate() {
        if unique && txn.get(&k)?.is_some() {
            return Err(duplicate(&t.indexes[i].name));
        }
        txn.put(&k, pk)?;
    }
    Ok(())
}

fn returning(
    binder: &mut Binder,
    items: &[SelectItem],
    table: &Table,
) -> Result<Option<(Vec<Expr>, Vec<(String, Option<DataType>)>)>> {
    if items.is_empty() {
        return Ok(None);
    }
    let scope = table_scope(table);
    let mut exprs = Vec::new();
    let mut cols = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                for (i, c) in table.columns.iter().enumerate() {
                    exprs.push(Expr::Column(i));
                    cols.push((c.name.clone(), Some(c.ty)));
                }
            }
            SelectItem::Expr { expr, alias } => {
                let (x, t) = binder.expr(expr, &scope)?;
                let name = alias.clone().unwrap_or_else(|| match expr {
                    ast::Expr::Column { name, .. } => name.clone(),
                    _ => "?column?".into(),
                });
                exprs.push(x);
                cols.push((name, t));
            }
        }
    }
    Ok(Some((exprs, cols)))
}

/// The result of a write: how many rows, and the `RETURNING` rows.
pub struct Written {
    /// Rows written.
    pub count: u64,
    /// `RETURNING` columns and rows, if asked.
    pub returning: Option<(Vec<(String, Option<DataType>)>, Vec<Row>)>,
}

fn project(exprs: &[Expr], row: &Row, ctx: &Ctx) -> Result<Row> {
    let env = Env {
        row,
        outer: None,
        ctx,
    };
    exprs.iter().map(|e| e.eval(&env)).collect()
}

/// `INSERT`.
pub fn insert(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    columns: &Option<Vec<String>>,
    source: &ast::Query,
    returning_items: &[SelectItem],
    params: &[Value],
) -> Result<Written> {
    let t = catalog::table(&*txn, table)?;
    let targets: Vec<usize> = match columns {
        None => (0..t.columns.len()).collect(),
        Some(names) => {
            let mut seen = HashSet::new();
            names
                .iter()
                .map(|n| {
                    let i = t.column(n).ok_or_else(|| {
                        Error::undefined_column(format!(
                            "column \"{n}\" of relation \"{}\" does not exist",
                            t.name
                        ))
                    })?;
                    if !seen.insert(i) {
                        return Err(Error::duplicate_column(format!(
                            "column \"{n}\" specified more than once"
                        )));
                    }
                    Ok(i)
                })
                .collect::<Result<_>>()?
        }
    };
    let (rows, writer, ret) = {
        let ro: &dyn KvTxn = &*txn;
        let mut binder = Binder::new(ro, reg).checked();
        // INSERT on each target column (or the table).
        if let Some(c) = binder.checker() {
            for &col in &targets {
                c.require(ro, Kind::Table, &t.name, INSERT, Some(&t.columns[col].name))?;
            }
        }
        let types = targets.iter().map(|&c| t.columns[c].ty).collect();
        let bound = binder.insert_source(source, types)?;
        let plan = crate::optimizer::optimize(bound.plan, ro);
        let width = plan.width();
        // DEFAULT VALUES: zero columns.
        if width > targets.len() {
            return Err(Error::syntax(
                "INSERT has more expressions than target columns",
            ));
        }
        // Without a column list, missing trailing values take defaults.
        if columns.is_some() && width < targets.len() && width > 0 {
            return Err(Error::syntax(
                "INSERT has more target columns than expressions",
            ));
        }
        let ret = returning(&mut binder, returning_items, &t)?;
        let check = new_row_check(&binder, &t, crate::policies::Command::Insert)?;
        let writer = Writer::new(ro, reg, t.clone())?;
        let ctx = Ctx::new(ro, reg, params);
        // Collected before anything is written (the source may read the
        // table), in bounded memory.
        let mut rows = crate::spill::Stash::new();
        for src in exec::build(&plan, &ctx, None)? {
            let src = src?;
            let mut row = Vec::with_capacity(t.columns.len());
            for c in 0..t.columns.len() {
                row.push(match targets.iter().position(|&x| x == c) {
                    Some(j) if j < src.len() => assign(ro, src[j].clone(), &t, c)?,
                    _ => writer.default_of(c, &ctx)?,
                });
            }
            writer.validate(&row, &ctx)?;
            admit(&check, &t, &row, &ctx)?;
            rows.push(row)?;
        }
        (std::rc::Rc::new(rows.seal()?), writer, ret)
    };
    let mut chunk = Vec::with_capacity(CHUNK);
    for r in rows.iter() {
        chunk.push(r?);
        if chunk.len() == CHUNK {
            insert_rows(txn, &writer, &chunk)?;
            chunk.clear();
        }
    }
    insert_rows(txn, &writer, &chunk)?;
    let returning = match ret {
        None => None,
        Some((exprs, cols)) => {
            let ctx = Ctx::new(&*txn, reg, params);
            Some((
                cols,
                rows.iter()
                    .map(|r| project(&exprs, &r?, &ctx))
                    .collect::<Result<_>>()?,
            ))
        }
    };
    Ok(Written {
        count: rows.len() as u64,
        returning,
    })
}

/// The access path and filter for `WHERE filter` on `table`.
pub fn bind_filter(
    binder: &mut Binder,
    table: &Table,
    filter: Option<&ast::Expr>,
) -> Result<(Access, Option<Expr>)> {
    let Some(f) = filter else {
        return Ok((Access::Full, None));
    };
    let pred = binder.condition(f, &table_scope(table))?;
    let mut parts = Vec::new();
    conjuncts(pred, &mut parts);
    let stats = crate::stats::Stats::new(binder.txn());
    Ok((
        choose_access(table, &parts, Some(&stats)),
        conjunction(parts),
    ))
}

/// `UPDATE`.
pub fn update(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    assignments: &[(String, ast::Expr)],
    filter: Option<&ast::Expr>,
    returning_items: &[SelectItem],
    params: &[Value],
) -> Result<Written> {
    let t = catalog::table(&*txn, table)?;
    let (changes, writer, ret) = {
        let ro: &dyn KvTxn = &*txn;
        let mut binder = Binder::new(ro, reg).checked();
        let scope = table_scope(&t);
        let mut sets = Vec::new();
        for (col, e) in assignments {
            let c = t.column(col).ok_or_else(|| {
                Error::undefined_column(format!(
                    "column \"{col}\" of relation \"{}\" does not exist",
                    t.name
                ))
            })?;
            if sets.iter().any(|(x, _)| *x == c) {
                return Err(Error::duplicate_column(format!(
                    "multiple assignments to same column \"{col}\""
                )));
            }
            // UPDATE on each column set (or the table).
            if let Some(ch) = binder.checker() {
                ch.require(ro, Kind::Table, &t.name, UPDATE, Some(col))?;
            }
            sets.push((c, binder.expr(e, &scope)?.0));
        }
        let (access, pred) = bind_filter(&mut binder, &t, filter)?;
        let ret = returning(&mut binder, returning_items, &t)?;
        let using = row_policy(&binder, &t, crate::policies::Command::Update)?;
        let check = new_row_check(&binder, &t, crate::policies::Command::Update)?;
        let writer = Writer::new(ro, reg, t.clone())?;
        let ctx = Ctx::new(ro, reg, params);
        // Each change kept as key, old row, new row — stashed, so every
        // row is chosen before any is written, in bounded memory.
        let mut changes = crate::spill::Stash::new();
        // Each change is the table's (a partition's, by number).
        for (ti, target) in writer.targets().iter().enumerate() {
            for r in secure_rows(target, &access, using.as_ref(), pred.as_ref(), &ctx)?.into_iter()
            {
                let (key, old) = exec::unkeyed(r?)?;
                let env = Env {
                    row: &old,
                    outer: None,
                    ctx: &ctx,
                };
                let mut new = old.clone();
                for (c, e) in &sets {
                    new[*c] = assign(ctx.txn, e.eval(&env)?, &t, *c)?;
                }
                writer.validate(&new, &ctx)?;
                admit(&check, &t, &new, &ctx)?;
                let mut change = vec![Value::Int(ti as i64)];
                change.extend(exec::keyed(key, old));
                change.extend(new);
                changes.push(change)?;
            }
        }
        (changes.seal()?, writer, ret)
    };
    let width = t.columns.len();
    let count = changes.len() as u64;
    let split = |mut r: Row| -> Result<(usize, Vec<u8>, Row, Row)> {
        let ti = table_number(&r)?;
        let key = take_key(&mut r, 1)?;
        r.drain(..2);
        let new = r.split_off(width);
        Ok((ti, key, r, new))
    };
    let (each, kept) = consume_or_keep(changes, ret.is_some());
    let mut chunk = Vec::with_capacity(CHUNK);
    for r in each {
        let (ti, key, old, new) = split(r?)?;
        chunk.push((ti, key, old, new));
        if chunk.len() == CHUNK {
            apply_updates(txn, reg, &writer, std::mem::take(&mut chunk))?;
        }
    }
    apply_updates(txn, reg, &writer, chunk)?;
    let returning = match ret {
        None => None,
        Some((exprs, cols)) => {
            let ctx = Ctx::new(&*txn, reg, params);
            let changes = kept.ok_or_else(|| Error::internal("the rows RETURNING reads"))?;
            Some((
                cols,
                changes
                    .iter()
                    .map(|r| project(&exprs, &split(r?)?.3, &ctx))
                    .collect::<Result<_>>()?,
            ))
        }
    };
    Ok(Written { count, returning })
}

/// Applies changes `(table number, key, old, new)` of [`Writer::targets`]:
/// in place, or — a partitioned table's row whose new key is another
/// partition's — moved there (deleted, then inserted).
fn apply_updates(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    w: &Writer,
    changes: Vec<(usize, Vec<u8>, Row, Row)>,
) -> Result<()> {
    let Some((_, parts)) = &w.partitions else {
        let chunk: Vec<(Vec<u8>, Row, Row)> =
            changes.into_iter().map(|(_, k, o, n)| (k, o, n)).collect();
        return update_rows(txn, reg, w, &chunk);
    };
    let mut stay: Vec<Vec<(Vec<u8>, Row, Row)>> = vec![Vec::new(); parts.len()];
    let mut leave: Vec<Vec<(Vec<u8>, Row)>> = vec![Vec::new(); parts.len()];
    let mut arrive: Vec<Vec<Row>> = vec![Vec::new(); parts.len()];
    for (ti, key, old, new) in changes {
        let to = w.partition_of(&new)?.expect("partitioned");
        if to == ti {
            stay[ti].push((key, old, new));
        } else {
            leave[ti].push((key, old));
            arrive[to].push(new);
        }
    }
    for (pw, rows) in parts.iter().zip(&leave) {
        if !rows.is_empty() {
            delete_rows(txn, reg, &pw.table, rows)?;
        }
    }
    for (pw, rows) in parts.iter().zip(&stay) {
        if !rows.is_empty() {
            update_rows(txn, reg, pw, rows)?;
        }
    }
    for (pw, rows) in parts.iter().zip(&arrive) {
        if !rows.is_empty() {
            insert_rows(txn, pw, rows)?;
        }
    }
    Ok(())
}

/// One row (validated already) inserted — into its partition, for a
/// partitioned table — and noted as the statement's write (its storage
/// finishes at the statement's end); its foreign keys are the caller's.
pub(crate) fn insert_routed(txn: &mut dyn KvTxn, w: &Writer, row: &Row) -> Result<()> {
    if let Some((_, parts)) = &w.partitions {
        let i = w.partition_of(row)?.expect("partitioned");
        return insert_routed(txn, &parts[i], row);
    }
    insert_row(txn, w, row)?;
    crate::storage::written(&w.table);
    Ok(())
}

/// Stores new rows (validated already), then checks their foreign keys.
pub fn insert_rows(txn: &mut dyn KvTxn, w: &Writer, rows: &[Row]) -> Result<()> {
    if let Some((_, parts)) = &w.partitions {
        // Each row to its partition.
        let mut by: Vec<Vec<Row>> = vec![Vec::new(); parts.len()];
        for row in rows {
            let i = w.partition_of(row)?.expect("partitioned");
            by[i].push(row.clone());
        }
        for (pw, rows) in parts.iter().zip(by) {
            if !rows.is_empty() {
                insert_rows(txn, pw, &rows)?;
            }
        }
        return Ok(());
    }
    for row in rows {
        insert_row(txn, w, row)?;
    }
    crate::storage::written(&w.table);
    crate::fk::check_children(&*txn, &w.table, rows.iter().map(|r| (None, r)))
}

/// Replaces rows `(key, old, new)` (validated already) and their index
/// entries, then runs the foreign key checks and actions.
pub fn update_rows(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    w: &Writer,
    changes: &[(Vec<u8>, Row, Row)],
) -> Result<()> {
    let t = &w.table;
    for (key, old, new) in changes {
        let new_key = if t.primary_key.is_empty() {
            key.clone()
        } else {
            row_key_of(t, new, None)
        };
        if new_key != *key {
            if w.storage.exists(&*txn, t, &new_key, false)? {
                return Err(duplicate(&format!("{}_pkey", t.name)));
            }
            w.storage.remove(txn, t, key)?;
            w.storage.write(txn, t, &new_key, new, None)?;
        } else {
            w.storage.write(txn, t, key, new, Some(old))?;
        }
        let (old_pk, new_pk) = (pk_suffix(t, key), pk_suffix(t, &new_key));
        let olds = w.keys.entries(&*txn, t, old, old_pk)?;
        let news = w.keys.entries(&*txn, t, new, new_pk)?;
        for (i, ((ok, _), (nk, unique))) in olds.into_iter().zip(news).enumerate() {
            if ok == nk && old_pk == new_pk {
                continue;
            }
            txn.delete(&ok)?;
            if unique && txn.get(&nk)?.is_some() {
                return Err(duplicate(&t.indexes[i].name));
            }
            txn.put(&nk, new_pk)?;
        }
    }
    crate::storage::written(t);
    crate::fk::check_children(&*txn, t, changes.iter().map(|(_, o, n)| (Some(o), n)))?;
    if !t.referenced_by.is_empty() {
        let changed: Vec<(Row, Option<Row>)> = changes
            .iter()
            .map(|(_, o, n)| (o.clone(), Some(n.clone())))
            .collect();
        crate::fk::parent_changed(txn, reg, t, &changed)?;
    }
    Ok(())
}

/// Deletes rows and their index entries, then runs the foreign key
/// actions of the tables referencing them.
pub fn delete_rows(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    t: &Table,
    rows: &[(Vec<u8>, Row)],
) -> Result<()> {
    let keys = IndexKeys::new(&*txn, reg, t)?;
    let storage = crate::storage::of(t)?;
    catalog::validate_table(&*txn, &t.name)?;
    for (key, row) in rows {
        storage.remove_row(txn, t, key, row)?;
        if t.indexes.is_empty() {
            continue;
        }
        let pk = pk_suffix(t, key);
        for (k, _) in keys.entries(&*txn, t, row, pk)? {
            txn.delete(&k)?;
        }
    }
    crate::storage::written(t);
    if !t.referenced_by.is_empty() {
        let removed: Vec<(Row, Option<Row>)> =
            rows.iter().map(|(_, r)| (r.clone(), None)).collect();
        crate::fk::parent_changed(txn, reg, t, &removed)?;
    }
    Ok(())
}

/// `DELETE`.
pub fn delete(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    filter: Option<&ast::Expr>,
    returning_items: &[SelectItem],
    params: &[Value],
) -> Result<Written> {
    let t = catalog::table(&*txn, table)?;
    let rows = {
        let ro: &dyn KvTxn = &*txn;
        let mut binder = Binder::new(ro, reg).checked();
        if let Some(c) = binder.checker() {
            c.require(ro, Kind::Table, &t.name, DELETE, None)?;
        }
        let (access, pred) = bind_filter(&mut binder, &t, filter)?;
        let ret = returning(&mut binder, returning_items, &t)?;
        let using = row_policy(&binder, &t, crate::policies::Command::Delete)?;
        let ctx = Ctx::new(ro, reg, params);
        // A partitioned table's rows are its partitions'.
        let targets: Vec<Table> = match &t.partition_by {
            None => vec![t.clone()],
            Some(_) => crate::partition::partitions(ro, &t)?,
        };
        // One table: its rows as found (each its key, then its values);
        // partitions': each also with its partition's number first.
        let rows = if let [target] = &targets[..] {
            secure_rows(target, &access, using.as_ref(), pred.as_ref(), &ctx)?
        } else {
            let mut rows = crate::spill::Stash::new();
            for (ti, target) in targets.iter().enumerate() {
                for r in
                    secure_rows(target, &access, using.as_ref(), pred.as_ref(), &ctx)?.into_iter()
                {
                    let mut row = vec![Value::Int(ti as i64)];
                    row.extend(r?);
                    rows.push(row)?;
                }
            }
            rows.seal()?
        };
        (rows, ret, targets)
    };
    let (rows, ret, targets) = (rows.0, rows.1, rows.2);
    let count = rows.len() as u64;
    let one = targets.len() == 1;
    let split = |mut r: Row| -> Result<(usize, Vec<u8>, Row)> {
        let (ti, at) = if one { (0, 0) } else { (table_number(&r)?, 1) };
        let key = take_key(&mut r, at)?;
        r.drain(..=at);
        Ok((ti, key, r))
    };
    let mut by: Vec<Vec<(Vec<u8>, Row)>> = vec![Vec::new(); targets.len()];
    let flush = |txn: &mut dyn KvTxn, by: &mut Vec<Vec<(Vec<u8>, Row)>>| -> Result<()> {
        for (target, chunk) in targets.iter().zip(by.iter_mut()) {
            if !chunk.is_empty() {
                delete_rows(txn, reg, target, chunk)?;
                chunk.clear();
            }
        }
        Ok(())
    };
    let (each, kept) = consume_or_keep(rows, ret.is_some());
    let mut n = 0;
    for r in each {
        let (ti, key, row) = split(r?)?;
        by[ti].push((key, row));
        n += 1;
        if n % CHUNK == 0 {
            flush(txn, &mut by)?;
        }
    }
    flush(txn, &mut by)?;
    let returning = match ret {
        None => None,
        Some((exprs, cols)) => {
            let ctx = Ctx::new(&*txn, reg, params);
            let rows = kept.ok_or_else(|| Error::internal("the rows RETURNING reads"))?;
            Some((
                cols,
                rows.iter()
                    .map(|r| project(&exprs, &split(r?)?.2, &ctx))
                    .collect::<Result<_>>()?,
            ))
        }
    };
    Ok(Written { count, returning })
}

/// A stash's rows to apply — consumed, or, when `keep` (`RETURNING` reads
/// them again), read from the stash kept.
#[allow(clippy::type_complexity)]
fn consume_or_keep(
    rows: crate::spill::Stored,
    keep: bool,
) -> (
    Box<dyn Iterator<Item = Result<Row>>>,
    Option<std::rc::Rc<crate::spill::Stored>>,
) {
    if keep {
        let rows = std::rc::Rc::new(rows);
        (rows.iter(), Some(rows))
    } else {
        (rows.into_iter(), None)
    }
}

/// The table number a stashed write's row starts with.
fn table_number(r: &Row) -> Result<usize> {
    match r.first() {
        Some(Value::Int(ti)) => Ok(*ti as usize),
        _ => Err(Error::internal("a stashed write without its table")),
    }
}

/// The key a stashed write's row holds at `at` (as [`exec::keyed`] puts
/// it), taken out.
fn take_key(r: &mut Row, at: usize) -> Result<Vec<u8>> {
    match r.get_mut(at).map(|v| std::mem::replace(v, Value::Null)) {
        Some(Value::Bytea(k)) => Ok(k.to_vec()),
        _ => Err(Error::internal("a stashed write without its key")),
    }
}

/// Writes the entries of a new index for every existing row: the rows
/// read with only the index's columns decoded — in parts of the table on
/// the statement's workers, a round of parts at a time (so memory stays
/// within a round's entries and the sort's `work_mem`) — and its entries
/// sorted and written in key order ([`IndexBuild`]).
pub fn backfill_index(txn: &mut dyn KvTxn, reg: &Registry, table: &Table, i: usize) -> Result<()> {
    let keys = IndexKeys::new(&*txn, reg, table)?;
    // The new index's keys: none yet, and no other transaction sees the
    // index before this one commits.
    let prefix = table.index_prefix(table.indexes[i].id);
    if let Some(end) = nucleus_kv::prefix_end(&prefix) {
        txn.fresh(&prefix, &end);
    }
    let storage = crate::storage::of(table)?;
    let mut wanted = vec![false; table.columns.len()];
    for &c in &table.indexes[i].columns {
        wanted[c] = true;
    }
    let rows_prefix = table.index_prefix(0);
    let rows_end = nucleus_kv::prefix_end(&rows_prefix);
    // The entries of the rows in `[start, end)`, each given to `to`.
    let part = |ro: &dyn KvTxn,
                start: Vec<u8>,
                end: Option<Vec<u8>>,
                to: &mut dyn FnMut(&[u8], &[u8], bool) -> Result<()>|
     -> Result<()> {
        for r in storage.rows(crate::storage::Scan {
            txn: ro,
            table,
            start,
            end,
            wanted: Some(&wanted),
            validated: false,
            bounds: Vec::new(),
            dicts: Vec::new(),
        })? {
            let (key, row) = r?;
            let pk = pk_suffix(table, &key);
            let (k, unique) = keys.entry(ro, table, &row, pk, i)?;
            to(&k, pk, unique)?;
        }
        Ok(())
    };
    let mut build = IndexBuild::new(vec![i]);
    {
        let ro: &dyn KvTxn = &*txn;
        let workers = crate::budget::workers();
        let shared = ro.shared().filter(|_| workers > 1);
        let cuts = match shared {
            Some(_) => {
                storage.split_points(ro, table, &rows_prefix, rows_end.as_deref(), BUILD_PARTS)?
            }
            None => Vec::new(),
        };
        if let (Some(shared), false) = (shared, cuts.is_empty()) {
            let mut bounds = Vec::with_capacity(cuts.len() + 1);
            let mut from = rows_prefix.clone();
            for c in cuts {
                bounds.push((from, Some(c.clone())));
                from = c;
            }
            bounds.push((from, rows_end.clone()));
            let ctx = Ctx::new(ro, reg, &[]);
            for round in bounds.chunks(workers) {
                let got =
                    crate::vector::on_workers(&ctx, shared, workers, round.len(), true, &|w, m| {
                        let (start, end) = round[m].clone();
                        let mut out = Entries::default();
                        part(w.txn, start, end, &mut |k, pk, u| {
                            out.push(k, pk, u);
                            Ok(())
                        })?;
                        Ok(out)
                    });
                for entries in got {
                    let entries = entries
                        .ok_or_else(|| Error::internal("a part of an index build did not run"))??;
                    build.extend(0, entries)?;
                }
            }
        } else {
            let s = &mut build.sorters[0];
            part(
                ro,
                rows_prefix.clone(),
                rows_end.clone(),
                &mut |k, pk, u| {
                    s.entries.push(k, pk, u);
                    s.spill_past_budget()
                },
            )?;
        }
    }
    build.write(txn, table, &|name| {
        Error::unique_violation(format!(
            "could not create unique index \"{name}\": key is duplicated"
        ))
    })
}

/// Parts of a table an index build reads at most (each a worker's turn):
/// many, so a round of them is a small share of a large table.
const BUILD_PARTS: usize = 1024;

/// Every key of a table — its rows' and its indexes' — written again in
/// the current key encoding (an upgrade from an older one; the decoder
/// reads both).
pub fn rewrite_keys(txn: &mut dyn KvTxn, reg: &Registry, table: &Table) -> Result<()> {
    let keys = IndexKeys::new(&*txn, reg, table)?;
    let storage = crate::storage::of(table)?;
    let prefix = table.data_prefix();
    let end = nucleus_kv::prefix_end(&prefix).expect("table prefix");
    let primary = table.index_prefix(0);
    let primary_end = nucleus_kv::prefix_end(&primary);
    let mut rows = crate::spill::Stash::new();
    for r in storage.rows(crate::storage::Scan {
        txn: &*txn,
        table,
        start: primary,
        end: primary_end,
        wanted: None,
        validated: false,
        bounds: Vec::new(),
        dicts: Vec::new(),
    })? {
        let (k, row) = r?;
        rows.push(exec::keyed(k, row))?;
    }
    let rows = rows.seal()?;
    catalog::delete_range(txn, &prefix, Some(&end))?;
    let mut build = IndexBuild::new((0..table.indexes.len()).collect());
    for r in rows.into_iter() {
        let (old_key, row) = exec::unkeyed(r?)?;
        let key = if table.primary_key.is_empty() {
            old_key // a hidden row id: an integer, encoded as before
        } else {
            row_key_of(table, &row, None)
        };
        storage.write(txn, table, &key, &row, None)?;
        build.add(&keys, &*txn, table, &row, pk_suffix(table, &key))?;
    }
    build.write(txn, table, &duplicate)?;
    crate::storage::written(table);
    Ok(())
}

/// Replaces every row of `table` (as now defined) with `rows` — each with
/// the key it had, which a table with a primary key computes afresh —
/// and writes their index entries; duplicates of a key or a unique index
/// are refused.
pub fn replace_rows(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &Table,
    rows: crate::spill::Stored,
) -> Result<()> {
    let prefix = table.data_prefix();
    let end = nucleus_kv::prefix_end(&prefix).expect("table prefix");
    catalog::delete_range(txn, &prefix, Some(&end))?;
    let keys = IndexKeys::new(&*txn, reg, table)?;
    let storage = crate::storage::of(table)?;
    let mut build = IndexBuild::new((0..table.indexes.len()).collect());
    for r in rows {
        let (old_key, row) = exec::unkeyed(r?)?;
        let key = if table.primary_key.is_empty() {
            old_key
        } else {
            row_key_of(table, &row, None)
        };
        if storage.exists(&*txn, table, &key, false)? {
            return Err(duplicate(&format!("{}_pkey", table.name)));
        }
        storage.write(txn, table, &key, &row, None)?;
        build.add(&keys, &*txn, table, &row, pk_suffix(table, &key))?;
    }
    build.write(txn, table, &duplicate)?;
    crate::storage::written(table);
    Ok(())
}

/// Every row of a table rewritten (after `ADD COLUMN … DEFAULT`).
pub fn rewrite_rows(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &Table,
    fill: &Value,
) -> Result<()> {
    let rows = {
        let ctx = Ctx::new(&*txn, reg, &[]);
        exec::matching_stash(table, &Access::Full, None, &ctx)?
    };
    let storage = crate::storage::of(table)?;
    for r in rows.into_iter() {
        let (key, row) = exec::unkeyed(r?)?;
        let mut filled = row.clone();
        let last = table.columns.len() - 1;
        filled[last] = fill.clone();
        storage.write(txn, table, &key, &filled, Some(&row))?;
    }
    crate::storage::written(table);
    Ok(())
}

/// For `EXPLAIN` of a write: the scan it would do.
pub fn explain_scan(
    txn: &dyn KvTxn,
    reg: &Registry,
    table: &str,
    filter: Option<&ast::Expr>,
) -> Result<Plan> {
    let t = catalog::table(txn, table)?;
    let mut binder = Binder::new(txn, reg).checked();
    let (access, filter) = bind_filter(&mut binder, &t, filter)?;
    Ok(Plan::Scan {
        table: t,
        access,
        filter,
        estimate: None,
    })
}
