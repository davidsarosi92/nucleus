//! The nucleus server: the PostgreSQL frontend/backend protocol (version 3)
//! over [`nucleus_sql::Session`], so `psql` and PostgreSQL drivers connect
//! to nucleus as to a plain PostgreSQL (PLAN.md §M12).
//!
//! Supported: startup (SSL and GSSAPI requests are declined, so clients
//! fall back to plain TCP), trust or cleartext-password authentication, the
//! simple query protocol, and the extended protocol — Parse, Bind,
//! Describe, Execute (with row limits and portal suspension), Sync, Close,
//! Flush — with text and binary formats for `bool`, `int2/4/8`,
//! `float4/8` and `text`.  One thread per connection.
//!
//! Not supported: COPY, cancel requests (accepted and ignored), the
//! `pg_catalog` tables `psql`'s `\d` commands query.

mod protocol;

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nucleus_kv::Kv;

pub use protocol::handle;

/// Server settings.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// If set, clients must send this password (cleartext authentication;
    /// use it on trusted networks only).
    pub password: Option<String>,
}

/// Accepts connections on `listener` forever, one thread each.
pub fn serve(listener: TcpListener, kv: Arc<dyn Kv>, config: Config) -> io::Result<()> {
    let config = Arc::new(config);
    let ids = Arc::new(AtomicU32::new(1));
    for stream in listener.incoming() {
        let stream: TcpStream = match stream {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let (kv, config) = (Arc::clone(&kv), Arc::clone(&config));
        let id = ids.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let _ = stream.set_nodelay(true);
            if let Err(e) = handle(stream, kv, &config, id)
                && e.kind() != io::ErrorKind::UnexpectedEof
            {
                eprintln!("nucleus-server: connection {id}: {e}");
            }
        });
    }
    Ok(())
}
