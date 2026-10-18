//! The type law kit (PLAN.md §M14.19, "Laws, checked for every type"):
//! the laws of `nucleus_sql::laws`, checked property-based on any type —
//! built-in or a module's — with values from a generator.
//!
//! ```ignore
//! let problems = nucleus_testkit::laws::check(ty, &mut |g| my_value(g), 2000);
//! assert!(problems.is_empty(), "{problems:#?}");
//! ```

use nucleus_sql::builtin::type_of;
use nucleus_sql::datetime::Interval;
use nucleus_sql::decimal::Decimal;
use nucleus_sql::laws::{Report, check_one, check_pair, check_triple};
use nucleus_sql::{DataType, Value};

/// A deterministic source of test values (SplitMix64).
pub struct Gen(u64);

impl Gen {
    /// A generator from a seed.
    pub fn new(seed: u64) -> Self {
        Gen(seed)
    }

    /// 64 random bits.
    pub fn u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.u64()) * u128::from(n)) >> 64) as u64
    }

    /// `true` once in `n` times.
    pub fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }

    /// One of `items`.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    /// `n` random bytes.
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.u64() as u8).collect()
    }
}

/// A generator of a type's values.
pub type Generator<'a> = dyn FnMut(&mut Gen) -> Value + 'a;

/// The laws `ty` breaks, for `cases` generated values (and pairs and
/// triples of them); empty when it keeps them all.
pub fn check(ty: DataType, generate: &mut Generator<'_>, cases: usize) -> Vec<String> {
    let t = type_of(ty);
    let mut g = Gen::new(seed_of(t.name()));
    let mut report = Report::new(t);
    let mut last: Vec<Value> = Vec::new();
    for _ in 0..cases {
        let a = generate(&mut g);
        // Equal values must be common, or equality's laws go untested:
        // b is often a again, or a through its text form.
        let b = match g.below(4) {
            0 => a.clone(),
            1 => t.input(&t.output(&a)).unwrap_or_else(|_| a.clone()),
            _ => generate(&mut g),
        };
        let c = if g.one_in(3) {
            b.clone()
        } else {
            last.get(g.below(last.len().max(1) as u64) as usize)
                .cloned()
                .unwrap_or_else(|| generate(&mut g))
        };
        check_one(t, &a, &mut report);
        check_pair(t, &a, &b, &mut report);
        check_pair(t, &b, &a, &mut report);
        check_triple(t, &a, &b, &c, &mut report);
        last.push(a);
        if last.len() > 64 {
            last.remove(0);
        }
    }
    report.problems
}

/// [`check`] with the kit's own generator, for the built-in types,
/// their arrays and `uuid`.
pub fn check_builtin(ty: DataType, cases: usize) -> Vec<String> {
    match generator(ty) {
        Some(mut make) => check(ty, &mut *make, cases),
        None => vec![format!("{}: the kit has no generator for it", ty.name())],
    }
}

fn seed_of(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

// ---------------------------------------------------------- generators

/// The kit's generator for a built-in type, an array of one, or `uuid`.
pub fn generator(ty: DataType) -> Option<Box<Generator<'static>>> {
    Some(match ty {
        DataType::Bool => Box::new(|g: &mut Gen| Value::Bool(g.one_in(2))),
        DataType::SmallInt | DataType::Integer | DataType::BigInt => {
            let (lo, hi) = ty.int_range().expect("an integer type");
            Box::new(move |g: &mut Gen| {
                Value::Int(match g.below(4) {
                    0 => *g.pick(&[0, 1, -1, lo, hi, lo + 1, hi - 1]),
                    1 => g.below(1000) as i64 - 500,
                    // Anywhere in the range.
                    _ => {
                        (i128::from(lo)
                            + (u128::from(g.u64()) % (i128::from(hi) - i128::from(lo) + 1) as u128)
                                as i128) as i64
                    }
                })
            })
        }
        DataType::Float => Box::new(|g: &mut Gen| {
            Value::Float(match g.below(4) {
                0 => *g.pick(&[
                    0.0,
                    -0.0,
                    1.0,
                    -1.5,
                    f64::NAN,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::MIN_POSITIVE,
                    f64::MAX,
                    5e-324,
                    0.1,
                ]),
                1 => (g.below(2000) as f64 - 1000.0) / 8.0,
                _ => f64::from_bits(g.u64()),
            })
        }),
        DataType::Text => Box::new(|g: &mut Gen| {
            let alphabet = [
                'a', 'b', 'Z', ' ', ',', '"', '\\', '{', '}', 'é', '😀', '\u{1}', 'Ω',
            ];
            let n = g.below(8) as usize;
            Value::Text((0..n).map(|_| *g.pick(&alphabet)).collect())
        }),
        DataType::Numeric => Box::new(|g: &mut Gen| {
            let m =
                (i128::from(g.u64() as i64) * i128::from(g.below(1 << 40) as i64)) % 10i128.pow(30);
            let m = if g.one_in(3) { m % 1000 } else { m };
            Value::Numeric(Decimal::new(m, g.below(20) as u32).expect("in range"))
        }),
        DataType::Date => {
            Box::new(|g: &mut Gen| Value::Date(g.below(2_451_000 + 1_000_000) as i32 - 2_451_000))
        }
        DataType::Timestamp | DataType::TimestampTz => Box::new(move |g: &mut Gen| {
            let us = g.below(1u64 << 58) as i64 - (1i64 << 56);
            if ty == DataType::Timestamp {
                Value::Timestamp(us)
            } else {
                Value::TimestampTz(us)
            }
        }),
        DataType::Interval => Box::new(|g: &mut Gen| {
            Value::Interval(Interval {
                months: g.below(2000) as i32 - 1000,
                days: g.below(20000) as i32 - 10000,
                micros: g.below(1u64 << 40) as i64 - (1i64 << 39),
            })
        }),
        DataType::Array(e) => {
            let mut elem = generator(*e)?;
            Box::new(move |g: &mut Gen| {
                let n = g.below(4) as usize;
                Value::Array(
                    (0..n)
                        .map(|_| if g.one_in(4) { Value::Null } else { elem(g) })
                        .collect(),
                )
            })
        }
        DataType::Ext(t) if t.name() == "uuid" => Box::new(move |g: &mut Gen| {
            let mut b = g.bytes(16);
            if g.one_in(4) {
                b.iter_mut().skip(1).for_each(|x| *x = 0);
            }
            Value::ext(t, b)
        }),
        DataType::Ext(_) => return None,
    })
}
