//! The nucleus protocol (`ncp`, DOCS §10) from a client's first byte: its
//! messages, the parameters and rows they carry, `COPY`'s data and the
//! login — with no password asked (so the fuzzer reaches the statements)
//! or with SCRAM-SHA-256 and OAUTHBEARER.
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
    let _ = nucleus_server::handle_native_stream(client, nucleus_fuzz::database(), &config, 1);
});
