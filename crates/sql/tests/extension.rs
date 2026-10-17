//! A module from outside the SQL crate adds a function, an aggregate, a
//! table function and a catalog view through the extension API alone
//! (PLAN.md principle 12), and a session with it uses them like the
//! built-ins.

use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::ext::*;
use nucleus_sql::{Output, Session};

/// `clamp(x, lo, hi)` over integers.
struct Clamp;

impl ScalarFunction for Clamp {
    fn name(&self) -> &str {
        "clamp"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        3..=3
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        if let Some(t) = args
            .iter()
            .find_map(|a| a.ty.filter(|t| *t != DataType::Int))
        {
            return Err(Error::undefined_function(format!(
                "function clamp({t}, …) does not exist"
            )));
        }
        Ok(Signature::new(
            vec![Arg::Untyped(DataType::Int); 3],
            Returns::Type(Some(DataType::Int)),
        ))
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        match args {
            [Value::Int(x), Value::Int(lo), Value::Int(hi)] => Ok(Value::Int((*x).clamp(*lo, *hi))),
            _ => Err(Error::internal("clamp of non-integers")),
        }
    }
}

/// `product(x)` of integers.
struct Product;

struct ProductAcc(Option<i64>);

impl Accumulator for ProductAcc {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        if let Value::Int(i) = args[0] {
            self.0 = Some(self.0.unwrap_or(1) * i);
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<Value> {
        Ok(self.0.map_or(Value::Null, Value::Int))
    }
}

impl Aggregate for Product {
    fn name(&self) -> &str {
        "product"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::keep(1, Some(DataType::Int)))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(ProductAcc(None))
    }
}

/// `countdown(n)`: n, n − 1, …, 1.
struct Countdown;

impl TableFunction for Countdown {
    fn name(&self) -> &str {
        "countdown"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<TableSignature> {
        Ok(TableSignature {
            args: vec![Arg::Untyped(DataType::Int)],
            columns: vec![("countdown".into(), Some(DataType::Int))],
        })
    }
    fn rows(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Vec<Row>> {
        let Value::Int(n) = args[0] else {
            return Ok(Vec::new());
        };
        Ok((1..=n).rev().map(|i| vec![Value::Int(i)]).collect())
    }
}

/// `sample.modules`: the session's modules.
struct Modules;

impl CatalogView for Modules {
    fn schema(&self) -> &str {
        "sample"
    }
    fn name(&self) -> &str {
        "modules"
    }
    fn columns(&self) -> Vec<(String, DataType)> {
        vec![("name".into(), DataType::Text)]
    }
    fn rows(&self, cx: &CallContext<'_>) -> Result<Vec<Row>> {
        Ok(cx
            .registry()
            .modules()
            .iter()
            .map(|m| vec![Value::Text(m.clone())])
            .collect())
    }
}

struct Sample;

impl Module for Sample {
    fn name(&self) -> &str {
        "sample"
    }
    fn register(&self, r: &mut Registry) -> Result<()> {
        r.add_function(&["clamp"], Arc::new(Clamp))?;
        r.add_aggregate(&["product"], Arc::new(Product))?;
        r.add_table_function(Arc::new(Countdown))?;
        r.add_schema("sample", "the sample module", false)?;
        r.add_view(Arc::new(Modules))
    }
}

fn session(registry: Arc<Registry>) -> Session {
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    Session::with_registry(db, registry)
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match s.execute(sql).unwrap().pop() {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_module_adds_functions_and_views() {
    let mut registry = Registry::builtin();
    registry.add(&Sample).unwrap();
    let mut s = session(Arc::new(registry));
    s.execute("CREATE TABLE t (a INT)").unwrap();
    s.execute("INSERT INTO t VALUES (2), (3), (NULL), (7)")
        .unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT clamp(a, 3, 5) FROM t WHERE a IS NOT NULL ORDER BY a"
        ),
        [["3"], ["3"], ["5"]]
    );
    assert_eq!(rows(&mut s, "SELECT product(a) FROM t"), [["42"]]);
    assert_eq!(
        rows(&mut s, "SELECT product(DISTINCT a % 2) FROM t WHERE a > 2"),
        [["1"]]
    );
    assert_eq!(
        rows(&mut s, "SELECT * FROM countdown(3) WITH ORDINALITY"),
        [["3", "1"], ["2", "2"], ["1", "3"]]
    );
    assert_eq!(
        rows(&mut s, "SELECT name FROM sample.modules"),
        [["builtin"], ["sample"]]
    );
    assert!(
        rows(&mut s, "SELECT name FROM nc_catalog.schemas")
            .iter()
            .any(|r| r[0] == "sample")
    );
    // Typed at bind time: a wrong argument type is refused before any row.
    assert_eq!(
        s.execute("SELECT clamp('x'::text, 1, 2)").unwrap_err().code,
        "42883"
    );
    // EXPLAIN shows them by name.
    let plan = rows(&mut s, "EXPLAIN SELECT product(a) FROM countdown(4) c(a)");
    let plan: Vec<&str> = plan.iter().map(|r| r[0].as_str()).collect();
    assert!(plan.iter().any(|l| l.contains("product(#0)")), "{plan:?}");
    assert!(plan.iter().any(|l| l.contains("countdown(4)")), "{plan:?}");
}

#[test]
fn without_the_module_there_is_none_of_it() {
    let mut s = session(Registry::shared_builtin());
    for sql in [
        "SELECT clamp(1, 2, 3)",
        "SELECT product(1)",
        "SELECT * FROM countdown(3)",
        "SELECT * FROM sample.modules",
    ] {
        assert!(s.execute(sql).is_err(), "{sql}");
    }
}

#[test]
fn registration_conflicts_are_refused_and_leave_the_registry_as_it_was() {
    let mut registry = Registry::builtin();
    registry.add(&Sample).unwrap();
    // The same module twice.
    assert_eq!(registry.add(&Sample).unwrap_err().code, "42710");
    // A name a built-in has (an aggregate and a function share one).
    struct Shadow;
    impl Module for Shadow {
        fn name(&self) -> &str {
            "shadow"
        }
        fn register(&self, r: &mut Registry) -> Result<()> {
            r.add_function(&["fresh"], Arc::new(Clamp))?;
            r.add_function(&["count"], Arc::new(Clamp))
        }
    }
    assert_eq!(registry.add(&Shadow).unwrap_err().code, "42710");
    assert!(registry.function("fresh").is_none());
    assert_eq!(registry.modules(), ["builtin", "sample"]);
    // A view in a schema nobody registered.
    struct Stray;
    impl Module for Stray {
        fn name(&self) -> &str {
            "stray"
        }
        fn register(&self, r: &mut Registry) -> Result<()> {
            struct V;
            impl CatalogView for V {
                fn schema(&self) -> &str {
                    "nowhere"
                }
                fn name(&self) -> &str {
                    "v"
                }
                fn columns(&self) -> Vec<(String, DataType)> {
                    Vec::new()
                }
                fn rows(&self, _: &CallContext<'_>) -> Result<Vec<Row>> {
                    Ok(Vec::new())
                }
            }
            r.add_view(Arc::new(V))
        }
    }
    assert_eq!(registry.add(&Stray).unwrap_err().code, "3F000");
}
