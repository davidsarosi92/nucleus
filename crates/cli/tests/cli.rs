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

    // Over the network, against `nucleus server` on a free port.
    let mut server = Command::new(env!("CARGO_BIN_EXE_nucleus"))
        .args(["server", d, "--listen", "127.0.0.1:0"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    std::io::BufReader::new(server.stderr.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let addr = first
        .split_whitespace()
        .find(|w| w.starts_with("127.0.0.1:"))
        .unwrap_or_else(|| panic!("no address in {first:?}"))
        .to_string();
    let (ok, out) = ncsql(&["-h", &addr, "-c", "SELECT name FROM dept"], "");
    assert!(ok && out.contains(" eng "), "{out}");
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
        ok && out.contains("DELETE 1") && out.contains(" 0 "),
        "{out}"
    );
    assert!(out.contains("on 127.0.0.1:"), "{out}");
    server.kill().unwrap();
    server.wait().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
