//! The status endpoint, the worker routes it may carry, and trace export.

use super::*;

#[test]
fn status_defaults_its_unavailable_code() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[status]
address = "127.0.0.1:2114"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("valid");
    assert_eq!(config.status.expect("present").unavailable_status_code, 503);
}

/// The mutation routes are off unless the file says otherwise, and that default is the feature.
///
/// A deployment that configured `[status]` for an orchestrator and then upgraded must not gain a
/// way to recycle its workers. The assertion is on the *default*, because that is the half an
/// upgrade cannot change.
#[test]
fn the_worker_routes_are_off_by_default() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[status]
address = "127.0.0.1:2114"
"#;
    let config = Config::from_toml(text).expect("config parses");
    assert!(
        !config.status.expect("present").allow_worker_reset,
        "a [status] table that says nothing about the mutation routes must not enable them"
    );
}

/// A wildcard bind plus a mutation endpoint is refused, and the message names the fix.
///
/// Not a warning: a `POST` with no authentication that recycles a worker, on an address that
/// accepts connections from anywhere, is a remote kill switch for the application — and the
/// operator who wrote `0.0.0.0` was almost certainly thinking about the *readiness probe* being
/// reachable, not about this.
#[test]
fn a_wildcard_status_address_cannot_carry_the_worker_routes() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[status]
address = "0.0.0.0:2114"
allow_worker_reset = true
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must be refused");
    let message = err.to_string();
    assert!(message.contains("allow_worker_reset"), "{message}");
    assert!(
        message.contains("every interface"),
        "the message has to say what is wrong with the address: {message}"
    );

    // And the same address without the flag is ordinary: a readiness probe on a wildcard bind is
    // a deployment somebody chose, and refusing it would be refusing the wrong thing.
    let text = text.replace("allow_worker_reset = true", "");
    let config = Config::from_toml(&text).expect("config parses");
    config
        .validate()
        .expect("a wildcard [status] without the mutation routes is legal");
}

/// A loopback or a specific private address is accepted, which is the whole of what the check
/// above is *not* refusing.
#[test]
fn a_specific_status_address_may_carry_the_worker_routes() {
    for address in ["127.0.0.1:2114", "10.1.2.3:2114", "[::1]:2114"] {
        let text = format!(
            r#"
[server]
http = "127.0.0.1:8080"
[status]
address = "{address}"
allow_worker_reset = true
"#
        );
        let config = Config::from_toml(&text).expect("config parses");
        config.validate().expect("a specific address is fine");
    }
}

/// A status *and* a metrics endpoint is a legal deployment: a container orchestrator polls
/// readiness while something else scrapes. Neither is a listener in the request path, so neither
/// satisfies the "no listener configured" refusal on its own.
#[test]
fn a_metrics_listener_alone_is_not_a_listener() {
    let text = r#"
[metrics]
address = "127.0.0.1:2112"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("no listener configured"), "{err}");
}

/// The telemetry table is off unless written — the same convention `[metrics]` and
/// `[server.access_log]` follow, so an existing deployment is not changed by upgrading.
#[test]
fn telemetry_is_absent_unless_the_table_is_written() {
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n";
    let config = Config::from_toml(text).expect("config parses");
    assert!(config.telemetry.is_none());
}

#[test]
fn a_telemetry_table_defaults_everything_but_its_endpoint() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[telemetry]
endpoint = "http://127.0.0.1:4318"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config
        .validate()
        .expect("a minimal telemetry table is valid");
    let telemetry = config.telemetry.expect("present");
    assert_eq!(telemetry.service_name, "volucer");
    assert_eq!(telemetry.timeout_ms, 10_000);
    assert_eq!(telemetry.batch_size, 512);
    assert_eq!(telemetry.interval_ms, 5_000);
    assert!(telemetry.headers.is_empty());
}

/// An endpoint that is not an origin is refused, and a bare `host:port` is the case that
/// matters: it is what `[metrics].address` takes, so an operator writing the telemetry endpoint
/// by analogy produces a value that parses as a *listener* and cannot be dialled as HTTP.
#[test]
fn rejects_a_telemetry_endpoint_that_is_not_an_origin() {
    for bad in ["127.0.0.1:4318", "unix:/run/otel.sock", "localhost", ""] {
        let text =
            format!("[server]\nhttp = \"127.0.0.1:8080\"\n[telemetry]\nendpoint = \"{bad}\"\n");
        let config = Config::from_toml(&text).expect("config parses");
        let err = config
            .validate()
            .expect_err(&format!("{bad:?} must be refused"));
        assert!(err.to_string().contains("telemetry.endpoint"), "{err}");
    }
}

/// **The shapes that must be accepted**, because a complete `/v1/traces` URL is what collector
/// documentation quotes verbatim and the first version of this validation rejected it: the
/// exporter trimmed the path while validation checked the raw string, so the two disagreed about
/// the same configuration and only one of them was documented. Fixed by sharing one normaliser
/// (`TelemetryConfig::base_endpoint`) between the two.
#[test]
fn accepts_every_spelling_of_the_same_endpoint() {
    for spelling in [
        "http://127.0.0.1:4318",
        "http://127.0.0.1:4318/",
        "http://127.0.0.1:4318/v1/traces",
        // A trailing slash *after* the path, which is a shape somebody writes.
        "http://127.0.0.1:4318/v1/traces/",
        "https://collector.example:4318",
    ] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n[telemetry]\nendpoint = \"{spelling}\"\n"
        );
        let config = Config::from_toml(&text).expect("config parses");
        config
            .validate()
            .unwrap_or_else(|err| panic!("{spelling:?} must be accepted: {err}"));

        let telemetry = config.telemetry.expect("present");
        let base = telemetry.base_endpoint();
        assert!(
            !base.ends_with("/v1/traces"),
            "{spelling:?} left the protocol path in {base:?}, so the export would be posted to \
             /v1/traces/v1/traces"
        );
        assert!(
            !base.ends_with('/'),
            "{spelling:?} left a trailing slash in {base:?}"
        );
    }
}

/// A zero deadline, a zero batch and an empty service name are three ways to configure
/// something that parses and can never work, so each is refused by name rather than accepted
/// and discovered by a background thread that drops every span.
#[test]
fn rejects_telemetry_settings_that_cannot_work() {
    let base =
        "[server]\nhttp = \"127.0.0.1:8080\"\n[telemetry]\nendpoint = \"http://127.0.0.1:4318\"\n";

    let text = format!("{base}timeout_ms = 0\n");
    let err = Config::from_toml(&text)
        .expect("config parses")
        .validate()
        .expect_err("a zero deadline must be refused");
    assert!(err.to_string().contains("timeout_ms"), "{err}");

    let text = format!("{base}batch_size = 0\n");
    let err = Config::from_toml(&text)
        .expect("config parses")
        .validate()
        .expect_err("a zero batch must be refused");
    assert!(err.to_string().contains("batch_size"), "{err}");

    let text = format!("{base}service_name = \"\"\n");
    let err = Config::from_toml(&text)
        .expect("config parses")
        .validate()
        .expect_err("an empty service name must be refused");
    assert!(err.to_string().contains("service_name"), "{err}");
}

/// A header name that is not a token is refused here rather than at the first export, where the
/// failure would be a dropped batch instead of a message naming the key.
#[test]
fn rejects_a_telemetry_header_name_that_is_not_a_token() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[telemetry]
endpoint = "http://127.0.0.1:4318"
[telemetry.headers]
"X-Api Key" = "secret"
"#;
    let err = Config::from_toml(text)
        .expect("config parses")
        .validate()
        .expect_err("a space is not a token character");
    assert!(err.to_string().contains("telemetry.headers"), "{err}");

    // And a real vendor header is accepted, spelled the way their documentation spells it.
    let text = r#"
[server]
http = "127.0.0.1:8080"
[telemetry]
endpoint = "http://127.0.0.1:4318"
[telemetry.headers]
"x-honeycomb-team" = "abc123"
"#;
    Config::from_toml(text)
        .expect("config parses")
        .validate()
        .expect("a token header name is valid");
}

/// An unknown key inside the table is refused, like every other table in this model: a
/// misspelled `endpont` must not leave a deployment exporting nothing with no message.
#[test]
fn rejects_an_unknown_key_in_the_telemetry_table() {
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n[telemetry]\nendpont = \"http://x:4318\"\n";
    let err = Config::from_toml(text).expect_err("a typo must not parse");
    assert!(err.to_string().contains("endpont"), "{err}");
}

#[test]
fn rejects_an_unavailable_status_that_is_not_an_http_code() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[status]
address = "127.0.0.1:2114"
unavailable_status_code = 99
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("not an HTTP status code"), "{err}");
}

/// A typo in a key inside either table is refused, like every other table in this model.
#[test]
fn rejects_an_unknown_key_in_the_metrics_table() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[metrics]
address = "127.0.0.1:2112"
addres = "typo"
"#;
    let err = Config::from_toml(text).expect_err("must fail");
    assert!(err.to_string().contains("addres"), "{err}");
}
