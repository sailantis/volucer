//! A complete configuration, the listeners, and every pool and socket spelling.

use super::*;

#[test]
fn parses_a_complete_config() {
    let text = r#"
[server]
default_backend = "fpm"
http = "127.0.0.1:8080"
fastcgi = "unix:/run/volucer.sock"

[backends.fpm]
kind = "external"
socket = "unix:/run/php/php8.3-fpm.sock"

[backends.win]
kind = "spawn_cgi"
binary = 'C:\php-8.3.31\php-cgi.exe'
workers = 8

[backends.app]
kind = "worker"
command = "php worker.php"
memory_limit_mb = 256
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");

    assert_eq!(config.server.default_backend.as_deref(), Some("fpm"));
    assert_eq!(config.backends.len(), 3);

    let fpm = &config.backends["fpm"];
    assert_eq!(fpm.kind(), "external");
    assert!(fpm.resets_per_request());

    let app = &config.backends["app"];
    assert_eq!(app.kind(), "worker");
    assert!(!app.resets_per_request());

    match &config.backends["win"] {
        BackendConfig::SpawnCgi(cgi) => {
            assert_eq!(cgi.workers, 8);
            assert_eq!(cgi.base_port, 19_001);
            assert_eq!(cgi.bind_host, "127.0.0.1");
        }
        other => panic!("expected spawn_cgi, got {other:?}"),
    }
}

#[test]
fn rejects_config_without_a_listener() {
    let config = Config::from_toml("").expect("empty config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("no listener configured"), "{err}");
}

#[test]
fn rejects_unknown_backend_as_default() {
    let text = r#"
[server]
default_backend = "nope"
http = "127.0.0.1:8080"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("default_backend"), "{err}");
}

/// **One backend needs no `default_backend`**, because there is nothing to choose between.
///
/// The rule is `exactly one`, not `the first one`: `backends` is a `BTreeMap`, so file order does not
/// survive parsing, and a first-one-wins rule would pick alphabetically — silently handing an
/// unrouted request to `assets` instead of `app`. Exactly-one has no order to get wrong.
#[test]
fn a_lone_backend_is_the_default_without_being_stated() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("a lone backend needs no default");
    assert_eq!(
        config.server.default_backend, None,
        "the file states none, and nothing may rewrite what the operator wrote"
    );
    assert_eq!(config.implied_backend(None), Some("app"));
    assert_eq!(config.inferred_default_backend(), Some("app"));
}

/// And it reaches the place a request actually consults.
///
/// `implied_backend` being right is not the same as the site carrying it: `[server]` is the unnamed
/// site, and every named site inherits from it, so an inference that stopped at the accessor would
/// leave the request path with `None` and fail every unrouted request at runtime.
#[test]
fn the_inferred_default_reaches_the_resolved_sites() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[sites.app]
hosts = ["a.test"]
document_root = "/srv/app"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("a lone backend needs no default");

    let sites = crate::sites::Sites::from_config(&config, Path::new("/srv/app"));
    assert_eq!(
        sites.default_site().default_backend.as_deref(),
        Some("app"),
        "the unnamed site must carry the inferred default, not the literal absence"
    );
    let site = sites.named().get("app").expect("the named site resolves");
    assert_eq!(
        site.default_backend.as_deref(),
        Some("app"),
        "a site that states nothing inherits the server's answer, inferred or written"
    );
}

/// **Two backends and no default stays refused**, and the message says how to fix it.
///
/// This is the half that must not soften: with several backends a request matching no route has
/// several possible answers, and choosing one would be a guess presented as a default.
#[test]
fn two_backends_without_a_default_are_refused_by_name() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[backends.zeta]
kind = "worker"
command = "php worker.php"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("ambiguity must be refused");
    let message = err.to_string();
    assert!(message.contains("default_backend"), "{message}");
    // Both names, sorted, so the operator is shown the choice rather than told it exists.
    assert!(
        message.contains("app") && message.contains("zeta"),
        "{message}"
    );
    assert!(
        message.contains("more than one"),
        "the message must explain why the inference did not apply: {message}"
    );
    assert_eq!(config.implied_backend(None), None);
}

/// A stated default is used as written, even when it is the only backend.
#[test]
fn a_stated_default_is_never_overridden_by_the_inference() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("valid");
    assert_eq!(
        config.implied_backend(config.server.default_backend.as_deref()),
        Some("app")
    );
    assert_eq!(
        config.inferred_default_backend(),
        None,
        "a stated default is not an inference, and `check` must not claim it was"
    );
}

/// No backends at all stays legal: the static-only deployment.
#[test]
fn no_backends_is_still_the_static_only_deployment() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
serve_only = "static"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config
        .validate()
        .expect("a static-only deployment needs no backend");
    assert_eq!(config.implied_backend(None), None);
    assert_eq!(config.inferred_default_backend(), None);
}

/// **A kind names the one backend of that kind**, which is the useful multi-backend spelling.
///
/// `app` and `assets` are different kinds, so `default_backend = "worker"` selects `app` without
/// repeating a name that a rename would break.
#[test]
fn a_kind_selects_the_one_backend_of_that_kind() {
    let text = r#"
[server]
default_backend = "worker"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[backends.assets]
kind = "spawn_cgi"
binary = "php-cgi"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config
        .validate()
        .expect("one worker makes the kind unambiguous");
    assert_eq!(config.implied_backend(Some("worker")), Some("app"));

    let sites = crate::sites::Sites::from_config(&config, Path::new("/srv/app"));
    assert_eq!(
        sites.default_site().default_backend.as_deref(),
        Some("app"),
        "the resolved site carries the name, not the kind the file wrote"
    );
}

/// The hyphenated spelling works too, because `check` prints kinds that way.
#[test]
fn a_kind_accepts_the_hyphenated_spelling_check_prints() {
    let text = r#"
[server]
default_backend = "spawn-cgi"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[backends.assets]
kind = "spawn_cgi"
binary = "php-cgi"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config
        .validate()
        .expect("`spawn-cgi` names the same kind as `spawn_cgi`");
    assert_eq!(config.implied_backend(Some("spawn-cgi")), Some("assets"));
}

/// **A backend named after a kind wins over the kind.**
///
/// This is the compatibility guarantee that makes the feature safe to add: before it existed,
/// `default_backend = "worker"` could only mean a backend *named* `worker`. If a kind took
/// precedence, adding this feature would have silently re-pointed that deployment's unrouted
/// requests at a different backend — a change nothing in the file records.
#[test]
fn a_backend_named_after_a_kind_is_not_shadowed_by_the_kind() {
    let text = r#"
[server]
default_backend = "worker"
http = "127.0.0.1:8080"
[backends.worker]
kind = "spawn_cgi"
binary = "php-cgi"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config
        .validate()
        .expect("the name exists, so it is not a kind lookup");
    assert_eq!(
        config.implied_backend(Some("worker")),
        Some("worker"),
        "the name must win: the file said a backend name, and `app` is merely of that kind"
    );
}

/// A kind that several backends share selects nothing, and is refused rather than guessed.
#[test]
fn a_kind_that_two_backends_share_is_refused() {
    let text = r#"
[server]
default_backend = "worker"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[backends.admin]
kind = "worker"
command = "php admin.php"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config
        .validate()
        .expect_err("an ambiguous kind must be refused");
    let message = err.to_string();
    assert!(message.contains("2 backends are of it"), "{message}");
    assert!(message.contains("name the one you mean"), "{message}");
    assert_eq!(config.implied_backend(Some("worker")), None);
}

/// A kind nothing has is reported as a kind, not as a missing name.
///
/// The distinction is the operator's fix: one is a typo in a name, the other is a kind they invented
/// or a deployment that is all `external`.
#[test]
fn a_kind_no_backend_has_names_the_kinds_that_are_present() {
    let text = r#"
[server]
default_backend = "upstream"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("no upstream exists");
    let message = err.to_string();
    assert!(message.contains("is a backend kind"), "{message}");
    assert!(message.contains("this deployment has worker"), "{message}");
}

/// The site rule reads the same way, and resolves to a name.
#[test]
fn a_site_may_also_state_a_kind() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[sites.admin]
hosts = ["admin.test"]
document_root = "/srv/admin"
default_backend = "spawn_cgi"
[backends.app]
kind = "worker"
command = "php worker.php"
[backends.admin]
kind = "spawn_cgi"
binary = "php-cgi"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config
        .validate()
        .expect("the site's kind is unambiguous too");

    let sites = crate::sites::Sites::from_config(&config, Path::new("/srv/app"));
    let admin = sites.named().get("admin").expect("the site resolves");
    assert_eq!(admin.default_backend.as_deref(), Some("admin"));
}

/// The kind list and `kind()` cannot drift apart.
///
/// `Config::validate` tells "a kind that selects nothing" from "a name that matches nothing" using
/// [`BackendConfig::is_kind`], which reads a literal list. If a variant were added without that
/// list, its kind would be reported as an unknown *name* — accurate-looking and misleading.
#[test]
fn the_kind_list_matches_what_the_variants_report() {
    // Parsed rather than constructed, because the `*Config` structs have no `Default` and inventing
    // one would be a fixture that could disagree with what the file parser accepts.
    let text = r#"
[server]
http = "127.0.0.1:8080"
[backends.w]
kind = "worker"
command = "php worker.php"
[backends.a]
kind = "attach"
relay = "unix:/run/app/workers.sock"
workers = 4
[backends.e]
kind = "external"
socket = "127.0.0.1:9102"
[backends.c]
kind = "spawn_cgi"
binary = "php-cgi"
[backends.u]
kind = "upstream"
origin = "http://127.0.0.1:9103"
"#;
    let config = Config::from_toml(text).expect("config parses");

    let mut reported: Vec<&str> = config.backends.values().map(BackendConfig::kind).collect();
    reported.sort_unstable();
    let mut known: Vec<&str> = BackendConfig::kinds().to_vec();
    known.sort_unstable();

    assert_eq!(
        reported, known,
        "`kind()` and `kinds()` must describe the same set: one is what a file may say, the other \
         is what a variant answers, and validation uses the second to explain the first"
    );
    for kind in &reported {
        assert!(
            BackendConfig::is_kind(kind),
            "`{kind}` is reported by `kind()` but `is_kind` does not accept it"
        );
    }
}

#[test]
fn rejects_non_loopback_php_cgi_bind() {
    let text = r#"
[server]
default_backend = "win"
http = "127.0.0.1:8080"
[backends.win]
kind = "spawn_cgi"
binary = "php-cgi"
bind_host = "0.0.0.0"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("loopback"), "{err}");
}

#[test]
fn parses_listener_forms() {
    assert_eq!(
        parse_listener("127.0.0.1:9000"),
        Ok(Listener::Tcp("127.0.0.1:9000".parse().unwrap()))
    );
    assert_eq!(
        parse_listener("unix:/run/php.sock"),
        Ok(Listener::Unix(PathBuf::from("/run/php.sock")))
    );
    assert!(parse_listener("unix:").is_err());
    assert!(parse_listener("").is_err());
    assert!(parse_listener("nonsense").is_err());
}

#[test]
fn the_checksum_is_required_unless_the_configuration_says_otherwise() {
    // The default matters more than the option: a configuration written before this
    // setting existed must keep the old wire behaviour, which is `required`.
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("config parses");
    match &config.backends["app"] {
        BackendConfig::Worker(w) => assert_eq!(w.checksum, ChecksumMode::Required),
        other => panic!("expected worker, got {other:?}"),
    }

    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
checksum = "optional"
"#;
    let config = Config::from_toml(text).expect("config parses");
    match &config.backends["app"] {
        BackendConfig::Worker(w) => {
            assert_eq!(w.checksum, ChecksumMode::Optional);
            assert!(!w.checksum.is_required());
        }
        other => panic!("expected worker, got {other:?}"),
    }
}

#[test]
fn a_worker_uses_the_better_transport_for_the_platform() {
    // The default is the whole point of the option: a deployment that never mentions
    // `transport` must still get the transport that platform is fast on. On Windows that
    // is the socket — PHP reads a pipe there in ~8 ms median against ~0.09 ms on a
    // socket, which was 21× on the whole route. The assertion is platform-conditional
    // rather than a fixed value because the correct answer is.
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let expected = if cfg!(windows) {
        WorkerTransport::Socket
    } else {
        WorkerTransport::Pipe
    };
    match &config.backends["app"] {
        BackendConfig::Worker(w) => {
            assert_eq!(w.transport, expected);
            assert_eq!(w.transport.is_pipe(), !cfg!(windows));
        }
        other => panic!("expected worker, got {other:?}"),
    }
}

#[test]
fn the_transport_can_always_be_stated_explicitly() {
    // The platform default has to be overridable in both directions, or the setting is
    // only half a setting: a Windows deployment may want the pipe, and a Linux one may
    // want the socket to compare the two on its own hardware.
    for (written, expected) in [
        ("pipe", WorkerTransport::Pipe),
        ("socket", WorkerTransport::Socket),
    ] {
        let text = format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
transport = "{written}"
"#
        );
        let config = Config::from_toml(&text).expect("config parses");
        match &config.backends["app"] {
            BackendConfig::Worker(w) => assert_eq!(w.transport, expected, "{written}"),
            other => panic!("expected worker, got {other:?}"),
        }
    }
    assert_eq!(WorkerTransport::Pipe.to_string(), "pipe");
    assert_eq!(WorkerTransport::Socket.to_string(), "socket");
}

#[test]
fn an_unknown_checksum_mode_is_refused_rather_than_defaulted() {
    // The setting decides whether integrity is checked, so a typo — `checksum = "opt"`,
    // say, or the `none` a reader might guess at — must not silently mean `required`.
    // Guessing either way is wrong, so the field is denied rather than defaulted.
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
checksum = "none"
"#;
    let err = Config::from_toml(text).expect_err("must not be accepted");
    let message = err.to_string();
    assert!(message.contains("unknown variant `none`"), "{message}");
    assert!(
        message.contains("expected `required` or `optional`"),
        "the error must name the two valid modes: {message}"
    );
}

#[test]
fn parses_a_spawn_cgi_unix_socket() {
    let text = r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "php-cgi"
socket = "unix:/run/volucer/cgi"
"#;
    let config = Config::from_toml(text).expect("config parses");
    match &config.backends["cgi"] {
        BackendConfig::SpawnCgi(cgi) => {
            assert_eq!(cgi.socket.as_deref(), Some("unix:/run/volucer/cgi"));
            // Defaults still present, so a TCP fallback stays expressible.
            assert_eq!(cgi.base_port, 19_001);
        }
        other => panic!("expected spawn_cgi, got {other:?}"),
    }
}

#[test]
fn parses_a_worker_unix_socket() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
transport = "socket"
socket = "unix:/run/volucer/worker"
"#;
    let config = Config::from_toml(text).expect("config parses");
    match &config.backends["app"] {
        BackendConfig::Worker(worker) => {
            assert_eq!(worker.socket.as_deref(), Some("unix:/run/volucer/worker"));
            assert_eq!(worker.transport, WorkerTransport::Socket);
        }
        other => panic!("expected worker, got {other:?}"),
    }
}

/// A worker with no `socket` key keeps the ephemeral loopback port.
///
/// This is the compatibility property that makes the new key a key rather than a
/// new transport variant: a configuration written before it exists still means
/// exactly what it meant.
#[test]
fn an_omitted_worker_socket_keeps_the_loopback_port() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
transport = "socket"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config
        .validate()
        .expect("a socket transport with no address is valid");
    match &config.backends["app"] {
        BackendConfig::Worker(worker) => assert!(worker.socket.is_none()),
        other => panic!("expected worker, got {other:?}"),
    }
}

/// The three mistakes this key exists to make impossible, each refused by name.
#[test]
fn a_worker_socket_is_validated() {
    let with = |socket: &str, transport: &str| {
        format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
transport = "{transport}"
socket = "{socket}"
"#
        )
    };
    let err = |socket: &str, transport: &str| {
        Config::from_toml(&with(socket, transport))
            .expect("the file parses; the refusal is validation")
            .validate()
            .expect_err("must be refused")
            .to_string()
    };

    // A bare path is relative, so it would bind a file named `unix:/run/x`.
    let message = err("/run/volucer/worker", "socket");
    assert!(message.contains("unix:"), "{message}");

    // Ignored, not honoured: the transport never opens a socket.
    let message = err("unix:/run/volucer/worker", "pipe");
    assert!(message.contains("transport"), "{message}");

    // The loopback transport picks its own port, so an explicit one is not a thing
    // this key can do.
    let message = err("127.0.0.1:9000", "socket");
    assert!(message.contains("ephemeral"), "{message}");

    if cfg!(unix) {
        Config::from_toml(&with("unix:/run/volucer/worker", "socket"))
            .expect("parses")
            .validate()
            .expect("a valid unix socket prefix is accepted");
    }
}

/// `[jobs.pool]` takes the same key, because a consumer's transport default is the
/// same one and an address has to be expressible wherever the transport can be.
#[test]
fn a_jobs_pool_socket_is_validated() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[jobs]
consume = ["emails"]
[jobs.pipelines.emails]
driver = "memory"
[jobs.pool]
command = "php consumer.php"
transport = "socket"
socket = "unix:/run/volucer/consumer"
"#;
    let config = Config::from_toml(text).expect("parses");
    if cfg!(unix) {
        config
            .validate()
            .expect("a valid consumer socket prefix is accepted");
    }
    let pool = config
        .jobs
        .as_ref()
        .and_then(|jobs| jobs.pool.as_ref())
        .expect("pool");
    assert_eq!(pool.socket.as_deref(), Some("unix:/run/volucer/consumer"));

    let bad = text.replace("unix:/run/volucer/consumer", "/run/volucer/consumer");
    let message = Config::from_toml(&bad)
        .expect("parses")
        .validate()
        .expect_err("a bare path is refused")
        .to_string();
    assert!(message.contains("jobs.pool.socket"), "{message}");
}

/// The per-slot suffix, so a worker and a `php-cgi` slot cannot disagree about it.
#[test]
fn a_worker_socket_path_uses_the_same_suffix_rule_as_cgi() {
    for index in [0u32, 1, 12, u32::MAX] {
        let cgi = cgi_socket_path("unix:/run/volucer/cgi", index);
        let worker = worker_socket_path("unix:/run/volucer/cgi", index);
        assert_eq!(
            cgi, worker,
            "the two slot paths must be the same for {index}"
        );
    }
    assert_eq!(
        worker_socket_path("unix:/run/volucer/worker", 3),
        PathBuf::from("/run/volucer/worker.3.sock")
    );
}

/// `children` decides both the pool's concurrency and `PHP_FCGI_CHILDREN`, and the two must
/// agree — which means `children = 1` must not ask php-cgi for "a master plus one child".
/// The failure this pins was measured, not hypothetical: with the raw value written
/// through, `children = 1` started **two** processes and served one pid, so an operator
/// who wrote `1` got two PHP boots for one request's worth of concurrency. The mapping lives
/// in `fcgi_children` and this is the test that keeps it there.
#[test]
fn children_decides_process_count_and_concurrency_together() {
    fn config(children: u32) -> SpawnCgiConfig {
        SpawnCgiConfig {
            binary: Some(PathBuf::from("/usr/bin/php-cgi")),
            command: None,
            workers: 1,
            bind_host: "127.0.0.1".to_string(),
            base_port: 19_001,
            socket: None,
            children,
            concurrency: 0,
            max_requests: None,
            working_dir: None,
            env: Default::default(),
            legacy_capacity: None,
            services: Vec::new(),
        }
    }

    // `0` and `1` are one process and one request: php-cgi spawns CHILDREN *in addition
    // to itself*, so writing `1` would be two processes for no extra concurrency.
    for children in [0, 1] {
        let c = config(children);
        assert_eq!(
            c.fcgi_children(),
            0,
            "children={children} must mean one process"
        );
        assert_eq!(
            c.slot_concurrency(),
            1,
            "children={children} must mean one request"
        );
    }

    // Above that, the two numbers are the same number, and `n` children means `n`
    // processes behind the master and `n` requests at once.
    for children in [2u32, 4, 16, 1024] {
        let c = config(children);
        assert_eq!(c.fcgi_children(), children);
        assert_eq!(c.slot_concurrency(), children as usize);
    }
}

/// A `children` value that would fork a machine to death is refused rather than obeyed.
#[test]
fn an_absurd_children_count_is_refused() {
    let text = r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "php-cgi"
children = 100000
"#;
    let err = Config::from_toml(text)
        .expect("parses; the refusal is validation")
        .validate()
        .expect_err("must be refused")
        .to_string();
    assert!(err.contains("children"), "{err}");
    assert!(
        err.contains("typo"),
        "the message must say what it probably is: {err}"
    );
}

/// The two drivers are told apart by which key is set, and neither is a default.
///
/// A table that names neither cannot be started — there is no program to run — and a table
/// that names both has said two different things about how one slot is started. Both are
/// refused at validation, so the failure is a message naming the keys rather than a pool of
/// slots that spawn nothing.
#[test]
fn a_spawn_cgi_backend_must_name_exactly_one_program() {
    fn error(text: &str) -> String {
        Config::from_toml(text)
            .expect("the shape parses; the refusal is validation")
            .validate()
            .expect_err("must be refused")
            .to_string()
    }

    let neither = r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
"#;
    let err = error(neither);
    assert!(err.contains("binary") && err.contains("command"), "{err}");

    let both = r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
command = "gunicorn app.wsgi --bind {address}"
"#;
    let err = error(both);
    assert!(
        err.contains("both") && err.contains("command"),
        "a file naming both must say so: {err}"
    );
}

/// A `command` deployment is accepted, and `concurrency` — not `children` — is its width.
#[test]
fn a_command_driver_uses_concurrency_for_its_slot_width() {
    let text = r#"
[server]
default_backend = "py"
http = "127.0.0.1:8080"
[backends.py]
kind = "spawn_cgi"
command = "gunicorn app.wsgi --bind {address}"
workers = 2
concurrency = 4
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("validates");
    let BackendConfig::SpawnCgi(cgi) = &config.backends["py"] else {
        panic!("expected spawn_cgi");
    };
    assert_eq!(cgi.driver(), SpawnDriver::Command);
    assert!(!cgi.is_php_cgi());
    assert_eq!(cgi.slot_concurrency(), 4);
    // Not consulted, so a `command` deployment does not accidentally write
    // `PHP_FCGI_CHILDREN` into a Python process.
    assert_eq!(cgi.fcgi_children(), 0);
    // The host total is the product, and it is what `check` prints: the command runs once per
    // slot, so four Gunicorn workers behind two slots is eight processes.
    let (processes, working) = cgi.process_count();
    assert_eq!(processes, 8, "2 slots × 4 concurrency");
    assert!(
        working.contains("workers 2") && working.contains("concurrency 4"),
        "the working must name both keys so the rule is readable: {working}"
    );
}

/// `process_count` resolves `workers = 0` rather than multiplying by zero.
///
/// The bug this pins is the obvious one: `0` means "one per logical CPU", so a bare product
/// would report zero processes for a deployment that is about to start as many as the machine
/// has cores — the opposite of what an operator reads the line for.
#[test]
fn the_process_count_resolves_a_zero_worker_count() {
    let text = r#"
[server]
default_backend = "py"
http = "127.0.0.1:8080"
[backends.py]
kind = "spawn_cgi"
command = "gunicorn app.wsgi --bind {address}"
concurrency = 3
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("validates");
    let BackendConfig::SpawnCgi(cgi) = &config.backends["py"] else {
        panic!("expected spawn_cgi");
    };
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1);
    assert_eq!(cgi.workers, 0, "the default is one per logical CPU");
    assert_eq!(cgi.slot_count(), cores);
    assert_eq!(cgi.process_count().0, cores * 3);
}

/// The php-cgi total counts the master, because `children` are spawned *in addition to it*.
///
/// This is the other half of the same arithmetic, and the reason the two drivers cannot share
/// one formula: `workers × children` would be four processes short in the example below, and
/// `check` is where an operator sizes `pids_limit`.
#[test]
fn the_process_count_of_a_php_cgi_pool_counts_the_master() {
    let text = r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
workers = 4
children = 4
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("validates");
    let BackendConfig::SpawnCgi(cgi) = &config.backends["cgi"] else {
        panic!("expected spawn_cgi");
    };
    // Four slots of one master plus four children: four more than `workers × children`.
    assert_eq!(cgi.process_count().0, 20);
    assert_ne!(cgi.process_count().0, 4 * 4, "the master is a process too");
}

/// `children` and `concurrency` describe one number, so a file setting both is refused.
///
/// The failure this prevents is silent: whichever the pool preferred would be the width, and
/// the other key would be a setting an operator believes is in force.
#[test]
fn children_and_concurrency_cannot_both_describe_one_slot() {
    let command_with_children = r#"
[server]
default_backend = "py"
http = "127.0.0.1:8080"
[backends.py]
kind = "spawn_cgi"
command = "gunicorn app.wsgi --bind {address}"
children = 4
"#;
    let err = Config::from_toml(command_with_children)
        .expect("parses")
        .validate()
        .expect_err("must be refused")
        .to_string();
    assert!(
        err.contains("children") && err.contains("concurrency"),
        "{err}"
    );

    let binary_with_concurrency = r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
concurrency = 4
"#;
    let err = Config::from_toml(binary_with_concurrency)
        .expect("parses")
        .validate()
        .expect_err("must be refused")
        .to_string();
    assert!(
        err.contains("concurrency") && err.contains("children"),
        "{err}"
    );
}

/// The renamed key is refused **by name**, not as an unknown field.
///
/// `deny_unknown_fields` would report `unknown field 'capacity'`, which is true and explains
/// nothing: the key did not disappear, it was renamed because `capacity` read like a total for
/// the backend while the number is per slot. Both halves are asserted — that the message names
/// `concurrency`, and that the new spelling is what makes the same file parse and validate.
#[test]
fn the_old_capacity_key_is_refused_with_the_new_name() {
    let old = r#"
[server]
default_backend = "py"
http = "127.0.0.1:8080"
[backends.py]
kind = "spawn_cgi"
command = "gunicorn app.wsgi --bind {address}"
workers = 4
capacity = 4
"#;
    let err = Config::from_toml(old)
        .expect("the old key still parses, so the refusal can name it")
        .validate()
        .expect_err("must be refused")
        .to_string();
    assert!(
        err.contains("capacity") && err.contains("concurrency"),
        "the refusal must name both spellings: {err}"
    );

    let new = old.replace("capacity = 4", "concurrency = 4");
    Config::from_toml(&new)
        .expect("parses")
        .validate()
        .expect("the new spelling is what the same file should say");
}

/// `{workers}` is refused by name, because a slot command has no sibling count.
///
/// The alternative is what used to happen: the text reaches the program as an argument, its
/// parser rejects it, and the operator reads an error naming neither volucer nor the key. The
/// refusal has to say what to write instead, so both halves are asserted.
#[test]
fn the_workers_placeholder_is_refused_with_the_concurrency_one() {
    let text = r#"
[server]
default_backend = "py"
http = "127.0.0.1:8080"
[backends.py]
kind = "spawn_cgi"
command = "gunicorn app.wsgi --workers {workers} --bind {address}"
workers = 4
concurrency = 4
"#;
    let err = Config::from_toml(text)
        .expect("parses")
        .validate()
        .expect_err("must be refused")
        .to_string();
    assert!(
        err.contains("{workers}") && err.contains("{concurrency}"),
        "the refusal must name both the mistake and the replacement: {err}"
    );
}

/// A token carrying both placeholders is substituted in one pass, and the two are independent.
///
/// `--workers {concurrency} --bind {address}` is the recommended form, so the pairing is the case
/// that has to keep working as the substitution grows. The address is arbitrary here because this
/// is the pool's `command()` being exercised, not a socket.
#[test]
fn a_command_carries_both_placeholders() {
    let text = r#"
[server]
default_backend = "py"
http = "127.0.0.1:8080"
[backends.py]
kind = "spawn_cgi"
command = "gunicorn app.wsgi --workers {concurrency} --bind {address}"
workers = 2
concurrency = 4
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("validates");
    let BackendConfig::SpawnCgi(cgi) = &config.backends["py"] else {
        panic!("expected spawn_cgi");
    };
    assert_eq!(cgi.slot_concurrency(), 4, "the number the pool admits");
    // The same substitution the pool performs, asserted on the configured string so the test
    // fails on the file rather than on a constructed `SlotLaunch`.
    let resolved = cgi
        .command
        .as_deref()
        .expect("command is set")
        .replace(
            FCGI_CONCURRENCY_PLACEHOLDER,
            &cgi.slot_concurrency().to_string(),
        )
        .replace(FCGI_ADDRESS_PLACEHOLDER, "127.0.0.1:19001");
    assert_eq!(
        resolved,
        "gunicorn app.wsgi --workers 4 --bind 127.0.0.1:19001"
    );
}

/// `concurrency = 0` means one, so `{concurrency}` must substitute `1` and never `0`.
///
/// A program handed `--workers 0` starts no workers while the pool still admits one request to
/// it — a slot that accepts and never answers. The distinction only exists because the resolved
/// value and the configured value differ here.
#[test]
fn a_zero_concurrency_substitutes_the_resolved_one() {
    let text = r#"
[server]
default_backend = "py"
http = "127.0.0.1:8080"
[backends.py]
kind = "spawn_cgi"
command = "gunicorn app.wsgi --workers {concurrency} --bind {address}"
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("validates");
    let BackendConfig::SpawnCgi(cgi) = &config.backends["py"] else {
        panic!("expected spawn_cgi");
    };
    assert_eq!(cgi.concurrency, 0, "unset");
    assert_eq!(cgi.slot_concurrency(), 1, "resolved: zero means one");
    assert_ne!(
        cgi.slot_concurrency().to_string(),
        cgi.concurrency.to_string(),
        "the substituted value is not the configured one, which is the point"
    );
}

/// A `binary` deployment written before `command` existed keeps its meaning exactly.
////// The compatibility property that makes this an optional key rather than a new variant: a
/// file that names only `binary` must be `PhpCgi`, with `children` as its width and no
/// requirement for a placeholder it has never heard of.
#[test]
fn an_existing_binary_configuration_is_still_the_php_cgi_driver() {
    let text = r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
children = 8
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("validates");
    let BackendConfig::SpawnCgi(cgi) = &config.backends["cgi"] else {
        panic!("expected spawn_cgi");
    };
    assert_eq!(cgi.driver(), SpawnDriver::PhpCgi);
    assert!(cgi.is_php_cgi());
    assert_eq!(cgi.slot_concurrency(), 8);
    assert_eq!(cgi.fcgi_children(), 8);
    assert!(cgi.working_dir.is_none(), "an absent key stays absent");
}

#[test]
fn a_cgi_socket_prefix_is_validated() {
    // On Windows the platform check fires first; elsewhere these are the path
    // checks, so assert only on what must hold everywhere.
    let err = validate_cgi_socket("/run/volucer/cgi").expect_err("no unix: prefix");
    assert!(err.contains("unix:"), "{err}");

    let err = validate_cgi_socket("unix:").expect_err("empty path");
    assert!(err.contains("empty"), "{err}");

    let err = validate_cgi_socket("unix:relative/cgi").expect_err("relative path");
    assert!(err.contains("absolute"), "{err}");

    if cfg!(unix) {
        validate_cgi_socket("unix:/run/volucer/cgi").expect("a plain absolute path is valid");
        // A path that looks finished is still just a prefix, which is the
        // documented contract — it binds `/run/php.sock.0.sock`.
        validate_cgi_socket("unix:/run/php.sock").expect("a finished-looking path is a prefix");
    }
}

#[test]
fn a_socket_path_that_only_fits_before_the_suffix_is_rejected() {
    // 107 is the portable `sun_path` maximum; the check must account for the
    // `.<slot>.sock` the pool appends, or slot 0 binds and slot 11 fails.
    //
    // The paths have to be absolute, or they never reach the length check —
    // they are rejected as relative first, and a test that exercises the
    // length rule has to get past that. The leading `/` is part of the path
    // like any other byte.
    let suffix = ".".len() + 10 + ".sock".len();
    let fits = format!("unix:/{}", "a".repeat(UNIX_SOCKET_PATH_MAX - suffix - 1));
    let too_long = format!("unix:/{}", "a".repeat(UNIX_SOCKET_PATH_MAX));

    if cfg!(unix) {
        // The boundary in both directions. `fits` is exactly the longest
        // prefix that still leaves room for a worst-case slot suffix, so one
        // byte more must be rejected.
        assert_eq!(
            fits.len() - "unix:".len() + suffix,
            UNIX_SOCKET_PATH_MAX,
            "the `fits` fixture must sit exactly on the boundary"
        );
        validate_cgi_socket(&fits).expect("a path with room for the suffix is accepted");
        assert!(
            validate_cgi_socket(&too_long).is_err(),
            "a path that leaves no room for the suffix must be rejected"
        );
    }
}

#[test]
fn cgi_socket_path_appends_the_slot() {
    assert_eq!(
        cgi_socket_path("unix:/run/volucer/cgi", 0),
        PathBuf::from("/run/volucer/cgi.0.sock")
    );
    assert_eq!(
        cgi_socket_path("unix:/run/volucer/cgi", 11),
        PathBuf::from("/run/volucer/cgi.11.sock")
    );
}
