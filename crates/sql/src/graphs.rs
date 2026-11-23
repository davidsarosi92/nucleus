//! Property graphs (SQL/PGQ, PLAN.md §M14.34 G4): `CREATE PROPERTY GRAPH`
//! over tables, and `GRAPH_TABLE (g MATCH … COLUMNS (…))` rewritten into
//! the query over those tables that means the same.
//!
//! A graph is a definition, not data: kept in the catalog (`00 1D
//! <name>`), each vertex table's and edge table's key, labels and
//! properties resolved when it is made (the standard's defaults: the
//! primary key, the table's name as its one label, every column a
//! property, an edge's ends by its foreign keys).
//!
//! `GRAPH_TABLE` is a query: each variable of its patterns is one of the
//! tables its labels allow, so the patterns mean one join for each choice
//! of tables that connect — their rows together (`UNION ALL`).  The
//! rewrite writes that query as SQL (the patterns' expressions are kept as
//! their text), so binding, the optimizer, every executor, privileges and
//! row security treat it as the same query written by hand.  A quantified
//! edge (`-[e]->{1,3}`) is a recursive query over its edge table, `ANY
//! SHORTEST` the same `USING KEY` (§M14.34 G3).  Patterns match as joins
//! do: two edge variables may be the same edge.

use std::collections::{BTreeMap, BTreeSet};

use nucleus_kv::KvTxn;

use crate::ast::{
    CreateGraph, EdgeDirection, GraphElementDef, GraphEndpointDef, GraphProperties, GraphTable,
    LabelExpr, Query, Statement,
};
use crate::catalog;
use crate::definitions::ident;
use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};
use crate::lexer::Token;

/// The catalog's prefix for graphs.
const GRAPHS: [u8; 2] = [0x00, 0x1D];

/// The most tables-to-variables choices one `GRAPH_TABLE` may mean.
const MOST_CHOICES: usize = 4096;

/// A property graph.
#[derive(Clone, Debug, PartialEq)]
pub struct Graph {
    /// Name.
    pub name: String,
    /// The role that made it.
    pub owner: String,
    /// Its vertex tables, then its edge tables.
    pub elements: Vec<Element>,
}

/// A vertex or edge table of a [`Graph`].
#[derive(Clone, Debug, PartialEq)]
pub struct Element {
    /// Its name in the graph (the table's, or `AS`).
    pub alias: String,
    /// The table.
    pub table: String,
    /// An edge table.
    pub edge: bool,
    /// The key's columns.
    pub key: Vec<String>,
    /// An edge's source.
    pub source: Option<Endpoint>,
    /// An edge's destination.
    pub destination: Option<Endpoint>,
    /// Its labels.
    pub labels: Vec<Label>,
}

/// An edge's end: the edge table's columns equal to the vertex table's.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    /// The vertex table's alias.
    pub vertex: String,
    /// The edge table's columns.
    pub columns: Vec<String>,
    /// The vertex table's columns.
    pub references: Vec<String>,
}

/// A label and its properties: (name, SQL text over the table's columns).
#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    /// Name.
    pub name: String,
    /// Properties.
    pub properties: Vec<(String, String)>,
}

impl Element {
    /// Its properties, over all its labels (each name once).
    fn properties(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for l in &self.labels {
            for (n, sql) in &l.properties {
                if !out.iter().any(|(m, _)| m == n) {
                    out.push((n.clone(), sql.clone()));
                }
            }
        }
        out
    }

    fn has_label(&self, name: &str) -> bool {
        self.labels.iter().any(|l| l.name == name)
    }
}

fn key(name: &str) -> Vec<u8> {
    let mut k = GRAPHS.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

fn encode(g: &Graph) -> Vec<u8> {
    let mut w = Writer::default();
    w.str(&g.owner);
    w.u32(g.elements.len() as u32);
    let strs = |w: &mut Writer, v: &[String]| {
        w.u32(v.len() as u32);
        for s in v {
            w.str(s);
        }
    };
    for e in &g.elements {
        w.str(&e.alias).str(&e.table).u8(u8::from(e.edge));
        strs(&mut w, &e.key);
        for end in [&e.source, &e.destination] {
            match end {
                None => {
                    w.u8(0);
                }
                Some(end) => {
                    w.u8(1).str(&end.vertex);
                    strs(&mut w, &end.columns);
                    strs(&mut w, &end.references);
                }
            }
        }
        w.u32(e.labels.len() as u32);
        for l in &e.labels {
            w.str(&l.name).u32(l.properties.len() as u32);
            for (n, sql) in &l.properties {
                w.str(n).str(sql);
            }
        }
    }
    w.0
}

fn decode(name: &str, v: &[u8]) -> Result<Graph> {
    let mut r = Reader::new(v);
    let owner = r.str()?;
    let strs = |r: &mut Reader| -> Result<Vec<String>> { (0..r.u32()?).map(|_| r.str()).collect() };
    let mut elements = Vec::new();
    for _ in 0..r.u32()? {
        let alias = r.str()?;
        let table = r.str()?;
        let edge = r.u8()? == 1;
        let key = strs(&mut r)?;
        let mut ends = Vec::new();
        for _ in 0..2 {
            ends.push(match r.u8()? {
                0 => None,
                _ => Some(Endpoint {
                    vertex: r.str()?,
                    columns: strs(&mut r)?,
                    references: strs(&mut r)?,
                }),
            });
        }
        let destination = ends.pop().flatten();
        let source = ends.pop().flatten();
        let mut labels = Vec::new();
        for _ in 0..r.u32()? {
            let name = r.str()?;
            let properties = (0..r.u32()?)
                .map(|_| Ok((r.str()?, r.str()?)))
                .collect::<Result<_>>()?;
            labels.push(Label { name, properties });
        }
        elements.push(Element {
            alias,
            table,
            edge,
            key,
            source,
            destination,
            labels,
        });
    }
    Ok(Graph {
        name: name.to_string(),
        owner,
        elements,
    })
}

/// The graph `name`.
pub fn get(txn: &dyn KvTxn, name: &str) -> Result<Option<Graph>> {
    txn.get(&key(name))?.map(|v| decode(name, &v)).transpose()
}

/// Every graph, by name.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Graph>> {
    let end = [GRAPHS[0], GRAPHS[1] + 1];
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&GRAPHS[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        let (k, v) = e?;
        let name = String::from_utf8(k[GRAPHS.len()..].to_vec())
            .map_err(|_| Error::internal("a graph's name"))?;
        out.push(decode(&name, &v)?);
    }
    Ok(out)
}

/// The graphs `table` is a vertex or edge table of.
pub fn using_table(txn: &dyn KvTxn, table: &str) -> Result<Vec<String>> {
    Ok(all(txn)?
        .into_iter()
        .filter(|g| g.elements.iter().any(|e| e.table == table))
        .map(|g| g.name)
        .collect())
}

/// Refuses to drop or rename `table` (or, given, its `column`) while a
/// graph uses it (`2BP01`).
pub fn check_unused(txn: &dyn KvTxn, table: &str, column: Option<&str>) -> Result<()> {
    for g in all(txn)? {
        for e in g.elements.iter().filter(|e| e.table == table) {
            let uses = match column {
                None => true,
                Some(c) => uses_column(&g, e, c),
            };
            if uses {
                let what = match column {
                    None => format!("table {table}"),
                    Some(c) => format!("column {c} of table {table}"),
                };
                return Err(Error::dependent_objects(format!(
                    "cannot drop or rename {what} because property graph {} depends on it",
                    g.name
                ))
                .with_hint("DROP PROPERTY GRAPH first"));
            }
        }
    }
    Ok(())
}

/// With `CASCADE`, drops the graphs using `table` (or its `column`);
/// without, refuses while one does (`2BP01`).
pub fn cascade(
    txn: &mut dyn KvTxn,
    table: &str,
    column: Option<&str>,
    cascade: bool,
) -> Result<()> {
    if !cascade {
        return check_unused(&*txn, table, column);
    }
    for g in all(&*txn)? {
        let uses = g
            .elements
            .iter()
            .filter(|e| e.table == table)
            .any(|e| match column {
                None => true,
                Some(c) => uses_column(&g, e, c),
            });
        if uses {
            txn.delete(&key(&g.name))?;
        }
    }
    Ok(())
}

/// Whether element `e` of `g` reads column `c` of its table: in its key,
/// an end's columns (or another edge's references to it), or a property.
fn uses_column(g: &Graph, e: &Element, c: &str) -> bool {
    if e.key.iter().any(|k| k == c) {
        return true;
    }
    for end in [&e.source, &e.destination].into_iter().flatten() {
        if end.columns.iter().any(|k| k == c) {
            return true;
        }
    }
    for other in &g.elements {
        for end in [&other.source, &other.destination].into_iter().flatten() {
            if end.vertex == e.alias && end.references.iter().any(|k| k == c) {
                return true;
            }
        }
    }
    e.properties().iter().any(|(_, sql)| {
        crate::parser::parse_expr(sql).is_ok_and(|x| {
            let mut found = false;
            columns_named(&x, &mut |n| found |= n == c);
            found
        })
    })
}

/// Calls `f` with each column an expression names (not inside subqueries).
fn columns_named(e: &crate::ast::Expr, f: &mut dyn FnMut(&str)) {
    use crate::ast::Expr as E;
    match e {
        E::Column { name, .. } => f(name),
        E::Row(xs) | E::Array(xs) => xs.iter().for_each(|x| columns_named(x, f)),
        E::Unary { expr, .. } | E::IsNull { expr, .. } | E::Cast { expr, .. } => {
            columns_named(expr, f)
        }
        E::Collate { expr, .. } => columns_named(expr, f),
        E::Binary { left, right, .. } | E::Operator { left, right, .. } => {
            columns_named(left, f);
            columns_named(right, f);
        }
        _ => {
            // Anything else: its columns as its debug text names them —
            // a conservative "maybe" is all a dependency check needs.
            let text = format!("{e:?}");
            let mut rest = text.as_str();
            while let Some(i) = rest.find("Column { table: ") {
                rest = &rest[i..];
                if let Some(j) = rest.find("name: \"") {
                    let after = &rest[j + 7..];
                    if let Some(k) = after.find('"') {
                        f(&after[..k]);
                    }
                }
                rest = &rest[1..];
            }
        }
    }
}

/// `CREATE PROPERTY GRAPH`: the definition, its defaults resolved.
pub fn create(txn: &mut dyn KvTxn, def: &CreateGraph) -> Result<()> {
    if get(&*txn, &def.name)?.is_some() {
        return Err(Error::duplicate_object(format!(
            "property graph \"{}\" already exists",
            def.name
        )));
    }
    let mut elements: Vec<Element> = Vec::new();
    for (d, edge) in def
        .vertices
        .iter()
        .map(|d| (d, false))
        .chain(def.edges.iter().map(|d| (d, true)))
    {
        let e = element(&*txn, d, edge, &elements)?;
        if elements.iter().any(|o| o.alias == e.alias) {
            return Err(Error::duplicate_object(format!(
                "alias \"{}\" used more than once in property graph \"{}\"",
                e.alias, def.name
            )));
        }
        elements.push(e);
    }
    // A label means the same properties wherever it is.
    let mut seen: BTreeMap<&str, (&str, BTreeSet<&str>)> = BTreeMap::new();
    for e in &elements {
        for l in &e.labels {
            let names: BTreeSet<&str> = l.properties.iter().map(|(n, _)| n.as_str()).collect();
            match seen.get(l.name.as_str()) {
                Some((first, other)) if *other != names => {
                    return Err(Error::invalid_object_definition(format!(
                        "label \"{}\" has different properties in \"{first}\" and \"{}\"",
                        l.name, e.alias
                    )));
                }
                Some(_) => {}
                None => {
                    seen.insert(&l.name, (&e.alias, names));
                }
            }
        }
    }
    let g = Graph {
        name: def.name.clone(),
        owner: crate::access::current().user.clone(),
        elements,
    };
    txn.put(&key(&g.name), &encode(&g))?;
    Ok(())
}

/// A table of the graph, resolved (`before`: the elements already).
fn element(
    txn: &dyn KvTxn,
    d: &GraphElementDef,
    edge: bool,
    before: &[Element],
) -> Result<Element> {
    let table = catalog::table(txn, &d.table)?;
    let alias = d.alias.clone().unwrap_or_else(|| d.table.clone());
    let column = |c: &str| -> Result<String> {
        table.column(c).map(|_| c.to_string()).ok_or_else(|| {
            Error::undefined_column(format!(
                "column \"{c}\" of table \"{}\" does not exist",
                table.name
            ))
        })
    };
    let key = match &d.key {
        Some(k) => k.iter().map(|c| column(c)).collect::<Result<Vec<_>>>()?,
        None if table.primary_key.is_empty() => {
            return Err(Error::invalid_object_definition(format!(
                "table \"{}\" has no primary key: give its KEY (columns)",
                table.name
            )));
        }
        None => table
            .primary_key
            .iter()
            .map(|&i| table.columns[i].name.clone())
            .collect(),
    };
    let mut used_fks: Vec<usize> = Vec::new();
    let mut end = |e: &Option<GraphEndpointDef>| -> Result<Option<Endpoint>> {
        let Some(e) = e else { return Ok(None) };
        let Some(v) = before.iter().find(|v| !v.edge && v.alias == e.vertex) else {
            return Err(Error::undefined_object(format!(
                "vertex table \"{}\" is not in the property graph",
                e.vertex
            )));
        };
        let (columns, references) = match &e.key {
            Some(k) => {
                let cols = k.iter().map(|c| column(c)).collect::<Result<Vec<_>>>()?;
                let refs = e.references.clone().unwrap_or_else(|| v.key.clone());
                (cols, refs)
            }
            None => {
                // The edge table's one foreign key to the vertex table
                // not taken by its other end.
                let fks: Vec<usize> = table
                    .foreign_keys
                    .iter()
                    .enumerate()
                    .filter(|(i, fk)| fk.table == v.table && !used_fks.contains(i))
                    .map(|(i, _)| i)
                    .collect();
                let [i] = fks[..] else {
                    return Err(Error::invalid_object_definition(format!(
                        "edge table \"{}\" has no single foreign key to \"{}\": give its KEY (columns) REFERENCES {} (columns)",
                        table.name, v.table, v.alias
                    )));
                };
                used_fks.push(i);
                let fk = &table.foreign_keys[i];
                let vt = catalog::table(txn, &v.table)?;
                let cols = fk
                    .columns
                    .iter()
                    .map(|&c| table.columns[c].name.clone())
                    .collect();
                let refs = fk
                    .ref_columns
                    .iter()
                    .map(|&c| vt.columns[c].name.clone())
                    .collect();
                (cols, e.references.clone().unwrap_or(refs))
            }
        };
        let vt = catalog::table(txn, &v.table)?;
        for r in &references {
            if vt.column(r).is_none() {
                return Err(Error::undefined_column(format!(
                    "column \"{r}\" of table \"{}\" does not exist",
                    vt.name
                )));
            }
        }
        if columns.len() != references.len() {
            return Err(Error::invalid_object_definition(format!(
                "edge table \"{}\": {} key columns referencing {} of \"{}\"",
                table.name,
                columns.len(),
                references.len(),
                v.alias
            )));
        }
        Ok(Some(Endpoint {
            vertex: v.alias.clone(),
            columns,
            references,
        }))
    };
    let (source, destination) = if edge {
        (end(&d.source)?, end(&d.destination)?)
    } else {
        (None, None)
    };
    let all_columns = |except: &[String]| -> Result<Vec<(String, String)>> {
        for c in except {
            column(c)?;
        }
        Ok(table
            .columns
            .iter()
            .filter(|c| !except.contains(&c.name))
            .map(|c| (c.name.clone(), ident(&c.name)))
            .collect())
    };
    let defs = if d.labels.is_empty() {
        vec![crate::ast::GraphLabelDef {
            name: None,
            properties: GraphProperties::All { except: vec![] },
        }]
    } else {
        d.labels.clone()
    };
    let mut labels: Vec<Label> = Vec::new();
    for l in &defs {
        let name = l.name.clone().unwrap_or_else(|| alias.clone());
        if labels.iter().any(|o| o.name == name) {
            return Err(Error::duplicate_object(format!(
                "label \"{name}\" given twice for \"{alias}\""
            )));
        }
        let properties = match &l.properties {
            GraphProperties::All { except } => all_columns(except)?,
            GraphProperties::None => vec![],
            GraphProperties::List(list) => {
                for (sql, _) in list {
                    crate::parser::parse_expr(sql)?;
                }
                list.iter()
                    .map(|(sql, n)| (n.clone(), sql.clone()))
                    .collect()
            }
        };
        for (i, (n, sql)) in properties.iter().enumerate() {
            if properties[..i].iter().any(|(m, _)| m == n) {
                return Err(Error::duplicate_column(format!(
                    "property \"{n}\" given twice in label \"{name}\""
                )));
            }
            let other = labels
                .iter()
                .flat_map(|l| &l.properties)
                .find(|(m, _)| m == n);
            if other.is_some_and(|(_, s)| s != sql) {
                return Err(Error::invalid_object_definition(format!(
                    "property \"{n}\" of \"{alias}\" is two different expressions"
                )));
            }
        }
        labels.push(Label { name, properties });
    }
    Ok(Element {
        alias,
        table: table.name.clone(),
        edge,
        key,
        source,
        destination,
        labels,
    })
}

/// `DROP PROPERTY GRAPH`.
pub fn drop(txn: &mut dyn KvTxn, name: &str, if_exists: bool) -> Result<()> {
    let Some(g) = get(&*txn, name)? else {
        if if_exists {
            return Ok(());
        }
        return Err(Error::undefined_object(format!(
            "property graph \"{name}\" does not exist"
        )));
    };
    let who = crate::access::current();
    if !who.superuser && who.user != g.owner {
        return Err(Error::insufficient_privilege(format!(
            "must be owner of property graph {name}"
        )));
    }
    txn.delete(&key(name))?;
    Ok(())
}

/// The graph as the statement that makes it, every default spelled out.
pub fn definition(g: &Graph) -> String {
    let list = |v: &[String]| v.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ");
    let element = |e: &Element| {
        let mut s = ident(&e.table);
        if e.alias != e.table {
            s.push_str(&format!(" AS {}", ident(&e.alias)));
        }
        s.push_str(&format!(" KEY ({})", list(&e.key)));
        for (word, end) in [("SOURCE", &e.source), ("DESTINATION", &e.destination)] {
            if let Some(end) = end {
                s.push_str(&format!(
                    " {word} KEY ({}) REFERENCES {} ({})",
                    list(&end.columns),
                    ident(&end.vertex),
                    list(&end.references)
                ));
            }
        }
        for l in &e.labels {
            s.push_str(&format!(" LABEL {}", ident(&l.name)));
            if l.properties.is_empty() {
                s.push_str(" NO PROPERTIES");
            } else {
                let props: Vec<String> = l
                    .properties
                    .iter()
                    .map(|(n, sql)| {
                        if *sql == ident(n) {
                            sql.clone()
                        } else {
                            format!("{sql} AS {}", ident(n))
                        }
                    })
                    .collect();
                s.push_str(&format!(" PROPERTIES ({})", props.join(", ")));
            }
        }
        s
    };
    let part = |edge: bool| {
        g.elements
            .iter()
            .filter(|e| e.edge == edge)
            .map(element)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut s = format!(
        "CREATE PROPERTY GRAPH {} VERTEX TABLES ({})",
        ident(&g.name),
        part(false)
    );
    if g.elements.iter().any(|e| e.edge) {
        s.push_str(&format!(" EDGE TABLES ({})", part(true)));
    }
    s
}

// ------------------------------------------------------------- GRAPH_TABLE

/// A variable of the patterns.
struct Var {
    name: String,
    edge: bool,
    labels: Vec<LabelExpr>,
    filters: Vec<String>,
    /// The elements it may be.
    candidates: Vec<usize>,
}

/// An edge pattern between two vertex variables.
struct Link {
    /// Its variable (an edge's), or `None` for a quantified edge's walk.
    edge: Option<usize>,
    left: usize,
    right: usize,
    direction: EdgeDirection,
    /// A quantified edge: its walk.
    walk: Option<usize>,
}

/// A quantified edge (`-[e]->{m,n}`): the walks of `min..=max` edges,
/// one recursive query.
struct Walk {
    /// The edge pattern's variable.
    var: String,
    labels: Vec<LabelExpr>,
    filters: Vec<String>,
    direction: EdgeDirection,
    min: u32,
    max: Option<u32>,
    /// `ANY SHORTEST`: one walk of the fewest edges per pair of ends.
    shortest: bool,
    /// The vertex variable it starts at.
    left: usize,
    /// Its edge tables.
    candidates: Vec<usize>,
}

/// A hidden column's name: a table's column `c` under its variable.
fn hidden(c: &str) -> String {
    format!("\u{1}{c}")
}

/// `GRAPH_TABLE (…)` as the query it means.
pub fn rewrite(txn: &dyn KvTxn, gt: &GraphTable) -> Result<Query> {
    let g = get(txn, &gt.graph)?.ok_or_else(|| {
        Error::undefined_object(format!("property graph \"{}\" does not exist", gt.graph))
    })?;
    let sql = rewrite_sql(&g, gt)?;
    match crate::parser::parse_one(&sql)? {
        Statement::Query(q) => Ok(q),
        _ => Err(Error::internal("GRAPH_TABLE's query")),
    }
}

/// The variables a text qualifies columns by (`v` of `v.name`).
fn qualifiers(sql: &str) -> Result<Vec<String>> {
    let toks = crate::lexer::tokenize(sql)?;
    Ok(toks
        .windows(2)
        .filter_map(|w| match (&w[0].0, &w[1].0) {
            (Token::Word(v) | Token::Quoted(v), Token::Op(".")) => Some(v.clone()),
            _ => None,
        })
        .collect())
}

/// `sql` with each `path_length(p)` of a path variable its length.
fn path_lengths(sql: &str, paths: &[(String, String)]) -> Result<String> {
    let toks = crate::lexer::tokenize_spans(sql)?;
    let mut out = String::new();
    let mut at = 0;
    let mut i = 0;
    while i < toks.len() {
        if let [
            (Token::Word(f), start),
            (Token::Op("("), _),
            (Token::Word(p) | Token::Quoted(p), _),
            (Token::Op(")"), end),
            ..,
        ] = &toks[i..]
            && f == "path_length"
        {
            let Some((_, len)) = paths.iter().find(|(n, _)| n == p) else {
                return Err(Error::undefined_column(format!(
                    "path variable \"{p}\" does not exist"
                )));
            };
            out.push_str(&sql[at..start.start]);
            out.push_str(&format!("({len})"));
            at = end.end;
            i += 4;
            continue;
        }
        i += 1;
    }
    out.push_str(&sql[at..]);
    Ok(out)
}

/// A table of the graph as a variable sees it: every property of its
/// kind (NULL where it has none), and the columns its key and ends join
/// by, under hidden names.
fn derived(g: &Graph, e: &Element, props_of_kind: &[String]) -> String {
    let props = e.properties();
    let mut cols: Vec<String> = Vec::new();
    for n in props_of_kind {
        match props.iter().find(|(m, _)| m == n) {
            Some((_, sql)) => cols.push(format!("{sql} AS {}", ident(n))),
            None => cols.push(format!("NULL AS {}", ident(n))),
        }
    }
    let mut needed: Vec<&String> = e.key.iter().collect();
    for end in [&e.source, &e.destination].into_iter().flatten() {
        needed.extend(&end.columns);
    }
    for other in &g.elements {
        for end in [&other.source, &other.destination].into_iter().flatten() {
            if end.vertex == e.alias {
                needed.extend(&end.references);
            }
        }
    }
    let mut done = BTreeSet::new();
    for c in needed {
        if done.insert(c) {
            cols.push(format!("{} AS {}", ident(c), ident(&hidden(c))));
        }
    }
    if cols.is_empty() {
        cols.push(format!("1 AS {}", ident(&hidden(""))));
    }
    format!("(SELECT {} FROM {})", cols.join(", "), ident(&e.table))
}

/// `a.c1 = b.r1 AND …`.
fn equal(a: &str, cols: &[String], b: &str, refs: &[String]) -> String {
    cols.iter()
        .zip(refs)
        .map(|(c, r)| {
            format!(
                "{}.{} = {}.{}",
                ident(a),
                ident(&hidden(c)),
                ident(b),
                ident(&hidden(r))
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn rewrite_sql(g: &Graph, gt: &GraphTable) -> Result<String> {
    let mut vars: Vec<Var> = Vec::new();
    let mut links: Vec<Link> = Vec::new();
    let mut walks: Vec<Walk> = Vec::new();
    // Each path variable and the length it is.
    let mut paths: Vec<(String, String)> = Vec::new();
    let mut fresh = 0;
    let mut var_of = |p: &crate::ast::GraphPattern, vars: &mut Vec<Var>| -> Result<usize> {
        let edge = p.edge.is_some();
        let name = match &p.var {
            Some(v) => v.clone(),
            None => {
                fresh += 1;
                format!("\u{1}{fresh}")
            }
        };
        let i = match vars.iter().position(|v| v.name == name) {
            Some(i) if vars[i].edge != edge => {
                return Err(Error::syntax(format!(
                    "\"{name}\" is both a vertex and an edge"
                )));
            }
            Some(i) => i,
            None => {
                vars.push(Var {
                    name,
                    edge,
                    labels: vec![],
                    filters: vec![],
                    candidates: vec![],
                });
                vars.len() - 1
            }
        };
        vars[i].labels.extend(p.label.clone());
        vars[i].filters.extend(p.filter.clone());
        Ok(i)
    };
    for path in &gt.paths {
        let mut at = var_of(&path.elements[0], &mut vars)?;
        let mut length: Vec<String> = Vec::new();
        let quantified = path.elements.iter().any(|e| e.quantifier.is_some());
        if path.any_shortest && (path.elements.len() != 3 || !quantified) {
            return Err(Error::feature(
                "ANY SHORTEST is supported over one quantified edge: (a)-[e]->+(b)",
            ));
        }
        for pair in path.elements[1..].chunks(2) {
            let [e, v] = pair else {
                return Err(Error::internal("a path ends with a vertex"));
            };
            let direction = e.edge.unwrap_or(EdgeDirection::Any);
            match e.quantifier {
                None => {
                    let edge = var_of(e, &mut vars)?;
                    let next = var_of(v, &mut vars)?;
                    links.push(Link {
                        edge: Some(edge),
                        left: at,
                        right: next,
                        direction,
                        walk: None,
                    });
                    length.push("1".into());
                    at = next;
                }
                Some((min, max)) => {
                    if max.is_none() && !path.any_shortest {
                        return Err(Error::feature(
                            "a quantifier without an upper bound needs ANY SHORTEST",
                        )
                        .with_hint("give an upper bound: {1,5}"));
                    }
                    let var = e.var.clone().unwrap_or_default();
                    if !var.is_empty()
                        && (vars.iter().any(|v| v.name == var)
                            || walks.iter().any(|w| w.var == var))
                    {
                        return Err(Error::feature(format!(
                            "\"{var}\" is a quantified edge's variable and another's"
                        )));
                    }
                    let next = var_of(v, &mut vars)?;
                    walks.push(Walk {
                        var,
                        labels: e.label.clone().into_iter().collect(),
                        filters: e.filter.clone().into_iter().collect(),
                        direction,
                        min,
                        max,
                        shortest: path.any_shortest,
                        left: at,
                        candidates: vec![],
                    });
                    let w = walks.len() - 1;
                    links.push(Link {
                        edge: None,
                        left: at,
                        right: next,
                        direction,
                        walk: Some(w),
                    });
                    length.push(format!(
                        "{}.{}",
                        ident(&hidden(&format!("w{w}"))),
                        ident(&hidden("len"))
                    ));
                    at = next;
                }
            }
        }
        if let Some(p) = &path.var {
            if paths.iter().any(|(n, _)| n == p) || vars.iter().any(|v| v.name == *p) {
                return Err(Error::syntax(format!(
                    "\"{p}\" names a path and another variable"
                )));
            }
            paths.push((p.clone(), length.join(" + ")));
        }
    }
    // Each label named is the graph's.
    let mut names = Vec::new();
    for l in vars
        .iter()
        .flat_map(|v| &v.labels)
        .chain(walks.iter().flat_map(|w| &w.labels))
    {
        label_names(l, &mut names);
    }
    for n in names {
        if !g.elements.iter().any(|e| e.has_label(&n)) {
            return Err(Error::undefined_object(format!(
                "label \"{n}\" does not exist in property graph \"{}\"",
                g.name
            )));
        }
    }
    let allowed = |edge: bool, labels: &[LabelExpr]| -> Vec<usize> {
        (0..g.elements.len())
            .filter(|&i| {
                let e = &g.elements[i];
                e.edge == edge && labels.iter().all(|l| matches(l, e))
            })
            .collect()
    };
    for v in &mut vars {
        v.candidates = allowed(v.edge, &v.labels);
    }
    // A walk goes from a vertex table to itself, by the same columns.
    let mut walk_vertex: Vec<Option<(String, Vec<String>)>> = Vec::new();
    for w in &mut walks {
        w.candidates = allowed(true, &w.labels);
        let mut by: Option<(String, Vec<String>)> = None;
        for &i in &w.candidates {
            let e = &g.elements[i];
            let (Some(s), Some(d)) = (&e.source, &e.destination) else {
                return Err(Error::internal("an edge without ends"));
            };
            let this = (s.vertex.clone(), s.references.clone());
            if d.vertex != s.vertex
                || d.references != s.references
                || by.as_ref().is_some_and(|b| *b != this)
            {
                return Err(Error::feature(
                    "a quantified edge's tables must each join one vertex table to itself, by the same columns",
                ));
            }
            by = Some(this);
        }
        walk_vertex.push(by);
    }
    // A quantified edge's variable is a group of edges: not readable.
    let mut texts: Vec<&String> = gt.columns.iter().map(|(sql, _)| sql).collect();
    texts.extend(&gt.filter);
    texts.extend(vars.iter().flat_map(|v| &v.filters));
    for t in &texts {
        for q in qualifiers(t)? {
            if walks.iter().any(|w| !w.var.is_empty() && w.var == q) {
                return Err(Error::feature(format!(
                    "\"{q}\" is a quantified edge's variable: its edges are a group, not one"
                )));
            }
        }
    }
    let total = vars
        .iter()
        .map(|v| v.candidates.len())
        .try_fold(1usize, |a, n| a.checked_mul(n))
        .unwrap_or(usize::MAX);
    if total > MOST_CHOICES {
        return Err(Error::program_limit(format!(
            "GRAPH_TABLE means {total} choices of tables, more than {MOST_CHOICES}: name labels to narrow them"
        )));
    }
    let (vertex_props, edge_props) = (all_props(g, false), all_props(g, true));
    let column_texts: Vec<String> = gt
        .columns
        .iter()
        .map(|(sql, _)| path_lengths(sql, &paths))
        .collect::<Result<_>>()?;
    let filter = gt
        .filter
        .as_deref()
        .map(|f| path_lengths(f, &paths))
        .transpose()?;
    let outputs: Vec<String> = gt.columns.iter().map(|(_, n)| ident(n)).collect();
    // Every choice of tables whose edges join their vertices: one SELECT.
    let mut selects = Vec::new();
    let mut pick = vec![0usize; vars.len()];
    let possible = vars.iter().all(|v| !v.candidates.is_empty())
        && walks.iter().all(|w| !w.candidates.is_empty());
    let mut more = possible;
    while more {
        let elem = |v: usize| &g.elements[vars[v].candidates[pick[v]]];
        let mut conds: Vec<String> = Vec::new();
        let mut joins = true;
        for l in &links {
            let (left, right) = (elem(l.left), elem(l.right));
            if let Some(w) = l.walk {
                let Some((vertex, refs)) = &walk_vertex[w] else {
                    joins = false;
                    break;
                };
                if left.alias != *vertex || right.alias != *vertex {
                    joins = false;
                    break;
                }
                let wn = hidden(&format!("w{w}"));
                let starts: Vec<String> = (0..refs.len()).map(|j| format!("s{j}")).collect();
                let ends: Vec<String> = (0..refs.len()).map(|j| format!("e{j}")).collect();
                conds.push(equal(&wn, &starts, &vars[l.left].name, refs));
                conds.push(equal(&wn, &ends, &vars[l.right].name, refs));
                if walks[w].min > 1 {
                    conds.push(format!(
                        "{}.{} >= {}",
                        ident(&wn),
                        ident(&hidden("len")),
                        walks[w].min
                    ));
                }
                continue;
            }
            let ev = l.edge.expect("an edge's variable");
            let e = elem(ev);
            let (Some(s), Some(d)) = (&e.source, &e.destination) else {
                return Err(Error::internal("an edge without ends"));
            };
            let en = &vars[ev].name;
            let way = |from: usize, to: usize| -> Option<String> {
                (s.vertex == elem(from).alias && d.vertex == elem(to).alias).then(|| {
                    format!(
                        "{} AND {}",
                        equal(en, &s.columns, &vars[from].name, &s.references),
                        equal(en, &d.columns, &vars[to].name, &d.references)
                    )
                })
            };
            let forward = matches!(l.direction, EdgeDirection::Right | EdgeDirection::Any)
                .then(|| way(l.left, l.right))
                .flatten();
            let backward = matches!(l.direction, EdgeDirection::Left | EdgeDirection::Any)
                .then(|| way(l.right, l.left))
                .flatten();
            match (forward, backward) {
                (None, None) => {
                    joins = false;
                    break;
                }
                (Some(c), None) | (None, Some(c)) => conds.push(c),
                // Either way, one row: an edge to itself is matched once.
                (Some(f), Some(b)) => conds.push(format!("(({f}) OR ({b}))")),
            }
        }
        if joins {
            let mut from = Vec::new();
            for (v, &p) in vars.iter().zip(&pick) {
                let e = &g.elements[v.candidates[p]];
                let props = if v.edge { &edge_props } else { &vertex_props };
                from.push(format!("{} AS {}", derived(g, e, props), ident(&v.name)));
            }
            for w in 0..walks.len() {
                if links.iter().any(|l| l.walk == Some(w)) {
                    from.push(ident(&hidden(&format!("w{w}"))));
                }
            }
            for v in &vars {
                for f in &v.filters {
                    conds.push(format!("({f})"));
                }
            }
            if let Some(f) = &filter {
                conds.push(format!("({f})"));
            }
            let cols: Vec<String> = column_texts
                .iter()
                .zip(&outputs)
                .map(|(sql, n)| format!("{sql} AS {n}"))
                .collect();
            let mut s = format!("SELECT {} FROM {}", cols.join(", "), from.join(", "));
            if !conds.is_empty() {
                s.push_str(&format!(" WHERE {}", conds.join(" AND ")));
            }
            selects.push(s);
        }
        // The next choice of tables.
        let mut v = 0;
        while v < vars.len() {
            pick[v] += 1;
            if pick[v] < vars[v].candidates.len() {
                break;
            }
            pick[v] = 0;
            v += 1;
        }
        more = v < vars.len();
    }
    if selects.is_empty() {
        // Nothing can match: the columns, no rows.
        let cols: Vec<String> = column_texts
            .iter()
            .zip(&outputs)
            .map(|(sql, n)| format!("{sql} AS {n}"))
            .collect();
        let mut from = Vec::new();
        for v in &vars {
            if let Some(&i) = v.candidates.first() {
                let props = if v.edge { &edge_props } else { &vertex_props };
                from.push(format!(
                    "{} AS {}",
                    derived(g, &g.elements[i], props),
                    ident(&v.name)
                ));
            } else {
                // No table: its properties, all NULL.
                let props = if v.edge { &edge_props } else { &vertex_props };
                let mut c: Vec<String> = props
                    .iter()
                    .map(|n| format!("NULL AS {}", ident(n)))
                    .collect();
                if c.is_empty() {
                    c.push("1".into());
                }
                from.push(format!("(SELECT {}) AS {}", c.join(", "), ident(&v.name)));
            }
        }
        let paths_null: Vec<(String, String)> = paths
            .iter()
            .map(|(p, _)| (p.clone(), "NULL::bigint".into()))
            .collect();
        let cols: Vec<String> = if paths.is_empty() {
            cols
        } else {
            gt.columns
                .iter()
                .zip(&outputs)
                .map(|((sql, _), n)| Ok(format!("{} AS {n}", path_lengths(sql, &paths_null)?)))
                .collect::<Result<_>>()?
        };
        return Ok(format!(
            "SELECT {} FROM {} WHERE false",
            cols.join(", "),
            from.join(", ")
        ));
    }
    // Each quantified edge's walks: a recursive query joining the edge
    // tables themselves (their indexes then serve each step).
    let mut ctes = Vec::new();
    for (w, walk) in walks.iter().enumerate() {
        let Some((vertex, refs)) = &walk_vertex[w] else {
            continue;
        };
        if !links.iter().any(|l| l.walk == Some(w)) {
            continue;
        }
        let n = refs.len();
        let col = |p: &str, j: usize| ident(&hidden(&format!("{p}{j}")));
        let s: Vec<String> = (0..n).map(|j| col("s", j)).collect();
        let e: Vec<String> = (0..n).map(|j| col("e", j)).collect();
        let len = ident(&hidden("len"));
        let walk_name = ident(&hidden(&format!("w{w}")));
        let wa = ident(&hidden("w"));
        let ev = if walk.var.is_empty() {
            hidden("e")
        } else {
            walk.var.clone()
        };
        // Where it starts: its first vertex's own conditions, when they
        // name it alone, start it there.
        let lv = &vars[walk.left];
        let own: Vec<String> = lv
            .filters
            .iter()
            .filter(|f| qualifiers(f).is_ok_and(|q| q.iter().all(|q| *q == lv.name)))
            .map(|f| format!("({f})"))
            .collect();
        let vertex_el = g
            .elements
            .iter()
            .find(|el| !el.edge && el.alias == *vertex)
            .ok_or_else(|| Error::internal("a walk's vertex table"))?;
        let start = format!(
            "{} AS {}",
            derived(g, vertex_el, &vertex_props),
            ident(&lv.name)
        );
        let lref: Vec<String> = refs
            .iter()
            .map(|r| format!("{}.{}", ident(&lv.name), ident(&hidden(r))))
            .collect();
        let filters: Vec<String> = walk.filters.iter().map(|f| format!("({f})")).collect();
        // Below the least length of ANY SHORTEST each length is its own
        // key (its walks go on); from it on, the fewest edges per pair.
        let phased = walk.shortest && walk.min >= 2;
        let m = walk.min;
        let mut bases = Vec::new();
        let mut steps = Vec::new();
        if walk.min == 0 {
            let mut q = format!(
                "SELECT {}, {}, 0 FROM {start}",
                lref.join(", "),
                lref.join(", ")
            );
            if !own.is_empty() {
                q.push_str(&format!(" WHERE {}", own.join(" AND ")));
            }
            bases.push(q);
        }
        for &i in &walk.candidates {
            let el = &g.elements[i];
            let (Some(src), Some(dst)) = (&el.source, &el.destination) else {
                return Err(Error::internal("an edge without ends"));
            };
            let side = |cols: &[String]| -> Vec<String> {
                cols.iter()
                    .map(|c| format!("{}.{}", ident(&ev), ident(&hidden(c))))
                    .collect()
            };
            let mut ways = Vec::new();
            if matches!(walk.direction, EdgeDirection::Right | EdgeDirection::Any) {
                ways.push((side(&src.columns), side(&dst.columns)));
            }
            if matches!(walk.direction, EdgeDirection::Left | EdgeDirection::Any) {
                ways.push((side(&dst.columns), side(&src.columns)));
            }
            let edge = format!("{} AS {}", derived(g, el, &edge_props), ident(&ev));
            for (from, to) in ways {
                if walk.min > 0 {
                    let mut q = format!(
                        "SELECT {}, {}, 1{} FROM {edge}",
                        from.join(", "),
                        to.join(", "),
                        if phased { ", 1" } else { "" }
                    );
                    let mut conds = Vec::new();
                    if !own.is_empty() {
                        q.push_str(&format!(", {start}"));
                        conds.extend(from.iter().zip(&lref).map(|(a, b)| format!("{a} = {b}")));
                        conds.extend(own.iter().cloned());
                    }
                    conds.extend(filters.iter().cloned());
                    if !conds.is_empty() {
                        q.push_str(&format!(" WHERE {}", conds.join(" AND ")));
                    }
                    bases.push(q);
                }
                let mut on: Vec<String> = from
                    .iter()
                    .zip(&e)
                    .map(|(a, b)| format!("{a} = {wa}.{b}"))
                    .collect();
                on.extend(filters.iter().cloned());
                let ws: Vec<String> = s.iter().map(|c| format!("{wa}.{c}")).collect();
                let phase = if phased {
                    format!(", CASE WHEN {wa}.{len} + 1 < {m} THEN {wa}.{len} + 1 ELSE {m} END")
                } else {
                    String::new()
                };
                let mut q = format!(
                    "SELECT {}, {}, {wa}.{len} + 1{phase} FROM {walk_name} AS {wa} JOIN {edge} ON {}",
                    ws.join(", "),
                    to.join(", "),
                    on.join(" AND ")
                );
                if let Some(max) = walk.max {
                    q.push_str(&format!(" WHERE {wa}.{len} < {max}"));
                }
                steps.push(q);
            }
        }
        let base = if bases.len() == 1 {
            bases.remove(0)
        } else {
            format!("({})", bases.join(" UNION ALL "))
        };
        let step = format!("({})", steps.join(" UNION ALL "));
        let names = s.iter().chain(&e).cloned().collect::<Vec<_>>().join(", ");
        ctes.push(if phased {
            let phase = ident(&hidden("phase"));
            format!(
                "{walk_name} ({names}, {len}, {phase}) USING KEY ({names}, {phase}, min({len})) AS ({base} UNION {step})"
            )
        } else if walk.shortest {
            format!(
                "{walk_name} ({names}, {len}) USING KEY ({names}, min({len})) AS ({base} UNION {step})"
            )
        } else {
            format!("{walk_name} ({names}, {len}) AS ({base} UNION ALL {step})")
        });
    }
    let body = selects.join(" UNION ALL ");
    if ctes.is_empty() {
        Ok(body)
    } else {
        Ok(format!("WITH RECURSIVE {} {body}", ctes.join(", ")))
    }
}

/// Every property name of a kind: a table without one reads NULL.
fn all_props(g: &Graph, edge: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in g.elements.iter().filter(|e| e.edge == edge) {
        for (n, _) in e.properties() {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

fn label_names(l: &LabelExpr, out: &mut Vec<String>) {
    match l {
        LabelExpr::Name(n) => out.push(n.clone()),
        LabelExpr::Any => {}
        LabelExpr::Not(x) => label_names(x, out),
        LabelExpr::And(a, b) | LabelExpr::Or(a, b) => {
            label_names(a, out);
            label_names(b, out);
        }
    }
}

/// Whether element `e`'s labels satisfy `l`.
fn matches(l: &LabelExpr, e: &Element) -> bool {
    match l {
        LabelExpr::Name(n) => e.has_label(n),
        LabelExpr::Any => !e.labels.is_empty(),
        LabelExpr::Not(x) => !matches(x, e),
        LabelExpr::And(a, b) => matches(a, e) && matches(b, e),
        LabelExpr::Or(a, b) => matches(a, e) || matches(b, e),
    }
}
