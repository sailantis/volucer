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

/// **A page may be written per error status, and only for a status that can be an error.**
///
/// The table is keyed by the code, so an unknown key is a typo. A `2xx` or `3xx` page would be one
/// nothing could ever reach — and accepting it is how an operator concludes the feature does not work
/// rather than that the code is wrong, which is the failure mode `check` exists to prevent.
#[test]
fn error_pages_are_keyed_by_status_and_only_errors_are_accepted() {
    let text = r#"
[server]
http = "127.0.0.1:8080"

[server.error_pages]
"404" = "pages/404.html"
"502" = "pages/502.html"
"429" = "pages/busy.html"
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("valid");
    let pages = &config.server.error_pages;
    assert_eq!(pages.codes(), vec![404, 429, 502]);
    assert_eq!(pages.page(502), Some(Path::new("pages/502.html")));
    assert_eq!(pages.page(500), None, "a code with no page has none");

    // A code that cannot be an error status is refused by name, and the page is the only key.
    for (key, expected) in [
        ("\"200\" = \"pages/ok.html\"", "not an error status"),
        ("\"302\" = \"pages/moved.html\"", "not an error status"),
        ("\"not-a-code\" = \"x.html\"", "not a status code"),
    ] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n\n[server.error_pages]\n{key}\n"
        );
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must be refused");
        let message = err.to_string();
        assert!(
            message.contains(expected),
            "{key}: the refusal has to say {expected:?}: {message}"
        );
    }
}

/// An empty table is the ordinary case and must not be reported as a configured feature.
#[test]
fn an_absent_access_table_admits_everything_and_costs_nothing() {
    // Presence is the switch, like every other table in this file: no `[server.access]` means no
    // policy object at all, which is what keeps an unconfigured request path free.
    let config = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("parses");
    assert!(config.server.access.is_none());
    assert!(
        config
            .server
            .access_policy()
            .expect("no table is not an error")
            .is_none()
    );
    config.validate().expect("an unconfigured deployment validates");
}

#[test]
fn access_rules_keep_the_order_they_are_written_in() {
    // The order *is* the security property: first match decides, so an exception inside a blocked
    // range has to stay on the line the operator wrote it on. A parser that reordered by range
    // width — the rule the host table uses — would silently give the opposite answer.
    let config = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"

[server.access]
default = "allow"
rules = [
  { action = "allow", range = "10.1.2.3" },
  { action = "deny",  range = "10.0.0.0/8" },
]
"#,
    )
    .expect("parses");
    config.validate().expect("the ranges parse and the default is allow");

    let policy = config
        .server
        .access_policy()
        .expect("the ranges parse")
        .expect("a stated table builds a policy");
    let specs: Vec<&str> = policy.rules().iter().map(|rule| rule.spec()).collect();
    assert_eq!(
        specs,
        ["10.1.2.3", "10.0.0.0/8"],
        "the written order must survive parsing: {specs:?}"
    );

    // And the ordering has an effect, which is what makes preserving it worth a test. **The
    // exception is written above the range it excepts from**, because the first match decides.
    let inside: std::net::IpAddr = "10.1.2.3".parse().expect("an address");
    let other: std::net::IpAddr = "10.9.9.9".parse().expect("an address");
    assert!(policy.admits(Some(inside)), "the exception is admitted");
    assert!(!policy.admits(Some(other)), "the range around it is not");
}

#[test]
fn access_refuses_a_range_it_cannot_parse_by_name_and_position() {
    // A dropped rule is a range that is *served*, so the failure has a direction and has to stop the
    // server. The position is in the message because an operator with forty rules cannot grep for
    // "invalid".
    let config = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"

[server.access]
rules = [
  { action = "allow", range = "10.0.0.0/8" },
  { action = "deny",  range = "10.0.0.0/64" },
]
"#,
    )
    .expect("the file parses; the range does not");
    let err = config.validate().expect_err("a /64 on a v4 range is not a range");
    let text = err.to_string();
    assert!(text.contains("server.access"), "{text}");
    assert!(text.contains("rule 2"), "{text}");
    assert!(text.contains("10.0.0.0/64"), "{text}");
}

#[test]
fn a_deny_default_is_accepted_on_its_own_and_refused_beside_a_unix_socket() {
    // The half that runs everywhere: a closed default is a legitimate policy, so it must not be
    // refused by the rule below for existing. Without this, the platform-gated test could pass
    // against a validator that refused `default = "deny"` unconditionally.
    let closed = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"

[server.access]
default = "deny"
rules = [{ action = "allow", range = "127.0.0.0/8" }]
"#,
    )
    .expect("parses");
    closed
        .validate()
        .expect("a closed default over a TCP listener is a legitimate policy");

    // A socket has no client address, so no rule can match one and `default = "deny"` would refuse
    // every request the listener receives. That reads as a broken rule rather than as a listener that
    // cannot be filtered, so it is refused by name. **Gated on the platform** because a `unix:`
    // listener is itself refused on Windows, before this rule is reached — so on Windows the test
    // would be asserting the earlier message.
    #[cfg(unix)]
    {
        let config = Config::from_toml(
            r#"
[server]
http = "unix:/tmp/volucer-access-test.sock"

[server.access]
default = "deny"
rules = [{ action = "allow", range = "127.0.0.0/8" }]
"#,
        )
        .expect("parses");
        let err = config
            .validate()
            .expect_err("a closed default cannot be applied to a socket");
        let text = err.to_string();
        assert!(text.contains("server.access.default"), "{text}");
        assert!(text.contains("unix:/tmp/volucer-access-test.sock"), "{text}");
        assert!(text.contains("no client address"), "{text}");

        // The same table with `allow` is fine: that is a rule that can never match rather than a
        // door that refuses everybody.
        let open = Config::from_toml(
            r#"
[server]
http = "unix:/tmp/volucer-access-test.sock"

[server.access]
rules = [{ action = "deny", range = "10.0.0.0/8" }]
"#,
        )
        .expect("parses");
        open.validate()
            .expect("an allow default over a socket is fine");
    }
}

#[test]
fn an_access_rule_refuses_an_unknown_action() {
    // `action` has two values and a typo must not silently mean one of them — the direction a
    // mistyped `deny` would fail in is "everything is served".
    let err = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"

[server.access]
rules = [{ action = "denyy", range = "10.0.0.0/8" }]
"#,
    )
    .expect_err("an unknown action is a parse error");
    assert!(format!("{err}").contains("denyy"), "{err}");
}

#[test]
fn an_absent_error_pages_table_is_empty() {
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n";
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("valid");
    assert!(config.server.error_pages.is_empty());
    assert!(config.server.error_pages.codes().is_empty());
}

// ------------------------------------------------------------ sendfile

#[test]
fn an_absent_sendfile_table_turns_the_feature_off() {
    // Absent has to mean off rather than "defaults to the document root": a header naming a path is a
    // request to read any file this process can read, so a default root would be one bug away from
    // disclosing the application's own sources.
    let config = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("parses");
    assert!(config.server.sendfile.is_none());
    config.validate().expect("an unconfigured deployment validates");
}

#[test]
fn a_sendfile_table_with_no_roots_is_refused() {
    let config = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"

[server.sendfile]
roots = []
"#,
    )
    .expect("parses");
    let err = config
        .validate()
        .expect_err("a table with no root cannot mean anything");
    let text = err.to_string();
    assert!(text.contains("server.sendfile.roots"), "{text}");
    assert!(text.contains("empty"), "{text}");
}

#[test]
fn a_sendfile_root_resolves_against_the_configuration_files_directory() {
    // The resolution is the loader's job, because "beside this file" is the only meaning that does
    // not depend on where the process was started. A `check` and a `serve` started from different
    // directories must agree, so this is pinned against a real directory rather than described.
    let dir = std::env::temp_dir().join(format!("volucer-sendfile-config-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("exports")).expect("create the root");

    let config = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"

[server.sendfile]
roots = ["exports"]
"#,
    )
    .expect("parses");
    config.validate().expect("the table is well-formed");

    let resolved = crate::SendfilePolicy::new(config.server.sendfile.as_ref(), &dir)
        .expect("the root resolves")
        .expect("a stated table builds a policy");
    assert_eq!(resolved.roots().len(), 1);
    assert!(
        resolved.roots()[0].ends_with("exports"),
        "a relative root is the configuration's own directory: {:?}",
        resolved.roots()
    );

    // And a root that does not exist is refused with its name, rather than skipped: a skipped root is
    // a feature that silently never works and a 403 that blames the application.
    let missing = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"

[server.sendfile]
roots = ["nope"]
"#,
    )
    .expect("parses");
    let err = crate::SendfilePolicy::new(missing.server.sendfile.as_ref(), &dir)
        .expect_err("a root that does not exist is a typo");
    assert!(err.contains("nope"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}


