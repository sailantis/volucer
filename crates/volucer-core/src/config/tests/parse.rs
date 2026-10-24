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

/// `children` decides both the pool's capacity and `PHP_FCGI_CHILDREN`, and the two must
/// agree — which means `children = 1` must not ask php-cgi for "a master plus one child".
/// The failure this pins was measured, not hypothetical: with the raw value written
/// through, `children = 1` started **two** processes and served one pid, so an operator
/// who wrote `1` got two PHP boots for one request's worth of capacity. The mapping lives
/// in `fcgi_children` and this is the test that keeps it there.
#[test]
fn children_decides_process_count_and_capacity_together() {
    fn config(children: u32) -> SpawnCgiConfig {
        SpawnCgiConfig {
            binary: Some(PathBuf::from("/usr/bin/php-cgi")),
            command: None,
            workers: 1,
            bind_host: "127.0.0.1".to_string(),
            base_port: 19_001,
            socket: None,
            children,
            capacity: 0,
            max_requests: None,
            working_dir: None,
            env: Default::default(),
        }
    }

    // `0` and `1` are one process and one request: php-cgi spawns CHILDREN *in addition
    // to itself*, so writing `1` would be two processes for no extra capacity.
    for children in [0, 1] {
        let c = config(children);
        assert_eq!(
            c.fcgi_children(),
            0,
            "children={children} must mean one process"
        );
        assert_eq!(
            c.slot_capacity(),
            1,
            "children={children} must mean one request"
        );
    }

    // Above that, the two numbers are the same number, and `n` children means `n`
    // processes behind the master and `n` requests at once.
    for children in [2u32, 4, 16, 1024] {
        let c = config(children);
        assert_eq!(c.fcgi_children(), children);
        assert_eq!(c.slot_capacity(), children as usize);
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

/// A `command` deployment is accepted, and `capacity` — not `children` — is its width.
#[test]
fn a_command_driver_uses_capacity_for_its_slot_width() {
    let text = r#"
[server]
default_backend = "py"
http = "127.0.0.1:8080"
[backends.py]
kind = "spawn_cgi"
command = "gunicorn app.wsgi --bind {address}"
workers = 2
capacity = 4
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("validates");
    let BackendConfig::SpawnCgi(cgi) = &config.backends["py"] else {
        panic!("expected spawn_cgi");
    };
    assert_eq!(cgi.driver(), SpawnDriver::Command);
    assert!(!cgi.is_php_cgi());
    assert_eq!(cgi.slot_capacity(), 4);
    // Not consulted, so a `command` deployment does not accidentally write
    // `PHP_FCGI_CHILDREN` into a Python process.
    assert_eq!(cgi.fcgi_children(), 0);
}

/// `children` and `capacity` describe one number, so a file setting both is refused.
///
/// The failure this prevents is silent: whichever the pool preferred would be the width, and
/// the other key would be a setting an operator believes is in force.
#[test]
fn children_and_capacity_cannot_both_describe_one_slot() {
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
        err.contains("children") && err.contains("capacity"),
        "{err}"
    );

    let binary_with_capacity = r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
capacity = 4
"#;
    let err = Config::from_toml(binary_with_capacity)
        .expect("parses")
        .validate()
        .expect_err("must be refused")
        .to_string();
    assert!(
        err.contains("capacity") && err.contains("children"),
        "{err}"
    );
}

/// A `binary` deployment written before `command` existed keeps its meaning exactly.
///
/// The compatibility property that makes this an optional key rather than a new variant: a
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
    assert_eq!(cgi.slot_capacity(), 8);
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
