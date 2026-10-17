//! The nucleus test kit: what the SQL layer, the front ends and modules
//! test with (PLAN.md §M14.19 — shared test material is a package, so a
//! module's tests need no files outside its folder).
//!
//! - [`slt`]: the sqllogictest runner, over any [`slt::Connection`].
//! - [`laws`]: the type law kit — the laws every type's abilities must
//!   keep, checked property-based.
//! - [`core_scripts`]: the SQL layer's scripts, which every front end and
//!   edition runs too.

pub mod laws;
pub mod slt;

/// The SQL layer's sqllogictest scripts: (file name, text), in name order.
pub fn core_scripts() -> &'static [(&'static str, &'static str)] {
    include!(concat!(env!("OUT_DIR"), "/scripts.rs"))
}

/// Whether `name` is picked by `SLT=<part of a name>` (all are without it).
pub fn picked(name: &str) -> bool {
    std::env::var("SLT").map_or(true, |only| name.contains(only.as_str()))
}
