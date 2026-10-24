//! The relay address, and the one backend that is uncapped by default.

use super::*;

/// A table is the same configuration a file is, and strictness survives the trip.
///
/// The three claims are the ones the command line depends on: typing comes from TOML (so
/// `workers = 8` is a number without this code knowing what a worker is), a table deserializes
/// to the same `Config` the parser produces, and an unknown key is still refused by name. The
/// last is what makes `-o` safe to offer: a typo on a command line is the failure mode with the
/// least visible evidence, and this is the assertion that it is caught.
#[test]
fn a_table_deserializes_like_the_file_it_came_from() {
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n\n[backends.app]\nkind = \"worker\"\ncommand = \"php worker.php\"\nworkers = 3\n";
    let from_text = Config::from_toml(text).expect("the text parses");
    let table: toml::Table = toml::from_str(text).expect("the text is a table");
    let from_table = Config::from_table(table).expect("the table deserializes");

    assert_eq!(
        from_table.server.http.as_deref(),
        from_text.server.http.as_deref()
    );
    assert_eq!(from_table.backends.len(), from_text.backends.len());
    match from_table.backends.get("app").expect("the backend") {
        BackendConfig::Worker(worker) => {
            assert_eq!(worker.workers, 3);
            assert_eq!(worker.command, "php worker.php");
        }
        other => panic!("`kind = \"worker\"` must build a worker: {other:?}"),
    }

    // The strictness that makes a `-o` pair safe to offer.
    let mut table: toml::Table =
        toml::from_str("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("the text is a table");
    table
        .entry("server".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .expect("a table")
        .insert("htpp".to_string(), toml::Value::String("x".into()));
    let err = Config::from_table(table).expect_err("an unknown key is refused");
    assert!(err.to_string().contains("htpp"), "{err}");
}

/// `max_connections` is off by default, and `0` is refused rather than read as "off".
///
/// The two halves are one test because the mistake they prevent is the same one: a number
/// that means something other than what it looks like. `None` is how "no cap" is spelled, so
/// a `0` would be a backend that can never serve anything — and the failure would surface on
/// the first request rather than at startup.
/// A relay named in the file wins over the one in the environment.
///
/// The precedence is the whole of the safety argument for the variable: a file that names an
/// address keeps meaning exactly that, so a stale variable in an operator's shell — or one
/// inherited from a parent unit — cannot silently move a deployment to a different socket.
#[test]
fn a_configured_relay_wins_over_the_environment() {
    let _serial = relay_env_lock();
    let previous = std::env::var(RELAY_SOCKET_ENV).ok();
    std::env::set_var(RELAY_SOCKET_ENV, "unix:/from/the/environment.sock");

    let attach = attach_with_relay(Some("unix:/from/the/file.sock"));
    assert_eq!("unix:/from/the/file.sock", attach.relay().unwrap());

    match previous {
        Some(value) => std::env::set_var(RELAY_SOCKET_ENV, value),
        None => std::env::remove_var(RELAY_SOCKET_ENV),
    }
}

/// An omitted relay is read from the environment, and that value is validated like any other.
#[test]
fn an_omitted_relay_comes_from_the_environment() {
    let _serial = relay_env_lock();
    let previous = std::env::var(RELAY_SOCKET_ENV).ok();
    std::env::set_var(RELAY_SOCKET_ENV, " unix:/from/the/environment.sock ");

    let attach = attach_with_relay(None);
    assert_eq!("unix:/from/the/environment.sock", attach.relay().unwrap());
    attach
        .validate("app")
        .expect("the environment's relay is valid");

    match previous {
        Some(value) => std::env::set_var(RELAY_SOCKET_ENV, value),
        None => std::env::remove_var(RELAY_SOCKET_ENV),
    }
}

/// Neither place set is a startup error that names both places.
///
/// The message is asserted rather than only the failure, because the reader of it is the
/// operator who has just discovered that "the supervisor is down" was never the question.
#[test]
fn a_relay_missing_from_both_places_is_refused_by_name() {
    let _serial = relay_env_lock();
    let previous = std::env::var(RELAY_SOCKET_ENV).ok();
    std::env::remove_var(RELAY_SOCKET_ENV);

    let err = attach_with_relay(None)
        .validate("app")
        .expect_err("nothing names a relay");
    assert!(err.contains("relay"), "{err}");
    assert!(err.contains(RELAY_SOCKET_ENV), "{err}");

    match previous {
        Some(value) => std::env::set_var(RELAY_SOCKET_ENV, value),
        None => std::env::remove_var(RELAY_SOCKET_ENV),
    }
}

/// An empty value means "unset", not "connect to nothing".
///
/// A unit file's `Environment=VOLUCER_RELAY_SOCKET=` produces exactly this, and so does a
/// shell's `export VOLUCER_RELAY_SOCKET=`. Reporting it as an empty *relay* would send the
/// operator looking for a typo in a file that does not mention the key.
#[test]
fn an_empty_environment_value_is_unset() {
    let _serial = relay_env_lock();
    let previous = std::env::var(RELAY_SOCKET_ENV).ok();
    std::env::set_var(RELAY_SOCKET_ENV, "   ");

    let attach = attach_with_relay(None);
    assert!(
        attach.relay().is_err(),
        "whitespace is not an address: {:?}",
        attach.relay()
    );

    match previous {
        Some(value) => std::env::set_var(RELAY_SOCKET_ENV, value),
        None => std::env::remove_var(RELAY_SOCKET_ENV),
    }
}

/// The environment is process-global, so the tests that write it cannot run concurrently.
///
/// Without this they pass alone and fail under `cargo test`'s default parallelism: one test
/// removes the variable while another expects its value, and the assertion that fires is the
/// one about precedence rather than about the environment. The same `OnceLock<Mutex<_>>` the
/// server's process-spawning tests use, for the same reason.
fn relay_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The two halves of the fallback must not be able to disagree about trimming.
///
/// One helper for both tests above rather than three literals, because a `relay` that trimmed
/// differently from the variable would pass every test that only ever used one of them.
fn attach_with_relay(relay: Option<&str>) -> AttachConfig {
    AttachConfig {
        relay: relay.map(str::to_string),
        workers: 1,
        max_requests: None,
        checksum: Default::default(),
        services: Vec::new(),
        grpc_services: Vec::new(),
    }
}

#[test]
fn an_external_backend_is_uncapped_by_default_and_refuses_a_zero() {
    let uncapped = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"

[backends.app]
kind = "external"
socket = "127.0.0.1:9000"
"#;
    let config = Config::from_toml(uncapped).expect("the fixture parses");
    config.validate().expect("no cap is valid");
    let external = config
        .backends
        .get("app")
        .and_then(|backend| match backend {
            BackendConfig::External(external) => Some(external),
            _ => None,
        })
        .expect("the backend is external");
    assert_eq!(
        None, external.max_connections,
        "the key being absent must mean no cap, not some default"
    );

    let zero = uncapped.replace(
        r#"socket = "127.0.0.1:9000""#,
        "socket = \"127.0.0.1:9000\"\nmax_connections = 0",
    );
    let err = Config::from_toml(&zero)
        .expect("the file parses; the value is what is wrong")
        .validate()
        .expect_err("a cap of zero must not validate");
    let message = err.to_string();
    assert!(
        message.contains("backends.app.max_connections"),
        "the message names the key: {message}"
    );
    assert!(
        message.contains("remove the key"),
        "and points at the spelling of \"no cap\": {message}"
    );
}
