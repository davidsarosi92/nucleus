//! The `nucleus` binary end to end.

use std::io::Write;
use std::process::{Command, Stdio};

fn nucleus(args: &[&str], stdin: &str) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nucleus"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}

/// The server's line saying where it listens (`… listening on …`): the
/// lines before it (a key made at first use, §M14.30 K1) passed over.
fn listening_line(stderr: std::process::ChildStderr) -> String {
    use std::io::BufRead;
    let mut r = std::io::BufReader::new(stderr);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap() == 0 || line.contains(" listening on ") {
            return line;
        }
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nucleus-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn version() {
    let (ok, out) = nucleus(&["--version"], "");
    assert!(
        ok && out.starts_with("nucleus ") && out.contains("Original author: Dávid Sárosi."),
        "{out}"
    );
}

#[test]
fn one_shot_commands_persist() {
    let dir = temp_dir("oneshot");
    let d = dir.to_str().unwrap();
    assert!(nucleus(&["kv", d, "put", "greeting", "hello world"], "").0);
    assert!(nucleus(&["kv", d, "put", "bin", "\\x00\\xff"], "").0);
    assert_eq!(
        nucleus(&["kv", d, "get", "greeting"], "").1,
        "hello world\n"
    );
    assert_eq!(nucleus(&["kv", d, "get", "bin"], "").1, "\\x00\\xff\n");
    assert!(nucleus(&["kv", d, "delete", "bin"], "").0);
    assert_eq!(
        nucleus(&["kv", d, "scan"], "").1,
        "greeting = hello world\n(1 rows)\n"
    );
    assert_eq!(nucleus(&["kv", d, "get", "bin"], "").1, "(not found)\n");
    let (ok, out) = nucleus(&["kv", d, "frobnicate"], "");
    assert!(!ok && out.contains("unknown command"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn shell_with_transactions() {
    let dir = temp_dir("shell");
    let d = dir.to_str().unwrap();
    let script = "put a 1\nbegin\nput b 2\nput c 3\nrollback\nbegin\nput d 4\ncommit\nscan\nget nope\nbogus\nquit\n";
    let (ok, out) = nucleus(&["kv", d], script);
    assert!(ok, "{out}");
    assert!(out.contains("a = 1\nd = 4\n(2 rows)\n"), "{out}");
    assert!(
        out.contains("(not found)") && out.contains("unknown command"),
        "{out}"
    );
    // An open transaction at end of input is rolled back.
    let (_, out) = nucleus(&["kv", d], "begin\nput z 9\n");
    assert!(out.contains("rolled back"), "{out}");
    assert_eq!(nucleus(&["kv", d, "get", "z"], "").1, "(not found)\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn sql_shell_and_command() {
    let dir = temp_dir("sql");
    let d = dir.to_str().unwrap();
    let script = "CREATE TABLE t (id INT PRIMARY KEY, name TEXT);\nINSERT INTO t VALUES (1, 'one'),\n  (2, NULL);\nSELECT * FROM t ORDER BY id;\n";
    let (ok, out) = nucleus(&["sql", d], script);
    assert!(ok, "{out}");
    assert!(out.contains("CREATE TABLE\nINSERT 0 2\n id | name \n----+------\n 1  | one  \n 2  |      \n(2 rows)\n"), "{out}");
    let (ok, out) = nucleus(&["sql", d, "-c", "SELECT count(*) AS n FROM t"], "");
    assert!(ok && out.contains(" n \n---\n 2 \n(1 row)"), "{out}");
    let (ok, out) = nucleus(&["sql", d, "-c", "SELECT * FROM nope"], "");
    assert!(
        !ok && out.contains("ERROR:  relation \"nope\" does not exist (42P01)"),
        "{out}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// PLAN.md §M14.18 O3: a repository of backups — a full one, then what
/// changed — and `nucleus restore --at` to any of them.
#[test]
fn backups_restored_to_a_point() {
    let dir = temp_dir("backups");
    let (db, repo) = (dir.join("db"), dir.join("repo"));
    let (d, r) = (db.to_str().unwrap(), repo.to_str().unwrap());
    let sql = |q: &str| {
        let (ok, out) = nucleus(&["sql", d, "-c", q], "");
        assert!(ok, "{q}: {out}");
        out
    };
    sql("CREATE TABLE t (id INT PRIMARY KEY, v TEXT); \
         INSERT INTO t SELECT g, 'v' || g FROM generate_series(1, 2000) g");
    let first = sql(&format!("BACKUP TO '{r}'"));
    assert!(
        first.contains("| full ") && first.contains("000001-full"),
        "{first}"
    );
    sql("UPDATE t SET v = 'changed' WHERE id <= 10; DELETE FROM t WHERE id > 1990");
    let second = sql(&format!("BACKUP TO '{r}'"));
    assert!(
        second.contains("| incremental ") && second.contains("000002-incr"),
        "{second}"
    );
    let count = |at: &str| {
        // (A time's colons are no file name on Windows.)
        let name: String = at
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let out = dir.join(format!("restored-{name}"));
        let o = out.to_str().unwrap();
        let mut args = vec!["restore", r, o];
        if !at.is_empty() {
            args.extend(["--at", at]);
        }
        let (ok, text) = nucleus(&args, "");
        assert!(ok, "{text}");
        let (ok, text) = nucleus(
            &[
                "sql",
                o,
                "-c",
                "SELECT count(*) || ' ' || sum(CASE WHEN v = 'changed' THEN 1 ELSE 0 END) AS n FROM t",
            ],
            "",
        );
        assert!(ok, "{text}");
        (text.lines().nth(2).unwrap_or("").trim().to_string(), text)
    };
    let (latest, text) = count("");
    assert_eq!(latest, "1990 10", "{text}");
    assert_eq!(count("000001-full").0, "2000 0");
    // By time: the last made by then — none before the first.
    assert_eq!(count("2999-01-01 00:00:00+00").0, "1990 10");
    let x = dir.join("x");
    let x = x.to_str().unwrap();
    let (ok, out) = nucleus(&["restore", r, x, "--at", "2000-01-01 00:00:00+00"], "");
    assert!(!ok && out.contains("no backup made by then"), "{out}");
    // Not restored: not made either.
    assert!(!dir.join("x").exists());
    let (ok, out) = nucleus(&["restore", r, x, "--at", "yesterday-ish"], "");
    assert!(
        !ok && out.contains("not a commit, a backup or a time"),
        "{out}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `:memory:` (PLAN.md §M14.17 E2): a database in the process, no
/// directory made; gone when it ends.
#[test]
fn a_database_in_memory() {
    let cwd = temp_dir("memory");
    std::fs::create_dir_all(&cwd).unwrap();
    let run = |args: &[&str], stdin: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_nucleus"))
            .args(args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                c.stdin.take().unwrap().write_all(stdin.as_bytes())?;
                c.wait_with_output()
            })
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
    };
    let out = run(
        &["sql", ":memory:"],
        "CREATE TABLE t (x INT);\nINSERT INTO t VALUES (1), (2);\nSELECT sum(x) AS s FROM t;\n",
    );
    assert!(out.contains(" s \n---\n 3 \n(1 row)"), "{out}");
    // Gone: a new process starts empty.
    let out = run(&["sql", ":memory:", "-c", "SELECT * FROM t"], "");
    assert!(out.contains("42P01"), "{out}");
    let out = run(&["kv", ":memory:"], "put a 1\nget a\n");
    assert!(out.contains('1'), "{out}");
    assert_eq!(
        std::fs::read_dir(&cwd).unwrap().count(),
        0,
        "a file was made"
    );
    std::fs::remove_dir_all(&cwd).unwrap();
}

/// `nucleus init` with no password given makes one, shown once, which
/// then logs in (§M14.17 E2).
#[test]
fn init_makes_a_password() {
    let dir = temp_dir("init");
    let d = dir.to_str().unwrap();
    let (ok, out) = nucleus(&["init", d], "\n");
    assert!(ok, "{out}");
    let password = out
        .lines()
        .find_map(|l| l.strip_prefix("its password, shown this once: "))
        .unwrap_or_else(|| panic!("{out}"))
        .to_string();
    assert_eq!(password.len(), 20);
    assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));
    assert!(out.contains(&format!("nucleus server {d}")), "{out}");
    let (ok, out) = nucleus(&["init", d], "\n");
    assert!(!ok && out.contains("already has an administrator"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `nucleus migrate` (PLAN.md §M14.16 M2): files applied in order, each
/// with its record in one transaction; reverted; checked.
#[test]
fn migrations_up_and_down() {
    let root = temp_dir("migrate");
    let (db, m) = (root.join("db"), root.join("migrations"));
    std::fs::create_dir_all(&m).unwrap();
    let (d, md) = (db.to_str().unwrap(), m.to_str().unwrap());
    let migrate = |args: &[&str]| {
        let mut all = vec!["migrate"];
        all.extend_from_slice(args);
        all.extend_from_slice(&[d, "--dir", md]);
        nucleus(&all, "")
    };
    let write = |name: &str, text: &str| std::fs::write(m.join(name), text).unwrap();
    write(
        "1_dept.sql",
        "-- migrate:up\nCREATE TABLE dept (id int PRIMARY KEY, name text);\n-- migrate:down\nDROP TABLE dept;\n",
    );
    write(
        "2_emp.sql",
        "-- migrate:up\nCREATE TABLE emp (id int PRIMARY KEY, dept_id int REFERENCES dept (id));\nCREATE INDEX emp_dept ON emp (dept_id)\n-- migrate:down\nDROP TABLE emp;\n",
    );
    let (ok, out) = migrate(&["status"]);
    assert!(
        ok && out.contains("1  dept") && out.matches("pending").count() == 2,
        "{out}"
    );
    let (ok, out) = migrate(&["up", "--to", "1"]);
    assert!(
        ok && out.contains("applied 1_dept") && !out.contains("2_emp"),
        "{out}"
    );
    let (ok, out) = migrate(&["up"]);
    assert!(ok && out.contains("applied 2_emp"), "{out}");
    let (ok, out) = migrate(&["up"]);
    assert!(ok && out.contains("up to date"), "{out}");
    let (ok, out) = nucleus(
        &[
            "sql",
            d,
            "-c",
            "SELECT version, name FROM nc_catalog.migrations",
        ],
        "",
    );
    assert!(
        ok && out.contains(" 1       | dept") && out.contains(" 2       | emp"),
        "{out}"
    );

    // A failing migration leaves nothing: not its first table, not its record.
    write(
        "3_bad.sql",
        "-- migrate:up\nCREATE TABLE half (id int PRIMARY KEY);\nCREATE TABLE emp (id int);\n-- migrate:down\nDROP TABLE half;\n",
    );
    let (ok, out) = migrate(&["up"]);
    assert!(
        !ok && out.contains("3_bad") && out.contains("42P07"),
        "{out}"
    );
    let (ok, out) = nucleus(
        &[
            "sql",
            d,
            "-c",
            "SELECT count(*) FROM nc_catalog.tables WHERE name = 'half'",
        ],
        "",
    );
    assert!(ok && out.contains(" 0"), "{out}");
    std::fs::remove_file(m.join("3_bad.sql")).unwrap();

    // An applied file changed is refused; so is a new one older than the newest.
    let two = std::fs::read_to_string(m.join("2_emp.sql")).unwrap();
    write("2_emp.sql", &two.replace("CREATE INDEX", "CREATE  INDEX"));
    let (ok, out) = migrate(&["up"]);
    assert!(!ok && out.contains("changed since it was applied"), "{out}");
    let (_, out) = migrate(&["status"]);
    assert!(out.contains("CHANGED"), "{out}");
    write("2_emp.sql", &two);
    write("0_early.sql", "-- migrate:up\nSELECT 1;\n");
    let (ok, out) = migrate(&["up"]);
    assert!(
        !ok && out.contains("older than the newest applied"),
        "{out}"
    );
    std::fs::remove_file(m.join("0_early.sql")).unwrap();

    // Down: the newest, then all.
    let (ok, out) = migrate(&["down"]);
    assert!(ok && out.contains("reverted 2_emp"), "{out}");
    let (ok, out) = nucleus(&["sql", d, "-c", "SELECT * FROM emp"], "");
    assert!(!ok && out.contains("42P01"), "{out}");
    let (ok, out) = migrate(&["up"]);
    assert!(ok && out.contains("applied 2_emp"), "{out}");
    let (ok, out) = migrate(&["down", "--all"]);
    assert!(
        ok && out.contains("reverted 2_emp") && out.contains("reverted 1_dept"),
        "{out}"
    );
    let (ok, out) = migrate(&["status"]);
    assert!(ok && out.matches("pending").count() == 2, "{out}");

    // Outside a block when it says so: a branch made by a migration.
    write(
        "3_branch.sql",
        "-- migrate:up transaction:false\nCREATE BRANCH trial;\n-- migrate:down transaction:false\nDROP BRANCH trial;\n",
    );
    let (ok, out) = migrate(&["up"]);
    assert!(ok && out.contains("applied 3_branch"), "{out}");
    let (ok, out) = migrate(&["down"]);
    assert!(ok && out.contains("reverted 3_branch"), "{out}");

    // `new`: a file named by the time, both directions in it.
    let (ok, out) = nucleus(&["migrate", "new", "Add Notes!", "--dir", md], "");
    assert!(ok && out.trim_end().ends_with("_add_notes.sql"), "{out}");
    let made = std::fs::read_to_string(out.trim_end()).unwrap();
    assert_eq!(made, "-- migrate:up\n\n\n-- migrate:down\n\n");
    std::fs::remove_dir_all(&root).unwrap();
}

/// `nucleus migrate up --dry-run` and `diff` (PLAN.md §M14.16 M4): on a
/// branch dropped after; the statements from the files' schema to the
/// database's, as a new migration — locally and against a server.
#[test]
fn migrations_dry_run_and_diff() {
    let root = temp_dir("migrate-diff");
    let (db, m) = (root.join("db"), root.join("migrations"));
    std::fs::create_dir_all(&m).unwrap();
    let (d, md) = (db.to_str().unwrap(), m.to_str().unwrap());
    std::fs::write(
        m.join("1_notes.sql"),
        "-- migrate:up\nCREATE TABLE notes (id int PRIMARY KEY, body text);\n-- migrate:down\nDROP TABLE notes;\n",
    )
    .unwrap();
    let migrate = |at: &[&str], args: &[&str]| {
        let mut all = vec!["migrate"];
        all.extend_from_slice(args);
        all.extend_from_slice(at);
        all.extend_from_slice(&["--dir", md]);
        nucleus(&all, "")
    };
    let local = [d];
    let (ok, out) = migrate(&local, &["up", "--dry-run"]);
    assert!(
        ok && out.contains("applied 1_notes")
            && out.contains("+ CREATE TABLE notes")
            && out.contains("nothing was changed"),
        "{out}"
    );
    let (ok, out) = migrate(&local, &["status"]);
    assert!(ok && out.contains("pending"), "{out}");
    let (ok, out) = nucleus(
        &["sql", d, "-c", "SELECT name FROM nc_catalog.branches"],
        "",
    );
    assert!(ok && !out.contains("dry_run"), "{out}");
    // A diff needs every file applied.
    let (ok, out) = migrate(&local, &["diff"]);
    assert!(!ok && out.contains("pending"), "{out}");
    let (ok, out) = migrate(&local, &["up"]);
    assert!(ok, "{out}");
    let (ok, out) = migrate(&local, &["diff"]);
    assert!(ok && out.contains("no difference"), "{out}");
    let (ok, out) = nucleus(
        &[
            "sql",
            d,
            "-c",
            "ALTER TABLE notes ADD COLUMN tag text DEFAULT 'misc'; CREATE INDEX notes_tag ON notes (tag)",
        ],
        "",
    );
    assert!(ok, "{out}");

    // Against a server: the same, over the network.
    let mut server = Command::new(env!("CARGO_BIN_EXE_nucleus"))
        .args(["server", d, "--listen", "127.0.0.1:0"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let first = listening_line(server.stderr.take().unwrap());
    let addr = first
        .split_whitespace()
        .find(|w| w.starts_with("127.0.0.1:"))
        .unwrap_or_else(|| panic!("no address in {first:?}"))
        .to_string();
    let remote = ["-h", addr.as_str()];
    let (ok, out) = migrate(&remote, &["diff"]);
    assert!(
        ok && out.contains("ALTER TABLE notes ADD COLUMN tag text DEFAULT 'misc';")
            && out.contains("CREATE INDEX notes_tag ON notes (tag);"),
        "{out}"
    );
    let (ok, out) = migrate(&remote, &["diff", "--new", "tags"]);
    assert!(ok && out.contains("recorded as applied"), "{out}");
    let made = std::fs::read_dir(&m)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with("_tags.sql"))
        .unwrap();
    let text = std::fs::read_to_string(&made).unwrap();
    assert!(
        text.contains("-- migrate:down\nDROP INDEX notes_tag;\nALTER TABLE notes DROP COLUMN tag;"),
        "{text}"
    );
    let (ok, out) = migrate(&remote, &["diff"]);
    assert!(ok && out.contains("no difference"), "{out}");
    // A dry run of the way down, on a branch over the network.
    let (ok, out) = migrate(&remote, &["down", "--dry-run"]);
    assert!(
        ok && out.contains("reverted ")
            && out.contains("- CREATE INDEX notes_tag")
            && out.contains("nothing was changed"),
        "{out}"
    );
    let (ok, out) = migrate(&remote, &["status"]);
    assert!(ok && out.matches("applied").count() == 2, "{out}");
    server.kill().unwrap();
    server.wait().unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

/// `nucleus dump --schema` (PLAN.md §M14.16 M1): the schema as SQL,
/// which makes the same schema in another database.
#[test]
fn the_schema_dumped_replays() {
    let (a, b) = (temp_dir("dump-a"), temp_dir("dump-b"));
    let (da, db) = (a.to_str().unwrap(), b.to_str().unwrap());
    let (ok, out) = nucleus(
        &["sql", da],
        "CREATE TABLE dept (id int PRIMARY KEY, name text NOT NULL UNIQUE);\n\
         CREATE TABLE emp (id int PRIMARY KEY, dept_id int REFERENCES dept (id), pay numeric(10,2) CHECK (pay > 0));\n\
         CREATE INDEX emp_dept ON emp (dept_id);\n\
         COMMENT ON TABLE emp IS 'people';\n\
         INSERT INTO dept VALUES (1, 'eng');\n",
    );
    assert!(ok, "{out}");
    let (ok, schema) = nucleus(&["dump", "--schema", da], "");
    assert!(ok, "{schema}");
    assert!(
        schema.contains("CREATE INDEX emp_dept ON emp (dept_id);\n")
            && schema.contains("COMMENT ON TABLE emp IS 'people';\n")
            && !schema.contains("eng"),
        "{schema}"
    );
    let (ok, out) = nucleus(&["sql", db], &schema);
    assert!(ok, "{out}");
    let (ok, again) = nucleus(&["dump", "--schema", db], "");
    assert!(ok && again == schema, "{again}");
    let (ok, out) = nucleus(&["dump", da], "");
    assert!(!ok && out.contains("--schema"), "{out}");
    std::fs::remove_dir_all(&a).unwrap();
    std::fs::remove_dir_all(&b).unwrap();
}

#[test]
fn code_from_the_schema() {
    // The generated code compiled and run is scripts/driver-tests.sh's
    // codegen step; here, what the command takes and refuses.
    let a = temp_dir("codegen");
    let da = a.to_str().unwrap();
    let (ok, out) = nucleus(
        &["sql", da],
        "CREATE TYPE mood AS ENUM ('sad', 'ok');\n\
         CREATE TABLE emp (id int PRIMARY KEY, name text NOT NULL, mood mood, pay numeric(10,2));\n\
         CREATE TABLE dept (id int PRIMARY KEY);\n",
    );
    assert!(ok, "{out}");
    let (ok, ts) = nucleus(&["codegen", "ts", da, "--tables", "emp"], "");
    assert!(ok, "{ts}");
    assert!(
        ts.contains("export interface Emp {\n  id: number;\n  name: string;\n  mood: \"sad\" | \"ok\" | null;\n  pay: nucleus.Decimal | null;\n}")
            && ts.contains("select: \"SELECT id, name, mood, pay FROM emp\"")
            && !ts.contains("Dept"),
        "{ts}"
    );
    let file = a.join("schema.py");
    let (ok, out) = nucleus(
        &["codegen", "python", da, "--out", file.to_str().unwrap()],
        "",
    );
    assert!(ok && out.is_empty(), "{out}");
    let py = std::fs::read_to_string(&file).unwrap();
    assert!(
        py.contains("class Dept:") && py.contains("    mood: Optional[Literal[\"sad\", \"ok\"]]\n"),
        "{py}"
    );
    let (ok, java) = nucleus(&["codegen", "java", da, "--package", "com.example"], "");
    assert!(
        ok && java.contains("package com.example;\n")
            && java.contains(
                "public record Emp(Integer id, String name, String mood, java.math.BigDecimal pay)"
            ),
        "{java}"
    );
    let (ok, rust) = nucleus(&["codegen", "rust", da, "--tables", "dept"], "");
    assert!(
        ok && rust.contains("pub struct Dept {\n    pub id: i32,\n}"),
        "{rust}"
    );
    let (ok, out) = nucleus(&["codegen", "ts", da, "--tables", "nope"], "");
    assert!(!ok && out.contains("no table nope"), "{out}");
    let (ok, out) = nucleus(&["codegen", "cobol", da], "");
    assert!(!ok && out.contains("unknown language cobol"), "{out}");
    // Named queries (§M14.16 M6): a function each; one that does not
    // check is an error at its place.
    let q = a.join("queries.sql");
    let qs = q.to_str().unwrap();
    std::fs::write(
        &q,
        "-- name: emp_by_name :one\nSELECT id, pay FROM emp WHERE name = $1;\n\n\
         -- name: raise :exec\nUPDATE emp SET pay = pay + $1 WHERE mood = $2;\n",
    )
    .unwrap();
    let (ok, py) = nucleus(&["codegen", "python", da, "--queries", qs], "");
    assert!(
        ok && py.contains("def emp_by_name(conn: Any, name: str) -> Optional[EmpByNameRow]:")
            && py.contains("    id: int\n    pay: Optional[decimal.Decimal]\n")
            && py.contains("def raise_(conn: Any, pay: decimal.Decimal, mood: Literal[\"sad\", \"ok\"]) -> int:"),
        "{py}"
    );
    std::fs::write(
        &q,
        "-- name: broken :many\nSELECT id\n  FROM emp WHERE nope = $1;\n",
    )
    .unwrap();
    let (ok, out) = nucleus(&["codegen", "ts", da, "--queries", qs], "");
    assert!(
        !ok && out.contains("queries.sql:2: broken: column \"nope\" does not exist"),
        "{out}"
    );
    std::fs::write(&q, "-- name: broken :many\nSELECT id,\n  FROM emp;\n").unwrap();
    let (ok, out) = nucleus(&["codegen", "ts", da, "--queries", qs], "");
    assert!(
        !ok && out.contains("queries.sql:3:3: syntax error"),
        "{out}"
    );
    std::fs::write(&q, "-- name: gone :one\nDELETE FROM emp;\n").unwrap();
    let (ok, out) = nucleus(&["codegen", "ts", da, "--queries", qs], "");
    assert!(
        !ok && out.contains("gone returns no rows: make it :exec"),
        "{out}"
    );
    std::fs::remove_dir_all(&a).unwrap();
}

fn ncsql(args: &[&str], stdin: &str) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ncsql"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}

#[test]
fn ncsql_local_and_over_the_network() {
    let dir = temp_dir("ncsql");
    let d = dir.to_str().unwrap();
    let schema = "CREATE TABLE dept (id INT PRIMARY KEY, name TEXT UNIQUE);\n\
                  CREATE TABLE emp (id INT PRIMARY KEY, dept_id INT REFERENCES dept ON DELETE CASCADE, salary NUMERIC(10, 2) CHECK (salary > 0));\n\
                  INSERT INTO dept VALUES (1, 'eng');\nINSERT INTO emp VALUES (10, 1, 100.5);\n";
    let (ok, out) = ncsql(&[d], schema);
    assert!(ok, "{out}");
    // Local: the commands read nc_catalog.
    let (ok, out) = ncsql(&[d, "-c", "\\d emp"], "");
    assert!(ok, "{out}");
    for want in [
        "Table \"emp\" (id 2, key prefix 0100000002, not analyzed)",
        " salary  | numeric(10,2) |          | ",
        "emp_pkey  PRIMARY KEY (id), index 0",
        "emp_dept_id_fkey  (dept_id) -> dept (id) on delete cascade",
        "emp_salary_check  CHECK (salary > 0)",
    ] {
        assert!(out.contains(want), "missing {want:?} in\n{out}");
    }
    let (ok, out) = ncsql(&[d, "-c", "\\nope"], "");
    assert!(!ok && out.contains("invalid command"), "{out}");
    // \copy loads a file (COPY … FROM STDIN), locally …
    let csv = dir.join("depts.csv");
    std::fs::write(&csv, "id,name\n2,ops\n3,\"r, d\"\n").unwrap();
    let f = csv.to_str().unwrap();
    let (ok, out) = ncsql(
        &[
            d,
            "-c",
            &format!("\\copy dept FROM '{f}' WITH (FORMAT csv, HEADER)"),
        ],
        "",
    );
    assert!(ok && out.contains("COPY 2"), "{out}");
    let (ok, out) = ncsql(&[d, "-c", "\\copy dept FROM '/no/such/file'"], "");
    assert!(!ok && out.contains("/no/such/file"), "{out}");

    // Over the network, against `nucleus server` on a free port.
    let mut server = Command::new(env!("CARGO_BIN_EXE_nucleus"))
        .args(["server", d, "--listen", "127.0.0.1:0"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Its first line only: the pipe is closed after it, and the server
    // goes on (its log lines never fail on a stderr gone).
    let first = listening_line(server.stderr.take().unwrap());
    let addr = first
        .split_whitespace()
        .find(|w| w.starts_with("127.0.0.1:"))
        .unwrap_or_else(|| panic!("no address in {first:?}"))
        .to_string();
    // While the server has the directory, nothing else may open it.
    let (ok, out) = nucleus(&["sql", d, "-c", "SELECT 1"], "");
    assert!(!ok && out.contains("in use by another process"), "{out}");
    let (ok, out) = ncsql(&["-h", &addr, "-c", "SELECT name FROM dept"], "");
    assert!(ok && out.contains(" eng ") && out.contains("r, d"), "{out}");
    // … and over the network (a duplicate key: nothing stored).
    std::fs::write(&csv, "4\tqa\n1\tdup\n").unwrap();
    let (ok, out) = ncsql(&["-h", &addr, "-c", &format!("\\copy dept FROM {f}")], "");
    assert!(
        !ok && out.contains("23505") && out.contains("line 2"),
        "{out}"
    );
    std::fs::write(&csv, "4\tqa\n").unwrap();
    let (ok, out) = ncsql(
        &[
            "-h",
            &addr,
            "-c",
            &format!("\\copy dept (id, name) FROM {f}"),
        ],
        "",
    );
    assert!(ok && out.contains("COPY 1"), "{out}");
    let (ok, out) = ncsql(&["-h", &addr, "-c", "\\fks dept"], "");
    assert!(
        ok && out.contains("emp_dept_id_fkey") && out.contains("cascade"),
        "{out}"
    );
    let (ok, out) = ncsql(
        &["-h", &addr],
        "BEGIN;\nDELETE FROM dept;\nCOMMIT;\nSELECT count(*) FROM emp;\n\\conninfo\n",
    );
    assert!(
        // (eng, and the three \copy loaded.)
        ok && out.contains("DELETE 4") && out.contains(" 0 "),
        "{out}"
    );
    assert!(out.contains("on 127.0.0.1:"), "{out}");
    server.kill().unwrap();
    server.wait().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// One HTTP GET to `addr`: the status line and the body.
fn http_get(addr: &str, path: &str, accept: Option<&str>) -> (String, String) {
    use std::io::Read;
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let accept = accept.map_or(String::new(), |a| format!("Accept: {a}\r\n"));
    write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n{accept}\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    (head.lines().next().unwrap().to_string(), body.to_string())
}

/// A sample's value in Prometheus's text format.
fn sample(text: &str, series: &str) -> f64 {
    text.lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("no {series} in\n{text}"))
        .parse()
        .unwrap()
}

/// `nucleus server --backup-to … --backup-every …` (PLAN.md §M14.18
/// O3c): backups on a schedule — every second one full, the last full
/// one kept with those after it, encrypted with `--backup-key` — counted
/// in the metrics; `nucleus restore` needs the key.
#[test]
fn scheduled_encrypted_backups() {
    let dir = temp_dir("scheduled");
    std::fs::create_dir_all(&dir).unwrap();
    let (db, repo, key) = (dir.join("db"), dir.join("repo"), dir.join("backup.key"));
    std::fs::write(&key, format!("{}\n", "ab".repeat(32))).unwrap();
    let (d, r, k) = (
        db.to_str().unwrap(),
        repo.to_str().unwrap(),
        key.to_str().unwrap(),
    );
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut server = Kill(
        Command::new(env!("CARGO_BIN_EXE_nucleus"))
            .args([
                "server",
                d,
                "--listen",
                "127.0.0.1:0",
                "--metrics",
                "127.0.0.1:0",
                "--backup-to",
                r,
                "--backup-every",
                "1s",
                "--backup-full-every",
                "2",
                "--backup-keep",
                "1",
                "--backup-key",
                k,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let first = listening_line(server.0.stderr.take().unwrap());
    let addrs: Vec<&str> = first
        .split_whitespace()
        .filter(|w| w.starts_with("127.0.0.1:"))
        .collect();
    let (addr, metrics) = (addrs[0].to_string(), addrs[1].to_string());
    let (ok, out) = ncsql(
        &["-h", &addr],
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT);\nINSERT INTO t SELECT g, 'a secret value' FROM generate_series(1, 100) g;\n",
    );
    assert!(ok, "{out}");
    let made = || {
        sample(
            &http_get(&metrics, "/metrics", None).1,
            "nucleus_backups_total",
        )
    };
    let after_insert = made();
    let mut waited = 0;
    while made() < (after_insert + 2.0).max(4.0) {
        assert!(waited < 600, "{}", http_get(&metrics, "/metrics", None).1);
        std::thread::sleep(std::time::Duration::from_millis(100));
        waited += 1;
    }
    let (_, m) = http_get(&metrics, "/metrics", None);
    assert_eq!(sample(&m, "nucleus_backup_interval_seconds"), 1.0, "{m}");
    assert_eq!(sample(&m, "nucleus_backup_failing"), 0.0, "{m}");
    assert_eq!(sample(&m, "nucleus_backup_failures_total"), 0.0, "{m}");
    assert!(
        sample(&m, "nucleus_backup_last_success_timestamp_seconds") > 1.7e9,
        "{m}"
    );
    drop(server);
    // The older chains removed: the first full backup is gone.
    let left: Vec<String> = std::fs::read_dir(&repo)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!left.iter().any(|n| n == "000001-full"), "{left:?}");
    assert!(left.len() <= 3, "{left:?}");
    // Nothing of the rows in the clear.
    for b in &left {
        for f in std::fs::read_dir(repo.join(b)).unwrap() {
            let bytes = std::fs::read(f.unwrap().path()).unwrap();
            assert!(!bytes.windows(14).any(|w| w == b"a secret value"));
        }
    }
    let out = dir.join("restored");
    let o = out.to_str().unwrap();
    let (ok, text) = nucleus(&["restore", r, o], "");
    assert!(!ok && text.contains("encrypted"), "{text}");
    let (ok, text) = nucleus(&["restore", r, o, "--backup-key", k], "");
    assert!(ok, "{text}");
    let (ok, text) = nucleus(&["sql", o, "-c", "SELECT count(*) FROM t"], "");
    assert!(ok && text.contains("100"), "{text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `nucleus server --restore-test-every …` (PLAN.md §M14.18 O4): the
/// last backup restored and checked on a schedule, as `nucleus restore
/// --test` does by hand — both catch a deliberately corrupted backup.
#[test]
fn scheduled_restore_tests_catch_a_corrupted_backup() {
    let dir = temp_dir("restoretests");
    std::fs::create_dir_all(&dir).unwrap();
    let (db, repo) = (dir.join("db"), dir.join("repo"));
    let (d, r) = (db.to_str().unwrap(), repo.to_str().unwrap());
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut server = Kill(
        Command::new(env!("CARGO_BIN_EXE_nucleus"))
            .args([
                "server",
                d,
                "--listen",
                "127.0.0.1:0",
                "--metrics",
                "127.0.0.1:0",
                "--backup-to",
                r,
                "--backup-every",
                "1s",
                "--backup-full-every",
                "1000",
                "--restore-test-every",
                "1s",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let first = listening_line(server.0.stderr.take().unwrap());
    let addrs: Vec<&str> = first
        .split_whitespace()
        .filter(|w| w.starts_with("127.0.0.1:"))
        .collect();
    let (addr, metrics) = (addrs[0].to_string(), addrs[1].to_string());
    let (ok, out) = ncsql(
        &["-h", &addr],
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT);\nCREATE INDEX t_v ON t (v);\nINSERT INTO t SELECT g, 'v' || g FROM generate_series(1, 1000) g;\n",
    );
    assert!(ok, "{out}");
    let value = |series: &str| sample(&http_get(&metrics, "/metrics", None).1, series);
    let wait_for = |what: &str, cond: &dyn Fn() -> bool| {
        for _ in 0..600 {
            if cond() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("{what}: {}", http_get(&metrics, "/metrics", None).1);
    };
    wait_for("a restore test", &|| {
        value("nucleus_restore_tests_total") >= 2.0
            && value("nucleus_restore_test_last_success_timestamp_seconds") > 0.0
    });
    assert_eq!(value("nucleus_restore_test_failing"), 0.0);
    assert_eq!(value("nucleus_restore_test_interval_seconds"), 1.0);
    let (ok, text) = nucleus(&["restore", r, "--test"], "");
    assert!(ok && text.contains("it holds"), "{text}");
    // A byte of the chain's first part flipped (the first backup, of an
    // empty database, may have none): the next test fails.
    let mut backups: Vec<_> = std::fs::read_dir(&repo)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    backups.sort();
    let part = backups
        .iter()
        .map(|b| b.join("part-000000"))
        .find(|p| p.exists())
        .unwrap();
    let mut b = std::fs::read(&part).unwrap();
    let mid = b.len() / 2;
    b[mid] ^= 0x40;
    std::fs::write(&part, &b).unwrap();
    wait_for("the restore test failing", &|| {
        value("nucleus_restore_test_failing") == 1.0
    });
    assert!(value("nucleus_restore_test_failures_total") >= 1.0);
    let (ok, text) = nucleus(&["restore", r, "--test"], "");
    assert!(
        !ok && text.contains("FAILED") && text.contains("SHA-256"),
        "{text}"
    );
    drop(server);
    // The scratch directory: marked, the database in it removed.
    let scratch = dir.join("db.restore-test");
    assert!(scratch.join(".nucleus-restore-test").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `nucleus server --metrics` (PLAN.md §M14.18): Prometheus's `GET
/// /metrics` moving with a workload, the same numbers in
/// `nc_catalog.metrics`, and the health checks.
#[test]
fn metrics_and_health_checks() {
    let dir = temp_dir("metrics");
    let d = dir.to_str().unwrap();
    /// The server killed even when an assertion fails.
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut server = Kill(
        Command::new(env!("CARGO_BIN_EXE_nucleus"))
            .args([
                "server",
                d,
                "--listen",
                "127.0.0.1:0",
                "--metrics",
                "127.0.0.1:0",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let first = listening_line(server.0.stderr.take().unwrap());
    let addrs: Vec<&str> = first
        .split_whitespace()
        .filter(|w| w.starts_with("127.0.0.1:"))
        .collect();
    let (addr, metrics) = (addrs[0].to_string(), addrs[1].to_string());
    assert!(
        first.contains(&format!("{metrics} (metrics and health checks)")),
        "{first}"
    );
    assert_eq!(
        http_get(&metrics, "/health/live", None).0,
        "HTTP/1.1 200 OK"
    );
    assert_eq!(
        http_get(&metrics, "/health/ready", None),
        ("HTTP/1.1 200 OK".to_string(), "ready\n".to_string())
    );
    let (status, before) = http_get(&metrics, "/metrics", None);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(sample(&before, "nucleus_ready"), 1.0);
    let writes = sample(&before, "nucleus_statements_total{kind=\"write\"}");
    let commits = sample(&before, "nucleus_commits_total");
    // A workload: the numbers move.
    let (ok, out) = ncsql(
        &["-h", &addr],
        "CREATE TABLE t (id INT PRIMARY KEY);\nINSERT INTO t SELECT * FROM generate_series(1, 100);\nSELECT count(*) FROM t;\n",
    );
    assert!(ok, "{out}");
    let (_, after) = http_get(&metrics, "/metrics", None);
    assert!(
        sample(&after, "nucleus_statements_total{kind=\"write\"}") >= writes + 2.0,
        "{after}"
    );
    assert!(
        sample(&after, "nucleus_commits_total") >= commits + 2.0,
        "{after}"
    );
    assert!(
        sample(&after, "nucleus_rows_returned_total") >= 101.0,
        "{after}"
    );
    assert!(sample(&after, "nucleus_wal_bytes_total") > 0.0, "{after}");
    assert!(
        sample(&after, "nucleus_sessions_opened_total{front_end=\"ncp\"}") >= 1.0,
        "{after}"
    );
    assert!(
        sample(
            &after,
            "nucleus_statement_duration_seconds_bucket{kind=\"read\",le=\"+Inf\"}"
        ) >= 1.0
    );
    assert!(
        after.contains("# TYPE nucleus_statement_duration_seconds histogram"),
        "{after}"
    );
    // No session open while scraped: the family is there all the same.
    assert!(after.contains("# TYPE nucleus_sessions gauge"), "{after}");
    assert!(
        sample(&after, "nucleus_oldest_transaction_seconds") < 60.0,
        "{after}"
    );
    // OpenMetrics when asked.
    let (_, om) = http_get(
        &metrics,
        "/metrics",
        Some("application/openmetrics-text; version=1.0.0"),
    );
    assert!(
        om.contains("# TYPE nucleus_commits counter") && om.ends_with("# EOF\n"),
        "{om}"
    );
    // nc_catalog.metrics reads the same registry.
    let (ok, out) = ncsql(
        &[
            "-h",
            &addr,
            "-c",
            "SELECT value FROM nc_catalog.metrics WHERE name = 'nucleus_commits_total'",
        ],
        "",
    );
    let in_sql: f64 = out
        .lines()
        .nth(2)
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or_else(|| panic!("{out}"));
    assert!(
        ok && in_sql >= sample(&after, "nucleus_commits_total"),
        "{out}"
    );
    assert_eq!(
        http_get(&metrics, "/nope", None).0,
        "HTTP/1.1 404 Not Found"
    );
    // `nucleus status`: the same numbers, for people.
    let (ok, out) = nucleus(&["status", "-h", &addr], "");
    assert!(
        ok && out.contains(": ready\n") && out.contains("statements "),
        "{out}"
    );
    assert!(
        out.contains("page cache") && out.contains("% hits"),
        "{out}"
    );
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `nucleus server --otlp` (PLAN.md §M14.18 O2b): a span per statement
/// reaches a collector — a listener here standing in for one — in the
/// client's trace when it names one (`SET traceparent`, a sqlcommenter
/// comment), its text without constants.
#[test]
fn statements_traced_over_otlp() {
    use std::io::{BufRead, Read};
    use std::sync::mpsc;

    let collector = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", collector.local_addr().unwrap());
    let (tx, rx) = mpsc::channel::<(String, serde_json::Value)>();
    std::thread::spawn(move || {
        for s in collector.incoming() {
            let Ok(s) = s else { return };
            let mut r = std::io::BufReader::new(s);
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                continue;
            }
            let mut len = 0;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let _ = r
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
            let path = line.split(' ').nth(1).unwrap_or("").to_string();
            let _ = tx.send((path, serde_json::from_slice(&body).unwrap()));
        }
    });
    let dir = temp_dir("otlp");
    let d = dir.to_str().unwrap();
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut server = Kill(
        Command::new(env!("CARGO_BIN_EXE_nucleus"))
            .args(["server", d, "--listen", "127.0.0.1:0", "--otlp", &endpoint])
            .env("OTEL_METRIC_EXPORT_INTERVAL", "200")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let first = listening_line(server.0.stderr.take().unwrap());
    let addr = first
        .split_whitespace()
        .find(|w| w.starts_with("127.0.0.1:"))
        .unwrap()
        .to_string();
    let parent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let (ok, out) = ncsql(
        &["-h", &addr],
        &format!(
            "CREATE TABLE t (id INT PRIMARY KEY, name TEXT);\n\
             SET traceparent = '{parent}';\n\
             INSERT INTO t VALUES (1, 'secret');\n\
             RESET traceparent;\n\
             SELECT name FROM t WHERE id = 1 /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/;\n\
             SELECT nope FROM t;\n"
        ),
    );
    assert!(out.contains("secret"), "{ok} {out}");
    // The spans, until all three looked for came.
    let mut spans: Vec<serde_json::Value> = Vec::new();
    let mut metrics = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let has = |spans: &[serde_json::Value], name: &str| spans.iter().any(|s| s["name"] == name);
    while !(metrics
        && has(&spans, "INSERT")
        && has(&spans, "SELECT")
        && spans.iter().any(|s| s["status"]["code"] == 2))
    {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let (path, body) = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("spans so far: {spans:#?}"));
        match path.as_str() {
            "/v1/traces" => spans.extend(
                body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned(),
            ),
            "/v1/metrics" => metrics = true,
            other => panic!("{other}"),
        }
    }
    let attr = |s: &serde_json::Value, k: &str| {
        s["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == k)
            .map(|a| a["value"].clone())
            .unwrap_or_else(|| panic!("no {k} in {s}"))
    };
    let insert = spans.iter().find(|s| s["name"] == "INSERT").unwrap();
    assert_eq!(
        insert["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736",
        "{insert}"
    );
    assert_eq!(insert["parentSpanId"], "00f067aa0ba902b7", "{insert}");
    assert_eq!(insert["kind"], 2);
    assert_eq!(attr(insert, "db.system.name")["stringValue"], "nucleus");
    // No constant in the text a trace keeps.
    let text = attr(insert, "db.query.text")["stringValue"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!text.contains("secret") && text.contains("$"), "{text}");
    let select = spans
        .iter()
        .find(|s| s["name"] == "SELECT" && s["traceId"] == "0af7651916cd43dd8448eb211c80319c")
        .unwrap_or_else(|| panic!("{spans:#?}"));
    assert_eq!(select["parentSpanId"], "b7ad6b7169203331");
    assert_eq!(attr(select, "db.response.returned_rows")["intValue"], "1");
    let failed = spans.iter().find(|s| s["status"]["code"] == 2).unwrap();
    assert_eq!(
        attr(failed, "db.response.status_code")["stringValue"],
        "42703"
    );
    // A statement outside a client's trace starts one of its own.
    let create = spans.iter().find(|s| s["name"] == "CREATE").unwrap();
    assert!(
        create.get("parentSpanId").is_none() && create["traceId"].as_str().unwrap().len() == 32
    );
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `nucleus mcp` end to end (PLAN.md §M14.10): an MCP client lists,
/// describes and queries a database over standard input and output.
#[test]
fn an_mcp_client_lists_describes_and_queries() {
    use serde_json::{Value, json};
    let dir = temp_dir("mcp");
    let d = dir.to_str().unwrap();
    let (ok, out) = nucleus(
        &["sql", d],
        "CREATE TABLE customers (id INT PRIMARY KEY, email TEXT NOT NULL, region TEXT);\n\
         COMMENT ON TABLE customers IS 'People who buy';\n\
         COMMENT ON COLUMN customers.email IS 'Login e-mail';\n\
         INSERT INTO customers VALUES (1, 'a@x', 'north'), (2, 'b@x', 'south'), (3, 'c@x', 'north');\n",
    );
    assert!(ok, "{out}");
    let requests = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": { "name": "test", "version": "1" } } }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "list_tables", "arguments": {} } }),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
            "name": "describe_table", "arguments": { "table": "customers" } } }),
        json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
            "name": "query", "arguments": {
                "sql": "SELECT region, count(*) AS n FROM customers WHERE id <= $1 GROUP BY region ORDER BY region",
                "params": [3] } } }),
        json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {
            "name": "query", "arguments": { "sql": "COMMIT; DELETE FROM customers" } } }),
        json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {
            "name": "query", "arguments": { "sql": "SELECT emial FROM customers" } } }),
        json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {
            "name": "sample_rows", "arguments": { "table": "customers", "limit": 2 } } }),
        json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {
            "name": "explain", "arguments": { "sql": "SELECT * FROM customers WHERE id = 2" } } }),
        json!({ "jsonrpc": "2.0", "id": 10, "method": "tools/call", "params": {
            "name": "summarize_table", "arguments": { "table": "customers" } } }),
    ];
    let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
    let (ok, out) = nucleus(&["mcp", d], &input);
    assert!(ok, "{out}");
    let replies: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    assert_eq!(replies.len(), 10, "{out}");
    let by_id = |id: u64| replies.iter().find(|r| r["id"] == id).unwrap();
    let content = |id: u64| &by_id(id)["result"]["structuredContent"];
    assert_eq!(by_id(1)["result"]["serverInfo"]["name"], "nucleus");
    assert_eq!(by_id(2)["result"]["tools"].as_array().unwrap().len(), 7);
    assert_eq!(
        content(3)["tables"],
        json!([{ "name": "customers", "comment": "People who buy", "rows": null }])
    );
    let table = content(4);
    assert_eq!(table["comment"], "People who buy");
    let email = &table["columns"][1];
    assert_eq!(
        (&email["name"], &email["comment"]),
        (&json!("email"), &json!("Login e-mail"))
    );
    assert_eq!(
        (&email["nullable"], &table["indexes"][0]["primary"]),
        (&json!(false), &json!(true))
    );
    assert_eq!(content(5)["rows"], json!([["north", 2], ["south", 1]]));
    // Nothing slips out of the read-only tool; errors carry their hint.
    assert_eq!(by_id(6)["result"]["isError"], true);
    let text = by_id(7)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("HINT: Perhaps you meant column \"email\"."),
        "{text}"
    );
    assert_eq!(content(8)["row_count"], 2);
    let plan = content(9)["plan"].to_string();
    assert!(
        plan.contains("Index Scan using primary key on customers"),
        "{plan}"
    );
    // One row a column, from SUMMARIZE.
    let columns = &content(10)["columns"];
    assert_eq!(columns[0]["column_name"], "id");
    assert_eq!(
        (&columns[0]["min"], &columns[0]["max"]),
        (&json!("1"), &json!("3"))
    );
    assert_eq!(columns[2]["approx_unique"], 2);
    assert_eq!(columns[2]["null_percentage"], 0.0);
    // The rows are all still there.
    let (_, out) = nucleus(&["sql", d, "-c", "SELECT count(*) FROM customers"], "");
    assert!(out.contains(" 3 "), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The key hierarchy (PLAN.md §M14.30 K1): an old secrets key file taken
/// into the keyring, wrapped by a root made at first use, and removed —
/// or kept, as the owner sets; `nc_catalog.keys` and `nucleus keys list`
/// show ids, never a key; a root that did not wrap the keyring is refused.
#[test]
fn key_hierarchy() {
    let dir = temp_dir("keys");
    std::fs::create_dir_all(&dir).unwrap();
    let side = |db: &str, what: &str| dir.join(format!("{db}.{what}"));
    let old = "ab".repeat(32);
    for (db, flags, kept) in [
        ("a", vec![], false),
        ("b", vec!["--old-key-files", "keep"], true),
    ] {
        let d = dir.join(db);
        let d = d.to_str().unwrap();
        std::fs::write(side(db, "secrets-key"), format!("{old}\n")).unwrap();
        let mut args = vec!["sql", d, "-c", "CREATE SECRET s VALUE 'x'"];
        args.extend(&flags);
        let (ok, out) = nucleus(&args, "");
        assert!(ok, "{out}");
        assert_eq!(side(db, "secrets-key").exists(), kept, "{db}: {out}");
        let ring = std::fs::read_to_string(side(db, "keyring")).unwrap();
        assert!(ring.starts_with("nucleus keyring 1\n") && ring.contains("\ndek secrets "));
        assert!(!ring.contains(&old), "the old key in the clear");
        assert!(side(db, "root-key").exists());
        let (ok, out) = nucleus(
            &[
                "sql",
                d,
                "-c",
                "SELECT name, role FROM nc_catalog.keys ORDER BY name",
            ],
            "",
        );
        assert!(
            ok && out.contains("root") && out.contains("secrets"),
            "{out}"
        );
        let (ok, out) = nucleus(&["keys", d, "list"], "");
        assert!(
            ok && out.contains("secrets  data") && !out.contains(&old),
            "{out}"
        );
    }
    // Another root: refused, naming both.
    let other = dir.join("other.key");
    std::fs::write(&other, format!("{}\n", "11".repeat(32))).unwrap();
    let d = dir.join("a");
    let (ok, out) = nucleus(
        &[
            "sql",
            d.to_str().unwrap(),
            "--root-key",
            other.to_str().unwrap(),
            "-c",
            "SELECT 1",
        ],
        "",
    );
    assert!(!ok && out.contains("wrapped by the root key"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// PLAN.md §M14.30 K1c: with a root key given, backups are encrypted
/// unasked, and the root alone restores one — the backup's keys going
/// into the new database's keyring, so its secrets open with that root.
#[test]
fn the_root_key_restores_an_encrypted_backup() {
    let dir = temp_dir("root-backup");
    std::fs::create_dir_all(&dir).unwrap();
    let root = dir.join("root.key");
    std::fs::write(&root, format!("{}\n", "5a".repeat(32))).unwrap();
    let (db, repo, new) = (dir.join("db"), dir.join("repo"), dir.join("new"));
    let (db, repo, new, root) = (
        db.to_str().unwrap(),
        repo.to_str().unwrap(),
        new.to_str().unwrap(),
        root.to_str().unwrap(),
    );
    let (ok, out) = nucleus(
        &["sql", db, "--root-key", root],
        &format!(
            "CREATE TABLE t (id INT PRIMARY KEY, v TEXT);\n\
             INSERT INTO t SELECT g, 'Tihany ' || g FROM generate_series(1, 50) g;\n\
             CREATE SECRET s VALUE 'x';\nBACKUP TO '{repo}';\n"
        ),
    );
    assert!(ok, "{out}");
    let m = std::fs::read_to_string(dir.join("repo/000001-full/manifest")).unwrap();
    assert!(
        m.contains("\nencryption chacha20-poly1305 ")
            && m.contains("\nkey backup ")
            && m.contains("\nkey secrets "),
        "{m}"
    );
    // Without the root: refused, saying what opens it.
    let (ok, out) = nucleus(&["restore", repo, new], "");
    assert!(!ok && out.contains("--root-key"), "{out}");
    // With it: restored, the keys taken in.
    let (ok, out) = nucleus(&["restore", repo, new, "--root-key", root], "");
    assert!(ok && out.contains("keys: backup, secrets in"), "{out}");
    let (ok, out) = nucleus(
        &[
            "sql",
            new,
            "--root-key",
            root,
            "-c",
            "SELECT count(*) FROM t UNION ALL SELECT count(*) FROM nc_catalog.keys",
        ],
        "",
    );
    assert!(ok && out.contains("50") && out.contains(" 3"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// PLAN.md §M14.30 K1d: the subjects' keys sealed under the keyring —
/// never in the clear on disk — and a backup's rows of subjects opened
/// again by the root alone and that sealed table.
#[test]
fn the_subject_keys_are_sealed_under_the_keyring() {
    let dir = temp_dir("sealed-subjects");
    std::fs::create_dir_all(&dir).unwrap();
    let root = dir.join("root.key");
    std::fs::write(&root, format!("{}\n", "c3".repeat(32))).unwrap();
    let (db, repo, new) = (dir.join("db"), dir.join("repo"), dir.join("new"));
    let table = dir.join("db.subject-keys");
    let (db, repo, new, root, table_s) = (
        db.to_str().unwrap(),
        repo.to_str().unwrap(),
        new.to_str().unwrap(),
        root.to_str().unwrap(),
        table.to_str().unwrap(),
    );
    let (ok, out) = nucleus(
        &["sql", db, "--root-key", root],
        &format!(
            "CREATE TABLE m (id INT PRIMARY KEY, who TEXT, body TEXT);\n\
             ALTER TABLE m SET SUBJECT (who);\n\
             INSERT INTO m VALUES (1, 'ann', 'Balaton'), (2, 'bob', 'Tisza');\n\
             BACKUP TO '{repo}';\n"
        ),
    );
    assert!(ok, "{out}");
    let sealed = std::fs::read_to_string(&table).unwrap();
    assert!(
        sealed.starts_with("nucleus subject keys sealed 1\n") && !sealed.contains("ann"),
        "{sealed}"
    );
    let m = std::fs::read_to_string(dir.join("repo/000001-full/manifest")).unwrap();
    assert!(m.contains("\nkey subjects "), "{m}");
    // The root and the sealed table: the rows of subjects opened.
    let (ok, out) = nucleus(
        &[
            "restore",
            repo,
            new,
            "--root-key",
            root,
            "--subject-keys",
            table_s,
        ],
        "",
    );
    assert!(
        ok && out.contains("subjects") && out.contains("(2 opened)"),
        "{out}"
    );
    let (ok, out) = nucleus(
        &[
            "sql",
            new,
            "--root-key",
            root,
            "-c",
            "SELECT body FROM m ORDER BY id",
        ],
        "",
    );
    assert!(
        ok && out.contains("Balaton") && out.contains("Tisza"),
        "{out}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
