//! nucleus from Rust: connect, create, insert with parameters, query, a
//! transaction.  Run a server first (`nucleus server ./mydb`), then
//! `cargo run -p nucleus-example`.

use nucleus_client::{Client, Config, Error, Value};

fn main() -> Result<(), Error> {
    // The defaults reach a local server: 127.0.0.1:20035, user/password nucleus.
    let mut config = Config::default();
    if let Some(port) = std::env::var("NUCLEUS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
    {
        config.port = port;
    }
    let mut db = Client::connect(config)?;
    println!("connected to {}", db.server_version());

    db.execute("DROP TABLE IF EXISTS rust_orders", &[])?;
    db.execute(
        "CREATE TABLE rust_orders (id INT PRIMARY KEY, customer TEXT NOT NULL, total NUMERIC(10, 2), tags TEXT[])",
        &[],
    )?;
    let total = |cents: i128| Value::Numeric {
        mantissa: cents,
        scale: 2,
    };
    db.execute(
        "INSERT INTO rust_orders VALUES ($1, $2, $3, $4)",
        &[
            Value::Int(1),
            "Anna".into(),
            total(1999),
            Value::Array(vec!["new".into()]),
        ],
    )?;

    // A transaction: both statements or neither.
    db.execute("BEGIN", &[])?;
    db.execute(
        "INSERT INTO rust_orders VALUES ($1, $2, $3, $4)",
        &[
            Value::Int(2),
            "Béla".into(),
            total(12000),
            Value::Array(vec!["vip".into(), "repeat".into()]),
        ],
    )?;
    db.execute(
        "UPDATE rust_orders SET total = total * 0.9 WHERE id = 2",
        &[],
    )?;
    db.execute("COMMIT", &[])?;

    let result = db.execute(
        "SELECT id, customer, total, tags FROM rust_orders ORDER BY id",
        &[],
    )?;
    for row in &result.rows {
        let cells: Vec<String> = result
            .columns
            .iter()
            .zip(row)
            .map(|(c, v)| format!("{}={v}", c.name))
            .collect();
        println!("{}", cells.join("  "));
    }

    match db.execute(
        "INSERT INTO rust_orders (id, customer) VALUES (1, 'dup')",
        &[],
    ) {
        Err(Error::Server(e)) => println!("as expected: {} {}", e.code, e.message),
        other => println!("unexpected: {other:?}"),
    }
    db.execute("DROP TABLE rust_orders", &[])?;
    db.close()?;
    Ok(())
}
