//! The Prometheus endpoint, and the health and readiness endpoint.
//!
//! Two tables, two listeners, and neither inherits the public one.

use super::*;

/// The Prometheus endpoint.
///
/// Its own listener rather than a route on `[server]`, and that is not a style choice. The HTTP
/// listener may be TLS-only, may be behind a redirect to HTTPS, and faces the public; a scrape
/// endpoint that inherited any of those would be one misconfiguration away from being either
/// unreachable or public. A separate address on loopback is the shape every other server in this
/// space uses, and it is the one an operator can firewall as a single line.
///
/// `PartialEq` only, because [`CollectedMetric::buckets`] is `f64` and an `f64` cannot be `Eq`:
/// `NaN != NaN`, so the reflexivity `Eq` promises does not hold. A bucket bound may not be `NaN`
/// (validation refuses it) but the *type* cannot know that, and a derive that claimed otherwise
/// would be the one lie a future refactor could build on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Where to listen, e.g. `127.0.0.1:2112` or `unix:/run/volucer/metrics.sock`.
    ///
    /// Required. There is no default, because a default of `0.0.0.0:2112` would expose every
    /// counter in the process to whoever can reach the host, and a default of `127.0.0.1:2112`
    /// would silently collide with a Prometheus already listening there.
    pub address: String,

    /// A prefix prepended to every metric this server provides, e.g. `myapp_`.
    ///
    /// Applied at render time, so the metric names in the code stay the ones RoadRunner's
    /// documentation and other people's dashboards use. `prefix = "myapp_"` therefore yields
    /// `myapp_rr_http_request_total`, which is what RoadRunner's own `prefix` does.
    #[serde(default)]
    pub prefix: Option<String>,

    /// The path the exposition is served at. Defaults to `/metrics`.
    ///
    /// Configurable because a deployment may already reverse-proxy `/metrics` for something else,
    /// and a fixed path would make that a routing conflict rather than a naming choice. Anything
    /// other than this path answers 404, so the endpoint does not become a second way to ask
    /// whether the server is up.
    #[serde(default = "default_metrics_path")]
    pub path: String,

    /// Application metrics this deployment accepts from its workers, by name.
    ///
    /// **This table is what makes `emitMetric` reach the exposition at all.** A registry refuses a
    /// name nobody described — it has no `# TYPE` and no `# HELP` to write — so without an entry
    /// here a worker's sample is decoded, counted as refused, and dropped. That is deliberate: the
    /// set of metric names a process publishes is a decision about the deployment, and letting an
    /// application widen it at runtime is how a `/metrics` page becomes unbounded.
    ///
    /// A table keyed by name rather than a list of objects, for the same reason `[backends]` and
    /// `[kv]` are: the name *is* the identity, and a table makes two declarations of one name
    /// unexpressible rather than something `validate` has to detect.
    ///
    /// RoadRunner's key of the same name, and its field names too — an operator arriving from it
    /// writes the same block.
    #[serde(default)]
    pub collect: BTreeMap<String, CollectedMetric>,
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

/// One application metric a deployment accepts from its workers.
///
/// The three kinds are Prometheus's, and the choice is not cosmetic: it decides what a sample
/// *means*. A counter's sample is an increment, a gauge's is the value now, and a histogram's is one
/// observation placed into buckets — and the registry applies exactly that, so a name declared as a
/// gauge and written by a counter is a refusal with a reason rather than a graph that is subtly
/// wrong.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectedMetric {
    /// What a sample for this name means.
    ///
    /// Spelled `type` in TOML, which is RoadRunner's spelling and the word Prometheus itself uses in
    /// `# TYPE`. It is a `kind` in Rust only because `type` is a keyword here.
    #[serde(rename = "type")]
    pub kind: CollectedKind,

    /// The `# HELP` text. Required, for the reason [`MetricsConfig::collect`] gives: a registry
    /// whose names carried no explanation would render a page of bare names, and the text has to be
    /// the *operator's* rather than the worker's, because the operator is the one who declared it.
    pub help: String,

    /// Upper bounds for a histogram, in whatever unit the application reports.
    ///
    /// Sorted here rather than required to be sorted, because order carries no meaning — the
    /// registry sorts too, and refusing an unsorted list would reject a block an operator wrote
    /// correctly. Absent is legal and means a sum and a count with no buckets, which is what an
    /// application that only wants an average should ask for.
    ///
    /// Refused on a counter or a gauge: a bucket list on a kind that has no buckets is a declaration
    /// that will be ignored, and a setting read by nothing is worse than one that does not exist.
    #[serde(default)]
    pub buckets: Vec<f64>,
}

/// What a sample means, which is what decides how it is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectedKind {
    /// A sample is an increment, and must be a non-negative integer.
    Counter,
    /// A sample is the value now.
    Gauge,
    /// A sample is one observation, placed into the declared buckets.
    Histogram,
}

/// The health and readiness endpoint.
///
/// Two endpoints on one listener, because they answer genuinely different questions and the
/// difference is the one a container orchestrator acts on: `/health` says the process is alive and
/// should not be restarted, and `/ready` says it can serve traffic now and should be sent some.
/// A readiness probe that returned 200 while every worker was still booting is how a rolling deploy
/// sends traffic to a server that answers 502.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusConfig {
    /// Where to listen, e.g. `127.0.0.1:2114`.
    ///
    /// Required, for the same reason [`MetricsConfig::address`] is.
    pub address: String,

    /// What to answer when the server is not ready. Defaults to 503.
    ///
    /// Configurable because the convention is not universal: Kubernetes reads any non-2xx as "do
    /// not send traffic", while some load balancers are configured to expect 503 specifically and
    /// treat anything else as a failed probe of the probe itself.
    #[serde(default = "default_unavailable_status")]
    pub unavailable_status_code: u16,

    /// Serve the worker-control routes: `GET /workers` and `POST /workers/{index}/reset`.
    ///
    /// **Off, and off is not "not yet documented" — it is the whole safety of the feature.** A `GET`
    /// is safe to hand to whoever can reach a port; a `POST` that recycles a worker is not. So a
    /// deployment that configured `[status]` for an orchestrator, and then upgraded, gains no
    /// mutation surface: the routes do not exist until this says so, and until they say so they are
    /// a 404 (`/workers`) and a 405 (anything else `POST`ed), not a 403 — an endpoint that exists and
    /// refuses is an endpoint an attacker can enumerate.
    ///
    /// The alternative protection — a token — was considered and not built: the listener is
    /// configured by whoever runs the process, and the same one-line misconfiguration that exposes
    /// it to the world would expose the token's own configuration too. What protects this is that it
    /// does not exist unless asked for, which is `[metrics]`'s rule one table over.
    #[serde(default)]
    pub allow_worker_reset: bool,
}

fn default_unavailable_status() -> u16 {
    503
}
