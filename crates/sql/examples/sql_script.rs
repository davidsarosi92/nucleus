//! Runs a SQL script against a fresh in-memory database and prints every
//! result — handy for looking at plans and outputs.
//!
//!     cargo run -p nucleus-sql --example sql_script -- script.sql

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: sql_script <file.sql>");
    let sql = std::fs::read_to_string(path).unwrap();
    let db = Database::open_on(
        Arc::new(SimPlatform::new(1)),
        Path::new("/db"),
        Options::default(),
    )
    .unwrap();
    let mut s = Session::new(Arc::new(db));
    for stmt in nucleus_sql::parser::parse(&sql).unwrap() {
        match s.run(&stmt, &[]) {
            Ok(Output::Command(tag)) => println!("{tag}"),
            Ok(Output::Rows { columns, rows }) => {
                println!(
                    "{}",
                    columns
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(" | ")
                );
                for r in rows {
                    println!(
                        "{}",
                        r.iter().map(|v| v.render()).collect::<Vec<_>>().join(" | ")
                    );
                }
            }
            Err(e) => println!("ERROR {e}"),
        }
        println!();
    }
}
