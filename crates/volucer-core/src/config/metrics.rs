//! The status listener: where it listens, and the three sets of routes it may carry.
//!
//! **One table, one listener.** The readiness probe, the Prometheus exposition and the
//! worker-control routes are paths on one address, and each of them exists because its sub-table
//! does. That is the whole model, and it replaced a `[metrics]` table and a `[status]` table that
//! each named an address for the same socket — two settings for one fact, which the server could
//! only reconcile by refusing a file where they disagreed.
//!
//! The listener is its own rather than a route on `[server]`, and that is not a style choice. The
//! HTTP listener may be TLS-only, may be behind a redirect to HTTPS, and faces the public; a scrape
//! endpoint that inherited any of those would be one misconfiguration away from being either
//! unreachable or public. A separate address on loopback is the shape every other server in this
//! space uses, and it is the one an operator can firewall as a single line.

use super::*;

/// The Prometheus exposition, served on the [`StatusConfig`] listener.
///
/// `PartialEq` only, because [`CollectedMetric::buckets`] is `f64` and an `f64` cannot be `Eq`:
/// `NaN != NaN`, so the reflexivity `Eq` promises does not hold. A bucket bound may not be `NaN`
/// (validation refuses it) but the *type* cannot know that, and a derive that claimed otherwise
/// would be the one lie a future refactor could build on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusMetrics {
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

    /// The `# HELP` text. Required, for the reason [`StatusMetrics::collect`] gives: a registry
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

/// The status listener, and the routes it carries.
///
/// **`listen` is required; everything else is a sub-table whose presence turns an endpoint on.**
/// That convention is `[server.tls]`'s and `[server.access_log]`'s, and it is what removes the
/// `enabled`-flag-versus-address disagreement: there is one address, and a route exists because the
/// block that describes it is written.
///
/// `[status]` alone is the ordinary deployment — a liveness and readiness probe for an orchestrator,
/// which is why [`StatusHealth`] is not a sub-table an operator has to remember to write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusConfig {
    /// Where to listen, e.g. `127.0.0.1:2114`.
    ///
    /// Required. There is no default, because a default of `0.0.0.0:2114` would expose the
    /// worker-control routes to whoever can reach the host, and a default of `127.0.0.1:2114` would
    /// silently collide with something already listening there.
    ///
    /// TCP only. `parse_listener` accepts `unix:` and this listener does not bind one, which
    /// validation refuses rather than leaving as a bind failure after the pools are up.
    ///
    /// `#[serde(default)]` and checked in `validate` rather than as a non-`Option` field, so that a
    /// file still carrying the pre-0.4 `address` key gets the refusal that names the new spelling
    /// instead of `toml`'s `missing field 'listen'` — which is true and explains nothing.
    #[serde(default)]
    pub listen: String,

    /// `/health` and `/ready`. The defaults are the whole of what most deployments need.
    #[serde(default = "default_health")]
    pub health: StatusHealth,

    /// The Prometheus exposition at `path`. Absent means this listener serves no metrics.
    ///
    /// Absent is a deployment somebody chose: a process may want a readiness probe without
    /// publishing every counter in it, and a second port is a second thing to firewall.
    #[serde(default)]
    pub metrics: Option<StatusMetrics>,

    /// `GET /workers` and `POST /workers/{index}/reset`. Absent means neither route exists.
    ///
    /// **Absent, and absent is not "not yet documented" — it is the whole safety of the feature.**
    /// A `GET` is safe to hand to whoever can reach a port; a `POST` that recycles a worker is not.
    /// A deployment that configured `[status]` for an orchestrator and then upgraded gains no
    /// mutation surface: while the routes do not exist `/workers` is a 404 and a `POST` anywhere is
    /// a 405, not a 403 — an endpoint that exists and refuses is an endpoint an attacker can
    /// enumerate.
    ///
    /// The alternative protection — a token — was considered and not built: the listener is
    /// configured by whoever runs the process, and the same one-line misconfiguration that exposes
    /// it to the world would expose the token's own configuration too.
    #[serde(default)]
    pub workers: Option<StatusWorkers>,

    /// The pre-0.4 spelling of [`StatusConfig::listen`].
    #[serde(default, skip_serializing)]
    pub address: Option<String>,

    /// The pre-0.4 spelling of [`StatusHealth::unavailable_status`].
    #[serde(default, skip_serializing)]
    pub unavailable_status_code: Option<u16>,

    /// The pre-0.4 spelling of [`StatusWorkers::allow`].
    #[serde(default, skip_serializing)]
    pub allow_worker_reset: Option<bool>,
}

/// `/health` and `/ready`: two endpoints on one path pair, both always present.
///
/// They answer genuinely different questions and the difference is the one a container orchestrator
/// acts on: `/health` says the process is alive and should not be restarted, and `/ready` says it
/// can serve traffic now and should be sent some. A readiness probe that returned 200 while every
/// worker was still booting is how a rolling deploy sends traffic to a server that answers 502.
///
/// Always served rather than opted into, because a `[status]` table that carried no probes would be
/// an address that answers nothing — and the operator who wrote one is configuring a probe, not an
/// empty socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusHealth {
    /// What to answer when the server is not ready. Defaults to 503.
    ///
    /// Configurable because the convention is not universal: Kubernetes reads any non-2xx as "do
    /// not send traffic", while some load balancers are configured to expect 503 specifically and
    /// treat anything else as a failed probe of the probe itself.
    #[serde(default = "default_unavailable_status")]
    pub unavailable_status: u16,
}

/// The worker-control routes, and what they may do.
///
/// **A method list rather than a `reset = true` flag**, because the routes are not equally
/// dangerous and a deployment that wants to *see* a pool should not have to arm a `POST` to do it.
/// `allow = ["workers.list"]` publishes the list and answers 404 to a reset; naming `workers.reset`
/// as well adds the recycle. An empty list is refused: a table that grants nothing is a block an
/// operator wrote expecting an endpoint, and the honest answer is an error rather than a listener
/// that answers 404 to a route it was configured for.
///
/// **`workers.restart` is the rolling deploy, and it is a third name rather than an alias.**
/// `workers.reset` replaces one named worker or refuses with `409`; `workers.restart` sweeps every
/// slot and replaces the ones that are not serving, answering `202` when a busy slot was left for the
/// next pass. A deploy pipeline wants the sweep and has no business picking a worker off by index, so
/// the two are granted separately. See `docs/11-OPERATIONS.md`.
///
/// The names are the call plane's own — the same `workers.list` a worker would name in `services` —
/// so one vocabulary describes both surfaces, and the method catalogue that decides what `services`
/// accepts decides what this accepts.
///
/// A wildcard address with a mutation in the list is refused, with the argument the flag used to get:
/// a `POST` with no authentication that replaces worker processes, reachable from anywhere, is a
/// remote deploy button. A list of `workers.list` alone is legal on any address, because a list of
/// workers is a fact about this process rather than a lever on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusWorkers {
    /// The methods these routes may serve: `"workers.list"`, `"workers.reset"`, `"workers.restart"`,
    /// or a subset of the three.
    pub allow: Vec<String>,
}

fn default_unavailable_status() -> u16 {
    503
}

/// The probes' defaults, which is what a `[status]` table without a `[status.health]` gets.
///
/// A named function rather than `#[derive(Default)]`, because those two are not the same thing
/// here: `Default` would give `unavailable_status` = `0`, which is not an HTTP status code at all,
/// and the `#[serde(default)]` on the *field* inside `StatusHealth` would not run for the table
/// that was defaulted as a whole. The value has to come from one place, and this is it.
fn default_health() -> StatusHealth {
    StatusHealth {
        unavailable_status: default_unavailable_status(),
    }
}
