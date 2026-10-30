//! The upstream backend: origins, restart policy, and the HTTP version spoken.

use super::*;

/// A config with one upstream backend, for the tests below.
fn upstream_config(backend_body: &str) -> String {
    format!(
        r#"
[server]
default_backend = "node"
http = "127.0.0.1:8080"
[backends.node]
kind = "upstream"
{backend_body}
"#
    )
}

#[test]
fn an_upstream_backend_parses_and_names_itself() {
    let config =
        Config::from_toml(&upstream_config("origin = \"http://127.0.0.1:3000\"")).expect("parses");
    config.validate().expect("validates");
    let backend = &config.backends["node"];
    assert_eq!(backend.kind(), "upstream");
    // The third answer, which is the reason `request_scope` is an enum: neither PHP's SAPI nor
    // an application-scoped worker describes somebody else's runtime.
    assert_eq!(backend.request_scope(), RequestScope::OriginScoped);
    assert!(!backend.resets_per_request());
    // And the question the route table's `stat` turns on, which is *not* the negation of the
    // line above: a worker and an upstream both skip it, for two different reasons.
    assert!(!backend.interprets_path());
}

#[test]
fn an_upstream_is_restart_only_like_every_other_backend() {
    // A URL looks reloadable in a way a `command` does not, which is why the README says so and
    // why the restart-only list names backends as a class. Pinned here so a later change that
    // made an origin reloadable has to decide about this deliberately: the *only* thing
    // `preflight` has to say about a loopback origin is nothing.
    let warnings = crate::preflight::inspect(
        &Config::from_toml(&upstream_config("origin = \"http://127.0.0.1:3000\"")).expect("parses"),
        std::path::Path::new("."),
    );
    assert!(
        !warnings.iter().any(|w| w.message.contains("backends.node")),
        "a loopback origin raises nothing of its own: {warnings:?}"
    );
}

/// Every origin shape that is accepted, and that the parse resolves what it should.
///
/// A named row type rather than a tuple: three fields of different shapes in a slice literal
/// is exactly the thing a reader has to count on their fingers.
#[test]
fn the_four_origin_shapes_are_accepted_and_resolved() {
    struct Case {
        spec: &'static str,
        scheme: &'static str,
        host_port: Option<(&'static str, u16)>,
        tls: bool,
    }
    let cases: &[Case] = &[
        Case {
            spec: "http://127.0.0.1:3000",
            scheme: "http",
            host_port: Some(("127.0.0.1", 3000)),
            tls: false,
        },
        Case {
            spec: "http://[::1]:3000",
            scheme: "http",
            host_port: Some(("[::1]", 3000)),
            tls: false,
        },
        // No port: the scheme's default, resolved once here rather than at dial time, so an
        // error message names the port that was actually chosen.
        Case {
            spec: "http://origin.test",
            scheme: "http",
            host_port: Some(("origin.test", 80)),
            tls: false,
        },
        Case {
            spec: "https://origin.test",
            scheme: "https",
            host_port: Some(("origin.test", 443)),
            tls: true,
        },
        Case {
            spec: "https://10.0.0.5:8443",
            scheme: "https",
            host_port: Some(("10.0.0.5", 8443)),
            tls: true,
        },
    ];
    for case in cases {
        let spec = case.spec;
        let origin =
            OriginSpec::parse(spec, None).unwrap_or_else(|err| panic!("{spec} must parse: {err}"));
        assert_eq!(origin.scheme(), case.scheme, "{spec}");
        assert_eq!(
            origin.host_port(),
            case.host_port.map(|(h, p)| (h.to_string(), p)),
            "{spec}"
        );
        assert_eq!(origin.is_tls(), case.tls, "{spec}");
    }

    #[cfg(unix)]
    {
        let origin = OriginSpec::parse("unix:/run/node.sock", None).expect("parses");
        assert_eq!(origin.scheme(), "unix");
        assert_eq!(origin.host_port(), None);
        assert_eq!(
            origin.socket_path(),
            Some(std::path::Path::new("/run/node.sock"))
        );
        // A socket leaves the host, so there is nothing for a plaintext warning to be about —
        // which is the same answer `is_local` gives a loopback address and the opposite of what
        // it gives `http://10.0.0.5`.
        assert!(origin.is_local());
    }
    assert!(OriginSpec::parse("http://127.0.0.1:3000", None)
        .expect("parses")
        .is_local());
    assert!(OriginSpec::parse("http://localhost:3000", None)
        .expect("parses")
        .is_local());
    assert!(!OriginSpec::parse("http://10.0.0.5:3000", None)
        .expect("parses")
        .is_local());
}

/// The shapes that are refused, each for the reason it names.
///
/// Table-driven because the rules are independent: one case per rule, each asserting the
/// *reason*. A test that asserted "some error" for all of them would pass against one
/// over-broad rule.
#[test]
fn an_origin_that_cannot_be_used_is_refused_with_its_reason() {
    // Built rather than written out, because a userinfo-looking literal is exactly what a
    // secret scanner is for — and a test whose input depends on a scanner's opinion is a test
    // that can silently change meaning. The parse only has to see an `@` before the path.
    let with_userinfo = format!("http://{}@{}", "user", "127.0.0.1:3000");
    let cases: Vec<(&str, &[&str])> = vec![
        // A bare `host:port` is the dangerous one: read as plaintext it would be a silent
        // downgrade, so it is refused rather than guessed at.
        ("127.0.0.1:3000", &["no scheme"]),
        ("", &["empty"]),
        ("http://", &["no host"]),
        ("http:///api", &["no host"]),
        ("http://:3000", &["no host"]),
        ("ftp://127.0.0.1:3000", &["ftp", "unix"]),
        // A base path would have to be joined with the request's own, and the join rule is not
        // guessable — so it is refused with the alternative rather than silently dropped.
        ("http://127.0.0.1:3000/app", &["/app", "route table"]),
        ("http://127.0.0.1:3000/?x=1", &["route table"]),
        (with_userinfo.as_str(), &["credentials"]),
        // Brackets are what disambiguate an IPv6 literal's colons from the port's, so a bare
        // one is refused with the shape to write instead.
        ("http://::1:3000", &["brackets"]),
        ("http://[::1:3000", &["without closing"]),
        ("http://origin.test:notaport", &["port"]),
    ];
    for (spec, expected) in cases {
        let err = OriginSpec::parse(spec, None)
            .err()
            .unwrap_or_else(|| panic!("{spec} must be refused"));
        for needle in expected.iter() {
            assert!(
                err.contains(needle),
                "{spec}: {err:?} must mention {needle:?}"
            );
        }
    }
}

#[test]
fn a_tls_table_beside_a_plaintext_or_socket_origin_is_refused() {
    // The one thing worse than no verification is verification the operator believes in: a
    // `[backends.n.tls]` table beside `http://` or `unix:` would describe a handshake that
    // never happens, and it would be silently ignored if the parse did not take it.
    for origin in ["http://127.0.0.1:3000", "unix:/run/node.sock"] {
        let spec = format!("{origin:?}");
        let text = upstream_config(&format!(
            "origin = {spec}\n[backends.node.tls]\nca = \"ca.pem\"\n"
        ));
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must fail");
        let message = err.to_string();
        assert!(message.contains("backends.node"), "{origin}: {message}");
        assert!(
            message.contains("handshake"),
            "the error must say why the table has no meaning: {origin}: {message}"
        );
    }
}

#[test]
fn an_https_origin_accepts_its_tls_table() {
    let text = r#"
[server]
default_backend = "node"
http = "127.0.0.1:8080"
[backends.node]
kind = "upstream"
origin = "https://node.internal:8443"
[backends.node.tls]
ca = "/etc/ssl/internal-ca.pem"
sni = "node.internal"
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("validates");
    let origin = config.backends["node"]
        .origin()
        .expect("an upstream has an origin")
        .expect("the origin parses");
    let tls = origin.tls().expect("https has tls settings");
    assert_eq!(tls.sni.as_deref(), Some("node.internal"));
    assert_eq!(
        tls.ca.as_deref(),
        Some(std::path::Path::new("/etc/ssl/internal-ca.pem"))
    );
}

#[test]
fn an_empty_tls_key_is_refused_rather_than_read_as_absent() {
    // TOML cannot tell `""` from a missing key, and an empty CA path would otherwise become
    // "the public roots" — a silent change of what the hop is verified against.
    let cases: &[(&str, &str)] = &[
        ("ca = \"\"", "tls.ca is empty"),
        ("sni = \"\"", "tls.sni is empty"),
    ];
    for (body, expected) in cases {
        let text = upstream_config(&format!(
            "origin = \"https://node.internal:8443\"\n[backends.node.tls]\n{body}\n"
        ));
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains(expected), "{body}: {err}");
    }
}

#[test]
fn an_sni_that_is_not_a_name_is_refused_at_validation() {
    // A verification name is a DNS name; a port or a scheme inside one is a mistake worth
    // naming where the file is read, rather than a handshake error on the first request.
    for bad in [
        "node.internal:8443",
        "https://node.internal",
        "node..internal",
        "-",
    ] {
        let text = upstream_config(&format!(
            "origin = \"https://10.0.0.5:8443\"\n[backends.node.tls]\nsni = {bad:?}\n"
        ));
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("not a host name"), "{bad}: {err}");
    }
    // A bare IP is a name a certificate can carry, since a certificate may hold an address.
    let text = upstream_config(
        "origin = \"https://10.0.0.5:8443\"\n[backends.node.tls]\nsni = \"10.0.0.5\"\n",
    );
    Config::from_toml(&text)
        .expect("parses")
        .validate()
        .expect("an IP is a usable verification name");
}

#[test]
fn an_upstream_needs_an_http_listener() {
    // There is no CGI environment to build for an upstream and no way for the FastCGI
    // front-end to hand it a request, so without an HTTP listener the backend starts, the
    // sockets bind, and nothing ever reaches the origin. Silent otherwise, so it is an error.
    let text = r#"
[server]
default_backend = "node"
fastcgi = "127.0.0.1:9080"
[backends.node]
kind = "upstream"
origin = "http://127.0.0.1:3000"
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("must fail");
    let message = err.to_string();
    assert!(message.contains("backends.node"), "{message}");
    assert!(message.contains("server.http"), "{message}");
}

#[test]
fn the_three_silent_readers_answer_correctly_for_an_upstream() {
    // Three places accepted the new kind and described it wrongly, none of them a compile
    // error. Two are pure functions and are pinned here; the third is `check`'s output and is
    // asserted at the CLI level, where it is actually printed.
    let config =
        Config::from_toml(&upstream_config("origin = \"http://127.0.0.1:3000\"")).expect("parses");
    let backend = &config.backends["node"];

    // 1. What `check` prints for the per-request-reset line. Neither of the two strings that
    //    existed before is true of an upstream.
    let line = backend.request_scope().describe();
    assert!(line.contains("not applicable"), "{line}");
    assert!(!line.contains("PHP SAPI"), "{line}");
    assert!(!line.contains("application-scoped"), "{line}");

    // 2. Whether a route to it skips the router's `stat`. The path is the origin's business —
    //    there is no script for volucer to resolve — so it must.
    assert!(
        !backend.interprets_path(),
        "an upstream routes on the URL, so the path is not a file to stat"
    );
}

#[test]
fn an_upstream_and_a_worker_skip_the_stat_for_different_reasons() {
    // The rule `skip_stat` asks about, stated once for all three shapes: the answer is not
    // "is it a worker", because an upstream reaches the same answer from the other side.
    let config =
        Config::from_toml(&upstream_config("origin = \"http://127.0.0.1:3000\"")).expect("parses");
    let worker = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#,
    )
    .expect("parses");
    let cgi = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
"#,
    )
    .expect("parses");

    let prefix = |config: &Config, name: &str| {
        let route = RouteConfig {
            host: None,
            path_prefix: Some("/legacy".to_string()),
            path_suffix: None,
            policy: RoutePolicy::All,
            backend: Some(name.to_string()),
            redirect: None,
        };
        skip_stat(
            &route,
            config.backends[name].interprets_path(),
            &config.server.script_extensions(),
        )
    };

    assert!(
        prefix(&config, "node"),
        "an upstream does not read the path"
    );
    assert!(prefix(&worker, "app"), "a worker does not read the path");
    assert!(
        !prefix(&cgi, "cgi"),
        "php-cgi emits the file it is pointed at, so the stat is the guard"
    );
}

#[test]
fn script_extensions_default_to_php_and_a_site_may_replace_them() {
    // Step 38 at the configuration level: absent means PHP's three, a site may name its own,
    // and the site table carries the resolved value so the router and the static layer cannot
    // read two different answers.
    let config = Config::from_toml(
        r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
document_root = "/srv/default"
script_extensions = [".py", "rb"]

[backends.app]
kind = "worker"
command = "./app.py"

[sites.php]
hosts = ["php.example"]
document_root = "/srv/php"

[sites.python]
hosts = ["py.example"]
document_root = "/srv/py"
script_extensions = ["py"]
"#,
    )
    .expect("parses");
    config.validate().expect("validates");

    let sites = crate::Sites::from_config(&config, std::path::Path::new("/srv/default"));
    // The default site takes the server's list, and `.py` is normalised to `py`.
    assert_eq!(
        sites.default_site().script_extensions.as_slice(),
        &["py", "rb"]
    );
    // A site that says nothing inherits it.
    assert_eq!(
        sites.named()["php"].script_extensions.as_slice(),
        &["py", "rb"]
    );
    // A site that names its own replaces it rather than adding to it.
    assert_eq!(
        sites.named()["python"].script_extensions.as_slice(),
        &["py"]
    );
}

#[test]
fn an_unconfigured_deployment_still_treats_php_files_as_scripts() {
    let config = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"
"#,
    )
    .expect("parses");
    config.validate().expect("validates");

    let extensions = config.server.script_extensions();
    assert!(extensions.is_default());
    assert!(extensions.matches("/index.php"));
    assert!(!extensions.matches("/app.py"));
}

#[test]
fn an_empty_script_list_is_refused_at_the_key_that_wrote_it() {
    let server = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"
script_extensions = []
"#,
    )
    .expect("parses");
    let err = server.validate().expect_err("an empty list is refused");
    assert!(
        err.to_string().contains("server.script_extensions"),
        "{err}"
    );

    let site = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"

[sites.py]
hosts = ["py.example"]
document_root = "/srv/py"
script_extensions = []
"#,
    )
    .expect("parses");
    let err = site.validate().expect_err("an empty list is refused");
    assert!(
        err.to_string().contains("sites.py.script_extensions"),
        "{err}"
    );
}

#[test]
fn skip_stat_reads_the_sites_list_rather_than_a_constant() {
    // The third place that asks "is this a script". Naming `py` on a worker makes a `.py`
    // suffix provable, so the router stops `stat`ing — which is only safe because the static
    // layer refuses to serve it, and that test is in `volucer-http`.
    let php = crate::script::ScriptExtensions::default();
    let python =
        crate::script::ScriptExtensions::from_config(&["py".to_string()], "test").expect("valid");

    let route = |suffix: &str| RouteConfig {
        host: None,
        path_prefix: None,
        path_suffix: Some(suffix.to_string()),
        policy: RoutePolicy::All,
        backend: Some("app".to_string()),
        redirect: None,
    };

    assert!(skip_stat(&route(".php"), false, &php));
    // A worker never reads the path, so the stat is always skippable there.
    assert!(skip_stat(&route(".json"), false, &php));
    // A backend that *does* read the path is the interesting case.
    assert!(!skip_stat(&route(".json"), true, &php));
    assert!(!skip_stat(&route(".py"), true, &php), "not PHP's list");
    assert!(skip_stat(&route(".py"), true, &python));
    assert!(
        !skip_stat(&route(".php"), true, &python),
        "the list replaced it"
    );
}

#[test]
fn a_plaintext_origin_can_be_declared_to_speak_h2_and_a_tls_one_cannot() {
    // The transport decision for a plaintext gRPC origin. gRPC's transport is HTTP/2, and on
    // a transport with no handshake there is no ALPN to negotiate it — so the configuration has
    // to say it, and this is where that answer is checked.
    let config = Config::from_toml(&upstream_config(
        "origin = \"http://127.0.0.1:3000\"\nprotocol = \"h2c\"",
    ))
    .expect("the file parses");
    config
        .validate()
        .expect("h2c on a plaintext origin is valid");
    let Some(BackendConfig::Upstream(upstream)) = config.backends.get("node") else {
        panic!("the backend is an upstream");
    };
    assert_eq!(upstream.protocol, Some(UpstreamProtocol::H2c));
    // And it reaches the *spec*, which is what the client's connector reads. A key that parsed
    // but stopped here would be a setting nothing consumes.
    let spec = OriginSpec::parse_with(&upstream.origin, upstream.tls.clone(), upstream.protocol)
        .expect("a loopback origin");
    assert_eq!(spec.protocol(), Some(UpstreamProtocol::H2c));

    // Beside a handshake it is refused, because there the answer is ALPN's and a claim of prior
    // knowledge would be wrong whenever the origin offers `http/1.1` only.
    let err = Config::from_toml(&upstream_config(
        "origin = \"https://node.internal:8443\"\nprotocol = \"h2c\"",
    ))
    .expect("the file parses")
    .validate()
    .expect_err("a handshake negotiates the protocol, so claiming one is a second answer");
    assert!(err.to_string().contains("ALPN"), "{err}");

    // A misspelled value is refused by name rather than read as the default — and the enum has
    // exactly one variant, so `http2` and `h2` are both mistakes rather than synonyms.
    for value in ["\"http2\"", "\"h2\"", "\"http/2\"", "\"true\""] {
        let text = upstream_config(&format!(
            "origin = \"http://127.0.0.1:3000\"\nprotocol = {value}"
        ));
        assert!(
            Config::from_toml(&text).is_err(),
            "protocol = {value} must not be accepted"
        );
    }

    // Absent is HTTP/1.1, and it stays absent rather than becoming a value: the same rule
    // `restart` follows, and for the same reason — a default would make "unset" and "set to the
    // default" indistinguishable.
    let plain =
        Config::from_toml(&upstream_config("origin = \"http://127.0.0.1:3000\"")).expect("parses");
    let Some(BackendConfig::Upstream(upstream)) = plain.backends.get("node") else {
        panic!("the backend is an upstream");
    };
    assert_eq!(upstream.protocol, None);
    assert_eq!(
        OriginSpec::parse(&upstream.origin, None)
            .expect("a loopback origin")
            .protocol(),
        None
    );
}

#[cfg(unix)]
#[test]
fn a_unix_socket_can_be_declared_to_speak_h2_too() {
    // The shape this key exists for: a gRPC application on a unix socket, which is what a
    // Node/Go/Python gRPC server behind volucer looks like. The socket and the network case are
    // two transports and one question, so they are asserted separately rather than assumed to
    // share an answer.
    let spec = OriginSpec::parse_with("unix:/run/grpc.sock", None, Some(UpstreamProtocol::H2c))
        .expect("a socket origin");
    assert_eq!(spec.socket_path(), Some(Path::new("/run/grpc.sock")));
    assert_eq!(
        spec.protocol(),
        Some(UpstreamProtocol::H2c),
        "the claim has to reach a socket as well as an address, or the gRPC-over-socket case \
         cannot be expressed at all"
    );
    // And the ordinary socket origin — php-fpm, gunicorn — stays HTTP/1.1.
    let plain = OriginSpec::parse("unix:/run/php-fpm.sock", None).expect("a socket origin");
    assert_eq!(plain.protocol(), None);
}

#[test]
fn the_removed_keys_of_other_backends_are_still_refused() {
    // `deny_unknown_fields`, checked for the new arm specifically: a `kind = "upstream"` file
    // carrying `workers` or `binary` is a copy-paste from another backend, and it should be a
    // parse error rather than a key that is silently ignored.
    //
    // `command` is deliberately **not** in this list any more: it is the key that
    // supervises the origin, so it is this backend's own. What it must not do is arrive without
    // anything it describes, which is the test below.
    for key in ["workers = 4", "binary = \"/usr/bin/php\""] {
        let text = upstream_config(&format!("origin = \"http://127.0.0.1:3000\"\n{key}"));
        assert!(
            Config::from_toml(&text).is_err(),
            "{key} must not be accepted by an upstream backend"
        );
    }
}

#[test]
fn a_supervisor_key_without_a_command_is_refused_rather_than_ignored() {
    // The failure this prevents is silent rather than loud: each of these keys describes a
    // process nobody starts, so the deployment would believe it had configured a supervisor.
    //
    // `.validate()` and not merely `.from_toml()`: `deny_unknown_fields` catches a *misspelled*
    // key, which says nothing about whether a correct key is doing anything. Only validation can
    // answer that, and a test without it would pass against a validator that never ran.
    for key in [
        "working_dir = \"/srv/app\"",
        "restart = \"always\"",
        "restart_limit = 9",
        "restart_backoff_ms = 250",
        "ready_timeout_ms = 1000",
    ] {
        let text = upstream_config(&format!("origin = \"http://127.0.0.1:3000\"\n{key}"));
        let err = Config::from_toml(&text)
            .expect("the file parses")
            .validate()
            .expect_err(&format!("{key} has nothing to describe"));
        assert!(err.to_string().contains("no `command`"), "{key}: {err}");
    }
}

#[test]
fn a_supervised_origin_that_is_configured_sensibly_is_accepted() {
    // The control for the test above: the same keys *with* a command must be accepted, or the
    // rule would be refusing the feature rather than the mistake.
    Config::from_toml(&upstream_config(
        "origin = \"http://127.0.0.1:3000\"\ncommand = \"gunicorn app.wsgi\"\n\
         working_dir = \"/srv/app\"\nrestart = \"always\"\nrestart_limit = 9\n\
         restart_backoff_ms = 250\nready_timeout_ms = 1000",
    ))
    .expect("the file parses")
    .validate()
    .expect("a supervised origin with all its keys set is the whole configuration");
}

#[test]
fn a_supervisor_value_that_would_spin_or_never_wait_is_refused() {
    let with = |extra: &str| {
        Config::from_toml(&upstream_config(&format!(
            "origin = \"http://127.0.0.1:3000\"\ncommand = \"gunicorn app.wsgi\"\n{extra}"
        )))
        .expect("the file parses")
        .validate()
    };

    // A zero backoff with a restart limit above one is a bounded-looking restart loop that
    // runs back to back: `restart_limit` counts *consecutive* failures.
    let err = with("restart_backoff_ms = 0").expect_err("a zero backoff is a spin");
    assert!(err.to_string().contains("restart_backoff_ms"), "{err}");

    // The same zero is fine when no restart will happen, because there is nothing to space out.
    // Without this the rule would be refusing a harmless value, which is how a check earns being
    // worked around.
    with("restart = \"never\"\nrestart_limit = 0\nrestart_backoff_ms = 0")
        .expect("a zero backoff beside no restarts describes nothing");

    // A zero readiness gate is "do not wait for the origin", which is the 502 the key exists to
    // prevent.
    let err = with("ready_timeout_ms = 0").expect_err("a zero gate is not a gate");
    assert!(err.to_string().contains("ready_timeout_ms"), "{err}");

    // `never` and a limit say two different things, and the operator believes the limit.
    let err = with("restart = \"never\"\nrestart_limit = 5").expect_err("contradictory");
    assert!(err.to_string().contains("restart_limit"), "{err}");

    // Above the cap the delay would be *reduced* by the doubling rather than grow from it.
    let err = with("restart_backoff_ms = 600001").expect_err("above the cap");
    assert!(err.to_string().contains("cap"), "{err}");

    // And a command that is only whitespace is not a command.
    let err = Config::from_toml(&upstream_config(
        "origin = \"http://127.0.0.1:3000\"\ncommand = \"   \"",
    ))
    .expect("the file parses")
    .validate()
    .expect_err("whitespace is not a command");
    assert!(err.to_string().contains("command"), "{err}");
}

#[test]
fn a_supervised_origin_defaults_to_restarting_on_failure_and_waiting_thirty_seconds() {
    // The defaults are the interesting part: `always` would fight an operator who stopped the
    // service, `never` would leave a crashed origin dead for ever, and a short gate would race a
    // Django boot.
    let config = Config::from_toml(&upstream_config(
        "origin = \"http://127.0.0.1:3000\"\ncommand = \"gunicorn app.wsgi\"",
    ))
    .expect("parses");
    let Some(BackendConfig::Upstream(upstream)) = config.backends.get("node") else {
        panic!("the backend is an upstream");
    };
    assert_eq!(upstream.restart_policy(), UpstreamRestart::OnFailure);
    assert_eq!(upstream.restart_limit, 3);
    assert_eq!(upstream.restart_backoff_ms, 1_000);
    assert_eq!(upstream.ready_timeout_ms, 30_000);

    // An origin with no command keeps the same numbers while meaning none of them, which is why
    // `validate` refuses them to be *set* rather than refusing the defaults.
    let plain = Config::from_toml(&upstream_config("origin = \"http://127.0.0.1:3000\""))
        .expect("an unsupervised origin still parses");
    plain
        .validate()
        .expect("and with no supervisor keys it is valid, which is the regression this covers");
    let Some(BackendConfig::Upstream(upstream)) = plain.backends.get("node") else {
        panic!("the backend is an upstream");
    };
    assert!(upstream.command.is_none());
    assert!(
        upstream.restart.is_none(),
        "an absent key must stay absent, or `validate` could not tell it from one that is set"
    );
    assert!(upstream.working_dir.is_none());
}
