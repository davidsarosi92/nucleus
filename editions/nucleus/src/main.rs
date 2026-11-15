//! The `nucleus` command-line tool.

mod branch;
mod codegen;
mod dump;
mod init;
mod kv;
mod mcp;
mod migrate;
mod restore;
mod server;
mod sql;
mod status;

use std::process::ExitCode;

const USAGE: &str = "\
usage:
  nucleus --version
  nucleus init <dir> [--user <name>] the database's first administrator (default
                                    admin), the password from NUCLEUS_PASSWORD
                                    or standard input
  nucleus kv <dir> get <key>
  nucleus kv <dir> put <key> <value>
  nucleus kv <dir> delete <key>
  nucleus kv <dir> scan [<prefix>]
  nucleus kv <dir> checkpoint | vacuum
  nucleus kv <dir>                  interactive shell (type `help`)
  nucleus sql [<dir> | :memory:] [-h <host[:port]>] [-U <user>] [-d <db>] [-c <sql>]
                                    SQL shell (also `ncsql`): a local database (or
                                    one in memory, gone at the end) or a server;
                                    a script on stdin, or one command
  nucleus migrate new <name> | status | up [--to <v>] | down [--to <v> | --all]
                  [<dir>] [connection options of nucleus sql] [--dir <migrations>]
                                    versioned migration files, each applied in
                                    one transaction with its record
  nucleus dump --schema [<dir>] [connection options of nucleus sql]
                                    the schema as the SQL that makes it
  nucleus codegen <ts|rust|python|java> [<dir>] [connection options of nucleus sql]
                  [--out <file>] [--tables <a,b> | --queries <file|dir>]
                  [--import <m>] [--package <p>] [--class <c>]
                                    a type per table for the language's driver,
                                    with its SELECT and a function reading a row;
                                    or a typed function per named query
  nucleus status [<dir>] [connection options of nucleus sql]
                                    a server's health for people: ready or not,
                                    sessions, transactions, statements and their
                                    latencies, the page cache, the files' sizes,
                                    refused logins (from nc_catalog.metrics)
  nucleus mcp [<dir>] [connection options of nucleus sql] [--allow-writes]
              [--memory] [--max-rows <n>] [--timeout <ms>]
                                    a Model Context Protocol server on stdin and
                                    stdout, for AI assistants: list, describe,
                                    sample and query (read-only unless
                                    --allow-writes); --memory: remember, recall,
                                    forget
  nucleus server <dir> [--listen <addr:port>] [--pg-compat [<addr:port>]] [--console [<addr:port>]]
                     [--front-end <name> [<addr:port>]] [--metrics [<addr:port>]] [--otlp <url>]
                     [--user <name>] [--password <pw> (or NUCLEUS_PASSWORD)] [--no-auth] [--log-queries]
                     [--tls-cert <pem file> --tls-key <pem file> [--tls-client-ca <pem file>]]
                     [--oidc-issuer <url> --oidc-audience <name> [--oidc-jwks <file>]
                      [--oidc-user-claim <claim>] [--oidc-role-claim <claim>]]
                     [--ldap-url <url> (--ldap-bind-dn <template with {user}> |
                      --ldap-search-base <dn> [--ldap-search-attribute <attr>]
                      [--ldap-search-dn <dn> --ldap-search-password <pw>])
                      [--ldap-starttls] [--ldap-ca <pem file>]]
                     [--trusted-proxy <cidr> …] [--max-connections <n>] [--insecure]
                     [--sync ordered|full] [--wal minimal|full] [--cache <size>]
                     [--index-cache <size>] [--secrets-key <file>] [--subject-keys <file>]
                     [--history <duration>] [--backup-key <file>]
                     [--backup-to <repository> --backup-every <duration>
                      [--backup-full-every <n>] [--backup-keep <n>]]
                                    the server: the nucleus protocol (default
                                    127.0.0.1:20035), and with --pg-compat (the
                                    front end `pg`) also PostgreSQL's (default
                                    127.0.0.1:5432), with --console a web console
                                    (default 127.0.0.1:20036; beyond loopback
                                    over TLS only), with --metrics Prometheus's
                                    GET /metrics and the health checks
                                    /health/live and /health/ready (default
                                    127.0.0.1:20037; beyond loopback over TLS
                                    unless --insecure), with --otlp the metrics
                                    pushed to an OpenTelemetry collector
                                    with a span per statement (OTLP/HTTP; or
                                    OTEL_EXPORTER_OTLP_ENDPOINT, _HEADERS,
                                    OTEL_METRIC_EXPORT_INTERVAL,
                                    OTEL_TRACES_SAMPLER[_ARG]);
                                    clients log in as
                                    nucleus / nucleus unless changed; with a
                                    certificate, TLS for clients that ask; with
                                    a client CA, a client certificate naming
                                    the user logs it in without a password; with
                                    an OIDC issuer, its identity tokens log in;
                                    with LDAP, the directory checks the passwords
                                    of roles without their own (over TLS).  Beyond
                                    loopback it refuses the default password of an
                                    unclaimed database and --no-auth unless --insecure;
                                    --sync full flushes the drive's cache at each
                                    commit (on macOS; the default, ordered, keeps
                                    writes in order without it); --wal full logs
                                    a new index's entries too (the default,
                                    minimal, checkpoints at its commit instead);
                                    --cache sets the page cache (default 128MB;
                                    PostgreSQL's shared_buffers); --index-cache
                                    the decoded index objects kept (HNSW graphs,
                                    text segments; default a quarter of the
                                    machine's memory); --secrets-key the file
                                    whose key seals secrets (CREATE SECRET;
                                    default <dir>.secrets-key, made when the
                                    first secret is); --subject-keys the file
                                    of the keys each subject's rows are sealed
                                    with in backups, destroyed by FORGET
                                    SUBJECT (default <dir>.subject-keys); --history keeps every
                                    snapshot of that long ago readable (7d,
                                    12h, '30 minutes'; AS OF TIMESTAMP;
                                    default none); --backup-to backs up
                                    each --backup-every (1d, 6h) to a
                                    repository (BACKUP TO's), each
                                    --backup-full-every-th backup (default
                                    7) a full one, the others incremental,
                                    the last --backup-keep full ones kept
                                    with those after them; --backup-key the
                                    file of the key (64 hex digits: openssl
                                    rand -hex 32; or NUCLEUS_BACKUP_KEY_FILE)
                                    backups are encrypted with
  nucleus branch <dir> create <name> [--from <branch>] [--at <time> --history <duration>]
                 | list | drop <name> | reset <name>
                                    a local database's branches: a copy made in
                                    constant time and space, writable on its
                                    own (nucleus sql <dir> -d <name>; a
                                    server's: CREATE BRANCH, and clients name
                                    the branch as their database); --at: as it
                                    was then, within the history the server
                                    keeps (its --history, given again)
  nucleus restore <repository> <dir> [--at <commit|backup|time>]
                 [--subject-keys <file>] [--backup-key <file>]
                                    a new database in <dir> from the backups
                                    BACKUP TO '<repository>' wrote: the last,
                                    or the last by --at (a commit, a backup's
                                    directory such as 000003-incr, or a time
                                    such as '2026-11-15 14:05:00+01') — its
                                    full backup and the incremental ones
                                    after it; indexes and statistics made
                                    again from the rows; the rows of subjects
                                    whose keys FORGET SUBJECT destroyed left
                                    out (keys from <file>, default
                                    <dir>.subject-keys); <repository> may be
                                    s3://bucket/prefix (AWS_ACCESS_KEY_ID,
                                    AWS_SECRET_ACCESS_KEY, AWS_REGION,
                                    AWS_ENDPOINT_URL for MinIO and others);
                                    encrypted backups need --backup-key (or
                                    NUCLEUS_BACKUP_KEY_FILE)

Keys and values are text; write any byte as \\xNN (and a backslash as \\\\).
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    sql::registry();
    // Repositories of backups in object storage (BACKUP TO 's3://…').
    nucleus_s3::register();
    let result = match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            // The Appropriate Legal Notices (AGPL-3.0 section 0; COPYRIGHT).
            println!(
                "nucleus {}\n\
                 Copyright (C) 2026 Dávid Sárosi. Original author: Dávid Sárosi.\n\
                 License AGPL-3.0-only <https://www.gnu.org/licenses/agpl-3.0.html>.\n\
                 This is free software: you are free to change and redistribute it.\n\
                 There is NO WARRANTY, to the extent permitted by law.",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Some("init") => init::run(&args[1..]),
        Some("kv") => kv::run(&args[1..]),
        Some("sql") => sql::run(&args[1..]),
        Some("mcp") => mcp::run(&args[1..]),
        Some("server") => server::run(&args[1..]),
        Some("restore") => restore::run(&args[1..]),
        Some("branch") => branch::run(&args[1..]),
        Some("dump") => dump::run(&args[1..]),
        Some("migrate") => migrate::run(&args[1..]),
        Some("codegen") => codegen::run(&args[1..]),
        Some("status") => status::run(&args[1..]),
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
