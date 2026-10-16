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
