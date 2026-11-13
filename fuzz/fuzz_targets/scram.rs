//! SCRAM-SHA-256 (RFC 5802 / 7677), both sides: the server reading a
//! client's two messages (raw, or a client's own with a password the
//! fuzzer chose — accepted exactly when it is the right one), and the
//! client (a driver) reading a server's.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nucleus_crypto::{ScramClient, ScramServer, ScramVerifier};

const PASSWORD: &str = "nucleus";

fn server(bound: bool) -> ScramServer {
    let v = ScramVerifier::new(PASSWORD, b"salt-salt-salt-1", 4096);
    let s = ScramServer::new(Some(v), b"fake", "server-nonce");
    if bound {
        s.with_channel_binding(b"the-certificate-hash".to_vec())
    } else {
        s
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let (a, b) = nucleus_fuzz::split(rest);
    let (Ok(a), Ok(b)) = (std::str::from_utf8(a), std::str::from_utf8(b)) else {
        return;
    };
    match mode % 3 {
        // A client's messages as they come.
        0 => {
            let mut s = server(mode & 4 != 0);
            if s.first(a).is_ok() {
                let _ = s.finish(b);
            }
            let unknown = ScramServer::new(None, b"fake", "n");
            let _ = unknown.finish(b);
        }
        // A real client with the fuzzer's password and nonce.
        1 => {
            if a.contains(',') || a.is_empty() || !a.bytes().all(|c| c.is_ascii_graphic()) {
                return;
            }
            // HMAC pads a key shorter than its block with zeros, so a
            // password and itself with NULs after are one (as in every
            // SCRAM; SASLprep forbids NUL in a password).
            if b.contains('\0') {
                return;
            }
            let mut c = ScramClient::new(b, a);
            let mut s = server(false);
            let first = c.first();
            let Ok(server_first) = s.first(&first) else {
                panic!("a client's first message refused: {first:?}");
            };
            let fin = c
                .respond(&server_first)
                .expect("the server's first message");
            let ok = s.finish(&fin);
            assert_eq!(ok.is_ok(), b == PASSWORD, "password {b:?}");
            if let Ok(v) = ok {
                c.verify(&v).expect("the server's signature");
            }
        }
        // A server's messages, to a client.
        _ => {
            let mut c = ScramClient::new(PASSWORD, "client-nonce");
            let _ = c.first();
            if c.respond(a).is_ok() {
                let _ = c.verify(b);
            }
        }
    }
});
