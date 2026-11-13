//! PostgreSQL's protocol (`nucleus-pg-compat`) from a client's first
//! byte: the startup packet, the simple and extended query protocols
//! (Parse, Bind with text and binary parameters, Describe, Execute),
//! `COPY`'s data, and the SASL exchange.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nucleus_server::Config;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let config = if mode & 1 == 0 {
        Config::trust()
    } else {
        Config::password("nucleus", "nucleus")
    };
    let client = Box::new(nucleus_fuzz::Client::new(rest));
    let _ = nucleus_pg_compat::handle_stream(client, nucleus_fuzz::database(), &config, 1);
});
