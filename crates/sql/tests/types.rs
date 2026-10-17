//! Registered types (PLAN.md §M14.19): a module's type — `geopoint`, with
//! equality and a distance but no order and no key encoding — is stored,
//! read back, compared and grouped, and refused exactly where it lacks an
//! ability (`ORDER BY`, `<`, `min`, primary keys, indexes), with an error
//! naming the ability.

use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::ext::Volatility;
use nucleus_sql::ext::*;
use nucleus_sql::{Output, Session};

/// A point on the earth: latitude and longitude in degrees.  Canonical
/// bytes: the two `f64`s big-endian, `-0` as `0`.
struct GeoPoint;

/// The interned type, for building values.
fn geopoint() -> TypeRef {
    TypeRef::named("geopoint").expect("registered")
}

fn point(lat: f64, lon: f64) -> Value {
    let canon = |x: f64| if x == 0.0 { 0.0f64 } else { x };
    let mut b = canon(lat).to_be_bytes().to_vec();
    b.extend_from_slice(&canon(lon).to_be_bytes());
    Value::ext(geopoint(), b)
}

fn coords(v: &Value) -> (f64, f64) {
    let b = v.ext_bytes().expect("a geopoint");
    let f = |i: usize| f64::from_be_bytes(b[i..i + 8].try_into().unwrap());
    (f(0), f(8))
}

struct Haversine;

impl Distance for Haversine {
    fn operator(&self) -> &str {
        "<->"
    }
    fn distance(&self, a: &Value, b: &Value) -> Result<f64> {
        let ((la, oa), (lb, ob)) = (coords(a), coords(b));
        let (la, lb) = (la.to_radians(), lb.to_radians());
        let (dl, dlon) = (lb - la, (ob - oa).to_radians());
        let h = (dl / 2.0).sin().powi(2) + la.cos() * lb.cos() * (dlon / 2.0).sin().powi(2);
        Ok(2.0 * 6371.0 * h.sqrt().asin())
    }
}

impl Type for GeoPoint {
    fn name(&self) -> &str {
        "geopoint"
    }
    fn category(&self) -> &str {
        "geometry"
    }
    fn input(&self, text: &str) -> Result<Value> {
        let bad =
            || Error::invalid_text(format!("invalid input syntax for type geopoint: {text:?}"));
        let inner = text
            .trim()
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .ok_or_else(bad)?;
        let (a, b) = inner.split_once(',').ok_or_else(bad)?;
        let lat: f64 = a.trim().parse().map_err(|_| bad())?;
        let lon: f64 = b.trim().parse().map_err(|_| bad())?;
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            return Err(bad());
        }
        Ok(point(lat, lon))
    }
    fn output(&self, v: &Value) -> String {
        let (lat, lon) = coords(v);
        format!("({lat},{lon})")
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn distance(&self) -> Option<&dyn Distance> {
        Some(&Haversine)
    }
    fn examples(&self) -> Vec<Value> {
        vec![
            point(0.0, 0.0),
            point(-0.0, 180.0),
            point(90.0, -180.0),
            point(47.5, 19.04),
        ]
    }
}

/// `geo_distance(a, b)` in kilometres (the operator `<->` comes when
/// modules can teach the parser operators).
struct GeoDistance;

impl ScalarFunction for GeoDistance {
    fn name(&self) -> &str {
        "geo_distance"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=2
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        let t = DataType::Ext(geopoint());
        Ok(Signature::new(
            vec![Arg::Untyped(t), Arg::Untyped(t)],
            Returns::Type(Some(DataType::Float)),
        ))
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        Ok(Value::Float(Haversine.distance(&args[0], &args[1])?))
    }
}

/// `geohash(p)`: the point's geohash, 8 characters — nearby points share
/// prefixes, so it orders points along a space-filling curve.
struct GeoHash;

fn geohash(lat: f64, lon: f64) -> String {
    const BASE32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    let (mut lat_r, mut lon_r) = ((-90.0, 90.0), (-180.0, 180.0));
    let mut out = String::new();
    let (mut bits, mut ch, mut even) = (0, 0usize, true);
    while out.len() < 8 {
        let (r, v): (&mut (f64, f64), f64) = if even {
            (&mut lon_r, lon)
        } else {
            (&mut lat_r, lat)
        };
        let mid = (r.0 + r.1) / 2.0;
        ch <<= 1;
        if v >= mid {
            ch |= 1;
            r.0 = mid;
        } else {
            r.1 = mid;
        }
        even = !even;
        bits += 1;
        if bits == 5 {
            out.push(BASE32[ch] as char);
            (bits, ch) = (0, 0);
        }
    }
    out
}

impl ScalarFunction for GeoHash {
    fn name(&self) -> &str {
        "geohash"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Untyped(DataType::Ext(geopoint()))],
            Returns::Type(Some(DataType::Text)),
        ))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (lat, lon) = coords(&args[0]);
        Ok(Value::Text(geohash(lat, lon)))
    }
}

struct Geo;

impl Module for Geo {
    fn name(&self) -> &str {
        "geo"
    }
    fn register(&self, r: &mut Registry) -> Result<()> {
        r.add_type(Arc::new(GeoPoint))?;
        r.add_function(&["geohash"], Arc::new(GeoHash))?;
        r.add_function(&["geo_distance"], Arc::new(GeoDistance))
    }
}

fn registry() -> Arc<Registry> {
    let mut r = Registry::builtin();
    r.add(&Geo).unwrap();
    Arc::new(r)
}

fn open(p: &Arc<SimPlatform>) -> Arc<dyn Kv> {
    Arc::new(Database::open_on(p.clone(), Path::new("/db"), Options::default()).unwrap())
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(Value::render).collect())
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn error(s: &mut Session, sql: &str) -> Error {
    s.execute(sql).expect_err(sql)
}

fn setup(s: &mut Session) {
    s.execute("CREATE TABLE shops (id INT PRIMARY KEY, name TEXT, at geopoint)")
        .unwrap();
    s.execute(
        "INSERT INTO shops VALUES (1, 'Pest', '(47.4979,19.0402)'), \
         (2, 'Buda', geopoint '(47.4925,19.0513)'), (3, 'Pest again', '(47.4979,19.0402)'), \
         (4, 'nowhere', NULL)",
    )
    .unwrap();
}

#[test]
fn stored_read_compared_and_grouped() {
    let p = Arc::new(SimPlatform::new(1));
    let mut s = Session::with_registry(open(&p), registry());
    setup(&mut s);
    assert_eq!(
        rows(&mut s, "SELECT at FROM shops WHERE id = 2"),
        [["(47.4925,19.0513)"]]
    );
    // Equality without an order.
    assert_eq!(
        rows(
            &mut s,
            "SELECT id FROM shops WHERE at = '(47.4979,19.0402)'::geopoint ORDER BY id"
        ),
        [["1"], ["3"]]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT count(*) FROM shops WHERE at <> geopoint '(47.4979,19.0402)'"
        ),
        [["1"]]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT id FROM shops WHERE at IN ('(47.4925,19.0513)', '(0,0)') ORDER BY id"
        ),
        [["2"]]
    );
    // Grouping and DISTINCT: equality and hashing.
    assert_eq!(
        rows(
            &mut s,
            "SELECT at::text, count(*) FROM shops WHERE at IS NOT NULL GROUP BY at ORDER BY 2, 1"
        ),
        [["(47.4925,19.0513)", "1"], ["(47.4979,19.0402)", "2"]]
    );
    assert_eq!(
        rows(&mut s, "SELECT count(DISTINCT at) FROM shops"),
        [["2"]]
    );
    // A distance, through the module's function.
    assert_eq!(
        rows(
            &mut s,
            "SELECT id FROM shops WHERE at IS NOT NULL \
             ORDER BY geo_distance(at, '(47.4925,19.0513)') LIMIT 1"
        ),
        [["2"]]
    );
    // Read back after reopening: the catalog names the type.
    drop(s);
    let mut s = Session::with_registry(open(&p), registry());
    assert_eq!(
        rows(&mut s, "SELECT name, at FROM shops WHERE id = 1"),
        [["Pest", "(47.4979,19.0402)"]]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT type FROM nc_catalog.columns WHERE table_name = 'shops' AND name = 'at'"
        ),
        [["geopoint"]]
    );
}

#[test]
fn refused_where_an_ability_is_missing() {
    let p = Arc::new(SimPlatform::new(1));
    let mut s = Session::with_registry(open(&p), registry());
    setup(&mut s);
    for (sql, code, says) in [
        (
            "SELECT * FROM shops ORDER BY at",
            "42883",
            "no ordering, which ORDER BY needs",
        ),
        (
            "SELECT * FROM shops WHERE at < '(0,0)'",
            "42883",
            "no ordering, which < needs",
        ),
        (
            "SELECT min(at) FROM shops",
            "42883",
            "no ordering, which min needs",
        ),
        (
            "CREATE TABLE places (at geopoint PRIMARY KEY)",
            "42704",
            "no key encoding, which a primary key needs",
        ),
        (
            "CREATE INDEX shops_at ON shops (at)",
            "42704",
            "no key encoding, which an index needs",
        ),
        (
            "SELECT geopoint 'nowhere'",
            "22P02",
            "invalid input syntax for type geopoint",
        ),
    ] {
        let e = error(&mut s, sql);
        assert_eq!(e.code, code, "{sql}: {e}");
        assert!(e.message.contains(says), "{sql}: {e}");
    }
    // The ordering error points at what the type does have.
    let e = error(&mut s, "SELECT * FROM shops ORDER BY at");
    assert!(e.message.contains("it has a distance"), "{e}");
}

#[test]
fn only_sessions_whose_registry_has_it_can_name_it() {
    let p = Arc::new(SimPlatform::new(1));
    let mut s = Session::with_registry(open(&p), registry());
    setup(&mut s);
    drop(s);
    let mut s = Session::new(open(&p));
    assert_eq!(error(&mut s, "SELECT geopoint '(1,2)'").code, "42704");
    assert_eq!(error(&mut s, "CREATE TABLE t (a geopoint)").code, "42704");
    // Its stored values still read (the type is known to this process).
    assert_eq!(
        rows(
            &mut s,
            "SELECT id FROM shops WHERE at IS NOT NULL ORDER BY id"
        ),
        [["1"], ["2"], ["3"]]
    );
}

#[test]
fn listed_with_its_abilities() {
    let mut s = Session::with_registry(open(&Arc::new(SimPlatform::new(1))), registry());
    assert_eq!(
        rows(
            &mut s,
            "SELECT name, category, module, equality, hashing, ordering, key_encoding, distance \
             FROM nc_catalog.types WHERE name IN ('geopoint', 'bigint') ORDER BY name"
        ),
        [
            ["bigint", "number", "builtin", "t", "t", "t", "t", "NULL"],
            ["geopoint", "geometry", "geo", "t", "t", "f", "f", "<->"],
        ]
    );
}

#[test]
fn a_type_this_process_lacks_still_has_a_name() {
    let t = TypeRef::named_or_missing("never_registered_anywhere");
    assert_eq!(t.name(), "never_registered_anywhere");
    let e = t.imp().input("x").unwrap_err();
    assert!(e.message.contains("is not available"), "{e}");
    assert_eq!(Value::ext(t, vec![0xab, 0x01]).render(), "\\xab01");
    // The same name again is the same stand-in.
    assert_eq!(TypeRef::named_or_missing("never_registered_anywhere"), t);
}

#[test]
fn geopoint_keeps_the_laws() {
    let _ = registry();
    let mut generate = |g: &mut nucleus_testkit::laws::Gen| {
        let lat = (g.below(18001) as f64 - 9000.0) / 100.0;
        let lon = (g.below(36001) as f64 - 18000.0) / 100.0;
        point(if g.one_in(8) { 0.0 } else { lat }, lon)
    };
    let problems = nucleus_testkit::laws::check(DataType::Ext(geopoint()), &mut generate, 2000);
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Teaching geopoint an ordering in SQL (PLAN.md §M14.19 "done when"):
/// `ORDER BY … USING` and an index in it work; dropping it while the index
/// uses it is refused; redefining it rebuilds the index; `CASCADE` drops
/// the index with it.
#[test]
fn taught_an_ordering_by_a_mapping() {
    let p = Arc::new(SimPlatform::new(1));
    let kv = open(&p);
    let reg = registry();
    let mut s = Session::with_registry(kv.clone(), reg.clone());
    setup(&mut s);
    s.execute(
        "INSERT INTO shops VALUES (5, 'Tokyo', '(35.68,139.69)'), (6, 'Quito', '(-0.18,-78.47)')",
    )
    .unwrap();
    let consistent = |kv: &Arc<dyn Kv>| {
        let txn = kv.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
        let problems = nucleus_sql::check::check(&*txn, &reg).unwrap();
        assert!(problems.is_empty(), "{problems:#?}");
    };
    // Before: no order, nothing taught.
    assert!(
        !error(&mut s, "SELECT * FROM shops ORDER BY at")
            .message
            .contains("taught")
    );
    assert_eq!(
        error(&mut s, "SELECT * FROM shops ORDER BY at USING by_geohash").code,
        "42704"
    );
    // A mapping that may vary is refused.
    let e = error(
        &mut s,
        "ALTER TYPE geopoint ADD ORDERING bad BY geohash(value) || gen_random_uuid()::text",
    );
    assert!(e.message.contains("immutable"), "{e}");
    s.execute("ALTER TYPE geopoint ADD ORDERING by_geohash BY geohash(value)")
        .unwrap();
    let by_hash = rows(
        &mut s,
        "SELECT id FROM shops WHERE at IS NOT NULL ORDER BY at USING by_geohash, id",
    );
    let mut want: Vec<(String, String)> =
        rows(&mut s, "SELECT id, at FROM shops WHERE at IS NOT NULL")
            .into_iter()
            .map(|r| {
                let v = GeoPoint.input(&r[1]).unwrap();
                let (lat, lon) = coords(&v);
                (geohash(lat, lon), r[0].clone())
            })
            .collect();
    want.sort();
    assert_eq!(
        by_hash.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        want.iter().map(|(_, id)| id.clone()).collect::<Vec<_>>()
    );
    // Plain ORDER BY now names the taught ordering.
    let e = error(&mut s, "SELECT * FROM shops ORDER BY at");
    assert!(e.message.contains("USING by_geohash"), "{e}");
    // An index in it, kept up through writes.
    s.execute("CREATE INDEX ON shops (at USING by_geohash)")
        .unwrap();
    consistent(&kv);
    s.execute("INSERT INTO shops VALUES (7, 'Lima', '(-12.05,-77.04)')")
        .unwrap();
    s.execute("UPDATE shops SET at = '(51.5,-0.12)' WHERE id = 6")
        .unwrap();
    s.execute("DELETE FROM shops WHERE id = 1").unwrap();
    consistent(&kv);
    assert_eq!(
        rows(
            &mut s,
            "SELECT name, orderings FROM nc_catalog.indexes WHERE table_name = 'shops' AND NOT is_primary"
        ),
        [["shops_at_idx", "{by_geohash}"]]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT type, name, mapping, used_by FROM nc_catalog.orderings"
        ),
        [["geopoint", "by_geohash", "geohash(value)", "{shops_at_idx}"]]
    );
    // Dropping it while the index uses it is refused.
    let e = error(&mut s, "ALTER TYPE geopoint DROP ORDERING by_geohash");
    assert_eq!(e.code, "2BP01", "{e}");
    assert!(e.message.contains("shops_at_idx"), "{e}");
    // Redefining it rebuilds the index (check() holds it to the new mapping).
    s.execute("ALTER TYPE geopoint ALTER ORDERING by_geohash BY upper(geohash(value))")
        .unwrap();
    consistent(&kv);
    // CASCADE drops the index with it.
    s.execute("ALTER TYPE geopoint DROP ORDERING by_geohash CASCADE")
        .unwrap();
    assert!(
        rows(
            &mut s,
            "SELECT name FROM nc_catalog.indexes WHERE table_name = 'shops' AND NOT is_primary"
        )
        .is_empty()
    );
    assert!(rows(&mut s, "SELECT name FROM nc_catalog.orderings").is_empty());
    consistent(&kv);
}
