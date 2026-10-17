//! The `nucleus` command-line tool.

mod kv;
mod server;
mod sql;

use std::process::ExitCode;

const USAGE: &str = "\
usage:
  nucleus --version
  nucleus kv <dir> get <key>
  nucleus kv <dir> put <key> <value>
  nucleus kv <dir> delete <key>
  nucleus kv <dir> scan [<prefix>]
  nucleus kv <dir> checkpoint | vacuum
  nucleus kv <dir>                  interactive shell (type `help`)
  nucleus sql [<dir>] [-h <host[:port]>] [-U <user>] [-d <db>] [-c <sql>]
                                    SQL shell (also `ncsql`): a local database or a
                                    server; a script on stdin, or one command
  nucleus server <dir> [--listen <addr:port>] [--password <pw>] [--log-queries]
                                    PostgreSQL-compatible server (default 127.0.0.1:5432)

Keys and values are text; write any byte as \\xNN (and a backslash as \\\\).
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("nucleus {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("kv") => kv::run(&args[1..]),
        Some("sql") => sql::run(&args[1..]),
        Some("server") => server::run(&args[1..]),
        Some("help" | "--help" | "-h") => {
            print!("{USAGE}");
            Ok(())
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nucleus: {e}");
            ExitCode::FAILURE
        }
    }
}
