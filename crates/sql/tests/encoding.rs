//! PLAN.md §M7 "done when": the key encoding round-trips and preserves
//! order, property-tested.

use std::cmp::Ordering;

use nucleus_sql::Value;
use nucleus_sql::encoding::{decode_key, decode_row, encode_key, encode_row};
use proptest::prelude::*;

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int),
        any::<f64>().prop_map(Value::Float),
        "[\\x00-\\x7f\u{e9}\u{1F600}]{0,12}".prop_map(Value::Text),
    ]
}

/// Values of one type (or NULL) — what a key column holds.
fn column(kind: u8) -> BoxedStrategy<Value> {
    let v = match kind {
        0 => any::<bool>().prop_map(Value::Bool).boxed(),
        1 => any::<i64>().prop_map(Value::Int).boxed(),
        2 => any::<f64>().prop_map(Value::Float).boxed(),
        _ => "[\\x00-\\x03a-c]{0,6}".prop_map(Value::Text).boxed(),
    };
    prop_oneof![1 => Just(Value::Null), 9 => v].boxed()
}

/// Key order: NULL first, then the value order.
fn key_cmp(a: &Value, b: &Value) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.sort_cmp(b),
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => {
            x.to_bits() == y.to_bits() || (x == y) || (x.is_nan() && y.is_nan())
        }
        _ => a == b,
    }
}

proptest! {
    #[test]
    fn single_columns_keep_order(kind in 0u8..4, seed in any::<u64>()) {
        let mut runner = proptest::test_runner::TestRunner::new_with_rng(
            Default::default(),
            proptest::test_runner::TestRng::from_seed(proptest::test_runner::RngAlgorithm::ChaCha, &{
                let mut s = [0u8; 32];
                s[..8].copy_from_slice(&seed.to_le_bytes());
                s
            }),
        );
        let strat = column(kind);
        for _ in 0..20 {
            let a = strat.new_tree(&mut runner).unwrap().current();
            let b = strat.new_tree(&mut runner).unwrap().current();
            let (ka, kb) = (encode_key(std::slice::from_ref(&a)), encode_key(std::slice::from_ref(&b)));
            prop_assert_eq!(ka.cmp(&kb), key_cmp(&a, &b), "{:?} vs {:?}", a, b);
        }
    }

    #[test]
    fn composite_keys_keep_order_and_round_trip(
        kinds in prop::collection::vec(0u8..4, 1..4),
        seed in any::<u64>(),
    ) {
        let mut runner = proptest::test_runner::TestRunner::new_with_rng(
            Default::default(),
            proptest::test_runner::TestRng::from_seed(proptest::test_runner::RngAlgorithm::ChaCha, &{
                let mut s = [0u8; 32];
                s[..8].copy_from_slice(&seed.to_le_bytes());
                s
            }),
        );
        let row = |r: &mut proptest::test_runner::TestRunner| -> Vec<Value> {
            kinds.iter().map(|k| column(*k).new_tree(r).unwrap().current()).collect()
        };
        for _ in 0..10 {
            let (a, b) = (row(&mut runner), row(&mut runner));
            let expect = a.iter().zip(&b).map(|(x, y)| key_cmp(x, y)).find(|o| *o != Ordering::Equal).unwrap_or(Ordering::Equal);
            let (ka, kb) = (encode_key(&a), encode_key(&b));
            prop_assert_eq!(ka.cmp(&kb), expect);
            let (back, used) = decode_key(&ka, a.len()).unwrap();
            prop_assert_eq!(used, ka.len());
            for (x, y) in back.iter().zip(&a) {
                prop_assert!(same(x, y) || (x.is_null() && y.is_null()), "{:?} -> {:?}", y, x);
            }
        }
    }

    #[test]
    fn rows_round_trip(row in prop::collection::vec(value(), 0..8)) {
        let back = decode_row(&encode_row(&row), row.len()).unwrap();
        prop_assert_eq!(back.len(), row.len());
        for (x, y) in back.iter().zip(&row) {
            prop_assert!(same(x, y) || (x.is_null() && y.is_null()));
        }
    }
}
