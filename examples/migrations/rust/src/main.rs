//! The sample project's Rust program (PLAN.md §M14.16 M6): the functions
//! nucleus codegen made of queries.sql (`queries.rs`), on a server whose
//! schema migrations/ made.  Run by ../run.sh.

// Generated (run.sh --generate): rustfmt leaves it as it is made.
#[rustfmt::skip]
mod queries;

use nucleus_client::{Client, Config, Error};

fn main() -> Result<(), Error> {
    let config = Config {
        port: std::env::var("NUCLEUS_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(20035),
        ..Config::default()
    };
    let mut db = Client::connect(config)?;

    // A transaction: the user and their posts, or none of them.
    db.execute("BEGIN", &[])?;
    let ann = queries::create_user(&mut db, 400, "ann@rust.example", Some("Ann"))?.expect("a row");
    assert_eq!(ann.plan, "free");
    queries::add_post(&mut db, 400, 400, "Hello", Some("The first post."))?;
    queries::add_post(&mut db, 401, 400, "Again", None)?;
    db.execute("COMMIT", &[])?;

    let ann = queries::user_by_email(&mut db, "ann@rust.example")?.expect("ann");
    assert_eq!(ann.display_name.as_deref(), Some("Ann"));
    assert!(queries::user_by_email(&mut db, "nobody@rust.example")?.is_none());
    assert_eq!(queries::upgrade(&mut db, ann.id)?, 1);

    let posts: Vec<(String, Option<String>)> = queries::posts_on_plan(&mut db, "pro", 10)?
        .into_iter()
        .filter(|p| p.author == "ann@rust.example")
        .map(|p| (p.title, p.body))
        .collect();
    assert_eq!(
        posts,
        [
            ("Again".to_string(), None),
            ("Hello".to_string(), Some("The first post.".to_string()))
        ]
    );
    let counts = queries::post_counts(&mut db)?;
    assert!(
        counts
            .iter()
            .any(|c| c.email == "ann@rust.example" && c.posts == 2)
    );
    println!("migrations example (rust): ok");
    Ok(())
}
