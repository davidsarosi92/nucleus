//! The `nucleus` command-line tool.
//!
//! Only `--version` for now: it exists so every build target links a real
//! binary from §M0 on.  The KV commands arrive with PLAN.md §M6.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("nucleus {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: nucleus --version");
            eprintln!("(the KV commands arrive with milestone M6, see PLAN.md)");
            ExitCode::from(2)
        }
    }
}
