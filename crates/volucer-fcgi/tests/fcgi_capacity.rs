//! What is `FCGI_GET_VALUES` worth, per server?
//!
//! The FastCGI spec has a capacity channel: a `GET_VALUES` record asking for
//! `FCGI_MAX_CONNS` and `FCGI_MAX_REQS` is the protocol's way to say how many connections
//! and how many concurrent requests a server can take. PHP populates both from its own
//! pool — `sapi/cgi/cgi_main.c` from `PHP_FCGI_CHILDREN`, `sapi/fpm/fpm/fpm_php.c` from
//! `pm.max_children` — so the question this answers is not whether the numbers exist but
//! what a *client* can do with them.
//!
//! It matters because `external` has no `workers` key: it holds one connection per request
//! in flight and lets the far side's accept loop bound the work. If the far side would
//! publish a number, this backend could know the pool's size without being told. If it will
//! not, the only honest options are "no cap" (today) or "operator-supplied cap".
//!
//! Run with the server already listening:
//!
//! ```text
//! PHP_FCGI_CHILDREN=4 php-cgi -b 127.0.0.1:9000 &
//! FCGI_ADDR=127.0.0.1:9000 cargo test -p volucer-fcgi --test fcgi_capacity -- --nocapture
//! ```
//!
//! Unix-oriented on purpose: the interesting servers are php-fpm and a preforking php-cgi,
//! neither of which exists on Windows. It is a diagnostic, not a gate — every assertion is
//! about *not hanging*, because both answers are legitimate.

use std::time::Duration;

use volucer_fcgi::{Client, Endpoint};

fn addr() -> Option<String> {
    std::env::var("FCGI_ADDR")
        .ok()
        .filter(|value| !value.is_empty())
}

/// Ask a running server what it can take, and print what it said.
///
/// The three outcomes are all real and are reported rather than asserted:
///
/// - **answered** — the client could read the pool's size without being told it, which
///   would make a `workers` key unnecessary *and* unnecessary to guess;
/// - **declined** — `FCGI_UNKNOWN_TYPE`, meaning the server speaks FastCGI but not this
///   request;
/// - **dropped** — the connection closes without a reply. This is the one that decides the
///   question: a client that relied on the answer would have to carry a fallback anyway.
#[test]
fn get_values_reports_whatever_the_server_is_willing_to_say() {
    let Some(addr) = addr() else {
        eprintln!("skipping: set FCGI_ADDR=host:port to a running FastCGI server");
        return;
    };

    let mut client = Client::connect(
        &Endpoint::parse(&addr).expect("FCGI_ADDR is unix:/path or host:port"),
        Some(Duration::from_secs(5)),
    )
    .expect("connect to the server");

    // Both spellings of the same pool's size. `max_connections` is what nginx asks for;
    // the FCGI_MAX_* names are what php's own `fcgi_set_mgmt_var` writes.
    for name in [
        &b"max_connections"[..],
        &b"FCGI_MAX_CONNS"[..],
        &b"FCGI_MAX_REQS"[..],
    ] {
        let printable = String::from_utf8_lossy(name).to_string();
        match client.get_values([name]) {
            Ok(values) => {
                let pairs: Vec<String> = values
                    .into_iter()
                    .map(|(key, value)| {
                        format!(
                            "{}={}",
                            String::from_utf8_lossy(&key),
                            String::from_utf8_lossy(&value)
                        )
                    })
                    .collect();
                println!("  GET_VALUES {printable}: answered {:?}", pairs);
            }
            Err(err) => println!("  GET_VALUES {printable}: declined or dropped ({err})"),
        }
    }
}
