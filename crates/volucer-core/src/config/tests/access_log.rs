//! The access log, the proxy table and the request-id header.

use super::*;

#[test]
fn an_access_log_table_parses_with_its_defaults() {
    // The defaults are the interesting part: a deployment that writes only a path gets nginx's
    // format and `X-Request-Id` adoption without asking, and both are what an operator expects.
    let text = r#"
[server]
http = "127.0.0.1:8080"

[server.access_log]
path = "/var/log/volucer/access.log"
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("valid");
    let log = config.server.access_log.expect("the table is present");
    assert_eq!(log.path, PathBuf::from("/var/log/volucer/access.log"));
    assert_eq!(log.format, AccessLogFormat::Combined);
    assert_eq!(log.request_id_header, "X-Request-Id");
}

#[test]
fn an_access_log_path_resolves_against_the_config_directory() {
    // The rule `[server.tls]`'s certificate follows, asserted rather than assumed: a relative
    // path must not depend on where the process was started, or two deployments of one config
    // log to different places.
    let relative = resolve_access_log_path(Path::new("logs/access.log"), Path::new("/etc/volucer"));
    assert_eq!(relative, PathBuf::from("/etc/volucer/logs/access.log"));
    // An absolute path is left alone, which is the case a systemd unit writes.
    let absolute = resolve_access_log_path(Path::new("/var/log/a.log"), Path::new("/etc/volucer"));
    assert_eq!(absolute, PathBuf::from("/var/log/a.log"));
}

#[test]
fn a_proxy_table_parses_and_an_absent_one_means_the_feature_is_off() {
    let text = r#"
[server]
http = "127.0.0.1:8080"

[server.proxy]
trusted_subnets = ["127.0.0.1", "10.0.0.0/8"]
trusted_headers = ["X-Forwarded-For"]
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("valid");
    let policy = config.server.client_ip().expect("the ranges parse");
    assert!(policy.is_enabled());
    assert!(policy.trusts("127.0.0.1".parse().unwrap()));
    assert!(policy.trusts("10.4.5.6".parse().unwrap()));
    assert!(!policy.trusts("203.0.113.7".parse().unwrap()));
    assert_eq!(policy.header_names(), vec!["X-Forwarded-For"]);

    // The default, and the one that matters: no table, no header consulting, every request
    // resolves to its own peer. An existing deployment is not changed by upgrading.
    let bare = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("parses");
    bare.validate().expect("valid");
    assert!(!bare.server.client_ip().expect("off").is_enabled());
}

#[test]
fn a_proxy_range_that_does_not_parse_stops_the_server_rather_than_being_dropped() {
    // The failure has a direction here: a dropped range is a proxy whose headers are suddenly
    // disbelieved, so a rate limit keyed on the address throttles every user at once and nothing
    // in the logs says why. Refusing to start is the only safe answer.
    let text = r#"
[server]
http = "127.0.0.1:8080"

[server.proxy]
trusted_subnets = ["10.0.0.0/8", "10.0.0.0/99"]
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("a bad range is refused");
    let message = err.to_string();
    assert!(
        message.contains("trusted_subnets") && message.contains("/99"),
        "the message must name the entry, got: {message}"
    );
}

#[test]
fn an_unknown_key_inside_the_proxy_table_is_refused() {
    // `deny_unknown_fields`, for the reason every other table has it: a misspelled key that is
    // ignored is a trust list that does not say what the operator thinks it says.
    let text = r#"
[server]
http = "127.0.0.1:8080"

[server.proxy]
trusted_subnet = ["10.0.0.0/8"]
"#;
    let err = Config::from_toml(text).expect_err("the typo is refused");
    assert!(!err.to_string().is_empty());
}

#[test]
fn an_empty_request_id_header_is_allowed_and_means_never_adopt() {
    // The nginx behaviour, reachable by configuration rather than by a second boolean: an
    // operator gets `$request_id`'s always-generate semantics by emptying one string.
    let text = r#"
[server]
http = "127.0.0.1:8080"

[server.access_log]
path = "access.log"
request_id_header = ""
"#;
    let config = Config::from_toml(text).expect("parses");
    config
        .validate()
        .expect("an empty header is a decision, not a mistake");
    assert_eq!(
        config.server.access_log.expect("present").request_id_header,
        ""
    );
}

#[test]
fn an_unusable_request_id_header_is_refused_by_name() {
    // A header name with a space or colon is not a header: `hyper` would reject it at the point
    // the front-end tried to use it, where the failure looks like "adoption silently never
    // happens" rather than like a typo. Refused at load, with the key named.
    for bad in ["X Request Id", "X-Request-Id:", "X-Request-Id ", "X\tId"] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n\n[server.access_log]\npath = \"a.log\"\nrequest_id_header = \"{bad}\"\n"
        );
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(
            err.to_string().contains("request_id_header"),
            "{bad:?} gave {err}"
        );
    }
    // And a legal one passes, so the check is not refusing everything.
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n\n[server.access_log]\npath = \"a.log\"\nrequest_id_header = \"X-Trace-Id\"\n";
    Config::from_toml(text)
        .expect("parses")
        .validate()
        .expect("a legal header name");
}

#[test]
fn the_access_log_format_is_read_strictly() {
    // A typo must be a parse error rather than a quiet fallback: an operator whose queries
    // expect JSON and who got `combined` has a silently broken pipeline, which is worse than a
    // refusal that names the key.
    let good = "[server]\nhttp = \"127.0.0.1:8080\"\n\n[server.access_log]\npath = \"a.log\"\nformat = \"json\"\n";
    let config = Config::from_toml(good).expect("parses");
    assert_eq!(
        config.server.access_log.expect("present").format,
        AccessLogFormat::Json
    );
    for bad in ["clf", "JSONL", "combined "] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n\n[server.access_log]\npath = \"a.log\"\nformat = \"{bad}\"\n"
        );
        assert!(
            Config::from_toml(&text).is_err(),
            "{bad:?} was accepted as a format"
        );
    }
}

#[test]
fn an_unknown_key_inside_the_access_log_table_is_refused() {
    // `deny_unknown_fields` on the new table, checked because it is what makes a typo like
    // `pth` a startup error instead of a log that goes nowhere.
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n\n[server.access_log]\npath = \"a.log\"\npth = \"typo\"\n";
    assert!(Config::from_toml(text).is_err());
}

#[test]
fn a_bare_access_log_boolean_is_not_a_table_and_is_refused() {
    // The `[server.tls]` rule: a table, not a flag. `access_log = true` would be a statement
    // with no destination, and the two would be able to disagree.
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\naccess_log = true\n";
    assert!(Config::from_toml(text).is_err());
}
