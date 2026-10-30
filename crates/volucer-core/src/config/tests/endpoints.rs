//! The status endpoint, the worker routes it may carry, and trace export.

use super::*;

/// The old shape is refused by name, not by `toml`'s `unknown field`.
///
/// **This is the whole reason the shims exist.** `[metrics]` did not disappear, it moved one table
/// down and its `address` became the listener's `listen` — so `unknown field 'metrics'` would be
/// true and explain nothing. Each refusal has to name the key that replaced it, or an upgrading
/// operator is left diffing documentation.
#[test]
fn each_pre_nesting_key_is_refused_by_the_name_of_its_replacement() {
    let cases = [
        (
            "[server]\nhttp = \"127.0.0.1:8080\"\n[metrics]\naddress = \"127.0.0.1:2112\"\n",
            "[status.metrics]",
        ),
        (
            "[server]\nhttp = \"127.0.0.1:8080\"\n[status]\naddress = \"127.0.0.1:2114\"\n",
            "[status] listen",
        ),
        (
            "[server]\nhttp = \"127.0.0.1:8080\"\n[status]\nlisten = \"127.0.0.1:2114\"\n\
             unavailable_status_code = 503\n",
            "[status.health]",
        ),
        (
            "[server]\nhttp = \"127.0.0.1:8080\"\n[status]\nlisten = \"127.0.0.1:2114\"\n\
             allow_worker_reset = true\n",
            "[status.workers] allow",
        ),
    ];
    for (text, replacement) in cases {
        let config = Config::from_toml(text).expect("the old shape must still parse");
        let err = config.validate().expect_err("and then be refused");
        let message = err.to_string();
        assert!(
            message.contains(replacement),
            "the refusal has to name {replacement:?}: {message}"
        );
        assert!(
            message.contains("0.4.0"),
            "and say which release moved it: {message}"
        );
    }
}

/// A `[status]` table with no `listen` is refused, rather than reported as a missing field by
/// `toml` before the other shims can be checked.
#[test]
fn a_status_table_without_a_listen_is_refused() {
    let config = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n[status]\n")
        .expect("an empty table parses");
    let err = config.validate().expect_err("must be refused");
    assert!(err.to_string().contains("no `listen`"), "{err}");
}

/// **A unix `server.http` is accepted where the platform can bind it**, and refused by name where it
/// cannot — the same answer `validate_cgi_socket` already gives a pool's socket prefix.
///
/// The two halves are one rule: a form is accepted exactly where something binds it, and `check`
/// must say so rather than printing a listener `serve` will refuse. The bug this test exists beside
/// was exactly that split — `check` accepted a unix listener on Windows and `serve` refused it.
#[test]
fn a_unix_http_listener_is_accepted_only_where_it_can_bind() {
    let text = "[server]\nhttp = \"unix:/run/volucer/http.sock\"\nserve_only = \"static\"\n";
    let config = Config::from_toml(text).expect("config parses");
    let outcome = config.validate();
    if cfg!(unix) {
        outcome.expect("a unix http listener is a listener like any other on a unix host");
    } else {
        let message = outcome
            .expect_err("a unix socket cannot be bound here, so check must say so")
            .to_string();
        assert!(message.contains("server.http"), "{message}");
        assert!(
            message.contains("not available on this platform"),
            "the message has to name the reason: {message}"
        );
    }
}

/// **A unix `server.fastcgi` follows the same rule**, which is the shape an `nginx` deployment uses.
#[test]
fn a_unix_fastcgi_listener_is_accepted_only_where_it_can_bind() {
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\nfastcgi = \"unix:/run/volucer/fcgi.sock\"\n\
                serve_only = \"static\"\n";
    let config = Config::from_toml(text).expect("config parses");
    let outcome = config.validate();
    if cfg!(unix) {
        outcome.expect("a unix fastcgi listener is a listener like any other on a unix host");
    } else {
        let message = outcome
            .expect_err("a unix socket cannot be bound here, so check must say so")
            .to_string();
        assert!(message.contains("server.fastcgi"), "{message}");
    }
}

/// The `https` listener is TCP, and a `unix:` address is refused when the file is read.
///
/// The same rule the status listener follows, and for the same reason: `parse_listener` accepts the
/// form, so without this the value passed `volucer check` and then failed at bind — a settings file
/// that validates and cannot start.
#[test]
fn a_unix_https_listener_is_refused() {
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\nhttps = \"unix:/tmp/s.sock\"\n\
                \n[server.tls]\ncertificate = \"c.pem\"\nkey = \"k.pem\"\n";
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must be refused");
    let message = err.to_string();
    assert!(message.contains("server.https"), "{message}");
    assert!(message.contains("TCP only"), "{message}");
    assert!(
        message.contains("host:port"),
        "the message has to say what to write instead: {message}"
    );
}

/// **A redirect cannot name a port a socket does not have.**
///
/// `redirect_to_https` answers with `Location: https://host:<port>`, and the port comes from the
/// listener that received the cleartext request. A unix socket has none, so the only honest answers
/// are a configuration error or a `Location` with no port — and the second is a URL that silently
/// goes to 443 and fails, which an operator would read as a bug in the redirect rather than a
/// property of the transport.
#[test]
fn a_cleartext_unix_listener_cannot_redirect_to_https() {
    // The platform refusal fires first on Windows, and it fires for the *same listener* — so the
    // assertion is the reason the configuration is refused, whichever rule got there first.
    if cfg!(not(unix)) {
        let text = "[server]\nhttp = \"unix:/run/volucer/http.sock\"\nredirect_to_https = true\n\
                    \n[server.tls]\ncertificate = \"c.pem\"\nkey = \"k.pem\"\nplain = true\n";
        let config = Config::from_toml(text).expect("config parses");
        let message = config.validate().expect_err("must be refused").to_string();
        assert!(
            message.contains("not available on this platform"),
            "a unix listener is refused here before its redirect can be: {message}"
        );
        return;
    }

    // `plain = true` so the *earlier* rule — "a redirect needs something that answers cleartext" —
    // is satisfied, and this test is about the socket rather than about that rule. Without it the
    // earlier refusal fires first, which is correct and is a different test.
    let text = "[server]\nhttp = \"unix:/run/volucer/http.sock\"\nredirect_to_https = true\n\
                \n[server.tls]\ncertificate = \"c.pem\"\nkey = \"k.pem\"\nplain = true\n";
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must be refused");
    let message = err.to_string();
    assert!(message.contains("redirect_to_https"), "{message}");
    assert!(
        message.contains("unix:/run/volucer/http.sock"),
        "the message must name the listener it is about: {message}"
    );
}

/// The status listener is TCP, and a `unix:` address is refused when the file is read.
///
/// `parse_listener` accepts the form for every other listener, so without this the value passed
/// `volucer check` and then failed at bind with `invalid port value` — a settings file that
/// validates and cannot start.
#[test]
fn a_unix_status_listener_is_refused() {
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n[status]\nlisten = \"unix:/tmp/s.sock\"\n";
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must be refused");
    let message = err.to_string();
    assert!(message.contains("TCP only"), "{message}");
    assert!(
        message.contains("host:port"),
        "the message has to say what to write instead: {message}"
    );
}

/// An empty `allow` is a table that grants nothing, which is a block an operator wrote expecting
/// an endpoint — so it is refused rather than left as a listener answering 404 to a configured
/// route.
#[test]
fn an_empty_worker_grant_is_refused() {
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n[status]\nlisten = \"127.0.0.1:2114\"\n\
                \n[status.workers]\nallow = []\n";
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must be refused");
    assert!(err.to_string().contains("empty `allow`"), "{err}");
}

/// A name that is not one of the listener's routes is refused, and the message lists them.
#[test]
fn an_unknown_worker_method_is_refused() {
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n[status]\nlisten = \"127.0.0.1:2114\"\n\
                \n[status.workers]\nallow = [\"workers.nuke\"]\n";
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must be refused");
    let message = err.to_string();
    assert!(message.contains("workers.nuke"), "{message}");
    assert!(
        message.contains("workers.list")
            && message.contains("workers.reset")
            && message.contains("workers.restart"),
        "the refusal has to say what the three routes are: {message}"
    );
}

/// **The rolling restart is a granted route, and a wildcard address refuses it like a reset.**
///
/// The sweep does not kill a process out from under a request, so it reads as gentler than a reset —
/// but it *does* replace every worker, which is a way to make a deployment run whatever code is on
/// disk. The protection is therefore the same, and asserting it here is what keeps the two from
/// drifting apart: an operator who grants a sweep on `0.0.0.0` has handed anyone who can reach the
/// port a deploy button.
#[test]
fn a_wildcard_status_address_cannot_carry_the_rolling_restart() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[status]
listen = "0.0.0.0:2114"

[status.workers]
allow = ["workers.list", "workers.restart"]
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must be refused");
    assert!(err.to_string().contains("workers.restart"), "{err}");

    // And on a specific address it is legal, because that is the deployment shape it exists for: a
    // deploy step that reaches the status port over the orchestrator's network.
    let text = text.replace("0.0.0.0:2114", "127.0.0.1:2114");
    let config = Config::from_toml(&text).expect("config parses");
    config
        .validate()
        .expect("a sweep on a loopback status listener is legal");
}

/// The exposition may not take a path the listener already serves.
///
/// Two endpoints answering one path would make one of them unreachable, and which one won would be
/// decided by the order of the router's arms rather than by the file.
#[test]
fn a_metrics_path_may_not_shadow_a_probe() {
    for path in ["/health", "/ready", "/workers"] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n[status]\nlisten = \"127.0.0.1:2114\"\n\
             \n[status.metrics]\npath = \"{path}\"\n"
        );
        let config = Config::from_toml(&text).expect("config parses");
        let err = config.validate().expect_err("must be refused");
        assert!(err.to_string().contains(path), "{path}: {err}");
    }
}

/// The probes survive every combination of the optional sub-tables, because a `[status]` with none
/// of them is the ordinary deployment.
#[test]
fn the_probes_are_present_whatever_the_sub_tables_say() {
    for extra in [
        "",
        "\n[status.metrics]\npath = \"/metrics\"\n",
        "\n[status.workers]\nallow = [\"workers.list\"]\n",
        "\n[status.metrics]\n\n[status.workers]\nallow = [\"workers.list\"]\n",
    ] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n[status]\nlisten = \"127.0.0.1:2114\"\n{extra}"
        );
        let config = Config::from_toml(&text).expect("config parses");
        config.validate().expect("valid");
        let status = config.status.expect("present");
        assert_eq!(
            status.health.unavailable_status, 503,
            "the probe has to answer 503 by default in every combination: {extra:?}"
        );
    }
}

#[test]
fn status_defaults_its_unavailable_code() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[status]
listen = "127.0.0.1:2114"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("valid");
    assert_eq!(
        config.status.expect("present").health.unavailable_status,
        503
    );
}

/// The worker routes are off unless the file says otherwise, and that default is the feature.
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
listen = "127.0.0.1:2114"
"#;
    let config = Config::from_toml(text).expect("config parses");
    assert!(
        config.status.expect("present").workers.is_none(),
        "a [status] table that says nothing about the worker routes must not enable them"
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
listen = "0.0.0.0:2114"

[status.workers]
allow = ["workers.list", "workers.reset"]
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must be refused");
    let message = err.to_string();
    assert!(message.contains("workers.reset"), "{message}");
    assert!(
        message.contains("every interface"),
        "the message has to say what is wrong with the address: {message}"
    );

    // And the same address without the reset is ordinary: a readiness probe, a scrape and a worker
    // list on a wildcard bind are a deployment somebody chose, and refusing them would be refusing
    // the wrong thing — the list is a fact about this process, not a lever on it.
    let text = text.replace(r#", "workers.reset""#, "");
    assert!(
        !text.contains("\"workers.reset\""),
        "the replacement must actually remove the name, or the assertion below is about the \
         original text"
    );
    let config = Config::from_toml(&text).expect("config parses");
    config
        .validate()
        .expect("a wildcard [status] that grants only the list is legal");
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
listen = "{address}"

[status.workers]
allow = ["workers.list", "workers.reset"]
"#
        );
        let config = Config::from_toml(&text).expect("config parses");
        config.validate().expect("a specific address is fine");
    }
}

/// A status listener alone is not a listener in the request path.
///
/// A container orchestrator polls readiness while something else scrapes. Neither serves a request,
/// so neither satisfies the "no listener configured" refusal on its own.
#[test]
fn a_metrics_listener_alone_is_not_a_listener() {
    let text = r#"
[status]
listen = "127.0.0.1:2112"

[status.metrics]
path = "/metrics"
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
listen = "127.0.0.1:2114"

[status.health]
unavailable_status = 99
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
[status]
listen = "127.0.0.1:2112"

[status.metrics]
path = "/metrics"
pat = "typo"
"#;
    let err = Config::from_toml(text).expect_err("must fail");
    assert!(err.to_string().contains("pat"), "{err}");
}

/// **`max_connections` is a key, not only a flag, and `0` means no ceiling.**
///
/// The flag used to carry the default itself, which is what made the key unreachable: a flag with a
/// `default_value` cannot be told apart from one an operator typed, so the file could never win.
/// The resolution order is asserted here as the *config* half — flag first, then this, then the
/// default — and `0` reads through as `0` rather than being turned into the default.
#[test]
fn max_connections_is_configurable_and_zero_means_no_ceiling() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
max_connections = 64
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("valid");
    assert_eq!(config.server.max_connections(), 64);

    // Absent: the value the flag always defaulted to, so an unconfigured deployment is unchanged.
    let text = r#"
[server]
http = "127.0.0.1:8080"
"#;
    let config = Config::from_toml(text).expect("parses");
    assert_eq!(
        config.server.max_connections(),
        512,
        "the default must be the number the flag used to carry"
    );

    // `0` is the honest spelling of unlimited and must not be read as "absent".
    let text = r#"
[server]
http = "127.0.0.1:8080"
max_connections = 0
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("0 is a deliberate setting");
    assert_eq!(config.server.max_connections(), 0);
}

/// **`request_timeout_ms` is configurable per backend, and `0` is refused by name.**
///
/// The value was hard-coded at thirty seconds, so a request that legitimately takes sixty — a report
/// export, a slow external call — could not be served at all: the pool killed the worker mid-request
/// and the client saw a 502. Nothing in the file said so and nothing in `check` reported it, which is
/// the pair of failures this key exists to end.
#[test]
fn a_request_timeout_is_configurable_per_backend_and_zero_is_refused() {
    for (kind, table) in [
        ("worker", "[backends.app]\nkind = \"worker\"\ncommand = \"php worker.php\""),
        (
            "spawn_cgi",
            "[backends.app]\nkind = \"spawn_cgi\"\nbinary = \"php-cgi\"",
        ),
    ] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\ndefault_backend = \"app\"\n\n{table}\n\
             request_timeout_ms = 60000\n"
        );
        let config = Config::from_toml(&text).expect("parses");
        config.validate().expect("a long deadline is legal");
        let timeout = match &config.backends["app"] {
            BackendConfig::Worker(w) => w.request_timeout_ms,
            BackendConfig::SpawnCgi(c) => c.request_timeout_ms,
            other => panic!("{other:?}"),
        };
        assert_eq!(timeout, Some(60_000), "{kind}");

        let text = text.replace("request_timeout_ms = 60000", "request_timeout_ms = 0");
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("0 must be refused");
        assert!(
            err.to_string().contains("request_timeout_ms = 0"),
            "the refusal has to name the key: {kind}: {err}"
        );
    }
}
