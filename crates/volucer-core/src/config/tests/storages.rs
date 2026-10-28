//! KV storages, the lock table, and the defaults both are pinned to.

use super::*;

/// A `[lock]` table is what makes `"lock"` a name a backend may put in `services`.
///
/// The `[lock]` namespace is the third shape of the allow-list, and the one that is easiest to get
/// wrong: its *name* is fixed — a lock is named by its resource, so there is exactly one lock backend
/// — which makes it look like the fixed half ([`CALL_NAMESPACES`]), while what it actually needs is
/// the check the KV half gets. A backend granted `"lock"` with no `[lock]` table is a worker that
/// boots, serves traffic, and cannot reach a lock it was told it had. That is precisely the silent
/// failure the KV case exists to prevent, so it is asserted here in both directions.
#[test]
fn a_lock_service_needs_a_lock_table_and_a_lock_table_is_enough() {
    let with_table = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[lock]
driver = "memory"
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["lock"]
"#;
    Config::from_toml(with_table)
        .expect("the file parses")
        .validate()
        .expect("a [lock] table is a lock service that is honoured");

    let without = with_table.replace("[lock]\ndriver = \"memory\"\n", "");
    let err = Config::from_toml(&without)
        .expect("the file still parses")
        .validate()
        .expect_err("must not validate");
    let message = err.to_string();
    assert!(
        message.contains("\"lock\""),
        "the error must quote the service that is wrong: {message}"
    );
    assert!(
        message.contains("needs a [lock] table"),
        "and say what the deployment is missing rather than only that the name is unknown: {message}"
    );

    // And the other direction: with the table present, a refusal that lists what *is* available must
    // not claim there is no lock backend.
    let err = Config::from_toml(
        &with_table.replace("services = [\"lock\"]", "services = [\"lock\", \"workes\"]"),
    )
    .expect("the file parses")
    .validate()
    .expect_err("`workes` is a typo for a fixed namespace");
    assert!(
        err.to_string().contains("has a [lock] table"),
        "a refusal that lists what is available must be told about the lock backend: {err}"
    );
}

/// An unknown key in a `[lock]` table is refused, and so is a misspelled `driver`.
#[test]
fn the_lock_table_is_typed_like_the_kv_one_it_sits_beside() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[lock]
driver = "memory"
max_bytes = 1024
"#;
    let err = Config::from_toml(text).expect_err("max_bytes is not a lock key");
    let message = err.to_string();
    assert!(
        message.contains("max_bytes"),
        "the refusal must name the key, which is the whole reason this is a tagged enum rather than \
         `driver` plus a free-form table: {message}"
    );

    let misspelled = text.replace("max_bytes", "driover");
    let err = Config::from_toml(&misspelled).expect_err("driover is not a lock key");
    assert!(
        err.to_string().contains("driover"),
        "and it must name a misspelled one too: {err}"
    );
}

/// `[lock] driver = "memory"` with no further keys is the whole of the single-node setup.
#[test]
fn the_memory_lock_driver_needs_no_keys_at_all() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[lock]
driver = "memory"
"#;
    let config = Config::from_toml(text).expect("the file parses");
    config
        .validate()
        .expect("nothing to configure is not a mistake");
    let lock = config.lock.as_ref().expect("the table is there");
    assert_eq!(
        lock.driver(),
        "memory",
        "a describe that said something other than the tag would be a lie in the one answer an \
         operator uses to find out what is running"
    );
}

/// An **absent** `[lock]` table is not the memory driver — it is no lock plane at all.
#[test]
fn a_configuration_with_no_lock_table_grants_no_lock_service() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
"#;
    let config = Config::from_toml(text).expect("the file parses");
    assert!(
        config.lock.is_none(),
        "absent must stay absent: defaulting to the memory driver would make `services = [\"lock\"]` \
         work in a deployment that never configured a lock backend, which is the silent grant the \
         allow-list exists to prevent"
    );
    let namespace = super::Grant::Namespace {
        namespace: "lock".to_string(),
    };
    assert!(!config.is_a_call_service(&namespace));
    // The table's absence is a different thing from the table being there with no driver line, and
    // the second is refused by name rather than read as the default.
    let text = format!("{text}\n[lock]\n");
    let err = Config::from_toml(&text).expect_err("an empty [lock] table names no driver");
    assert!(
        err.to_string().contains("driver"),
        "and the refusal must name the missing key: {err}"
    );
}

/// The lock backend's Redis rules are the KV ones, because they are one function.
///
/// Asserted separately rather than trusted to the shared helper, because the two tables reach it from
/// different places — a loop over a `BTreeMap` and a single `Option` — and the interesting failure is
/// the second call site quietly not being wired up at all.
#[cfg(feature = "redis")]
#[test]
fn the_lock_backend_gets_the_same_redis_checks_as_a_kv_storage() {
    let with = |tail: &str| {
        let text = format!(
            r#"
[server]
http = "127.0.0.1:8080"
[lock]
driver = "redis"
{tail}
"#
        );
        Config::from_toml(&text)
            .expect("the file parses")
            .validate()
    };

    with("url = \"redis://127.0.0.1:6379\"").expect("a plain loopback URL is the ordinary case");

    let err = with("url = \"127.0.0.1:6379\"").expect_err("a bare host:port is not a URL");
    assert!(err.to_string().contains("lock.url"), "{err}");
    assert!(err.to_string().contains("redis://"), "{err}");

    let err = with("url = \"redis://:secret@redis.example.com:6379\"")
        .expect_err("a credential over plaintext must be refused");
    let message = err.to_string();
    assert!(
        !message.contains("secret"),
        "and the password must not be echoed: {message}"
    );

    let err = with("url = \"redis://127.0.0.1:6379\"\npool_size = 0")
        .expect_err("a zero pool could serve no call");
    assert!(err.to_string().contains("lock.pool_size"), "{err}");

    let err = with("url = \"redis://127.0.0.1:6379\"\nread_timeout_ms = 0")
        .expect_err("a zero deadline is a command that can never be resolved");
    let message = err.to_string();
    assert!(message.contains("lock.read_timeout_ms"), "{message}");
    assert!(
        message.contains("five-second"),
        "and it must quote the default back: {message}"
    );
}

/// The default read timeout is the one the documentation promises, and it is reachable by omitting it.
///
/// Pinned because the value is written twice — here and in the lock crate's own documentation — and
/// a default that drifted would be a deadline nobody could predict from the file.
#[cfg(feature = "redis")]
#[test]
fn an_omitted_lock_read_timeout_is_five_seconds() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[lock]
driver = "redis"
url = "redis://127.0.0.1:6379"
"#;
    let config = Config::from_toml(text).expect("the file parses");
    let lock = config.lock.as_ref().expect("the table is there");
    let LockConfig::Redis(redis) = lock else {
        panic!("the driver is redis");
    };
    assert_eq!(redis.read_timeout_ms, 5000);
    assert_eq!(
        redis.pool_size,
        default_redis_pool_size(),
        "and the pool default is the KV one, shared through the same function"
    );
}

/// A key inside a `[kv.<name>]` table that the driver does not have is refused.
///
/// The whole reason the table is a tagged enum rather than `driver` plus a free-form map: this is
/// the `adress` typo that RoadRunner's shape accepts and silently ignores, leaving a client pointed
/// at the default port with nothing in the log.
#[test]
fn an_unknown_key_in_a_kv_table_is_refused_rather_than_ignored() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[kv.cache]
driver = "memory"
adress = "redis://127.0.0.1:6379"
"#;
    let err = Config::from_toml(text).expect_err("must not be accepted");
    assert!(
        err.to_string().contains("adress"),
        "the error must name the key: {err}"
    );
}

/// A zero `max_bytes` is refused rather than taken as "unbounded".
///
/// An operator who writes zero means something, and it is not what this would do. Refusing is the
/// only way they find out before the OOM killer does — and accepting it as "no limit" would make the
/// one setting that bounds the cache the one setting that can disable it.
#[test]
fn a_zero_kv_bound_is_refused_with_the_default_quoted_back() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[kv.cache]
driver = "memory"
max_bytes = 0
"#;
    let config = Config::from_toml(text).expect("the file parses");
    let err = config.validate().expect_err("must not validate");
    let message = err.to_string();
    assert!(
        message.contains("kv.cache.max_bytes"),
        "the error must name the key: {message}"
    );
    assert!(
        message.contains("67108864"),
        "and quote the default it would have had: {message}"
    );
}

/// The `max_bytes` default is the one `volucer-kv` would have used, pinned as a value.
///
/// The number is written twice — here for a configuration that omits the key, in `volucer-kv` for a
/// store built by a test or by a caller that has no configuration — and the duplication is deliberate:
/// making this crate depend on a *driver* to name a default would invert the dependency the workspace
/// is built on, since everything depends on `volucer-core`. So the number is pinned here, where an edit
/// to it is visible in a diff that also has to change the driver, and the driver's own test pins its
/// copy. Two pins rather than one silent disagreement.
#[test]
fn the_kv_default_bound_is_pinned_where_both_crates_can_be_checked_against_it() {
    assert_eq!(
        default_kv_max_bytes(),
        64 * 1024 * 1024,
        "if this changes, volucer-kv's DEFAULT_MAX_BYTES must change with it, or a deployment \
         that omits the key gets a different bound from one that names it"
    );
}

/// The driver name matches the serde tag, so `config.describe` and the file cannot disagree.
#[test]
fn the_kv_driver_name_matches_its_tag() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[kv.cache]
driver = "memory"
"#;
    let config = Config::from_toml(text).expect("the file parses");
    let storage = config.kv.get("cache").expect("cache is configured");
    assert_eq!(
        storage.driver(),
        "memory",
        "a describe that said something other than the tag would be a lie in the one answer an \
         operator uses to find out what is running"
    );
}

/// `volucer.example.toml` parses and validates.
///
/// It is documentation, and a documentation file that does not parse is documentation that
/// lies: an operator copies a block out of it, gets a message about a key that does not exist,
/// and has no way to tell whether the file is wrong or they are. **Nothing else in the tree
/// opens this file** — no test, no CI job — so before this, every edit to it was unverified,
/// including the `[status]` tables added alongside it.
///
/// It is deliberately *not* checked for being complete: a table that the example documents but
/// nothing else uses would still parse, and asserting on the contents of a comments-heavy file
/// is a test that fails whenever someone improves the prose. What is asserted is the one
/// property a reader depends on — that the file is a valid configuration — plus that the two
/// new tables are in it at all, since a deleted block is the failure mode a parse check alone
/// would not notice.
/// `Config::load` expands `${...}` from the process environment, and `from_toml` does not.
///
/// The split is the property, not an implementation detail: `from_toml` has ~230 callers in this
/// crate's own tests, and a parser that read the process environment would make every one of them
/// depend on the machine it ran on. So this test writes the environment — and holds a lock while
/// it does, because the environment is process-global.
#[test]
fn load_expands_the_environment_and_from_toml_does_not() {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _serial = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let path = std::env::temp_dir().join(format!("volucer-expansion-{}.toml", std::process::id()));
    // A number, a listener and a string: the three positions the expansion has to reach, and
    // the two a post-parse approach could not.
    std::fs::write(
        &path,
        "default_backend = \"app\"\n\
         [server]\nhttp = \"127.0.0.1:${VOLUCER_TEST_PORT}\"\n\
         [backends.app]\nkind = \"worker\"\ncommand = \"php worker.php\"\n\
         workers = ${VOLUCER_TEST_WORKERS}\n\
         [server.access_log]\npath = \"${VOLUCER_TEST_LOG:-logs/access.log}\"\n",
    )
    .expect("write the fixture");

    std::env::set_var("VOLUCER_TEST_PORT", "12345");
    std::env::set_var("VOLUCER_TEST_WORKERS", "3");
    let loaded = Config::load(&path);
    let unexpanded = Config::from_toml(&std::fs::read_to_string(&path).expect("read"));
    std::env::remove_var("VOLUCER_TEST_PORT");
    std::env::remove_var("VOLUCER_TEST_WORKERS");
    let _ = std::fs::remove_file(&path);

    let config = loaded.expect("the file loads with its variables set");
    assert_eq!(config.server.http.as_deref(), Some("127.0.0.1:12345"));
    let BackendConfig::Worker(worker) = &config.backends["app"] else {
        panic!("the fixture's `app` is a worker")
    };
    assert_eq!(worker.workers, 3, "a number position expands too");
    assert_eq!(
        config
            .server
            .access_log
            .as_ref()
            .map(|log| log.path.clone()),
        Some(PathBuf::from("logs/access.log")),
        "an unset variable falls back to its default"
    );

    // And without the expansion the same bytes are not a configuration at all, which is why
    // `from_toml` cannot be the entry point a deployment uses.
    assert!(
        unexpanded.is_err(),
        "the raw file should not parse, or the split is untested"
    );
}

#[test]
fn the_example_configuration_parses_and_validates() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("volucer.example.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} is not readable: {err}", path.display()));

    let config = Config::from_toml(&text)
        .unwrap_or_else(|err| panic!("{} does not parse: {err}", path.display()));
    config
        .validate()
        .unwrap_or_else(|err| panic!("{} does not validate: {err}", path.display()));

    // The example documents these two as commented-out defaults, so the check is that the
    // documentation of them survives an edit rather than that they are enabled in it.
    for table in [
        "# [status]",
        "# [status.health]",
        "# [status.metrics]",
        "# [status.workers]",
        "# [telemetry]",
        "# [server.env]",
        "# include = [",
    ] {
        assert!(
            text.contains(table),
            "{} no longer documents {table}",
            path.display()
        );
    }
    // And the expansion, because a `${...}` in the example is the only place an operator sees
    // the syntax before writing one. It is documented as prose, so the assertion is on the form
    // rather than on a line that a later edit could reflow.
    for form in ["${NAME}", "${NAME:-default}", "$$"] {
        assert!(
            text.contains(form),
            "{} no longer documents the {form} expansion",
            path.display()
        );
    }
    // The `services` key is new with the call path and is the one an operator must find to
    // grant anything at all, so its absence from the example is a documentation failure.
    assert!(
        text.contains("# services = ["),
        "{} no longer documents the per-backend `services` key",
        path.display()
    );
    // Same argument for the KV table: it is the first service whose *name* an operator chooses,
    // and `services = ["kv.sessions"]` cannot be written at all without knowing that.
    assert!(
        text.contains("# [kv.sessions]"),
        "{} no longer documents a [kv.*] storage",
        path.display()
    );
    // And for the lock table, which is the one place the fencing warning has to be readable by
    // the person about to configure it — the warning asks for itself in the documentation, and a
    // deployment never reads the documentation before copying the example.
    assert!(
        text.contains("# [lock]"),
        "{} no longer documents the [lock] table",
        path.display()
    );
    assert!(
        text.contains("It is NOT a correctness\n# primitive") || text.contains("NOT a correctness"),
        "{} no longer carries the warning that a lock is not a correctness primitive; that \
         warning is the reason this feature is safe to ship at all",
        path.display()
    );
    // And for the queue, where two things have to be readable by the person configuring it: that a
    // pipeline is only polled when `consume` names it, and that the memory driver loses everything on
    // a restart. Both are sentences an operator must find *before* deploying, not after.
    assert!(
        text.contains("# [jobs]"),
        "{} no longer documents the [jobs] table",
        path.display()
    );
    assert!(
        text.contains("a restart loses") || text.contains("restart loses"),
        "{} no longer says that the memory queue loses everything on a restart",
        path.display()
    );
    // And the sentence that keeps a deployment from pointing `[jobs.pool] command` at its request
    // worker, which the protocol refuses by name but only *after* the consumer has started.
    assert!(
        text.contains("where a request worker resets per-request state"),
        "{} no longer distinguishes a consumer's process from a request worker's: both are \
         `php <file>`, and the difference is the one thing a reader has to know here",
        path.display()
    );
    // And the rate limit, where the two things a reader has to find *before* configuring it are
    // that `key = "header"` is not an authenticated identity and that the ip key follows
    // `[server.proxy]` rather than the socket peer. Both are mistakes that produce a limit that
    // reads as configured and limits the wrong thing.
    assert!(
        text.contains("# [server.rate_limit]"),
        "{} no longer documents the [server.rate_limit] table",
        path.display()
    );
    assert!(
        text.contains("an authenticated identity"),
        "{} no longer warns that a header key is not an authenticated identity: a client \
         chooses that value, so the warning is the whole safety of the mode",
        path.display()
    );
    assert!(
        text.contains("NOT the socket peer"),
        "{} no longer says that the ip key follows [server.proxy] rather than the socket peer, \
         which is the difference between limiting a client and limiting nginx",
        path.display()
    );
    // And the sentence a deployment needs *before* it decides to skip `[status.metrics]`: that skipping
    // it means nothing drains the worker-metric queue. It was a real leak, and the fix (a cap) is
    // a backstop rather than the intended reason to have the table.
    assert!(
        text.contains("NOTHING DRAINS THE QUEUE"),
        "{} no longer says that a deployment without [status.metrics] never drains the worker-metric \
         queue, which is the fact that made the queue a leak",
        path.display()
    );
    // And the sentence about rotation, because it is the one thing a deployment must do *after*
    // configuring the log: a server whose log is rotated without a reopen writes into an unlinked
    // inode for ever, silently.
    assert!(
        text.contains("SIGHUP CLOSES AND REOPENS THE FILE"),
        "{} no longer says that SIGHUP reopens the access log, which is what makes logrotate \
         work rather than silently detach the log",
        path.display()
    );
    // And the worker grant, whose *absent* default is the whole safety of the feature: a
    // deployment that configured `[status]` for an orchestrator must be able to read that it has
    // not also gained a way to recycle its workers.
    assert!(
        text.contains("# allow = [\"workers.list\"]"),
        "{} no longer documents the [status.workers] allow list",
        path.display()
    );
    assert!(
        text.contains("There is no authentication"),
        "{} no longer says that the worker routes are unauthenticated, which is the reason the \
         grant is absent by default",
        path.display()
    );
    // And the file driver, whose two facts a reader must find *before* choosing it: that a
    // relative path resolves against the config file, and that it does not share a directory
    // between two processes. Both are mistakes that read as configured and fail silently.
    assert!(
        text.contains("# driver = \"file\""),
        "{} no longer documents the file KV driver",
        path.display()
    );
    assert!(
        text.contains("beside the deployment"),
        "{} no longer says that a relative kv path resolves against this file's directory",
        path.display()
    );
    assert!(
        text.contains("share a directory between two"),
        "{} no longer says that two processes must not share a file store's directory, which \
         is the one deployment it silently misbehaves in",
        path.display()
    );
    // And `setIfAbsent`, because it is the answer to a sentence the deployment will read: the
    // lock table says a lock is not a correctness primitive, and the reader's next question is
    // what is. The ordering matters — a deployment that finds this after choosing a long lock
    // TTL has already chosen wrong.
    assert!(
        text.contains("answer to what a lock cannot do"),
        "{} no longer points an application at setIfAbsent from where it is told a lock is not \
         a correctness primitive",
        path.display()
    );
    assert!(
        text.contains("a success, not a failure"),
        "{} no longer says that a refused conditional write is a success, which is the one thing \
         a caller must not retry",
        path.display()
    );
}

/// The configuration the Docker smoke test writes is parsed and validated here, so that a change
/// to the configuration's shape is caught by `cargo test` rather than only by CI's `docker` job.
///
/// The fixture lives inside a shell heredoc, which nothing else reads: it is a configuration file
/// an operator never opens, and the only thing that has ever parsed it is a container starting.
/// That made the suite blind to it. Removing `[status] address` left it writing a key that is
/// refused by name, and the first thing to notice was a published-image check several minutes
/// into a workflow — on two jobs at once, with the reason buried in `docker logs`.
#[test]
fn the_docker_smoke_test_configuration_parses_and_validates() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("packaging")
        .join("docker")
        .join("smoke-test.sh");
    let script = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} is not readable: {err}", path.display()));

    // The heredoc is quoted (`<<'EOF'`), so the body is literal: no shell expansion has to be
    // undone before it is a TOML file.
    let body = script
        .split_once("<<'EOF'\n")
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| panic!("{} no longer writes a configuration file", path.display()));
    let text = body
        .split_once("\nEOF\n")
        .map(|(config, _)| config)
        .unwrap_or_else(|| panic!("{} no longer closes its heredoc with EOF", path.display()));

    let config = Config::from_toml(text).unwrap_or_else(|err| {
        panic!(
            "{} writes a configuration that does not parse: {err}",
            path.display()
        )
    });
    config.validate().unwrap_or_else(|err| {
        panic!(
            "{} writes a configuration that does not validate: {err}",
            path.display()
        )
    });

    // The probe reaches the listener through `[status]`, so a fixture without one is a container
    // whose `HEALTHCHECK` can only fail — which is the failure this test exists to move forward.
    assert!(
        config.status.is_some(),
        "{} no longer configures [status], so `volucer health` has no address to read",
        path.display()
    );
}

/// A `[kv.*]` file store parses, resolves its default bound, and refuses two things.
///
/// The refusals are the interesting half: an empty path would make the store's directory the
/// process's working directory — hashed cache files scattered beside the application's own — and
/// a zero bound is the same statement the memory driver refuses.
#[test]
fn a_file_store_is_parsed_and_its_two_unusable_shapes_are_refused() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[kv.sessions]
driver = "file"
path = "var/kv/sessions"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("valid");
    let storage = config.kv.get("sessions").expect("the storage is there");
    assert_eq!(storage.driver(), "file");
    match storage {
        KvStorageConfig::File(file) => {
            assert_eq!(
                file.max_bytes,
                64 * 1024 * 1024,
                "an omitted bound takes the memory driver's default, so one number is learned"
            );
            assert_eq!(file.path, Path::new("var/kv/sessions"));
        }
        other => panic!("expected a file store, got {}", other.driver()),
    }

    // An empty path.
    let text = r#"
[server]
http = "127.0.0.1:8080"
[kv.sessions]
driver = "file"
path = ""
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("an empty path is refused");
    assert!(err.to_string().contains("path is empty"), "{err}");

    // A zero bound.
    let text = r#"
[server]
http = "127.0.0.1:8080"
[kv.sessions]
driver = "file"
path = "var/kv"
max_bytes = 0
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("a zero bound is refused");
    assert!(err.to_string().contains("max_bytes is 0"), "{err}");
}

/// A relative `path` resolves against the config file's directory, exactly as the access log's
/// does — the rule that makes a deployment's file layout a property of the file rather than of
/// wherever the process was started.
#[test]
fn a_relative_kv_path_resolves_against_the_config_directory() {
    assert_eq!(
        resolve_kv_path(Path::new("var/kv"), Path::new("/etc/volucer")),
        Path::new("/etc/volucer/var/kv")
    );
    assert_eq!(
        resolve_kv_path(Path::new("/srv/kv"), Path::new("/etc/volucer")),
        Path::new("/srv/kv"),
        "an absolute path is left alone"
    );
}
