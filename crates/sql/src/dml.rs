//! Writes: INSERT, UPDATE, DELETE — constraints and index maintenance.

use std::collections::HashSet;
use std::sync::Arc;

use nucleus_kv::KvTxn;

use crate::ast::{self, SelectItem};
use crate::binder::{Binder, Scope, ScopeCol, conjunction, conjuncts};
use crate::catalog::{self, Table};
use crate::error::{Error, Result};
use crate::exec::{self, index_key, pk_suffix, row_key_of};
use crate::expr::{Ctx, Env, Expr};
use crate::ext::Registry;
use crate::index_method::{IndexMethod, RowRef};
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

/// How a table's index entries are made: per B-tree index, per column,
/// the column's value or (a column indexed in a taught ordering) its
/// mapping, bound once; an index of a registered method
/// ([`crate::index_method`]) keeps itself, told each change ([`keep`](Self::keep)).
pub struct IndexKeys {
    maps: Vec<Vec<Option<Expr>>>,
    /// The B-tree indexes' positions.
    btrees: Vec<usize>,
    /// The other indexes' positions and methods.
    methods: Vec<(usize, Arc<dyn IndexMethod>)>,
}

impl IndexKeys {
    /// Binds the mappings of `table`'s indexes.
    pub fn new(txn: &dyn KvTxn, reg: &Registry, table: &Table) -> Result<Self> {
        let mut binder = Binder::new(txn, reg);
        let mut maps = Vec::with_capacity(table.indexes.len());
        let (mut btrees, mut methods) = (Vec::new(), Vec::new());
        for (i, ix) in table.indexes.iter().enumerate() {
            match crate::index_method::of(ix)? {
                Some(m) => methods.push((i, m)),
                None => btrees.push(i),
            }
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
        Ok(Self {
            maps,
            btrees,
            methods,
        })
    }

    /// The B-tree indexes' positions in the table's list.
    pub fn btrees(&self) -> &[usize] {
        &self.btrees
    }

    /// Whether some index is a registered method's.
    pub fn has_methods(&self) -> bool {
        !self.methods.is_empty()
    }

    /// Tells every registered method's index of a change to a row: `old`
    /// as it was (`None`: inserted), `new` as it is (`None`: deleted).
    pub fn keep(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        old: Option<RowRef>,
        new: Option<RowRef>,
    ) -> Result<()> {
        for (i, m) in &self.methods {
            m.write(txn, t, *i, old, new)?;
        }
        Ok(())
    }

    /// Every B-tree entry of `row` (primary key bytes `pk`): the index's
    /// position, the key and whether it takes part in uniqueness.
    pub fn entries(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        row: &Row,
        pk: &[u8],
    ) -> Result<Vec<(usize, Vec<u8>, bool)>> {
        // Mappings are immutable: they need nothing of the session.
        static NONE: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
        let ctx = Ctx::new(txn, NONE.get_or_init(Registry::empty), &[]);
        let env = Env {
            row,
            outer: None,
            ctx: &ctx,
        };
        self.btrees
            .iter()
            .map(|&i| {
                let (k, unique) = self.entry_in(&env, t, row, pk, i)?;
                Ok((i, k, unique))
            })
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
        let threads = crate::budget::workers();
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
    runs: Vec<Arc<crate::spill::Sealed>>,
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
    /// The stored generated columns and their expressions, in order.
    generated: Vec<(usize, Expr)>,
    /// The embedding columns and the columns their texts are made of.
    embeddings: Vec<(usize, Vec<usize>)>,
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
        let mut generated = Vec::new();
        let mut embeddings = Vec::new();
        for (c, col) in table.columns.iter().enumerate() {
            match &col.generated {
                Some(ast::Generated::Stored(text)) => {
                    generated.push((c, binder.expr(&parse_expr(text)?, &scope)?.0));
                }
                Some(ast::Generated::Embedding { source, .. }) => {
                    let e = binder.expr(&parse_expr(source)?, &scope)?.0;
                    let mut reads = e.columns();
                    reads.sort_unstable();
                    reads.dedup();
                    embeddings.push((c, reads));
                }
                None => {}
            }
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
            generated,
            embeddings,
            keys,
            storage,
            partitions,
            bound,
        })
    }

    /// A new or changed row made whole: its stored generated columns
    /// computed (from the row's other columns), then
    /// [`validate`](Self::validate)d.
    pub fn complete(&self, row: &mut Row, ctx: &Ctx) -> Result<()> {
        for (c, e) in &self.generated {
            let v = e.eval(&Env {
                row,
                outer: None,
                ctx,
            })?;
            row[*c] = assign(ctx.txn, v, &self.table, *c)?;
        }
        self.validate(row, ctx)
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

    /// A new row's stored form made beforehand, on any thread (`None` for a
    /// partitioned table — its partitions may store rows differently — or a
    /// row its storage stores otherwise).
    pub fn encode_new(&self, row: &Row) -> Option<Vec<u8>> {
        if self.partitions.is_some() {
            return None;
        }
        self.storage.encode_new(&self.table, row)
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
        let bounds = parts
            .iter()
            .map(|w| w.table.partition_of.as_ref().map(|(_, b)| b));
        let to = crate::partition::route(spec, bounds, row).map_err(|_| {
            Error::check_violation(format!(
                "no partition of relation \"{}\" found for row",
                self.table.name
            ))
        })?;
        Ok(Some(to))
    }
}

/// The columns `columns` names of `t` (all, without a list): an `INSERT`'s
/// or a `COPY`'s.
pub fn target_columns(t: &Table, columns: &Option<Vec<String>>) -> Result<Vec<usize>> {
    let Some(names) = columns else {
        // Generated columns are made, not given.
        return Ok((0..t.columns.len())
            .filter(|&c| t.columns[c].generated.is_none())
            .collect());
    };
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
            if t.columns[i].generated.is_some() {
                return Err(generated_given(t, i));
            }
            Ok(i)
        })
        .collect()
}

/// `428C9`: a value given for generated column `c` of `t`.
pub fn generated_given(t: &Table, c: usize) -> Error {
    let col = &t.columns[c];
    Error::generated_always(format!(
        "column \"{}\" of relation \"{}\" is a generated column: its value is made from {}, not given",
        col.name,
        t.name,
        col.generated.as_ref().map_or("", |g| g.expr())
    ))
}

/// New rows of a table from the values a statement gives for some of its
/// columns — `INSERT`'s source rows, `COPY`'s fields or typed rows (PLAN.md
/// §M14.20 W3: one builder, bound once per statement): each value assigned
/// to its column, the other columns' defaults, then the constraints of a
/// new row and the row-level security check.
pub struct RowBuilder {
    /// The table's writer.
    pub writer: Writer,
    /// The columns given, in order.
    targets: Vec<usize>,
    /// The columns not given (their defaults taken).
    rest: Vec<usize>,
    /// The targets are every column, in order.
    in_order: bool,
    check: Option<Expr>,
}

impl RowBuilder {
    /// A builder of rows given `targets`' values, checked by `check` (the
    /// policies' condition for new rows).
    pub fn new(writer: Writer, targets: Vec<usize>, check: Option<Expr>) -> Self {
        let rest = (0..writer.table.columns.len())
            .filter(|c| !targets.contains(c))
            .collect();
        let in_order = targets.iter().copied().eq(0..writer.table.columns.len());
        Self {
            writer,
            targets,
            rest,
            in_order,
            check,
        }
    }

    /// The columns given.
    pub fn targets(&self) -> &[usize] {
        &self.targets
    }

    /// Whether rows can be built on any thread, in any order: the
    /// defaults taken, the checks and the policy's condition neither
    /// volatile (`nextval`, `random()`) nor with subqueries.
    pub fn parallel_safe(&self) -> bool {
        let w = &self.writer;
        w.defaults
            .iter()
            .flatten()
            .all(crate::vector::parallel_safe)
            && w.generated
                .iter()
                .all(|(_, e)| crate::vector::parallel_safe(e))
            && w.checks
                .iter()
                .all(|(_, e)| crate::vector::parallel_safe(e))
            && self.check.as_ref().is_none_or(crate::vector::parallel_safe)
    }

    /// The row of `vals`, the first targets' values (fewer: the rest take
    /// their defaults).
    pub fn build_values(&self, ctx: &Ctx, vals: Vec<Value>) -> Result<Row> {
        let t = &self.writer.table;
        let mut row = vec![Value::Null; t.columns.len()];
        let n = vals.len().min(self.targets.len());
        for (&c, v) in self.targets.iter().zip(vals) {
            row[c] = assign(ctx.txn, v, t, c)?;
        }
        self.finish(row, n, ctx)
    }

    /// The row of a `COPY` record's fields, one per target (counted by the
    /// caller), parsed straight into the columns' types.
    pub(crate) fn build_text(&self, ctx: &Ctx, rec: &crate::copy::Record) -> Result<Row> {
        let t = &self.writer.table;
        let field = |i: usize, c: usize| match rec.get(i) {
            Some(s) => assign_text(ctx.txn, s, t, c),
            None => Ok(Value::Null),
        };
        // Every column, in order (the common load): the row made as it is
        // parsed, without a row of NULLs first.
        if self.in_order {
            let row = (0..t.columns.len())
                .map(|c| field(c, c))
                .collect::<Result<Row>>()?;
            return self.finish(row, self.targets.len(), ctx);
        }
        let mut row = vec![Value::Null; t.columns.len()];
        for (i, &c) in self.targets.iter().enumerate() {
            row[c] = field(i, c)?;
        }
        self.finish(row, self.targets.len(), ctx)
    }

    /// Defaults for the columns not given (and the targets past the first
    /// `given`), then the checks.
    fn finish(&self, mut row: Row, given: usize, ctx: &Ctx) -> Result<Row> {
        for &c in self.rest.iter().chain(&self.targets[given..]) {
            row[c] = self.writer.default_of(c, ctx)?;
        }
        self.writer.complete(&mut row, ctx)?;
        admit(&self.check, &self.writer.table, &row, ctx)?;
        Ok(row)
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
    fit_column(col, v)
}

/// [`assign`] of the text `s` (a load's field), parsed straight into
/// the column's type.
pub fn assign_text(txn: &dyn KvTxn, s: &str, table: &Table, c: usize) -> Result<Value> {
    let col = &table.columns[c];
    match col.ty {
        DataType::Enum(_) => assign(txn, Value::Text(s.to_owned()), table, c),
        ty => fit_column(col, Value::from_text(s, ty)?),
    }
}

/// A value of column `col`'s type made to fit it: its length, `char(n)`'s
/// padding, `numeric(p, s)`'s scale.
fn fit_column(col: &crate::catalog::Column, v: Value) -> Result<Value> {
    // A registered type's modifier (`vector(3)`).
    if let (DataType::Ext(t), Some(n)) = (col.ty, col.max_len)
        && !v.is_null()
    {
        return t.imp().fit(v, n);
    }
    // A string's bytes are at least its characters: most need no count.
    if let (Some(max), Value::Text(s)) = (col.max_len, &v)
        && s.len() > max as usize
        && s.chars().count() > max as usize
    {
        return Err(Error::string_too_long(format!(
            "value too long for type character varying({max})"
        )));
    }
    if let Value::Char(s) = v {
        return Ok(Value::Char(match col.max_len {
            Some(n) => crate::types::fit_char_owned(s, n)?,
            None => s,
        }));
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
    let targets = target_columns(&t, columns)?;
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
        let builder = RowBuilder::new(Writer::new(ro, reg, t.clone())?, targets, check);
        let ctx = Ctx::new(ro, reg, params);
        // Collected before anything is written (the source may read the
        // table), in bounded memory.
        let mut rows = crate::spill::Stash::new();
        for src in exec::build(&plan, &ctx, None)? {
            rows.push(builder.build_values(&ctx, src?)?)?;
        }
        (std::rc::Rc::new(rows.seal()?), builder.writer, ret)
    };
    let mut chunk = Vec::with_capacity(CHUNK);
    for r in rows.iter() {
        chunk.push(r?);
        if chunk.len() == CHUNK {
            insert_rows(txn, &writer, std::mem::take(&mut chunk))?;
        }
    }
    insert_rows(txn, &writer, chunk)?;
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
            if t.columns[c].generated.is_some() {
                return Err(generated_given(&t, c));
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
                writer.complete(&mut new, &ctx)?;
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
    for (pw, rows) in parts.iter().zip(arrive) {
        insert_rows(txn, pw, rows)?;
    }
    Ok(())
}

/// A new row for [`insert_batch`]: built and checked already, its
/// storage's [`encode_new`](crate::storage::TableStorage::encode_new) form
/// if made beforehand, and its place (what an error at it names).
pub(crate) struct Built {
    pub row: Row,
    pub encoded: Option<Vec<u8>>,
    pub place: usize,
}

/// An error, and the place of the row it is at (`None`: at no one row).
type Failed = (Option<usize>, Error);

/// Stores new rows — built and checked already, handed over — and their
/// index entries, then checks their foreign keys: one path for `INSERT`,
/// `COPY` and rows an `UPDATE` moves between partitions (PLAN.md §M14.20
/// W3b).  A partitioned table's rows go to their partitions, routed by the
/// bounds; hidden row ids are taken for the batch at once; the rows go to
/// the storage at once ([`crate::storage::TableStorage::insert_batch`]);
/// each referenced key is checked once.  An error at a row goes through
/// `at` with the row's place — the first row's in their order, as a row
/// at a time would fail.  `fks`: whether to check the foreign keys (not
/// when the statement fails after these rows anyway: its own error is the
/// one, as the checks come at a statement's end).
pub(crate) fn insert_batch(
    txn: &mut dyn KvTxn,
    w: &Writer,
    rows: Vec<Built>,
    fks: bool,
    at: &dyn Fn(usize, Error) -> Error,
) -> Result<()> {
    store_batch(txn, w, rows, fks).map_err(|(place, e)| match place {
        Some(p) => at(p, e),
        None => e,
    })
}

/// The first of two errors, by the place of their rows (one at no row, a
/// foreign key's or the store's, is a later one).
fn first_failure(a: Option<Failed>, b: Failed) -> Failed {
    match a {
        Some(a) if a.0.is_some_and(|p| b.0.is_none_or(|q| p < q)) => a,
        _ => b,
    }
}

fn store_batch(
    txn: &mut dyn KvTxn,
    w: &Writer,
    rows: Vec<Built>,
    fks: bool,
) -> std::result::Result<(), Failed> {
    if rows.is_empty() {
        return Ok(());
    }
    if let Some((_, parts)) = &w.partitions {
        // Each row to its partition, in order — up to a row that fits none,
        // which fails after the rows before it.
        let mut by: Vec<Vec<Built>> = (0..parts.len()).map(|_| Vec::new()).collect();
        let mut fail = None;
        for b in rows {
            match w.partition_of(&b.row) {
                Ok(i) => by[i.expect("partitioned")].push(b),
                Err(e) => {
                    fail = Some((Some(b.place), e));
                    break;
                }
            }
        }
        // Of the partitions' errors, the first row's.
        let mut first: Option<Failed> = None;
        for (pw, rows) in parts.iter().zip(by) {
            if let Err(f) = store_batch(txn, pw, rows, fks) {
                first = Some(first_failure(first, f));
            }
        }
        return match (first, fail) {
            (Some(f), _) | (None, Some(f)) => Err(f),
            (None, None) => Ok(()),
        };
    }
    let t = &w.table;
    let at_row = |place: usize| move |e: Error| (Some(place), e);
    let none = |e: Error| (None, e);
    let first_id = if t.primary_key.is_empty() {
        Some(catalog::next_rowids(txn, t, rows.len() as u64).map_err(none)?)
    } else {
        None
    };
    // The keys rows reference, and each row's key and index entries, made
    // before the rows are handed over — up to a row whose entries fail,
    // which fails after the rows before it.
    let fk_keys = (fks && !t.foreign_keys.is_empty())
        .then(|| crate::fk::child_keys(t, rows.iter().map(|b| (None, &b.row))));
    let indexed = !w.keys.btrees().is_empty();
    let kept = w.keys.has_methods();
    let mut embedded: Vec<Vec<u8>> = Vec::new();
    let mut places = Vec::with_capacity(rows.len());
    let mut entries: Vec<(Vec<u8>, Vec<(usize, Vec<u8>, bool)>)> = Vec::new();
    let mut batch = Vec::with_capacity(rows.len());
    let mut fail = None;
    for (i, b) in rows.into_iter().enumerate() {
        let key = row_key_of(t, &b.row, first_id.map(|f| f + i as i64));
        if indexed {
            let pk = pk_suffix(t, &key);
            match w.keys.entries(&*txn, t, &b.row, pk) {
                Ok(es) => entries.push((pk.to_vec(), es)),
                Err(e) => {
                    fail = Some((Some(b.place), e));
                    break;
                }
            }
        }
        if !w.embeddings.is_empty() {
            embedded.push(pk_suffix(t, &key).to_vec());
        }
        places.push(b.place);
        batch.push(crate::storage::NewRow {
            key,
            row: b.row,
            encoded: b.encoded,
        });
    }
    // The rows, for the registered methods' indexes (handed over below).
    let news: Vec<(Vec<u8>, Row)> = if kept {
        batch
            .iter()
            .map(|r| (pk_suffix(t, &r.key).to_vec(), r.row.clone()))
            .collect()
    } else {
        Vec::new()
    };
    let stored = batch.len();
    let dup = w
        .storage
        .insert_batch(txn, t, batch, first_id.is_some())
        .map_err(none)?;
    for (i, (pk, es)) in entries.into_iter().enumerate().take(dup.unwrap_or(stored)) {
        for (j, k, unique) in es {
            if unique && txn.get(&k).map_err(|e| none(e.into()))?.is_some() {
                return Err((Some(places[i]), duplicate(&t.indexes[j].name)));
            }
            txn.put(&k, &pk).map_err(|e| none(e.into()))?;
        }
    }
    for (pk, row) in news.iter().take(dup.unwrap_or(stored)) {
        w.keys.keep(txn, t, None, Some((pk, row))).map_err(none)?;
    }
    // A job for each new row's vectors (its text made by the workers).
    for pk in embedded.iter().take(dup.unwrap_or(stored)) {
        for &(c, _) in &w.embeddings {
            crate::embedding::enqueue_row(txn, t, c, pk).map_err(none)?;
        }
    }
    if let Some(i) = dup {
        return Err(at_row(places[i])(duplicate(&format!("{}_pkey", t.name))));
    }
    if let Some(f) = fail {
        return Err(f);
    }
    crate::storage::written(t);
    if let Some(keys) = fk_keys {
        crate::fk::check_keys(&*txn, t, &keys).map_err(none)?;
    }
    Ok(())
}

/// Stores new rows (validated already), then checks their foreign keys
/// ([`insert_batch`]).
pub fn insert_rows(txn: &mut dyn KvTxn, w: &Writer, rows: Vec<Row>) -> Result<()> {
    let rows = rows
        .into_iter()
        .enumerate()
        .map(|(place, row)| Built {
            row,
            encoded: None,
            place,
        })
        .collect();
    insert_batch(txn, w, rows, true, &|_, e| e)
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
        for ((_, ok, _), (i, nk, unique)) in olds.into_iter().zip(news) {
            if ok == nk && old_pk == new_pk {
                continue;
            }
            txn.delete(&ok)?;
            if unique && txn.get(&nk)?.is_some() {
                return Err(duplicate(&t.indexes[i].name));
            }
            txn.put(&nk, new_pk)?;
        }
        w.keys
            .keep(txn, t, Some((old_pk, old)), Some((new_pk, new)))?;
        // A changed text is a job; a moved row takes its job along.
        for (c, reads) in &w.embeddings {
            if old_pk != new_pk {
                crate::embedding::moved(txn, t, *c, old_pk, new_pk)?;
            }
            if reads.iter().any(|&r| old[r] != new[r]) {
                crate::embedding::enqueue_row(txn, t, *c, new_pk)?;
            }
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
    let embedded: Vec<usize> = crate::embedding::columns_of(t).collect();
    for (key, row) in rows {
        storage.remove_row(txn, t, key, row)?;
        for &c in &embedded {
            crate::embedding::dequeue(txn, t, c, pk_suffix(t, key))?;
        }
        if t.indexes.is_empty() {
            continue;
        }
        let pk = pk_suffix(t, key);
        for (_, k, _) in keys.entries(&*txn, t, row, pk)? {
            txn.delete(&k)?;
        }
        keys.keep(txn, t, Some((pk, row)), None)?;
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
    // The new index's keys: none yet, and no other transaction sees the
    // index before this one commits.
    let prefix = table.index_prefix(table.indexes[i].id);
    if let Some(end) = nucleus_kv::prefix_end(&prefix) {
        txn.fresh(&prefix, &end);
    }
    if let Some(m) = crate::index_method::of(&table.indexes[i])? {
        return m.build(txn, reg, table, i);
    }
    let keys = IndexKeys::new(&*txn, reg, table)?;
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
    let mut build = IndexBuild::new(keys.btrees().to_vec());
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
    build_methods(txn, reg, table, &keys)
}

/// Builds every registered method's index of `table` again, over its rows
/// as now written (their prefixes emptied with the table's).
fn build_methods(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &Table,
    keys: &IndexKeys,
) -> Result<()> {
    for (i, m) in &keys.methods {
        m.build(txn, reg, table, *i)?;
    }
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
    let mut build = IndexBuild::new(keys.btrees().to_vec());
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
    build_methods(txn, reg, table, &keys)
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

/// Every row of a table rewritten with its last column — a stored
/// generated column, just added — computed (and the row checked).
pub fn generate_rows(txn: &mut dyn KvTxn, reg: &Registry, table: &Table) -> Result<()> {
    let w = Writer::new(&*txn, reg, table.clone())?;
    let last = table.columns.len() - 1;
    let rows = {
        let ctx = Ctx::new(&*txn, reg, &[]);
        let mut rows = crate::spill::Stash::new();
        for r in exec::matching(table, &Access::Full, None, &ctx)? {
            let (key, mut row) = r?;
            w.complete(&mut row, &ctx)?;
            rows.push(exec::keyed(key, row))?;
        }
        rows.seal()?
    };
    for r in rows.into_iter() {
        let (key, row) = exec::unkeyed(r?)?;
        let mut old = row.clone();
        old[last] = Value::Null;
        w.storage.write(txn, table, &key, &row, Some(&old))?;
    }
    crate::storage::written(table);
    Ok(())
}

/// Refuses what a table's generated columns may not be: an expression
/// reading a generated column, calling a volatile function or holding a
/// subquery; a generated column in the partitioning.
pub fn check_generated(txn: &dyn KvTxn, reg: &Registry, t: &Table) -> Result<()> {
    let scope = table_scope(t);
    for (c, col) in t.columns.iter().enumerate() {
        let Some(g) = &col.generated else {
            continue;
        };
        let e = Binder::new(txn, reg)
            .expr(&parse_expr(g.expr())?, &scope)?
            .0;
        if e.has_subquery() {
            return Err(Error::feature(format!(
                "column \"{}\": a generation expression cannot use a subquery",
                col.name
            )));
        }
        let mut volatile = false;
        e.walk(&mut |x| {
            if let Expr::Call(f, _) = x {
                volatile |= f.volatility() == crate::ext::Volatility::Volatile;
            }
        });
        if volatile {
            return Err(Error::invalid_object_definition(format!(
                "column \"{}\": a generation expression cannot call a volatile function: its value would not follow from the row",
                col.name
            )));
        }
        if let Some(r) = e
            .columns()
            .into_iter()
            .find(|&r| t.columns[r].generated.is_some())
        {
            return Err(Error::invalid_object_definition(format!(
                "column \"{}\": a generation expression cannot read generated column \"{}\"",
                col.name, t.columns[r].name
            )));
        }
        if t.partition_by
            .as_ref()
            .is_some_and(|p| p.columns.contains(&c))
        {
            return Err(Error::feature(format!(
                "generated column \"{}\" cannot be in the partitioning",
                col.name
            )));
        }
    }
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
