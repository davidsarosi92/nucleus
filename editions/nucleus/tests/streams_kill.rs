//! Streams, a task queue and a changefeed against a server killed
//! mid-run (PLAN.md §M16.5 S7): consumers committing what they consume
//! with their position and workers finishing tasks with their work, over
//! the network, while `nucleus server` is killed (`SIGKILL` on Unix) and
//! started again three times.  The clients reconnect and go on; at the
//! end every entry was written once, every task's work done once, and
//! the changefeed holds exactly the work's inserts.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nucleus_client::{Client, Config, Error, TxnStatus, Value};

const ENTRIES: i64 = 20_000;
const TASKS: i64 = 2000;

/// The server, killed when dropped.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `nucleus server` on `dir`; its port.
fn start(dir: &str) -> (Server, u16) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nucleus"))
        .args(["server", dir, "--listen", "127.0.0.1:0"])
        .env_remove("NUCLEUS_PASSWORD")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Its first lines: a recovery's report perhaps, then where it listens.
    let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    while !line.contains("127.0.0.1:") {
        line.clear();
        if std::io::BufRead::read_line(&mut stderr, &mut line).unwrap() == 0 {
            break;
        }
    }
    let port = line
        .split_whitespace()
        .find_map(|w| w.strip_prefix("127.0.0.1:"))
        .and_then(|p| {
            p.trim_end_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .ok()
        })
        .unwrap_or_else(|| panic!("no address in {line:?}"));
    (Server(child), port)
}

fn connect(port: &Mutex<u16>) -> Option<Client> {
    Client::connect(Config {
        port: *port.lock().unwrap(),
        connect_timeout: Duration::from_secs(2),
        ..Config::default()
    })
    .ok()
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        Value::Integer(n) => i64::from(*n),
        other => panic!("not an integer: {other:?}"),
    }
}

fn one(c: &mut Client, sql: &str) -> i64 {
    int(&c.rows(sql, &[]).unwrap()[0][0])
}

fn ints(c: &mut Client, sql: &str) -> Vec<i64> {
    c.rows(sql, &[])
        .unwrap()
        .iter()
        .map(|r| int(&r[0]))
        .collect()
}

/// A consumer's batch: consumed, written, committed together.  Whether
/// the group has consumed every entry.
fn consume(c: &mut Client) -> Result<bool, Error> {
    let got = c.stream_of("events").process("g", 25, 0, |c, rows| {
        let values: Vec<String> = rows
            .iter()
            // seq, time, n.
            .map(|r| format!("({}, {})", int(&r[0]), int(&r[2])))
            .collect();
        c.execute(
            &format!("INSERT INTO done VALUES {}", values.join(", ")),
            &[],
        )
        .map(|_| ())
    })?;
    Ok(got == 0 && c.stream_of("events").position("g")? == ENTRIES)
}

/// A worker's task: taken, its work and its finish committed together.
/// Whether no task is left.
fn work(c: &mut Client) -> Result<bool, Error> {
    let got = c.rows("TAKE n FROM jobs LEASE '1 second'", &[])?;
    let Some(task) = got.first() else {
        return Ok(int(&c.rows("SELECT count(*) FROM jobs", &[])?[0][0]) == 0);
    };
    let (id, attempt, n) = (int(&task[0]), int(&task[1]), int(&task[2]));
    c.execute("BEGIN", &[])?;
    let r = c
        .execute(&format!("INSERT INTO work VALUES ({n})"), &[])
        .and_then(|_| c.execute(&format!("FINISH TASK {id} OF jobs ATTEMPT {attempt}"), &[]))
        .and_then(|_| c.execute("COMMIT", &[]));
    if r.is_err() && c.status() != TxnStatus::Idle {
        // A lost lease (55000, 40001): the work rolled back.
        let _ = c.execute("ROLLBACK", &[]);
    }
    Ok(false)
}

/// A client that reconnects whenever its connection fails, until `step`
/// says it is done.
fn client(
    port: Arc<Mutex<u16>>,
    stop: Arc<AtomicBool>,
    step: fn(&mut Client) -> Result<bool, Error>,
) {
    let mut c: Option<Client> = None;
    while !stop.load(Ordering::Relaxed) {
        let Some(conn) = c.as_mut() else {
            c = connect(&port);
            if c.is_none() {
                std::thread::sleep(Duration::from_millis(20));
            }
            continue;
        };
        match step(conn) {
            Ok(true) => return,
            Ok(false) => {}
            Err(Error::Server(_)) => {}
            Err(_) => c = None,
        }
    }
}

#[test]
fn streams_and_queues_survive_a_killed_server() {
    let dir = std::env::temp_dir().join(format!("nucleus-streams-kill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let d = dir.to_str().unwrap().to_string();
    let (mut server, p) = start(&d);
    let port = Arc::new(Mutex::new(p));
    {
        let mut c = connect(&port).expect("connected");
        for sql in [
            "CREATE STREAM events (n INT NOT NULL)",
            "CREATE TABLE done (seq BIGINT NOT NULL, n INT NOT NULL)",
            "CREATE QUEUE jobs (n INT NOT NULL) WITH (max_attempts = 100)",
            "CREATE TABLE work (n INT NOT NULL)",
            "CREATE CHANGEFEED work_feed ON work",
        ] {
            c.execute(sql, &[]).unwrap();
        }
        for from in (1..=ENTRIES).step_by(500) {
            c.execute(
                &format!(
                    "INSERT INTO events (n) SELECT g FROM generate_series({from}, {}) g",
                    from + 499
                ),
                &[],
            )
            .unwrap();
        }
        c.execute(
            &format!("INSERT INTO jobs (n) SELECT g FROM generate_series(1, {TASKS}) g"),
            &[],
        )
        .unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for step in [consume, consume, work, work, work] {
        let (port, stop) = (port.clone(), stop.clone());
        threads.push(std::thread::spawn(move || client(port, stop, step)));
    }
    // Killed at a quarter of the way, half and three quarters (of the
    // entries or of the tasks, whichever comes first).
    let mut interrupted = Vec::new();
    for k in 1..=3 {
        let until = Instant::now() + Duration::from_secs(60);
        let (consumed, left) = loop {
            assert!(
                Instant::now() < until && threads.iter().all(|t| !t.is_finished()),
                "no progress to kill {k} (a client failed?)"
            );
            if let Some(progress) = connect(&port).and_then(|mut m| {
                let consumed = m.rows("SELECT stream_position('events', 'g')", &[]).ok()?;
                let left = m.rows("SELECT count(*) FROM jobs", &[]).ok()?;
                Some((int(&consumed[0][0]), int(&left[0][0])))
            }) && (progress.0 >= k * ENTRIES / 4 || progress.1 <= TASKS - k * TASKS / 4)
            {
                break progress;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        interrupted.push(consumed < ENTRIES && left > 0);
        drop(server);
        let (s, p) = start(&d);
        server = s;
        *port.lock().unwrap() = p;
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    while threads.iter().any(|t| !t.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().unwrap();
    }
    assert!(
        interrupted[0],
        "the first kill came after the work: {interrupted:?}"
    );

    let mut c = connect(&port).expect("connected");
    // Every entry consumed, written once, as it was appended.
    assert_eq!(
        one(&mut c, "SELECT stream_position('events', 'g')"),
        ENTRIES
    );
    assert_eq!(
        ints(&mut c, "SELECT seq FROM events ORDER BY seq"),
        (1..=ENTRIES).collect::<Vec<_>>()
    );
    assert_eq!(
        ints(&mut c, "SELECT seq FROM done ORDER BY seq"),
        (1..=ENTRIES).collect::<Vec<_>>(),
        "consumed entries written exactly once"
    );
    assert_eq!(
        one(
            &mut c,
            "SELECT count(*) FROM done d JOIN events e ON e.seq = d.seq AND e.n = d.n"
        ),
        ENTRIES
    );
    // Every task's work done once, none left, none dead.
    assert_eq!(
        ints(&mut c, "SELECT n FROM work ORDER BY n"),
        (1..=TASKS).collect::<Vec<_>>(),
        "each task's work exactly once"
    );
    assert_eq!(one(&mut c, "SELECT count(*) FROM jobs"), 0);
    assert_eq!(one(&mut c, "SELECT count(*) FROM jobs_dead"), 0);
    // The changefeed: the work's inserts, dense, each once.
    assert_eq!(
        ints(&mut c, "SELECT seq FROM work_feed ORDER BY seq"),
        (1..=TASKS).collect::<Vec<_>>()
    );
    drop(c);
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}
