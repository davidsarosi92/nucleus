//! `geo`: a sample nucleus module — a type, a function and a catalog
//! view, added through nucleus's extension API alone (`nucleus_sql::ext`),
//! without touching the core.  See README.md for how it is built.
//!
//! - **`geopoint`** — a point on the earth, `'(lat,lon)'` in degrees.  It
//!   has equality (and hashing) and a distance, `<->`, in kilometres; no
//!   order and no key encoding, so `ORDER BY at` and a primary key on it
//!   are refused — `ORDER BY at <-> '(47.5,19.04)'` sorts nearest first.
//! - **`geohash(p)`** — the point's geohash, immutable, so it can define an
//!   ordering: `ALTER TYPE geopoint ADD ORDERING by_geohash BY
//!   geohash(value)`.
//! - **`geo.cities`** — a catalog view: a few cities and where they are.

use std::ops::RangeInclusive;
use std::sync::Arc;

use nucleus_sql::ext::*;

/// The module: register it in a session's registry.
///
/// ```
/// let mut registry = nucleus_sql::ext::Registry::builtin();
/// registry.add(&nucleus_example_extension::Geo).unwrap();
/// ```
pub struct Geo;

impl Module for Geo {
    fn name(&self) -> &str {
        "geo"
    }

    fn register(&self, r: &mut Registry) -> Result<()> {
        r.add_type(Arc::new(GeoPoint))?;
        r.add_function(&["geohash"], Arc::new(GeoHash))?;
        r.add_schema("geo", "the geo module's views", false)?;
        r.add_view(Arc::new(Cities))
    }
}

/// The interned `geopoint` type (once [`Geo`] is registered).
pub fn geopoint() -> TypeRef {
    TypeRef::named("geopoint").expect("the geo module is registered")
}

/// A `geopoint` value.  Canonical bytes: latitude and longitude as `f64`
/// big-endian, `-0` as `0` — equal points, equal bytes.
pub fn point(lat: f64, lon: f64) -> Value {
    let canon = |x: f64| if x == 0.0 { 0.0f64 } else { x };
    let mut b = canon(lat).to_be_bytes().to_vec();
    b.extend_from_slice(&canon(lon).to_be_bytes());
    Value::ext(geopoint(), b)
}

/// A `geopoint`'s latitude and longitude.
pub fn coords(v: &Value) -> (f64, f64) {
    let b = v.ext_bytes().expect("a geopoint");
    let f = |i: usize| f64::from_be_bytes(b[i..i + 8].try_into().expect("16 bytes"));
    (f(0), f(8))
}

/// The type.
pub struct GeoPoint;

/// The great-circle distance, in kilometres.
pub struct Haversine;

impl Distance for Haversine {
    fn operator(&self) -> &str {
        "<->"
    }

    fn distance(&self, a: &Value, b: &Value) -> Result<f64> {
        let ((la, oa), (lb, ob)) = (coords(a), coords(b));
        let (la, lb) = (la.to_radians(), lb.to_radians());
        let (dlat, dlon) = (lb - la, (ob - oa).to_radians());
        let h = (dlat / 2.0).sin().powi(2) + la.cos() * lb.cos() * (dlon / 2.0).sin().powi(2);
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

    /// Registration checks the laws on these.
    fn examples(&self) -> Vec<Value> {
        vec![
            point(0.0, 0.0),
            point(-90.0, 180.0),
            point(47.4979, 19.0402),
        ]
    }
}

/// `geohash(p)`: 8 characters; nearby points share prefixes.
pub struct GeoHash;

/// The geohash of a point.
pub fn geohash(lat: f64, lon: f64) -> String {
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

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let t = DataType::Ext(geopoint());
        // A string literal takes the type; anything else typed must be one.
        if let Some(other) = args[0].ty.filter(|a| *a != t && !args[0].text_literal) {
            return Err(Error::undefined_function(format!(
                "function geohash({other}) does not exist"
            )));
        }
        Ok(Signature::new(
            vec![Arg::Untyped(t)],
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

/// `geo.cities`: a few cities.
pub struct Cities;

const CITIES: &[(&str, f64, f64)] = &[
    ("Budapest", 47.4979, 19.0402),
    ("Vienna", 48.2082, 16.3738),
    ("Tokyo", 35.6762, 139.6503),
    ("Quito", -0.1807, -78.4678),
    ("Sydney", -33.8688, 151.2093),
];

impl CatalogView for Cities {
    fn schema(&self) -> &str {
        "geo"
    }

    fn name(&self) -> &str {
        "cities"
    }

    fn columns(&self) -> Vec<(String, DataType)> {
        vec![
            ("name".into(), DataType::Text),
            ("at".into(), DataType::Ext(geopoint())),
        ]
    }

    fn rows(&self, _: &CallContext<'_>) -> Result<Vec<Row>> {
        Ok(CITIES
            .iter()
            .map(|(n, lat, lon)| vec![Value::Text((*n).into()), point(*lat, *lon)])
            .collect())
    }
}
