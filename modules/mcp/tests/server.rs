//! The MCP server against a database of its own: what it sends the
//! database, and what it answers the client.

use nucleus_mcp::{Answer, Database, Failure, Options, Server};
use serde_json::{Value, json};

/// Records what it is asked; answers each statement with one row of its
/// text (or a failure for `fail`).
#[derive(Default)]
struct Fake {
    log: Vec<(String, bool)>,
}

impl Database for Fake {
    fn run(&mut self, sql: &str, read_only: bool) -> Vec<Result<Answer, Failure>> {
        self.log.push((sql.to_string(), read_only));
        if sql.contains("fail") {
            return vec![Err(Failure {
                code: "42703".into(),
                message: "column \"fail\" does not exist".into(),
                hint: Some("Perhaps you meant column \"fall\".".into()),
            })];
        }
        if sql.starts_with("SELECT") || sql.starts_with("EXPLAIN") {
            let rows = (0..5).map(|i| vec![Some(format!("{i}")), None]).collect();
            return vec![Ok(Answer::Rows {
                columns: vec!["n".into(), "nothing".into()],
                types: vec!["integer".into(), "text".into()],
                rows,
            })];
        }
        vec![Ok(Answer::Command(
            sql.split(' ').next().unwrap_or("").to_string(),
        ))]
    }
}

fn call(s: &mut Server, id: u64, method: &str, params: Value) -> Value {
    let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let reply = s.handle(&msg.to_string()).expect("a reply");
    assert_eq!(reply["id"], id);
    reply
}

#[test]
fn initialize_and_list_tools() {
    let mut db = Fake::default();
    let mut s = Server::new(&mut db, Options::default());
    let r = call(
        &mut s,
        1,
        "initialize",
        json!({ "protocolVersion": "2025-03-26" }),
    );
    assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(r["result"]["serverInfo"]["name"], "nucleus");
    // A version it does not know: its newest.
    let r = call(
        &mut s,
        2,
        "initialize",
        json!({ "protocolVersion": "1999-01-01" }),
    );
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    // A notification has no answer.
    assert!(
        s.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none()
    );
    let tools = call(&mut s, 3, "tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "list_tables",
            "describe_table",
            "sample_rows",
            "query",
            "explain",
            "lint"
        ]
    );
    assert_eq!(call(&mut s, 4, "ping", json!({}))["result"], json!({}));
    assert_eq!(call(&mut s, 5, "nope", json!({}))["error"]["code"], -32601);
    assert_eq!(s.handle("{not json").unwrap()["error"]["code"], -32700);
}

#[test]
fn reads_are_read_only_and_limited() {
    let mut db = Fake::default();
    let options = Options {
        max_rows: 3,
        timeout_ms: 250,
        ..Options::default()
    };
    {
        let mut s = Server::new(&mut db, options);
        let r = call(
            &mut s,
            1,
            "tools/call",
            json!({ "name": "query", "arguments": { "sql": "SELECT n FROM t WHERE a = $1", "params": ["it's"] } }),
        );
        let out = &r["result"]["structuredContent"];
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(out["row_count"], 5);
        assert_eq!(out["truncated"], true);
        assert_eq!(out["rows"], json!([[0, null], [1, null], [2, null]]));
        // An error, with its hint, is the tool's — not the protocol's.
        let r = call(
            &mut s,
            2,
            "tools/call",
            json!({ "name": "query", "arguments": { "sql": "SELECT fail" } }),
        );
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(
            r["result"]["content"][0]["text"],
            "ERROR: column \"fail\" does not exist (42703)\nHINT: Perhaps you meant column \"fall\"."
        );
        // No write tool unless allowed.
        let r = call(
            &mut s,
            3,
            "tools/call",
            json!({ "name": "execute", "arguments": { "sql": "DELETE FROM t" } }),
        );
        assert_eq!(r["error"]["code"], -32602);
        let r = call(
            &mut s,
            4,
            "tools/call",
            json!({ "name": "query", "arguments": {} }),
        );
        assert_eq!(r["result"]["isError"], true);
    }
    let log: Vec<(&str, bool)> = db.log.iter().map(|(s, r)| (s.as_str(), *r)).collect();
    assert_eq!(
        log,
        [
            ("SET statement_timeout = '250ms'", false),
            ("BEGIN READ ONLY", false),
            ("SELECT n FROM t WHERE a = 'it''s'", true),
            ("ROLLBACK", false),
            ("BEGIN READ ONLY", false),
            ("SELECT fail", true),
            ("ROLLBACK", false),
        ]
    );
}

#[test]
fn writes_when_allowed() {
    let mut db = Fake::default();
    let options = Options {
        allow_writes: true,
        timeout_ms: 0,
        ..Options::default()
    };
    {
        let mut s = Server::new(&mut db, options);
        let names = call(&mut s, 1, "tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(names, 7);
        let r = call(
            &mut s,
            2,
            "tools/call",
            json!({ "name": "execute", "arguments": { "sql": "INSERT INTO t VALUES (1)" } }),
        );
        assert_eq!(
            r["result"]["structuredContent"],
            json!({ "command": "INSERT" })
        );
    }
    assert_eq!(db.log, [("INSERT INTO t VALUES (1)".to_string(), false)]);
}
