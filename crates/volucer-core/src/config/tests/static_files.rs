//! Static files, body limits, rate limiting, environment, and the front controller.

use super::*;

#[test]
fn static_files_are_on_unless_a_deployment_says_otherwise() {
    let config = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("config parses");
    config.validate().expect("a listener alone is enough");
    let statics = config.server.statics();
    assert!(
        statics.enabled,
        "a site serves its own assets unless it says not to"
    );
    // A front controller is unaffected by the default: `/about` still reaches it, because
    // what changed is only the answer for a path that names a real, non-script file.
    assert_eq!(statics.dotfiles, Dotfiles::Deny);
    assert!(statics.directory_index);
    assert_eq!(statics.max_multirange_bytes, 1024 * 1024);

    // And turning it off is one key.
    let off = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\nstatic_files = false\n")
        .expect("config parses");
    assert!(!off.server.statics().enabled);
}

/// A FastCGI-only deployment never serves static content, and `volucer` refuses to
/// start one that asks for it. Leaving the key out is not asking.
#[test]
fn a_fastcgi_only_deployment_may_leave_the_static_key_out() {
    let text = r#"
[server]
fastcgi = "127.0.0.1:9000"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config
        .validate()
        .expect("the default must not make a FastCGI-only config unshippable");

    // But an explicit request with no HTTP listener is a mistake worth reporting.
    for written in [
        "static_files = true\n",
        "static_files = { enabled = true }\n",
    ] {
        let text = format!("[server]\nfastcgi = \"127.0.0.1:9000\"\n{written}");
        let config = Config::from_toml(&text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("static"), "{written}: {err}");
    }
}

#[test]
fn parses_a_static_section() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[server.static_files]
enabled = true
precompressed = true
directory_index = false
dotfiles = "allow"
cache_control = "public, max-age=31536000, immutable"
max_multirange_bytes = 4096
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");

    let statics = config.server.statics();
    assert!(statics.enabled);
    assert!(statics.precompressed);
    assert!(!statics.directory_index);
    assert_eq!(statics.dotfiles, Dotfiles::Allow);
    assert_eq!(statics.cache_control, "public, max-age=31536000, immutable");
    assert_eq!(statics.max_multirange_bytes, 4096);
}

#[test]
fn rejects_static_content_with_nowhere_to_serve_it() {
    let text = r#"
[server]
fastcgi = "127.0.0.1:9000"
[server.static_files]
enabled = true
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("no server.http"), "{err}");
}

#[test]
fn rejects_an_empty_cache_control_and_a_zero_multirange_limit() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[server.static_files]
enabled = true
cache_control = "  "
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("cache_control"), "{err}");

    let text = r#"
[server]
http = "127.0.0.1:8080"
[server.static_files]
enabled = true
max_multirange_bytes = 0
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("max_multirange_bytes"), "{err}");
}

#[test]
fn a_body_limit_that_is_absent_or_stated_is_one_answer() {
    // The point of the key is that the answer must not depend on *which* front-end asks,
    // so it is resolved once, here, and both callers read it.
    let text = r#"
[server]
http = "127.0.0.1:8080"
"#;
    let config = Config::from_toml(text).expect("config parses");
    assert_eq!(config.server.max_body_bytes(), DEFAULT_MAX_BODY_BYTES);
    assert_eq!(config.server.max_body_bytes(), 32 * 1024 * 1024);
    config.validate().expect("an absent key is valid");

    let text = r#"
[server]
http = "127.0.0.1:8080"
max_body_bytes = 1048576
"#;
    let config = Config::from_toml(text).expect("config parses");
    assert_eq!(config.server.max_body_bytes(), 1_048_576);
    config.validate().expect("a stated limit is valid");
}

#[test]
fn a_zero_body_limit_is_refused_rather_than_read_as_unlimited() {
    // Zero cannot mean "no limit": RoadRunner's `max_request_size` says "zero means no
    // limit" and volucer's does not, so the *names* differ too. Reading a zero as unlimited
    // is the one interpretation that turns a typo into an unbounded allocation.
    let text = r#"
[server]
http = "127.0.0.1:8080"
max_body_bytes = 0
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("max_body_bytes"), "{err}");
    assert!(err.to_string().contains("32 MiB"), "{err}");
}

#[test]
fn a_misspelled_body_limit_key_is_rejected_rather_than_ignored() {
    // `deny_unknown_fields` on `ServerConfig` is what makes the unit in the name safe: a
    // deployment that copies RoadRunner's `max_request_size` gets an error, not a server that
    // silently keeps the default while reading as configured.
    let text = r#"
[server]
http = "127.0.0.1:8080"
max_request_size = 256
"#;
    let err = Config::from_toml(text).expect_err("an unknown key must be refused");
    assert!(err.to_string().contains("max_request_size"), "{err}");
}

#[test]
fn a_rate_limit_table_that_is_absent_is_an_off_limiter() {
    // Off is `None`, not a policy that allows everything: a front-end with no policy does one
    // `Option` check and never touches a map or a clock.
    let text = r#"
[server]
http = "127.0.0.1:8080"
"#;
    let config = Config::from_toml(text).expect("config parses");
    assert!(config.server.rate_limiter().expect("valid").is_none());
    config.validate().expect("an absent table is valid");
}

#[test]
fn a_stated_rate_limit_resolves_its_defaults() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[server.rate_limit]
rate = 60
interval_ms = 60000
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("valid");
    let limiter = config
        .server
        .rate_limiter()
        .expect("valid")
        .expect("a table is a limiter");

    // Defaults, asserted together because each is a departure from either RoadRunner or the
    // obvious reading, and a future edit could change any of them silently.
    assert_eq!(limiter.key(), crate::ratelimit::KeyMode::Ip);
    assert_eq!(limiter.burst(), 60, "the burst defaults to the rate");
    assert_eq!(limiter.max_entries(), crate::ratelimit::DEFAULT_MAX_ENTRIES);
    assert_eq!(limiter.per_second(), 1.0, "60 requests per 60 seconds");

    // And the default interval is one second, so `rate` alone means "per second".
    let text = r#"
[server]
http = "127.0.0.1:8080"
[server.rate_limit]
rate = 5
"#;
    let config = Config::from_toml(text).expect("config parses");
    assert_eq!(
        config
            .server
            .rate_limiter()
            .expect("valid")
            .expect("a limiter")
            .per_second(),
        5.0
    );
}

#[test]
fn an_unusable_rate_limit_is_refused_by_the_same_constructor_the_server_uses() {
    // `validate` calls `rate_limiter()`, so these four assertions are about the file being
    // refused *and* about the reason reaching the operator — not about a second copy of the
    // rules that could drift from the one the listener applies.
    let cases = [
        // A rate of zero, which is the shape a template renders when a variable is empty.
        ("[server.rate_limit]\nrate = 0\n", "rate"),
        // A header named beside a key that does not read one.
        (
            "[server.rate_limit]\nrate = 1\nheader = \"X-Client-Id\"\n",
            "header",
        ),
        // A header key with no name, where the name is the identity.
        (
            "[server.rate_limit]\nrate = 1\nkey = \"header\"\n",
            "header",
        ),
    ];
    for (table, needle) in cases {
        let text = format!("[server]\nhttp = \"127.0.0.1:8080\"\n{table}");
        let parsed = Config::from_toml(&text).expect("the TOML itself is well-formed");
        let err = parsed.validate().expect_err("must fail");
        assert!(
            err.to_string().contains(needle),
            "the refusal must name {needle:?}: {err}"
        );
        assert!(
            err.to_string().contains("server.rate_limit"),
            "and the table it came from: {err}"
        );
    }

    // A table with no `rate` at all is refused by the deserializer rather than by `validate`,
    // because `rate` has no default: a rate limit whose rate defaulted would be a table that
    // appears to protect something. It is asserted here because it is the same question — a
    // table that is all defaults must not silently mean "no limit" — with a different half of
    // the code answering it.
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n[server.rate_limit]\n";
    let err = Config::from_toml(text).expect_err("a rate is required");
    assert!(err.to_string().contains("rate"), "{err}");
}

#[test]
fn a_rate_limit_key_mode_is_parsed_strictly() {
    // A typo must not silently choose a mode: `key = "ip "` or `key = "address"` would
    // otherwise limit something the operator did not write, and a per-caller limit that
    // became a global one is the failure that matters here.
    for bad in ["ip ", "address", "IP_HEADER", ""] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n[server.rate_limit]\nrate = 1\nkey = {bad:?}\n"
        );
        assert!(
            Config::from_toml(&text).is_err(),
            "key = {bad:?} must not parse"
        );
    }

    // RoadRunner's `header` mode is the one value that is not a default, and it is accepted
    // with the key it requires.
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n[server.rate_limit]\nrate = 1\n\
                key = \"header\"\nheader = \"X-Client-Id\"\n";
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("valid");
}

#[test]
fn an_unknown_static_key_is_rejected_rather_than_ignored() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[server.static_files]
enable = true
"#;
    // A typo that silently leaves static serving off would be a confusing
    // bug to diagnose from the outside, which is why the section denies
    // unknown fields like the rest of the model.
    assert!(Config::from_toml(text).is_err());
}

#[test]
fn a_deployment_can_be_described_entirely_in_the_file() {
    // The point of these keys: a systemd unit or a container image should not
    // have to carry a command line to say where the application lives.
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
document_root = "./public"
front_controller = "public/index.php"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");
    assert_eq!(
        config.server.document_root.as_deref(),
        Some(Path::new("./public"))
    );
    assert_eq!(
        config.server.resolve_front_controller(None).as_deref(),
        Some("public/index.php")
    );
}

#[test]
fn the_command_line_front_controller_wins_over_the_file() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
front_controller = "index.php"
"#;
    let config = Config::from_toml(text).expect("config parses");

    assert_eq!(
        config
            .server
            .resolve_front_controller(Some("app.php"))
            .as_deref(),
        Some("app.php")
    );
    // An empty flag is not "unset": it is the operator asking for files only.
    assert_eq!(config.server.resolve_front_controller(Some("   ")), None);
}

/// `[server.env]` is a baseline, and a backend's own value wins over it.
///
/// The precedence is the whole content of the feature: written the other way round, a deployment
/// that moved one backend onto a different database could not express it, which is the case
/// [`JobsPoolConfig::env`] documents as ordinary rather than exotic.
#[test]
fn a_backends_own_env_wins_over_the_servers_baseline() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[server.env]
APP_ENV = "production"
DATABASE_URL = "postgres://shared/db"
[backends.app]
kind = "worker"
command = "php worker.php"
[backends.app.env]
DATABASE_URL = "postgres://its-own/db"
[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");

    let BackendConfig::Worker(worker) = &config.backends["app"].with_server_env(&config.server)
    else {
        panic!("the fixture's `app` is a worker")
    };
    assert_eq!(
        worker.env.get("APP_ENV").map(String::as_str),
        Some("production"),
        "the baseline reaches a backend that does not name the key"
    );
    assert_eq!(
        worker.env.get("DATABASE_URL").map(String::as_str),
        Some("postgres://its-own/db"),
        "and the backend's own value wins"
    );

    // `spawn_cgi` has the key too, and it is the kind that had no way to configure its
    // application at all before it.
    let BackendConfig::SpawnCgi(cgi) = &config.backends["cgi"].with_server_env(&config.server)
    else {
        panic!("the fixture's `cgi` is a spawn_cgi backend")
    };
    assert_eq!(
        cgi.env.get("APP_ENV").map(String::as_str),
        Some("production")
    );

    // The configuration itself is untouched: `config.describe` reports what the file says.
    let BackendConfig::Worker(as_written) = &config.backends["app"] else {
        unreachable!()
    };
    assert!(
        !as_written.env.contains_key("APP_ENV"),
        "the merge is a copy, not a rewrite of the parsed configuration"
    );
}

/// An `env` key a process cannot be started with is a configuration error, not a spawn failure.
///
/// `Command::env` accepts anything and fails at spawn with an `InvalidInput` that names no key,
/// which is a startup failure an operator cannot trace to a line in their file.
#[test]
fn an_unusable_env_entry_is_refused_by_name() {
    let base = "[server]\ndefault_backend = \"app\"\nhttp = \"127.0.0.1:8080\"\n\
                [backends.app]\nkind = \"worker\"\ncommand = \"php worker.php\"\n";
    // Each case names the table *and* declares whatever makes that table reachable, because a
    // table that never loads is a fixture that says nothing.
    for (table, extra) in [
        ("server.env", "[server.env]\n\"A=B\" = \"x\"\n".to_string()),
        (
            "backends.app.env",
            "[backends.app.env]\n\"A=B\" = \"x\"\n".to_string(),
        ),
        (
            "jobs.pool.env",
            "[jobs]\nconsume = [\"emails\"]\n\
             [jobs.pipelines.emails]\ndriver = \"memory\"\n\
             [jobs.pool]\ncommand = \"php consumer.php\"\n\
             [jobs.pool.env]\n\"A=B\" = \"x\"\n"
                .to_string(),
        ),
    ] {
        let config = Config::from_toml(&format!("{base}{extra}"))
            .expect("the file parses; the entry is the mistake");
        let err = config.validate().expect_err(table);
        assert!(err.to_string().contains(table), "{table}: {err}");
    }
}

#[test]
fn the_front_controller_defaults_to_index_php() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
"#;
    let config = Config::from_toml(text).expect("config parses");
    assert_eq!(
        config.server.resolve_front_controller(None).as_deref(),
        Some("index.php")
    );
}

#[test]
fn rejects_an_empty_config_file_paths_and_a_front_controller_above_the_root() {
    // An empty string in TOML is indistinguishable from an absent key, so
    // "only real .php files" must be said on the command line, not here.
    let text = r#"
[server]
http = "127.0.0.1:8080"
front_controller = ""
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("front_controller"), "{err}");

    let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = ""
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("document_root"), "{err}");

    for front in ["/etc/passwd", "../index.php", "a\\b.php"] {
        let text = format!("[server]\nhttp = \"127.0.0.1:8080\"\nfront_controller = {front:?}\n");
        let config = Config::from_toml(&text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(
            err.to_string().contains("below the document root"),
            "{front}: {err}"
        );
    }
}
