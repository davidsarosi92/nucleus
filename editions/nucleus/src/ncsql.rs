//! `ncsql`: the nucleus SQL shell (the same as `nucleus sql`).

#[path = "keys.rs"]
#[allow(dead_code)] // `nucleus keys` is `nucleus`'s.
mod keys;
#[path = "sql.rs"]
mod sql;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match sql::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ncsql: {e}");
            ExitCode::FAILURE
        }
    }
}
