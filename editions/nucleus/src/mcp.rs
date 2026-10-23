//! `nucleus mcp`: the Model Context Protocol server (the `mcp` module)
//! over standard input and output, for a local database directory or a
//! server — the connection options are `nucleus sql`'s.

use std::io::{BufReader, Write};

use crate::sql::{Backend, Failure, Res, Target};

type Result<T> = std::result::Result<T, String>;

impl nucleus_mcp::Database for Backend {
    fn run(
        &mut self,
        sql: &str,
        read_only: bool,
    ) -> Vec<std::result::Result<nucleus_mcp::Answer, nucleus_mcp::Failure>> {
        let failure = |code: &str, message: String, hint: Option<String>| nucleus_mcp::Failure {
            code: code.into(),
            message,
            hint,
        };
        if read_only {
            // Every statement is a read, or nothing runs.
            match nucleus_sql::parser::parse(sql) {
                Err(e) => return vec![Err(failure(e.code, e.message, e.hint))],
                Ok(stmts) => {
                    if !stmts.iter().all(nucleus_sql::ext::read_only) {
                        return vec![Err(failure(
                            "25006",
                            "this tool runs reads only (queries, EXPLAIN, SHOW, ANALYZE QUERY)"
                                .into(),
                            Some(
                                "Writes go through the execute tool, which `nucleus mcp \
                                 --allow-writes` offers."
                                    .into(),
                            ),
                        ))];
                    }
                }
            }
        }
        match self.execute(sql) {
            Err(lost) => vec![Err(failure("08006", lost, None))],
            Ok(results) => results
                .into_iter()
                .map(|r| match r {
                    Ok(Res::Rows(t)) => Ok(nucleus_mcp::Answer::Rows {
                        columns: t.columns,
                        types: t.types,
                        rows: t.rows,
                    }),
                    Ok(Res::Command(tag)) => Ok(nucleus_mcp::Answer::Command(tag)),
                    Err(Failure {
                        code,
                        message,
                        hint,
                    }) => Err(failure(&code, message, hint)),
                })
                .collect(),
        }
    }
}

const USAGE: &str = "\
usage: nucleus mcp [<dir>] [connection options of nucleus sql] [--allow-writes]
                   [--max-rows <n>] [--timeout <ms>]
A Model Context Protocol server on standard input and output: an MCP client
(an AI assistant, an IDE) lists, describes, samples and queries the database.
Reads run in read-only transactions; --allow-writes adds a tool that writes.
--max-rows: the most rows a tool returns (default 100); --timeout: each
statement's limit in milliseconds (default 5000, 0: none).
";

pub fn run(args: &[String]) -> Result<()> {
    let mut target = Target::new("nucleus mcp");
    let mut options = nucleus_mcp::Options::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{a} needs {what}"));
        if target.option(a, &mut value)? {
            continue;
        }
        match a.as_str() {
            "--allow-writes" => options.allow_writes = true,
            "--max-rows" => {
                options.max_rows = value("a number")?
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or("--max-rows needs a positive number")?;
            }
            "--timeout" => {
                options.timeout_ms = value("milliseconds")?
                    .parse()
                    .map_err(|_| "--timeout needs milliseconds")?;
            }
            "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            s => return Err(format!("unknown option {s}\n{USAGE}")),
        }
    }
    let mut backend = target.open()?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let served = nucleus_mcp::Server::new(&mut backend, options)
        .serve(BufReader::new(stdin.lock()), stdout.lock())
        .map_err(|e| e.to_string());
    let _ = std::io::stdout().flush();
    let closed = backend.close();
    served.and(closed)
}
