//! `[rpc]`: the listener that gives a FastCGI application a call plane.
//!
//! The rules these cover are the ones that make the listener safe rather than merely present: a
//! grant and a listener have to arrive together, a backend that grants nothing is handed no address,
//! and the address itself may not be a wildcard bind.

use super::*;

fn config(text: &str) -> Config {
    Config::from_toml(text).expect("parses")
}

/// The smallest deployment that serves the call plane: one `external` backend granting a cache.
const WORKING: &str = r#"
[server]
http = "127.0.0.1:8080"
default_backend = "app"

[backends.app]
kind = "external"
socket = "127.0.0.1:9000"
services = ["kv.cache"]

[kv.cache]
driver = "memory"

[rpc]
listen = "unix:/run/volucer/rpc.sock"
"#;

#[test]
fn a_grant_and_a_listener_arrive_together() {
    // The pair rule in both directions, which is the whole reason `[rpc]` is validated here rather
    // than on its own: each half alone is a key that looks active and cannot work. A listener with
    // no grant is a socket that refuses every call; a grant with no listener is an application told
    // nothing.
    let working = config(WORKING);
    working
        .validate()
        .expect("a grant beside a listener is valid");
    assert_eq!(
        working.rpc_grants().get("app"),
        Some(&vec!["kv.cache".to_string()])
    );

    let without_listener =
        config(&WORKING.replace("[rpc]\nlisten = \"unix:/run/volucer/rpc.sock\"\n", ""));
    let err = without_listener
        .validate()
        .expect_err("a grant with no listener is refused");
    let text = err.to_string();
    assert!(
        text.contains("backends.app") && text.contains("[rpc]"),
        "the refusal names the backend that granted and the table that is missing: {text}"
    );

    let without_grant = config(&WORKING.replace("services = [\"kv.cache\"]\n", ""));
    let err = without_grant
        .validate()
        .expect_err("a listener with no grant is refused");
    let text = err.to_string();
    assert!(
        text.contains("[rpc]") && text.contains("services"),
        "the refusal names the table and the key that would make it useful: {text}"
    );
}

#[test]
fn a_backend_that_grants_nothing_is_told_nothing() {
    // The security rule as a *value*, not as a comment: `rpc_grants` is what both the listener's
    // token map and the request path's injection are built from, so a backend missing from it is a
    // backend whose application is handed neither an address nor a token.
    let mut config = config(WORKING);
    config.backends.insert(
        "public".to_string(),
        BackendConfig::External(ExternalConfig {
            socket: "127.0.0.1:9001".to_string(),
            max_connections: None,
            services: Vec::new(),
        }),
    );
    let grants = config.rpc_grants();
    assert!(grants.contains_key("app"));
    assert!(
        !grants.contains_key("public"),
        "a backend with no services is not a grant"
    );
}

#[test]
fn a_worker_backend_is_not_in_the_grants_it_already_reaches_over_its_channel() {
    // The exclusion that keeps the feature honest: a `worker`'s registry is installed on its own
    // channels, and a token for it would be a second way in to the same namespaces that nothing
    // needs. It is also what keeps `[rpc]`'s "no grant" refusal from firing for a deployment that
    // only ever had workers — which would be a table every existing worker deployment could not
    // add without also granting something to a CGI backend it does not have.
    let text = r#"
[server]
http = "127.0.0.1:8080"
default_backend = "app"

[backends.app]
kind = "worker"
command = "php worker.php"
services = ["kv.cache"]

[kv.cache]
driver = "memory"
"#;
    let config = config(text);
    assert!(
        config.rpc_grants().is_empty(),
        "a worker reaches its namespaces over its channel, so it is not an [rpc] grant"
    );
    assert!(!config.serves_rpc());
    config
        .validate()
        .expect("a worker deployment is valid and needs no [rpc]");
}

#[test]
fn the_two_cgi_kinds_and_an_upstream_all_grant() {
    // The three kinds this feature is for, asserted together because they share one validation path
    // and a `match` that forgot one would still pass a test written per kind against a *worker*.
    let text = r#"
[server]
http = "127.0.0.1:8080"
default_backend = "fpm"

[backends.fpm]
kind = "external"
socket = "127.0.0.1:9000"
services = ["kv.cache", "lock"]

[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
services = ["kv.cache"]

[backends.node]
kind = "upstream"
origin = "http://127.0.0.1:3000"
services = ["jobs"]

[kv.cache]
driver = "memory"
[lock]
driver = "memory"
[jobs]
pipelines = {}

[rpc]
listen = "unix:/run/volucer/rpc.sock"
"#;
    let config = config(text);
    config.validate().expect("all three kinds may grant");
    let grants = config.rpc_grants();
    assert_eq!(
        grants.get("fpm").map(Vec::as_slice),
        Some(&["kv.cache".to_string(), "lock".to_string()][..])
    );
    assert_eq!(
        grants.get("cgi").map(Vec::as_slice),
        Some(&["kv.cache".to_string()][..])
    );
    assert_eq!(
        grants.get("node").map(Vec::as_slice),
        Some(&["jobs".to_string()][..])
    );
}

#[test]
fn a_service_the_deployment_does_not_have_is_refused_by_name() {
    // The typo this exists to catch, and it is the same message a worker gets: one validator, so
    // `services = ["kv.session"]` against a `[kv.sessions]` table cannot be caught for a worker and
    // silently accepted for an `external` backend.
    let text = WORKING.replace("kv.cache\"]", "kv.session\"]");
    let err = config(&text)
        .validate()
        .expect_err("a storage that does not exist is refused");
    let message = err.to_string();
    assert!(
        message.contains("kv.session") && message.contains("kv.cache"),
        "the refusal names the missing storage and lists the ones the deployment has: {message}"
    );
}

#[test]
fn a_wildcard_bind_is_refused_and_a_loopback_and_a_path_are_not() {
    // The rule the status listener already follows, with a stronger reason: every call on this
    // socket mutates something, and an unauthenticated mutation surface on every interface is never
    // what an operator meant.
    let broad = WORKING.replace("unix:/run/volucer/rpc.sock", "0.0.0.0:6001");
    let err = config(&broad)
        .validate()
        .expect_err("a wildcard bind is refused");
    let message = err.to_string();
    assert!(
        message.contains("every interface"),
        "the refusal says what is wrong with the address: {message}"
    );

    // Loopback and a unix path are both fine, and the two are asserted together so a fix that
    // refused one of them by accident is visible.
    for address in ["127.0.0.1:6001", "unix:/run/volucer/rpc.sock"] {
        let text = WORKING.replace("unix:/run/volucer/rpc.sock", address);
        config(&text)
            .validate()
            .unwrap_or_else(|err| panic!("{address} must be accepted: {err}"));
    }
}

#[test]
fn the_three_zeroes_are_each_refused_rather_than_read_as_unlimited() {
    // Every one of them has an `Option` or a default that already says "no limit", so a zero is a
    // value an operator wrote meaning something the key cannot express — and each would be
    // discovered at the worst moment rather than at startup.
    for (key, needle) in [
        ("max_connections", "no call could ever be served"),
        ("idle_timeout_seconds", "never be closed"),
        ("call_timeout_seconds", "abandoned the moment it was sent"),
    ] {
        let text = format!("{WORKING}{key} = 0\n");
        let err = config(&text)
            .validate()
            .expect_err(&format!("{key} = 0 must be refused"));
        let message = err.to_string();
        assert!(
            message.contains(key),
            "{key}: the refusal names the key: {message}"
        );
        assert!(
            message.contains(needle),
            "{key}: the refusal says what the zero would do: {message}"
        );
    }
}

#[test]
fn the_defaults_are_the_documented_ones() {
    // Asserted rather than left in a doc comment, because the idle timeout is the value an operator
    // reasons about when deciding whether to set it at all.
    let config = config(WORKING);
    let rpc = config.rpc.expect("[rpc] is present");
    assert_eq!(rpc.idle_timeout_seconds, 300);
    assert_eq!(rpc.call_timeout_seconds, 10);
    assert_eq!(rpc.max_connections, None);
    assert_eq!(rpc.idle_timeout().as_secs(), 300);
    assert_eq!(rpc.call_timeout().as_secs(), 10);
}

#[test]
fn a_misspelled_key_is_refused_rather_than_ignored() {
    // `deny_unknown_fields`, the rule every table in this file follows: `idle_timeout` for
    // `idle_timeout_seconds` is a deployment that believes it set a deadline.
    let err = Config::from_toml(&format!("{WORKING}idle_timeout = 5\n"))
        .expect_err("an unknown key is refused at parse time");
    assert!(
        err.to_string().contains("idle_timeout"),
        "the parse error names the key: {err}"
    );
}
