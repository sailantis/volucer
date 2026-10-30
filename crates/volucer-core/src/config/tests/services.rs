//! The `services` allow-list checked against the tables, and the queue tables.

use super::*;

/// `services` is absent by default, and the default is the safe one.
///
/// Asserted rather than assumed because it is the whole security argument: a backend that faces
/// the public must not become able to restart its siblings because somebody added a key
/// elsewhere. The default is `Vec::new`, and `#[serde(default)]` is the only thing making an
/// omitted key mean "no capability" rather than a parse error — so a change to either is a change
/// to the security posture, and this is what notices.
#[test]
fn a_worker_without_the_services_key_may_call_nothing() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("the file parses");
    config
        .validate()
        .expect("an omitted services list is valid");
    let BackendConfig::Worker(worker) = config.backends.get("app").expect("app is configured")
    else {
        panic!("app is a worker backend");
    };
    assert!(
        worker.services.is_empty(),
        "an omitted services key must grant nothing, got {:?}",
        worker.services
    );
}

/// A `services` entry naming a KV storage is checked against the `[kv.*]` tables, not a driver list.
///
/// This is the shape the second half of the allow-list takes, and the failure it prevents is
/// specific: `services = ["kv.sessions"]` with no `[kv.sessions]` table is a worker that boots,
/// serves traffic, and cannot reach a cache it was told it had. It is the same class of bug as the
/// misspelled namespace above and it has to be caught in the same place.
#[test]
fn a_services_entry_naming_a_kv_storage_must_match_a_configured_table() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[kv.sessions]
driver = "memory"
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["kv.sessions"]
"#;
    Config::from_toml(text)
        .expect("the file parses")
        .validate()
        .expect("a storage that exists is a service that is honoured");

    // The same file with the table removed. The value is what is wrong, so the error must be about
    // the *service*, and it must name the storages there are — otherwise an operator has to guess.
    let missing = text.replace("[kv.sessions]\ndriver = \"memory\"\n", "");
    let config = Config::from_toml(&missing).expect("the file still parses");
    let err = config.validate().expect_err("must not validate");
    let message = err.to_string();
    assert!(
        message.contains("\"kv.sessions\""),
        "the error must quote the service that is wrong: {message}"
    );
    assert!(
        message.contains("no [kv.*] table is configured at all"),
        "and say that there is no storage to name: {message}"
    );
}

/// A bare `"kv"` is refused rather than resolved to the only storage there is.
///
/// Tempting to allow, and wrong: a deployment that adds a second storage later would find the
/// first worker's calls quietly going somewhere else, which is the kind of wrongness that shows up
/// as a cache miss rate rather than as an error.
#[test]
fn a_bare_kv_service_is_refused_because_it_does_not_say_which_storage() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[kv.sessions]
driver = "memory"
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["kv"]
"#;
    let config = Config::from_toml(text).expect("the file parses");
    let err = config.validate().expect_err("must not validate");
    let message = err.to_string();
    assert!(
        message.contains("\"kv.sessions\""),
        "the error must show what the entry should have said: {message}"
    );
}

/// A `[jobs]` table is what makes `"jobs"` a name a backend may put in `services`.
///
/// The fourth shape of the allow-list and, like `lock`, the one that needs the KV-style check rather
/// than the fixed half: the *name* is fixed — a producer names the pipeline in the call, not the
/// service — while what it needs is the presence of a `[jobs]` table. A worker granted `"jobs"` with
/// no queues behind it is a worker that serves traffic and cannot push anything.
#[test]
fn a_jobs_service_needs_a_jobs_table_and_a_jobs_table_is_enough() {
    let with_table = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[jobs]
consume = ["emails"]
[jobs.pipelines.emails]
driver = "memory"
[jobs.pool]
command = "php consumer.php"
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["jobs"]
"#;
    Config::from_toml(with_table)
        .expect("the file parses")
        .validate()
        .expect("a [jobs] table is a jobs service that is honoured");

    let without = with_table.replace(
        "[jobs]\nconsume = [\"emails\"]\n[jobs.pipelines.emails]\ndriver = \"memory\"\n[jobs.pool]\ncommand = \"php consumer.php\"\n",
        "",
    );
    assert!(
        !without.contains("[jobs]"),
        "the fixture must have been changed"
    );
    let err = Config::from_toml(&without)
        .expect("the file still parses")
        .validate()
        .expect_err("must not validate");
    let message = err.to_string();
    assert!(message.contains("\"jobs\""), "{message}");
    assert!(
        message.contains("needs a [jobs] table"),
        "and say what the deployment is missing: {message}"
    );
}

/// A `consume` entry with no pipeline behind it is refused, and so is a duplicate.
///
/// The failure this prevents is specific and silent: a consumer that starts, finds no such pipeline,
/// and reports no error — so the deployment looks healthy while its mail is never sent. It is the same
/// class of mistake the `services` allow-list catches, one layer up.
#[test]
fn jobs_consume_is_checked_against_the_pipelines_that_exist() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[jobs]
consume = ["emails"]
[jobs.pipelines.emails]
driver = "memory"
[jobs.pool]
command = "php consumer.php"
"#;
    Config::from_toml(text)
        .expect("the file parses")
        .validate()
        .expect("a pipeline that exists is a pipeline that can be consumed");

    let typo = text.replace("consume = [\"emails\"]", "consume = [\"emials\"]");
    let err = Config::from_toml(&typo)
        .expect("the file parses")
        .validate()
        .expect_err("must not validate");
    let message = err.to_string();
    assert!(message.contains("\"emials\""), "{message}");
    assert!(
        message.contains("\"emails\""),
        "and name the pipelines there are, so the fix is a rename: {message}"
    );

    let twice = text.replace(
        "consume = [\"emails\"]",
        "consume = [\"emails\", \"emails\"]",
    );
    let err = Config::from_toml(&twice)
        .expect("the file parses")
        .validate()
        .expect_err("must not validate");
    assert!(err.to_string().contains("twice"), "{err}");
}

/// Consuming without a pool is refused, and so are the two values that would make the pool useless.
#[test]
fn a_consumer_without_a_command_or_with_an_impossible_pool_is_refused() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[jobs]
consume = ["emails"]
[jobs.pipelines.emails]
driver = "memory"
"#;
    // No `[jobs.pool]` at all, while consuming: there is nothing to default the command to, and a
    // consumer with no command is a server that consumes a queue with nobody to hand jobs to.
    let err = Config::from_toml(text)
        .expect("the file parses")
        .validate()
        .expect_err("a consumer needs a command");
    assert!(err.to_string().contains("[jobs.pool]"), "{err}");

    // A pool, but no consumers: allowed, because that is a producer-only deployment which happens to
    // share its configuration file with a consuming one.
    let producer = text.replace("consume = [\"emails\"]\n", "");
    Config::from_toml(&producer)
        .expect("the file parses")
        .validate()
        .expect("a producer-only deployment needs no pool at all");

    let with_pool = |extra: &str| {
        let text = format!("{text}\n[jobs.pool]\ncommand = \"php consumer.php\"\n{extra}\n");
        Config::from_toml(&text)
            .expect("the file parses")
            .validate()
    };

    with_pool("").expect("a command and nothing else is the whole configuration");

    let err = with_pool("job_lease_seconds = 0").expect_err("a zero lease is a job for everybody");
    let message = err.to_string();
    assert!(message.contains("job_lease_seconds"), "{message}");
    assert!(
        message.contains("every consumer"),
        "and say what it would do rather than only that it is wrong: {message}"
    );

    let err = with_pool("prefetch = 0").expect_err("asking for nothing reserves nothing");
    assert!(err.to_string().contains("prefetch"), "{err}");

    let err =
        Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n[jobs.pool]\ncommand = \"  \"\n")
            .expect("the file parses")
            .validate()
            .expect_err("a whitespace command is not a command");
    assert!(err.to_string().contains("command"), "{err}");
}

/// A `[jobs.pool]` refuses the keys that would mean something different here than on a backend.
///
/// `max_requests` is the interesting one: it is what RoadRunner calls `max_jobs` on a jobs pool, and
/// accepting it would let an operator set a *request* limit on a pool that serves no requests — so the
/// key is refused, and the refusal has to say what it is called here.
#[test]
fn a_jobs_pool_refuses_the_backend_keys_that_do_not_mean_what_they_say() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[jobs]
consume = ["emails"]
[jobs.pipelines.emails]
driver = "memory"
[jobs.pool]
command = "php consumer.php"
max_requests = 100
"#;
    let err = Config::from_toml(text).expect_err("max_requests is not a jobs pool key");
    let message = err.to_string();
    assert!(message.contains("max_requests"), "{message}");
    assert!(
        message.contains("max_jobs"),
        "and it must say what the key is called here, or the operator has to guess: {message}"
    );

    // `services` is refused too, and for a different reason: a consumer's channel carries jobs, not
    // calls, so there is no call surface for a services list to grant.
    let err = Config::from_toml(&text.replace("max_requests = 100", "services = [\"config\"]"))
        .expect_err("a consumer has no call surface to grant");
    assert!(err.to_string().contains("services"), "{err}");
}

/// The three keys a consumer pool inherits from a backend are the ones a *process* needs, and each is
/// there because omitting it would be a silent misconfiguration rather than a missing feature.
///
/// `transport` is the one worth the test. Its default is platform-dependent — a socket on Windows —
/// and a consumer pool that could not express it would be a pool that read its pipe the slow way on the
/// one platform where that was measured to cost 21×. `checksum` is the same trade the deployment
/// already accepted for its request workers, and `env` is how a consumer is pointed at a different
/// database than its web nodes.
#[test]
fn a_jobs_pool_carries_the_three_keys_a_process_needs() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[jobs]
consume = ["emails"]
[jobs.pipelines.emails]
driver = "memory"
[jobs.pool]
command = "php consumer.php"
checksum = "optional"
transport = "pipe"
env = { QUEUE_DATABASE = "jobs" }
working_dir = "."
"#;
    let config = Config::from_toml(text).expect("the file parses");
    config.validate().expect("every key here is valid");
    let pool = config.jobs.as_ref().and_then(|jobs| jobs.pool.as_ref());
    let pool = pool.expect("the pool table was read");
    assert!(
        matches!(pool.checksum, ChecksumMode::Optional),
        "the checksum policy is inherited rather than re-decided: {:?}",
        pool.checksum
    );
    assert!(
        matches!(pool.transport, WorkerTransport::Pipe),
        "and so is the transport, which is the only way a deployment can pick one for its consumers"
    );
    assert_eq!(
        pool.env.get("QUEUE_DATABASE").map(String::as_str),
        Some("jobs"),
        "a consumer pointed at a different database than its web nodes is the ordinary case"
    );
}

/// The driver name matches the serde tag, so `config.describe` and the file cannot disagree.
#[test]
fn a_pipeline_driver_name_matches_its_tag() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[jobs.pipelines.emails]
driver = "memory"
"#;
    let config = Config::from_toml(text).expect("the file parses");
    let pipeline = config
        .jobs
        .as_ref()
        .and_then(|jobs| jobs.pipelines.get("emails"))
        .expect("emails is configured");
    assert_eq!(pipeline.driver(), "memory");
}

/// A Redis pipeline is checked for the same things a Redis lock and a Redis KV storage are, and the
/// `read_timeout_ms = 0` case is the one that is this driver's own.
///
/// A zero here would make every reserve time out the instant it was sent — a consumer that reports a
/// backend failure on every poll and never consumes anything, with a configuration that validates.
/// The lock backend refuses the same value for the same reason.
#[cfg(feature = "redis")]
#[test]
fn a_redis_pipeline_is_checked_for_shape_and_for_a_zero_command_deadline() {
    let with = |extra: &str| {
        let text = format!(
            r#"
[server]
http = "127.0.0.1:8080"
[jobs.pipelines.emails]
driver = "redis"
url = "redis://127.0.0.1:6379"
{extra}
"#
        );
        Config::from_toml(&text)
            .expect("the file parses")
            .validate()
    };

    with("").expect("a URL and nothing else is the whole configuration");
    with("pool_size = 4\nread_timeout_ms = 250")
        .expect("and both optional keys are read rather than ignored");

    let err = with("read_timeout_ms = 0").expect_err("a zero deadline is not a deadline");
    assert!(err.to_string().contains("read_timeout_ms"), "{err}");

    // A misspelled driver is refused by name rather than read as the default. Written as its own
    // document because TOML forbids a duplicate key, and `with` already writes `driver`.
    let err = Config::from_toml(
        "[server]\nhttp = \"127.0.0.1:8080\"\n[jobs.pipelines.emails]\ndriver = \"redys\"\n",
    )
    .expect_err("a misspelled driver is refused by name, not read as the default");
    let message = err.to_string();
    assert!(message.contains("unknown variant `redys`"), "{message}");
    assert!(
        message.contains("`memory`"),
        "and list what this build does have: {message}"
    );

    // A credential over a plaintext connection is refused here for the same reason it is on every
    // other Redis table: a password on the wire in the clear is the one mistake where a divergent
    // check between two tables would matter. Its own document again, because `with` writes `url`.
    let err = Config::from_toml(
        "[server]\nhttp = \"127.0.0.1:8080\"\n[jobs.pipelines.emails]\ndriver = \"redis\"\n\
         url = \"redis://user:secret@127.0.0.1:6379\"\n",
    )
    .expect("the file parses")
    .validate()
    .expect_err("a password over `redis://` is refused");
    let message = err.to_string();
    assert!(message.contains("rediss"), "{message}");
}

/// An AMQP pipeline is checked for its URL's shape, for a credential on a plaintext connection, for a zero
/// command deadline and for an empty queue name — the four keys that would each produce a pipeline that
/// starts and then does nothing useful.
///
/// The empty-queue case is the one worth having: RabbitMQ would *accept* `queue = ""` and hand back a
/// server-named queue, so the symptom is a queue that exists, receives nothing, and reports no error.
#[cfg(feature = "amqp")]
#[test]
fn an_amqp_pipeline_is_checked_for_shape_and_for_an_empty_queue_name() {
    let with = |url: &str, extra: &str| {
        let text = format!(
            r#"
[server]
http = "127.0.0.1:8080"
[jobs.pipelines.emails]
driver = "amqp"
url = "{url}"
{extra}
"#
        );
        Config::from_toml(&text).expect("the file parses").validate()
    };

    with("amqp://guest:guest@127.0.0.1:5672", "")
        .expect_err("a credential over plaintext is refused, as it is on every other remote table");
    with("amqp://127.0.0.1:5672", "")
        .expect("a plaintext URL with no credential is the ordinary local case");
    with("amqps://guest:guest@broker.example.com:5671", "")
        .expect("and a credential over TLS is fine");
    with("amqp://127.0.0.1:5672", "read_timeout_ms = 250")
        .expect("the deadline is read rather than ignored");

    let err = with("amqps://broker:5671", "read_timeout_ms = 0")
        .expect_err("a zero deadline is not a deadline");
    assert!(err.to_string().contains("read_timeout_ms"), "{err}");

    let err = with("http://127.0.0.1:5672", "").expect_err("that is not an AMQP URL");
    assert!(err.to_string().contains("amqp://"), "{err}");

    let err = with("amqp://127.0.0.1:5672", "queue = \"  \"")
        .expect_err("an empty queue name is a server-named queue nothing can route to");
    let message = err.to_string();
    assert!(message.contains("queue"), "{message}");
    assert!(
        message.contains("server-named"),
        "the refusal must say what would happen rather than only that it is empty: {message}"
    );
}

/// A misspelled AMQP driver is refused by name, and a build without the feature refuses the driver rather
/// than accepting a table nothing can build.
///
/// The two halves in one test because they are one question — what does a file naming a feature-gated
/// driver do — and the answer must differ by the *build* rather than by the file. `#[cfg]` decides which
/// assertion runs, not which file is read, so a file that only works in one build cannot be written by
/// accident. This is the same shape `a_feature_gated_driver_is_refused_by_name_when_the_build_has_no_such_driver`
/// uses for `redis`.
#[test]
fn an_amqp_pipeline_is_refused_by_name_when_the_build_has_no_amqp_driver() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[jobs.pipelines.emails]
driver = "amqp"
url = "amqp://127.0.0.1:5672"
"#;
    let parsed = Config::from_toml(text);
    #[cfg(feature = "amqp")]
    {
        let config = parsed.expect("the amqp driver is compiled in, so this must parse");
        config
            .validate()
            .expect("a local URL with no credential is a valid configuration");
        let pipeline = config
            .jobs
            .as_ref()
            .and_then(|jobs| jobs.pipelines.get("emails"))
            .expect("emails is configured");
        assert_eq!(pipeline.driver(), "amqp");
    }
    #[cfg(not(feature = "amqp"))]
    {
        let err = parsed.expect_err("the amqp driver is not compiled in, so this must not parse");
        let message = err.to_string();
        assert!(
            message.contains("unknown variant `amqp`"),
            "the refusal must name the variant, so the operator knows the driver is unknown rather than \
             misspelled: {message}"
        );
        assert!(
            message.contains("`memory`"),
            "and list what this build does have: {message}"
        );
    }
}

/// A queue argument's TOML type reaches the broker's type system, so `x-queue-type = "quorum"` really is a
/// string and `x-max-priority = 10` really is a number.
///
/// Asserted because a field table is untyped at the wire level and a wrong tag is not an error at parse
/// time: an integer sent as a string is a broker closing the channel with an argument-type complaint. This
/// is the check that the mapping in `field_value` is not a silent coercion.
#[cfg(feature = "amqp")]
#[test]
fn queue_argument_types_are_kept_rather_than_stringified() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[jobs.pipelines.emails]
driver = "amqp"
url = "amqp://127.0.0.1:5672"
arguments = { "x-queue-type" = "quorum", "x-max-priority" = 10, "x-single-active-consumer" = true, "x-message-ttl" = 60000 }
"#;
    let config = Config::from_toml(text).expect("the file parses");
    config.validate().expect("these are all valid arguments");
    let pipeline = config
        .jobs
        .as_ref()
        .and_then(|jobs| jobs.pipelines.get("emails"))
        .expect("emails is configured");
    let PipelineConfig::Amqp(amqp) = pipeline else {
        panic!("emails is an amqp pipeline");
    };

    assert!(
        matches!(
            amqp.arguments.get("x-queue-type"),
            Some(AmqpArgument::Text(text)) if text == "quorum"
        ),
        "a string argument stays a string, which is what the broker needs for a queue type"
    );
    assert!(
        matches!(
            amqp.arguments.get("x-max-priority"),
            Some(AmqpArgument::Integer(10))
        ),
        "and a number stays a number: sending 10 as text is a channel the broker closes"
    );
    assert!(matches!(
        amqp.arguments.get("x-single-active-consumer"),
        Some(AmqpArgument::Flag(true))
    ));
    assert!(matches!(
        amqp.arguments.get("x-message-ttl"),
        Some(AmqpArgument::Integer(60000))
    ));
}

/// A `[jobs]` table with no `[jobs.pool]` and a non-empty `consume` is refused on the AMQP driver too —
/// the rule is the pipeline's, not the driver's.
#[cfg(feature = "amqp")]
#[test]
fn an_amqp_pipeline_still_needs_a_consumer_pool_to_be_consumed() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[jobs]
consume = ["emails"]
[jobs.pipelines.emails]
driver = "amqp"
url = "amqp://127.0.0.1:5672"
"#;
    let err = Config::from_toml(text)
        .expect("the file parses")
        .validate()
        .expect_err("a consumer without a command cannot start a consumer");
    assert!(err.to_string().contains("pool"), "{err}");
}

/// A `[kv.*]` table whose driver is misspelled is refused, naming the key it did not understand.
#[test]
fn an_unknown_kv_driver_is_refused_rather_than_ignored() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[kv.cache]
driver = "redys"
url = "redis://127.0.0.1:6379"
"#;
    let err = Config::from_toml(text).expect_err("must not be accepted");
    let message = err.to_string();
    assert!(
        message.contains("unknown variant `redys`"),
        "the error must name the variant: {message}"
    );
    assert!(
        message.contains("`memory`"),
        "and say what there is: {message}"
    );
}

/// `driver = "redis"` is refused when the build has no Redis, and accepted when it has.
///
/// The two halves in one test because they are one question — what does a file naming a feature-gated
/// driver do — and the answer must differ by the *build* rather than by the file. A configuration that
/// named `redis` and silently got no store would be worse than either answer: `volucer check` would
/// pass and the deployment would refuse every `kv.*` call at run time, with nothing saying why.
///
/// Note that `#[cfg]` decides which assertion runs, not which file is read, so a file that only works
/// in one build is impossible to write by accident.
#[test]
fn a_feature_gated_driver_is_refused_by_name_when_the_build_has_no_such_driver() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[kv.cache]
driver = "redis"
url = "redis://127.0.0.1:6379"
"#;
    let parsed = Config::from_toml(text);
    #[cfg(feature = "redis")]
    {
        let config = parsed.expect("the redis driver is compiled in, so this must parse");
        config
            .validate()
            .expect("a plaintext redis URL with no credential is a valid configuration");
        let storage = config.kv.get("cache").expect("cache is configured");
        assert_eq!(storage.driver(), "redis");
    }
    #[cfg(not(feature = "redis"))]
    {
        let err = parsed.expect_err("the redis driver is not compiled in, so this must not parse");
        let message = err.to_string();
        assert!(
            message.contains("unknown variant `redis`"),
            "the refusal must name the variant, so the operator knows the driver is unknown rather \
             than misspelled: {message}"
        );
        assert!(
            message.contains("`memory`"),
            "and list what this build does have: {message}"
        );
    }
}

/// A Redis URL that is not a Redis URL is refused, and one carrying a credential over plaintext too.
///
/// The second is the one worth having: `redis://user:password@host` puts a password on the wire in the
/// clear, and an operator who wrote it has almost certainly not noticed. It is an error rather than a
/// warning because a *configuration* is the one place where nobody is watching a log at the moment the
/// decision is made — and `rediss://` is the fix, which the message says.
#[cfg(feature = "redis")]
#[test]
fn a_redis_url_is_checked_for_shape_and_for_a_credential_on_a_plaintext_connection() {
    let with = |url: &str| {
        let text = format!(
            r#"
[server]
http = "127.0.0.1:8080"
[kv.cache]
driver = "redis"
url = "{url}"
"#
        );
        Config::from_toml(&text)
            .expect("the file parses")
            .validate()
    };

    with("redis://127.0.0.1:6379").expect("a plain loopback URL is the ordinary case");
    with("rediss://user:secret@redis.example.com:6380").expect("a credential over TLS is fine");

    let err = with("127.0.0.1:6379").expect_err("a bare host:port is not a URL this accepts");
    assert!(err.to_string().contains("redis://"), "{err}");

    let err = with("redis://user:secret@redis.example.com:6379")
        .expect_err("a credential over plaintext must be refused");
    let message = err.to_string();
    assert!(
        !message.contains("secret"),
        "and the message must not repeat the password: {message}"
    );
    assert!(
        message.contains("rediss://"),
        "it must say what to do instead: {message}"
    );
}

/// A zero connection pool is refused, because nothing could ever be served from it.
#[cfg(feature = "redis")]
#[test]
fn a_zero_redis_pool_size_is_refused_with_the_default_quoted_back() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[kv.cache]
driver = "redis"
url = "redis://127.0.0.1:6379"
pool_size = 0
"#;
    let err = Config::from_toml(text)
        .expect("the file parses")
        .validate()
        .expect_err("must not validate");
    let message = err.to_string();
    assert!(message.contains("kv.cache.pool_size"), "{message}");
    assert!(message.contains("16"), "and quote the default: {message}");
}

/// A `services` entry may name a **method**, and the verbs are checked against the catalogue.
///
/// The narrow grant is the point of the catalogue: a backend that only needs to read a cache should
/// not be handed `delete` and `clear` along with `get`. The catalogue is also where a verb no
/// namespace serves is caught — `lock.steal` reads like a method, was documented as one, and is
/// implemented nowhere, so a message that lists the real verbs is the difference between a typo and
/// a missing feature.
#[test]
fn a_services_entry_may_name_one_method_and_is_checked_against_the_catalogue() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[kv.cache]
driver = "memory"
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["kv.cache.get", "kv.cache.has", "workers.list"]
"#;
    Config::from_toml(text)
        .expect("the file parses")
        .validate()
        .expect("verb grants on a table that exists are honoured");

    // A verb no storage serves, which is the `lock.steal` class of mistake.
    let bad_verb = text.replace("\"kv.cache.get\"", "\"kv.cache.frobnicate\"");
    let err = Config::from_toml(&bad_verb)
        .expect("the file still parses")
        .validate()
        .expect_err("a verb no namespace serves must be refused");
    let message = err.to_string();
    assert!(message.contains("frobnicate"), "{message}");
    assert!(
        message.contains("\"get\""),
        "and list the verbs that do exist, because a typo and a missing feature read the \
         same otherwise: {message}"
    );

    // A verb a namespace does not have, on the fixed side. `lock.steal` is the name that was in the
    // documentation while nothing implemented it.
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[lock]
driver = "memory"
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["lock.steal"]
"#;
    let err = Config::from_toml(text)
        .expect("the file parses")
        .validate()
        .expect_err("lock.steal is not a method");
    let message = err.to_string();
    assert!(message.contains("steal"), "{message}");
    assert!(
        message.contains("release_all"),
        "and name a verb that does exist: {message}"
    );
}

/// **A `workers.reset` grant on a backend with no pool is refused, not documented.**
///
/// Only a `worker` or an `attach` backend owns a `WorkerPool`, so a FastCGI application granted a
/// reset was a grant nothing could honour — it passed `check`, the application dialled the call
/// plane, and the call could only ever fail. The refusal names the kinds that can take it, because
/// the fix is either to drop the grant or to change the backend's kind.
#[test]
fn a_workers_reset_grant_is_refused_on_a_backend_with_no_pool() {
    for kind in ["external", "spawn_cgi", "upstream"] {
        let text = format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[rpc]
listen = "127.0.0.1:22080"
[backends.app]
kind = "{kind}"
services = ["workers.reset"]
"#
        );
        // Each kind needs its own required key; the grant is what is under test, so the file is
        // completed only enough for parsing to reach `validate`.
        let text = match kind {
            "external" => format!("{text}socket = \"127.0.0.1:9000\"\n"),
            "spawn_cgi" => format!("{text}command = \"php-cgi\"\n"),
            _ => format!("{text}origin = \"http://127.0.0.1:3000\"\n"),
        };
        let config =
            Config::from_toml(&text).unwrap_or_else(|err| panic!("the {kind} file parses: {err}"));
        let message = match config.validate() {
            Ok(()) => panic!("a {kind} backend must refuse workers.reset"),
            Err(err) => err.to_string(),
        };
        assert!(
            message.contains("no worker pool"),
            "the {kind} refusal must say why: {message}"
        );
        assert!(
            message.contains("workers.list"),
            "and name the grant that does work there: {message}"
        );
    }

    // And the same grant on a kind that does own a pool is accepted, so the rule follows the
    // capability rather than the table. No `[rpc]` here: a worker reaches its registry over its own
    // channel, and `[rpc]` refuses a file where no backend dials it — a rule that is about the
    // listener rather than about the grant.
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["workers.reset"]
"#;
    Config::from_toml(text)
        .expect("the file parses")
        .validate()
        .expect("a worker backend owns a pool, so a reset is grantable");
}

/// A grant is spelled in one of four ways, and anything else is refused as a spelling rather than
/// as a namespace that does not exist.
///
/// The distinction matters for the message: `"kv..get"` is a typo, while `"workes"` is a name for
/// something this server does not have, and an operator should be told which mistake they made.
#[test]
fn a_grant_is_parsed_into_one_of_four_shapes_or_refused() {
    use super::super::{is_a_grant_spelling, Grant};

    assert!(is_a_grant_spelling("workers"));
    assert!(is_a_grant_spelling("workers.list"));
    assert!(is_a_grant_spelling("kv.sessions"));
    assert!(is_a_grant_spelling("kv.sessions.get"));
    assert!(is_a_grant_spelling("kv.get"));
    assert!(!is_a_grant_spelling(""));
    assert!(!is_a_grant_spelling("kv..get"));
    assert!(!is_a_grant_spelling("kv.sessions.get.extra"));

    // `kv.get` is a verb across storages; `kv.sessions` is a storage. The fixed verb set is what
    // tells them apart, and the alternative — reading `kv.get` as a storage called `get` — is a
    // grant nobody writes on purpose.
    assert_eq!(
        Grant::parse("kv.get"),
        Some(Grant::Method {
            namespace: "kv".to_string(),
            verb: "get".to_string()
        })
    );
    assert_eq!(
        Grant::parse("kv.sessions"),
        Some(Grant::Storage {
            storage: "sessions".to_string()
        })
    );

    // And a narrow grant covers its call and not its neighbours, which is the whole reason a grant
    // is parsed rather than compared as a string.
    let get = Grant::parse("kv.cache.get").expect("parses");
    assert!(get.covers("kv.cache.get"));
    assert!(!get.covers("kv.cache.set"), "read must not imply write");
    assert!(
        !get.covers("kv.other.get"),
        "and must not reach another storage"
    );

    // The verb-across-storages shape covers `kv.<storage>.<verb>` and nothing longer, so a name that
    // is not a call at all cannot be reached through it.
    let any_get = Grant::parse("kv.get").expect("parses");
    assert!(any_get.covers("kv.cache.get"));
    assert!(any_get.covers("kv.sessions.get"));
    assert!(
        !any_get.covers("kv.cache.get.extra"),
        "a four-part name is not a kv call, so nothing may cover it"
    );
    assert!(!any_get.covers("kv.cache.set"));

    // And a namespace-wide name covers everything in its namespace, which is what keeps
    // `services = ["workers"]` meaning what it did before grants could name one method.
    let workers = Grant::parse("workers").expect("parses");
    assert!(workers.covers("workers.list"));
    assert!(workers.covers("workers.reset"));
}

