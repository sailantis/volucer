//! The metrics endpoint, the collected metrics it accepts, and their bounds.

use super::*;

/// The metrics table is off unless written, and its presence is what turns it on — there is no
/// `enabled` flag, for the reason `[server.tls]` has none either.
#[test]
fn metrics_is_absent_unless_the_table_is_written() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
"#;
    let config = Config::from_toml(text).expect("config parses");
    assert!(config.metrics.is_none());
    assert!(config.status.is_none());
}

#[test]
fn metrics_defaults_its_path_and_accepts_a_prefix() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[metrics]
address = "127.0.0.1:2112"
prefix = "myapp_"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("valid");
    let metrics = config.metrics.expect("present");
    assert_eq!(metrics.path, "/metrics");
    assert_eq!(metrics.prefix.as_deref(), Some("myapp_"));
}

#[test]
fn rejects_a_metrics_address_that_is_not_an_endpoint() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[metrics]
address = "2112"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("metrics.address"), "{err}");
}

#[test]
fn rejects_a_metrics_path_without_a_leading_slash() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[metrics]
address = "127.0.0.1:2112"
path = "metrics"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("must start with"), "{err}");
}

/// A prefix is prepended to a name, so it has to be a valid name start. A leading digit would
/// produce `9foo_rr_http_request_total`, which Prometheus refuses to parse — and it would refuse
/// the whole scrape, not just that line.
#[test]
fn rejects_a_metrics_prefix_that_is_not_a_name_start() {
    for bad in ["9app_", "app-dash", "app dot", "app:colon"] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n[metrics]\naddress = \"127.0.0.1:2112\"\n\
             prefix = \"{bad}\"\n"
        );
        let config = Config::from_toml(&text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("metrics.prefix"), "{bad}: {err}");
    }
}

/// An empty prefix is the same statement as an absent one, so the two spellings agree rather
/// than one of them being an error.
#[test]
fn an_empty_metrics_prefix_is_accepted() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[metrics]
address = "127.0.0.1:2112"
prefix = ""
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("valid");
}

/// A collected metric is a name, a kind and a help line, and all three reach the registry.
///
/// The parse is asserted as well as the validation, because the key is `type` in TOML and `kind`
/// in Rust — a rename that lost the `serde` attribute would make every deployment's block an
/// unknown field, which is an error at the one place an operator copies from.
#[test]
fn a_collected_metric_states_its_name_kind_and_help() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[metrics]
address = "127.0.0.1:2112"

[metrics.collect.app_orders_total]
type = "counter"
help = "Orders the application counted"

[metrics.collect.app_queue_depth]
type = "gauge"
help = "Jobs waiting"

[metrics.collect.app_latency_seconds]
type = "histogram"
help = "Time to answer"
buckets = [0.5, 0.1, 1.0]
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("valid");

    let metrics = config.metrics.expect("present");
    assert_eq!(metrics.collect.len(), 3);

    let counter = &metrics.collect["app_orders_total"];
    assert_eq!(counter.kind, CollectedKind::Counter);
    assert_eq!(counter.help, "Orders the application counted");
    assert!(
        counter.buckets.is_empty(),
        "an absent bucket list is empty rather than an error, because a counter has no buckets"
    );

    // Order in the file is not a declaration of order, so it is not preserved and must not be
    // refused either — the registry sorts its own bounds, and so does this.
    assert_eq!(
        metrics.collect["app_latency_seconds"].buckets,
        vec![0.5, 0.1, 1.0],
        "the bounds are kept as written; sorting is the registry's job, not the parser's"
    );
}

/// A metric name that Prometheus cannot parse is refused at startup.
///
/// The failure mode this prevents is silent in the worst way: the family still registers and
/// still renders, and a scraper that cannot parse one line drops the *whole* page — so one typo
/// in one name costs every metric, and the only symptom is an empty graph.
#[test]
fn rejects_a_collected_metric_name_prometheus_cannot_parse() {
    for bad in [
        "app-orders", // a hyphen
        "9orders",    // a leading digit
        "app.orders", // a dot, which means a label in Prometheus's own vocabulary
        "app orders", // a space
        "app:orders", // a colon, reserved for recording rules
    ] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n[metrics]\naddress = \"127.0.0.1:2112\"\n\
             [metrics.collect.\"{bad}\"]\ntype = \"counter\"\nhelp = \"x\"\n"
        );
        let config = Config::from_toml(&text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(
            err.to_string().contains("is not a valid metric name"),
            "{bad}: {err}"
        );
    }
}

/// The `rr_` namespace belongs to the server, so an application metric may not claim a name in
/// it.
///
/// Refused rather than silently prefixed: a prefix would make the name in `emitMetric` differ
/// from the name in the graph, which is a worse surprise at 3 a.m. than a refusal at startup.
/// The collision it prevents is real — `rr_http_request_total` is a name every operator's
/// dashboard already has, and an application that declared it would replace a server metric with
/// its own under the same label-free series.
#[test]
fn rejects_a_collected_metric_that_claims_the_servers_namespace() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[metrics]
address = "127.0.0.1:2112"

[metrics.collect.rr_http_request_total]
type = "counter"
help = "Mine now"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("rr_"), "{err}");
}

/// `buckets` on a kind that has no buckets is a setting read by nothing, so it is refused.
///
/// This is the `spawn_fpm` argument: a key that parses, validates and is ignored is worse than
/// one that does not exist, because an operator has no way to find out which it is.
#[test]
fn rejects_buckets_on_a_kind_that_has_none() {
    for kind in ["counter", "gauge"] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n[metrics]\naddress = \"127.0.0.1:2112\"\n\
             [metrics.collect.app_x]\ntype = \"{kind}\"\nhelp = \"x\"\nbuckets = [1.0]\n"
        );
        let config = Config::from_toml(&text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("buckets"), "{kind}: {err}");
    }
}

/// A bound has to be a number a `le` label can carry, which rules out what TOML will happily
/// parse: `inf` and `nan` are legal TOML floats and neither is a bucket bound.
#[test]
fn rejects_a_bucket_bound_that_is_not_a_finite_non_negative_number() {
    for bad in ["inf", "-1.0", "nan"] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n[metrics]\naddress = \"127.0.0.1:2112\"\n\
             [metrics.collect.app_x]\ntype = \"histogram\"\nhelp = \"x\"\nbuckets = [{bad}]\n"
        );
        let config = Config::from_toml(&text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("bucket bound"), "{bad}: {err}");
    }
}

/// The help text is required and may not be blank, because `# HELP` is what makes a name
/// readable later and an empty one is the same as none.
#[test]
fn rejects_a_collected_metric_with_an_empty_help() {
    for help in ["", "   "] {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:8080\"\n[metrics]\naddress = \"127.0.0.1:2112\"\n\
             [metrics.collect.app_x]\ntype = \"counter\"\nhelp = \"{help}\"\n"
        );
        let config = Config::from_toml(&text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("help"), "{help:?}: {err}");
    }
}

/// A kind that is not one of the three is refused at parse time, by `serde`, rather than
/// becoming a metric nothing can write.
#[test]
fn rejects_an_unknown_collected_kind() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[metrics]
address = "127.0.0.1:2112"

[metrics.collect.app_x]
type = "summary"
help = "x"
"#;
    let err = Config::from_toml(text).expect_err("must be refused");
    assert!(err.to_string().contains("summary"), "{err}");
}

/// **This test is why the workspace pins `toml = "1"`.** A newline and a trailing comma inside an
/// inline table are TOML **1.1**; 1.0 refused both, so this block was unwritable and a table of
/// application metrics had to be one `[metrics.collect.<name>]` header per metric, four levels
/// deep. It is asserted here rather than trusted to the version number because the difference is
/// invisible until someone writes the block: a pin back to `0.8` compiles, passes every other
/// test, and breaks the syntax in the one file an operator copies from.
///
/// The *values* are asserted too, not just that it parses. A `collect = { … }` that deserialized
/// to an empty table would satisfy the parser and configure nothing.
#[test]
fn an_inline_table_may_span_lines_and_end_with_a_comma() {
    let text = r#"
[server]
http = "127.0.0.1:8080"

[metrics]
address = "127.0.0.1:2112"
prefix = "myapp_"

collect = {
    app_orders_total = {
        type = "counter",
        help = "Orders the application counted",
    },
    app_queue_depth = { type = "gauge", help = "Jobs waiting" },
}
"#;
    let config = Config::from_toml(text).expect("TOML 1.1 inline tables parse");
    config.validate().expect("valid");

    let metrics = config.metrics.expect("present");
    assert_eq!(
        metrics.collect.len(),
        2,
        "the inline table has to reach the registry, not merely parse"
    );
    assert_eq!(
        metrics.collect["app_orders_total"].kind,
        CollectedKind::Counter
    );
    assert_eq!(
        metrics.collect["app_orders_total"].help,
        "Orders the application counted"
    );
    assert_eq!(
        metrics.collect["app_queue_depth"].kind,
        CollectedKind::Gauge,
        "a one-line entry and a multi-line entry are the same kind of entry"
    );
}

/// The price of the form above, pinned so it is a documented rule rather than a surprise: an inline
/// table is **closed in the file it is written in**. Nothing in the same *text* may add an entry to
/// it — not a dotted key, and not a header.
///
/// The refusal is the parser's, and it names the key: "cannot extend value of type inline table".
/// It has to be a test because the rule is about the **text**, which is not where one would look
/// for it: `-o metrics.collect.app_jobs.type=gauge` adds the same entry without complaint, because
/// an override is laid onto the already-parsed table and closure does not survive the parse. That
/// asymmetry is exactly what makes this easy to get wrong in a document, so both halves are pinned
/// — here that the text is refused, and in `an_override_reaches_a_closed_inline_table` that the
/// command line is not.
#[test]
fn an_inline_collect_table_is_closed_in_the_text() {
    // A dotted key reaching into a closed inline table.
    let dotted = r#"
[server]
http = "127.0.0.1:8080"

[metrics]
address = "127.0.0.1:2112"
collect = { app_orders_total = { type = "counter", help = "Orders" } }
collect.app_jobs.type = "counter"
"#;
    let err = Config::from_toml(dotted).expect_err("must be refused");
    assert!(err.to_string().contains("inline table"), "{err}");

    // And the same, spelled as a header.
    let header = r#"
[server]
http = "127.0.0.1:8080"

[metrics]
address = "127.0.0.1:2112"
collect = { app_orders_total = { type = "counter", help = "Orders" } }

[metrics.collect.app_jobs]
type = "counter"
help = "Jobs"
"#;
    let err = Config::from_toml(header).expect_err("must be refused");
    assert!(err.to_string().contains("inline table"), "{err}");
}

/// An `-o` pair *can* add an entry to a `collect` written as an inline table — and this is the
/// half of the rule that is easy to state wrongly.
///
/// An override is applied to the parsed `toml::Table`, before deserialization, and inline-table
/// closure is a property of the parser's view of the **text**. Once the document has become a
/// table there is no "inline" left to be closed, so a pair that inserts
/// `metrics.collect.app_jobs` simply lands beside the entries that were written. This test does
/// what `apply_override` does — insert a parsed literal at a path — and then deserializes, because
/// the claim only holds if the value both lands and is understood. `apply_override` itself lives in
/// the binary crate and is covered by `an_override_builds_tables_and_refuses_to_replace_a_value`.
#[test]
fn an_override_reaches_a_closed_inline_table() {
    let text = r#"
[server]
http = "127.0.0.1:8080"

[metrics]
address = "127.0.0.1:2112"
collect = { app_orders_total = { type = "counter", help = "Orders" } }
"#;
    let mut table: toml::Table = text.parse().expect("the file parses");

    let added: toml::Value = "v = { type = \"gauge\", help = \"Jobs waiting\" }"
        .parse::<toml::Table>()
        .expect("the override value is TOML")
        .remove("v")
        .expect("the fixture names it");
    table["metrics"]["collect"]
        .as_table_mut()
        .expect("collect is a table")
        .insert("app_jobs".to_string(), added);

    let config = Config::from_table(table).expect("the table deserializes");
    let metrics = config.metrics.expect("present");
    assert_eq!(metrics.collect.len(), 2, "the override added a metric");
    assert_eq!(
        metrics.collect["app_jobs"].kind,
        CollectedKind::Gauge,
        "and it is the entry the pair named"
    );
}
