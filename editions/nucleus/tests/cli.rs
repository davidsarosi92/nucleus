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

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nucleus-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn version() {
    let (ok, out) = nucleus(&["--version"], "");
    assert!(ok && out.starts_with("nucleus "), "{out}");
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
    use std::io::BufRead;

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
    let mut first = String::new();
    std::io::BufReader::new(server.stderr.take().unwrap())
        .read_line(&mut first)
        .unwrap();
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
