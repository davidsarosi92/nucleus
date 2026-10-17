//! `geo-demo`: the `geo` module in use.
//!
//! - `geo-demo <dir>` — embedded: opens (or creates) a database in `<dir>`
//!   and runs a few statements with the module registered.
//! - `geo-demo serve <dir> [addr]` — a custom build of the server: the
//!   nucleus protocol (default `127.0.0.1:20035`) whose sessions have the
//!   module; connect with `ncsql` or any driver.

use std::net::TcpListener;
use std::sync::Arc;

use nucleus_example_extension::Geo;
use nucleus_kv::{Database, Kv, Options};
use nucleus_server::{Config, Ncp, serve_front_end};
use nucleus_sql::ext::{Output, Registry, Session};

fn registry() -> Arc<Registry> {
    let mut r = Registry::builtin();
    r.add(&Geo).expect("geo registers");
    Arc::new(r)
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [serve, dir, rest @ ..] if serve == "serve" => {
            let addr = rest.first().map_or("127.0.0.1:20035", String::as_str);
            let db = Database::open(dir, Options::default()).map_err(|e| e.to_string())?;
            let listener = TcpListener::bind(addr).map_err(|e| e.to_string())?;
            eprintln!("geo-demo: nucleus protocol on {addr}, with the geo module");
            let config = Config::default().with_registry(registry());
            serve_front_end(Arc::new(Ncp), listener, Arc::new(db), config)
                .map_err(|e| e.to_string())
        }
        [dir] => demo(dir),
        _ => Err("usage: geo-demo <dir> | geo-demo serve <dir> [addr]".into()),
    }
}

fn demo(dir: &str) -> Result<(), String> {
    let db: Arc<dyn Kv> =
        Arc::new(Database::open(dir, Options::default()).map_err(|e| e.to_string())?);
    let mut s = Session::with_registry(db, registry());
    for sql in [
        "CREATE TABLE IF NOT EXISTS shops (id INT PRIMARY KEY, name TEXT, at geopoint)",
        "DELETE FROM shops",
        "INSERT INTO shops VALUES (1, 'Pest', '(47.4979,19.0402)'), (2, 'Buda', '(47.4925,19.0513)'), (3, 'Wien', '(48.2082,16.3738)')",
        // The distance, nearest first.
        "SELECT name, round((at <-> '(47.5,19.05)')::numeric, 2) AS km FROM shops ORDER BY at <-> '(47.5,19.05)'",
        // The catalog view.
        "SELECT c.name, round((c.at <-> s.at)::numeric) AS km_from_pest FROM geo.cities c, shops s WHERE s.id = 1 ORDER BY 2",
        // An ordering taught in SQL, by the module's function.
        "ALTER TYPE geopoint ADD ORDERING by_geohash BY geohash(value)",
        "SELECT name, geohash(at) FROM shops ORDER BY at USING by_geohash",
        "ALTER TYPE geopoint DROP ORDERING by_geohash",
        // What it cannot do is refused, naming the missing ability.
        "SELECT name FROM shops ORDER BY at",
    ] {
        println!("> {sql}");
        match s.execute(sql) {
            Ok(outs) => {
                for out in outs {
                    match out {
                        Output::Rows { rows, .. } => {
                            for r in rows {
                                let cells: Vec<String> = r.iter().map(|v| v.render()).collect();
                                println!("  {}", cells.join(" | "));
                            }
                        }
                        Output::Command(tag) => println!("  {tag}"),
                    }
                }
            }
            Err(e) => println!("  error: {e}"),
        }
    }
    Ok(())
}
