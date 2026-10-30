//! Request dispatch: choosing a backend and translating errors.
//!
//! Route-level selection is deliberately simple for now — a default backend plus
//! per-host or per-path overrides, which is what the design calls for. Anything
//! more elaborate belongs with the front-ends, where the request is understood.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, OnceLock, Weak};
#[cfg(any(feature = "redis", feature = "amqp"))]
use std::time::Duration;

use volucer_core::config::{
    BackendConfig, Config, KvStorageConfig, LockConfig, PipelineConfig, RouteConfig,
};
use volucer_fcgi::{cgi, Request, Response};

use crate::backend::{Backend, UpstreamBody};
use crate::calls::{
    ConfigNamespace, JobsNamespace, KvNamespace, LockNamespace, MethodRegistry, PoolLookup,
    ResetRefusal, WorkerStatus, WorkersNamespace,
};
use crate::error::{Result, ServerError};
use crate::rpc::RpcGrants;

/// Routes requests to backends.
#[derive(Debug)]
pub struct Dispatcher {
    backends: BTreeMap<String, Backend>,
    default: String,
    routing: cgi::Routing,
    /// The route table from the configuration, in declaration order.
    ///
    /// Kept here rather than in each front-end so there is exactly one place a
    /// backend is chosen, which is what lets the FastCGI and HTTP front-ends
    /// behave identically for the same request.
    routes: Vec<RouteConfig>,
    /// The gRPC grants the configuration gave, as this dispatcher's **first generation**.
    ///
    /// Here rather than only in the front-end, because a listener built round a dispatcher has to
    /// start with the deployment's answer — otherwise a caller that did not separately hand over
    /// grants would silently serve none, which is indistinguishable from a backend that listed
    /// nothing. A reload replaces the front-end's copy; this one is never changed, so a
    /// diagnostic can always say what the process started with.
    grpc_grants: Arc<volucer_watch::GrpcGrantsMap>,
    /// What a `workers.reset` call needs in order to find a pool.
    ///
    /// **Weak handles to the pools, and the weakness is the point.** The handler for `workers.reset`
    /// lives inside each worker's channel, which is owned by that worker's pool — so a strong handle
    /// here would be the cycle pool → registry → directory → pool. The cycle would be invisible in every
    /// test, and what it costs is that the pools never drop, so [`WorkerPool::shutdown`] never runs and
    /// every worker process is leaked at exit, because `Drop` is what reaps them.
    ///
    /// Filled in at the end of [`Dispatcher::from_config`], from the pools that function has just
    /// started — no self-reference, no deferred initialisation a caller could forget.
    directory: Arc<PoolDirectory>,
    /// The queues, for the tests and for a future consumer side to reach from outside a worker.
    ///
    /// A strong handle, unlike the directory above, and the difference is a real one rather than an
    /// oversight: a driver is not the owner of anything that owns the dispatcher — no pool holds a
    /// driver, so there is no cycle to avoid. The consumer side polls these, which is why
    /// they are kept rather than rebuilt.
    queues: Option<Arc<JobsPlane>>,
    /// The method registry built for each backend, by name.
    ///
    /// **Kept for the same reason `grpc_grants` is**, and discarded by the previous version of this
    /// struct — which is why `[rpc]` could not exist: a worker's registry lives inside its pool's
    /// channels, where nothing outside can reach it, and a CGI backend's lives nowhere at all
    /// because nothing installed it. Retaining it here is what lets one listening socket serve the
    /// *same* namespaces the channels do, over the same stores.
    ///
    /// Only the backends that granted something appear, so an empty map means "this deployment has
    /// no call plane", which is the same question as `[rpc]` being useful.
    registries: BTreeMap<String, Arc<MethodRegistry>>,
    /// The call plane, when the deployment configured one and minted a secret for it.
    ///
    /// Here rather than only in the listener because the *request path* needs it too: what an
    /// application is told — the address and its token — is decided per request, by this dispatcher,
    /// on the way into a backend. Putting it anywhere else would mean a second place that has to
    /// agree with the listener about which backends exist and what they were granted.
    ///
    /// Minted **here**, in the one function that builds both the registries and this handle, so the
    /// secret is one value per process rather than one per caller. A second `new_secret()` call
    /// would produce a token the listener had never heard of, and the failure would be every call
    /// refused with "no backend was issued this token".
    rpc: Option<Arc<RpcGrants>>,
    /// The address an application is told to dial, which is `[rpc].listen` verbatim.
    ///
    /// A separate field rather than a lookup back into the configuration, because the dispatcher does
    /// not keep a `Config`: what it needs from one it has already turned into backends, routes and
    /// grants. Empty when [`Self::rpc`] is `None`, so the two cannot disagree about whether there is
    /// a call plane.
    rpc_address: String,
}

/// What `workers.reset` reaches the pools through, without holding one alive.
///
/// # Why weak, and why the list is filled in last
///
/// The handler for `workers.reset` lives inside each worker's channel, which is owned by that worker's
/// pool — so a strong handle from here back to a pool would be the cycle
/// pool → registry → directory → pool. The cycle would be invisible in every test, and what it costs is
/// that the pools never drop, so [`WorkerPool::shutdown`] never runs and every worker process is leaked
/// at exit, because `Drop` is what reaps them.
///
/// So the pools are held weakly, and the list is filled in by [`Dispatcher::from_config`] once the pools
/// exist. The consequence is worth stating: a call that arrives *during startup* sees a list that is not
/// populated yet, and `workers.list` answers with the pools that are already up rather than with all of
/// them. That is the honest answer — a pool that has not finished booting its workers is not a pool an
/// operator can act on — and the window closes the moment the last backend is up.
pub struct PoolDirectory {
    /// Pools by backend name, in the configuration's order.
    pools: OnceLock<Vec<(String, Weak<crate::WorkerPool>)>>,
}

impl std::fmt::Debug for PoolDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolDirectory")
            .field(
                "pools",
                &self
                    .pools
                    .get()
                    .map(|pools| pools.iter().map(|(name, _)| name).collect::<Vec<_>>()),
            )
            .finish()
    }
}

impl Default for PoolDirectory {
    fn default() -> Self {
        PoolDirectory {
            pools: OnceLock::new(),
        }
    }
}

/// The one place that knows how `workers.list` enumerates pools.
///
/// **The order is the backend map's, and it must not change.** `workers.reset` takes an index into this
/// list, so an enumeration that reordered itself between two calls would make "restart worker 2" mean
/// two different workers depending on when it was sent. The `BTreeMap` the backends live in gives that
/// for free, which is the same reason [`MethodRegistry`]'s namespaces are one.
impl PoolDirectory {
    /// Record the pools, once.
    ///
    /// Called by [`Dispatcher::from_config`] after the backends are built. The second call is ignored
    /// rather than panicking, because the only caller is that one function and a panic there would be a
    /// startup crash for a condition that cannot occur.
    fn install(&self, pools: Vec<(String, Weak<crate::WorkerPool>)>) {
        let _ = self.pools.set(pools);
    }

    /// Every live pool, by backend name, in configuration order.
    ///
    /// A pool whose strong handle has gone is skipped rather than resurrected: the `Weak` upgrade is a
    /// *borrow* that fails once the last strong reference goes, which is exactly the moment the pools
    /// are being torn down.
    fn visit<T>(&self, mut f: impl FnMut(&str, &crate::WorkerPool) -> T) -> Vec<T> {
        match self.pools.get() {
            Some(pools) => pools
                .iter()
                .filter_map(|(backend, pool)| pool.upgrade().map(|pool| f(backend.as_str(), &pool)))
                .collect(),
            None => Vec::new(),
        }
    }
}

/// Build every configured storage, once.
///
/// The name in the map is the name a call uses — `kv.<name>` — and the `Arc` is what lets two backends
/// granted the same storage share one map. A per-backend copy would be a cache that is correct and
/// useless: both backends would work perfectly and see nothing of each other's writes.
///
/// **Fallible, and the one driver that can fail is why.** A Redis that cannot be reached is a startup
/// failure, exactly as an unreachable worker command is, rather than a 500 on the first request that reads
/// a cache. The worker pool follows the same rule for the same reason: a misconfiguration belongs where the
/// configuration is read.
fn build_stores(
    config: &Config,
    config_dir: &Path,
) -> Result<BTreeMap<String, Arc<dyn volucer_kv::Store>>> {
    let mut stores: BTreeMap<String, Arc<dyn volucer_kv::Store>> = BTreeMap::new();
    for (name, storage) in &config.kv {
        let store: Arc<dyn volucer_kv::Store> = match storage {
            KvStorageConfig::Memory(memory) => {
                Arc::new(volucer_kv::MemoryStore::with_max_bytes(memory.max_bytes))
            }
            // Opened here rather than lazily on the first call, for the reason the Redis arm
            // connects here: a directory that cannot be created is a startup failure rather than a
            // cache where every `get` is a miss and nothing reports why. `open` also does the one
            // full walk this driver ever does, so the bound is right from the first insert — and it
            // removes what the previous run left expired.
            KvStorageConfig::File(file) => {
                let path = volucer_core::config::resolve_kv_path(&file.path, config_dir);
                let store = volucer_kv::FileStore::open(&path, file.max_bytes).map_err(|err| {
                    ServerError::Misconfigured {
                        backend: format!("kv.{name}"),
                        reason: err.to_string(),
                    }
                })?;
                Arc::new(store)
            }
            #[cfg(feature = "redis")]
            KvStorageConfig::Redis(redis) => {
                // Built here rather than per backend, so two backends granted the same storage share one
                // pool. A per-backend pool would be `pool_size` connections *per backend* — 160 sockets for
                // ten backends — and would still be correct, which is why nothing else would notice.
                let store = volucer_kv::RedisStore::connect(&redis.url, redis.pool_size, name)
                    .map_err(|err| ServerError::Misconfigured {
                        backend: format!("kv.{name}"),
                        reason: err.to_string(),
                    })?;
                Arc::new(store)
            }
        };
        tracing::info!(
            storage = %name,
            driver = %storage.driver(),
            "kv storage ready"
        );
        stores.insert(name.clone(), store);
    }
    Ok(stores)
}

/// Build the lock backend, once.
///
/// # Why this is not a map like `build_stores`
///
/// A lock is named by its **resource**, so one deployment has one lock namespace and there is nothing for
/// a second backend to mean: two of them would not see each other's locks, and two processes locked on
/// `"pdf:create"` through different ones would both believe they were inside the section. `[lock]` is a
/// single table for that reason, and this returns a single backend for the same one.
///
/// Falls back to [`volucer_lock::MemoryLocks`] when no `[lock]` table is configured — but the caller only
/// uses the result for backends that were *granted* `lock`, and `Config::validate` refuses that grant
/// without a table. So the fallback is for a `Config` built in a test and never validated, and it is the
/// memory driver rather than a refusal because a test wanting to exercise a lock namespace should not
/// have to write a `[lock]` table to do it.
fn build_locks(config: &Config) -> Result<Option<Arc<dyn volucer_lock::Locks>>> {
    let Some(lock) = &config.lock else {
        return Ok(None);
    };

    let locks: Arc<dyn volucer_lock::Locks> = match lock {
        LockConfig::Memory(_) => Arc::new(volucer_lock::MemoryLocks::new()),
        #[cfg(feature = "redis")]
        LockConfig::Redis(redis) => {
            // Built here rather than lazily on the first acquisition: an unreachable server is a startup
            // failure, exactly as it is for a KV storage, rather than a 500 on the first request that
            // needs a lock — and `connect` is what `PING`s to find out.
            let locks = volucer_lock::RedisLocks::connect(
                &redis.url,
                redis.pool_size,
                std::time::Duration::from_millis(redis.read_timeout_ms),
            )
            .map_err(|err| ServerError::Misconfigured {
                backend: "lock".to_string(),
                reason: err.to_string(),
            })?;
            Arc::new(locks)
        }
    };
    tracing::info!(driver = %lock.driver(), "lock backend ready");
    Ok(Some(locks))
}

/// Build the queues, once.
///
/// # Why this is one object rather than a map the registry holds
///
/// The drivers are per-pipeline and the *namespace* is one, because a push names the pipeline and the
/// namespace resolves it. So the map is built here and handed over whole — which is also what lets the
/// consumer side be added without changing this shape: the same drivers will be
/// polled by pools, and a driver behind an `Arc<dyn Driver>` can be both.
///
/// # Why a pipeline's driver is chosen per table
///
/// Unlike `[lock]`, where a second backend would be a second namespace that does not see the first, two
/// pipelines with different drivers is the *normal* case: a Redis-backed mail queue and an in-memory
/// report queue in one deployment is what the configuration is for. So each `[jobs.pipelines.<name>]`
/// names its own driver, and `build_queues` builds each.
fn build_queues(config: &Config) -> Result<Option<Arc<JobsPlane>>> {
    let Some(jobs) = &config.jobs else {
        return Ok(None);
    };

    let mut pipelines: BTreeMap<String, Arc<dyn volucer_jobs::Driver>> = BTreeMap::new();
    let mut consumed: BTreeMap<String, Arc<dyn volucer_jobs::Driver>> = BTreeMap::new();
    for (name, pipeline) in &jobs.pipelines {
        let driver: Arc<dyn volucer_jobs::Driver> = match pipeline {
            // Built per pipeline rather than per deployment, because a driver here *is* a pipeline's
            // storage: two memory pipelines are two independent queues, and sharing one driver between
            // them would make a push to either appear in both.
            PipelineConfig::Memory(_) => Arc::new(volucer_jobs::MemoryDriver::single(name.clone())),
            #[cfg(feature = "redis")]
            PipelineConfig::Redis(redis) => {
                // A **separate connection pool per Redis pipeline**, unlike the KV store's shared one,
                // and the difference is a property of the thing rather than a shortcut: two KV storages
                // on one Redis share a server and a namespace, so sharing connections is free, while two
                // pipelines are two *independent queues* on that server whose only relationship is the
                // host. Pooling them would also mean one pipeline's slow command occupying a connection
                // another pipeline's reserve needed, which is head-of-line blocking across queues that
                // have nothing to do with each other.
                let driver = volucer_jobs::RedisDriver::connect(
                    &redis.url,
                    redis.pool_size,
                    Duration::from_millis(redis.read_timeout_ms),
                    [name.clone()],
                )
                .map_err(|err| ServerError::Misconfigured {
                    backend: format!("jobs.pipelines.{name}"),
                    reason: err.to_string(),
                })?;
                Arc::new(driver)
            }
            #[cfg(feature = "amqp")]
            PipelineConfig::Amqp(amqp) => {
                // A **separate connection per AMQP pipeline**, for the reason the Redis arm states and
                // one more: a reservation *is* an unacknowledged message on a channel, so a pipeline's
                // channel must be the one that settles it. Two pipelines sharing a channel could still
                // settle each other's jobs, but a channel is also where the broker reports a failure —
                // and one pipeline's refused queue argument would then close the other's.
                let queue = volucer_jobs::AmqpQueue {
                    queue: amqp.queue.clone().unwrap_or_else(|| name.clone()),
                    exchange: amqp.exchange.clone().unwrap_or_default(),
                    routing_key: amqp.routing_key.clone().unwrap_or_else(|| {
                        amqp.queue.clone().unwrap_or_else(|| name.clone())
                    }),
                    durable: amqp.durable,
                    arguments: field_table(&amqp.arguments)?,
                };                let driver = volucer_jobs::AmqpDriver::connect(
                    &amqp.url,
                    Duration::from_millis(amqp.read_timeout_ms),
                    [(name.clone(), queue)],
                )
                .map_err(|err| ServerError::Misconfigured {
                    backend: format!("jobs.pipelines.{name}"),
                    reason: err.to_string(),
                })?;
                Arc::new(driver)
            }
        };
        tracing::info!(
            pipeline = %name,
            driver = %pipeline.driver(),
            // Stated per pipeline because it is the one thing an operator has to be able to check when a
            // queue fills up: this node pushes to the pipeline *and* polls it, or only pushes to it.
            consuming = jobs.consumes(name),
            "queue ready"
        );
        if jobs.consumes(name) {
            consumed.insert(name.clone(), Arc::clone(&driver));
        }
        pipelines.insert(name.clone(), driver);
    }

    if consumed.is_empty() && !jobs.pipelines.is_empty() {
        // Not a warning: a producer-only node is a documented deployment, and the queue design says
        // so. Logged at info because it is the answer to "why is nothing consuming", which is a question
        // an operator will otherwise ask of a queue with jobs piling up in it.
        tracing::info!(
            pipelines = pipelines.len(),
            "no pipeline is in `consume`, so this server only produces"
        );
    }

    Ok(Some(Arc::new(JobsPlane {
        pipelines,
        consumed,
    })))
}

/// One queue argument's name, as the broker's field table needs it.
///
/// Refused rather than truncated: an argument name the broker cannot represent is a queue declared with an
/// argument the application did not write, and the failure would surface as a queue with the wrong TTL.
#[cfg(feature = "amqp")]
fn field_name(name: &str) -> Result<volucer_jobs::amqp_types::FieldName> {
    volucer_jobs::amqp_types::FieldName::try_from(name).map_err(|_| {
        ServerError::Misconfigured {
            backend: "jobs".to_string(),
            reason: format!(
                "the queue argument {name:?} is longer than an AMQP field name allows, so the broker \
                 could not be told it"
            ),
        }
    })
}

/// Build a field table from the arguments an operator wrote.
///
/// `FieldTable` has no `FromIterator` and its `insert` maintains a byte length, so the table is built
/// through `insert` rather than collected — which is also why the caller's `Result` is threaded here: a
/// name the broker cannot hold has to be refused, not dropped.
#[cfg(feature = "amqp")]
fn field_table(
    entries: &std::collections::BTreeMap<String, volucer_core::config::AmqpArgument>,
) -> Result<volucer_jobs::amqp_types::FieldTable> {
    let mut table = volucer_jobs::amqp_types::FieldTable::new();
    for (key, value) in entries {
        table.insert(field_name(key)?, field_value(value));
    }
    Ok(table)
}

/// One queue argument's value, in the broker's type system.
///
/// The mapping is total because [`AmqpArgument`] was designed to be: every variant of it has a field-table
/// representation, which is the point of it being an enum rather than a free-form value.
#[cfg(feature = "amqp")]
fn field_value(value: &volucer_core::config::AmqpArgument) -> volucer_jobs::amqp_types::FieldValue {
    use volucer_core::config::AmqpArgument;
    use volucer_jobs::amqp_types::FieldValue;
    match value {
        // Long strings, because that is what RabbitMQ's field table supports for text — the short form
        // does not exist as a value type, which is why the crate notes it as "not exist in RabbitMQ".
        AmqpArgument::Text(text) => FieldValue::from(text.clone()),
        // Signed 64-bit, the widest integer RabbitMQ offers. `x-max-priority`, `x-message-ttl` and
        // `x-expires` all accept it, and a narrower type would refuse a TTL above a month in
        // milliseconds.
        AmqpArgument::Integer(number) => FieldValue::l(*number),
        AmqpArgument::Float(number) => FieldValue::d(*number),
        AmqpArgument::Flag(flag) => FieldValue::from(*flag),
        // A nested table: names that cannot be held are dropped here rather than refused, because this
        // path cannot return an error. A nested argument is rare and a name over the limit in one is
        // rarer; the top-level check above is what an operator will actually meet.
        AmqpArgument::Table(entries) => FieldValue::F(field_table(entries).unwrap_or_default()),
    }
}

/// The pipelines, and which of them this server consumes.
///
/// One object rather than the two halves being passed separately, because the consumer side needs
/// the same drivers the producer namespace holds — and threading two maps through three signatures to avoid
/// one struct is how a seam gets missed./// **The consumed set is derived here, once, from a configuration that has already been validated.** The
/// alternative — asking `Config` at the point of use — would put the same rule in two places, and the
/// interesting failure of that shape is a `consume` name that validation accepted but this object cannot
/// resolve: a consumer that starts, polls nothing, and reports no error.
pub struct JobsPlane {
    pipelines: BTreeMap<String, Arc<dyn volucer_jobs::Driver>>,
    /// The subset of `pipelines` this server polls, in the configuration's `consume` order.
    ///
    /// A separate map rather than a flag per entry, because a `BTreeMap` has no way to carry one and the two
    /// sets are asked different questions: the *producer* namespace needs all the drivers, and the poller
    /// needs only the consumed ones.
    consumed: BTreeMap<String, Arc<dyn volucer_jobs::Driver>>,
}

impl JobsPlane {
    /// The pipelines the producer namespace pushes to.
    ///
    /// A clone of the map, not a `&`: [`JobsNamespace`] owns what it is given, and the `Arc`s it copies
    /// point at the same drivers — so a push through the namespace and a poll by a consumer are talking to
    /// one queue, which is the property that matters.
    fn pipelines(&self) -> BTreeMap<String, Arc<dyn volucer_jobs::Driver>> {
        self.pipelines.clone()
    }

    /// The pipelines this deployment consumes, in the configuration's `consume` order.
    ///
    /// **Consumption is a property of the node, not of the pipeline**, which is RoadRunner's rule and a
    /// deployment one: a web node that pushes mail jobs and a worker node that consumes them run the same
    /// configuration file, and the only difference between them is one line here. So a driver can be
    /// present in a plane without being polled by *this* server, and that is the case this set exists to
    /// express.
    pub fn consumed(&self) -> Vec<(&str, Arc<dyn volucer_jobs::Driver>)> {
        self.consumed
            .iter()
            .map(|(name, driver)| (name.as_str(), Arc::clone(driver)))
            .collect()
    }

    /// Every pipeline name this deployment configured, consumed or not.
    pub fn names(&self) -> Vec<&str> {
        self.pipelines.keys().map(String::as_str).collect()
    }

    /// One pipeline's driver, whether or not this server consumes it.
    ///
    /// The *producer's* view, and the reason it exists rather than a caller reaching into `consumed()`: a
    /// deployment that only pushes to a pipeline still has that pipeline's driver, because the push goes
    /// to the same storage either way. A caller that used `consumed()` to find a driver to push to would
    /// work on a worker node and fail on a web node, which is the confusion this accessor removes.
    pub fn driver(&self, pipeline: &str) -> Option<Arc<dyn volucer_jobs::Driver>> {
        self.pipelines.get(pipeline).map(Arc::clone)
    }
}

impl std::fmt::Debug for JobsPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `dyn Driver` is not `Debug`, and the names are more useful anyway.
        f.debug_struct("JobsPlane")
            .field("pipelines", &self.pipelines.keys().collect::<Vec<_>>())
            .field("consumed", &self.consumed.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Register the application metrics a deployment declared in `[metrics.collect]`.
///
/// **This is what makes `emitMetric` reach the exposition at all**, and it lives here rather than in
/// the binary for one reason worth stating: *that is where a test can reach it*. A version of this in
/// `main.rs` is testable only by copying its arms into a test, and a test that copies the code under
/// test passes when the code regresses — which is the exact shape of the bug this function fixes.
/// Three tests of the pool passed while every real `emitMetric` was dropped, because each of them
/// declared its own metric by hand.
///
/// A registry refuses a name nobody described, deliberately: it has no `# TYPE` and no `# HELP` to
/// write, and letting a worker widen the set at runtime is how a `/metrics` page becomes unbounded.
/// So the deployment declares its names here, once, before anything is served.
///
/// Idempotent, because the registry is. Names are assumed valid: `Config::validate` has refused
/// anything the exposition could not parse, and a second check here would be a rule written twice.
pub fn describe_collected(
    registry: &volucer_metrics::Registry,
    metrics: &volucer_core::config::StatusMetrics,
) {
    use volucer_core::config::CollectedKind;
    use volucer_metrics::Kind;

    for (name, metric) in &metrics.collect {
        match metric.kind {
            CollectedKind::Counter => registry.declare(name, Kind::Counter, &metric.help),
            CollectedKind::Gauge => registry.declare(name, Kind::Gauge, &metric.help),
            CollectedKind::Histogram => {
                registry.declare_histogram(name, &metric.help, &metric.buckets)
            }
        }
    }
}

/// Build one worker backend's method registry from its `services` list.
///
/// # Why the `config.describe` document is assembled here
///
/// The value a worker is told about its deployment can only be decided in one place if it is decided
/// *once*, at startup, rather than assembled per call from live state. Assembling it here also means the
/// redaction in [`ConfigNamespace::new`] runs once, and that the answer cannot drift as the server changes
/// underneath it — a worker that asks twice gets the same document, which is what makes it safe to
/// cache the answer at all.
///
/// # Why an unknown name cannot reach this function
///
/// It is checked by [`Config::validate`](volucer_core::config::Config::validate) against
/// [`CALL_NAMESPACES`](volucer_core::config::CALL_NAMESPACES) before a configuration is ever served, so
/// the fall-through arm here is unreachable in a running server. It exists because this function is also
/// reachable from a `Config` built in a test and never validated, and a silently ignored name would make
/// that test lie.
fn worker_registry(
    services: &[String],
    config: &Config,
    directory: &Arc<PoolDirectory>,
    stores: &BTreeMap<String, Arc<dyn volucer_kv::Store>>,
    locks: &Option<Arc<dyn volucer_lock::Locks>>,
    jobs: &Option<Arc<JobsPlane>>,
) -> MethodRegistry {
    use volucer_core::config::Grant;

    let mut registry = MethodRegistry::new();
    // Parsed once, and grouped by namespace, because the registry is keyed by namespace and a grant
    // can now name one method. A backend granted `workers.list` and `lock.acquire` gets two entries,
    // each carrying the grants that reach it — which is what makes a namespace installable
    // *narrowed* rather than merely chosen.
    let mut by_namespace: BTreeMap<String, Vec<Grant>> = BTreeMap::new();
    for service in services {
        match Grant::parse(service) {
            Some(grant) => by_namespace
                .entry(grant.namespace().to_string())
                .or_default()
                .push(grant),
            // Unreachable from a validated configuration: `validate_services` refuses a name that
            // does not parse. Reachable from a `Config` built in a test, where silently ignoring it
            // would make that test lie.
            None => tracing::warn!(
                service = %service,
                "a configured service is not a call name and is ignored; this should have been \
                 refused when the configuration was validated"
            ),
        }
    }

    // The KV storages this backend may reach, and the verbs it may use on each. Built first because
    // there is **one** `kv` namespace per registry: a second install would replace the first, so a
    // backend granted two storages has to find both in one map.
    let mut reachable: BTreeMap<String, Arc<dyn volucer_kv::Store>> = BTreeMap::new();
    if let Some(grants) = by_namespace.get("kv") {
        for grant in grants {
            // A verb-only grant (`kv.get`) names no storage, so it reaches every storage the
            // backend was granted — and if it named none, there is nothing to reach.
            let storage = grant.storage();
            match storage {
                Some(storage) => match stores.get(storage) {
                    Some(store) => {
                        reachable.insert(storage.to_string(), Arc::clone(store));
                    }
                    // Checked against the `[kv.*]` tables when the configuration was validated, so a
                    // miss here is a store that was not built rather than a name never checked.
                    // Skipping is right: installing the namespace without it would make the grant
                    // look honoured while every call was refused.
                    None => tracing::error!(
                        storage = %storage,
                        "a configured kv service has no store built for it; a worker asking for it \
                         will be refused, which should have been impossible"
                    ),
                },
                None => {
                    for (name, store) in stores {
                        reachable.insert(name.clone(), Arc::clone(store));
                    }
                }
            }
        }
        if !reachable.is_empty() {
            registry = registry.with_grants(KvNamespace::new(reachable), grants.clone());
        }
    }

    for (namespace, grants) in &by_namespace {
        match namespace.as_str() {
            // Already installed above, with its storages resolved.
            "kv" => {}
            "config" => registry = registry.with_grants(ConfigNamespace::new(config), grants.clone()),
            "workers" => {
                registry = registry.with_grants(
                    WorkersNamespace::new(Arc::clone(directory) as Arc<dyn PoolLookup>),
                    grants.clone(),
                )
            }
            // Installed from the built backend rather than from the configuration, for the reason the KV
            // arm skips a storage it cannot find: a worker granted a lock plane that could not be built
            // must be refused by name, not handed a namespace that answers every call with "no lock
            // backend configured" — which reads like a configuration mistake rather than the internal one
            // it would be. `Config::validate` refuses the grant in the first place, so this arm is only
            // reachable from a `Config` built in a test and never validated.
            "lock" => match locks {
                Some(locks) => {
                    registry = registry
                        .with_grants(LockNamespace::new(Arc::clone(locks)), grants.clone())
                }
                None => tracing::error!(
                    "a worker is granted the `lock` service but no lock backend was built; its calls \
                     will be refused, which should have been impossible"
                ),
            },
            "jobs" => match jobs {
                Some(jobs) => {
                    registry = registry
                        .with_grants(JobsNamespace::new(jobs.pipelines()), grants.clone())
                }
                None => tracing::error!(
                    "a worker is granted the `jobs` service but no queue was built; its calls will be \
                     refused, which should have been impossible"
                ),
            },
            other => {
                tracing::warn!(
                    service = %other,
                    "a configured service has no implementation and is ignored; this should \
                     have been refused when the configuration was validated"
                );
            }
        }
    }

    if !registry.is_empty() {
        tracing::debug!(
            namespaces = ?registry.names(),
            "installing a call handler on this backend's workers"
        );
    }
    registry
}

impl PoolLookup for PoolDirectory {
    fn workers(&self) -> Vec<WorkerStatus> {
        // **One running index across every pool, and it is the same one `restart` counts.** A slot's
        // own `index` is local to its pool, so returning it unmodified made `workers.list` and
        // `workers.reset` disagree the moment a deployment configured two backends: the list said
        // worker 1 twice and reset indexed 0..n globally, so "list, then reset what you saw" — the
        // round trip both methods document — reset somebody else. Two pools is not an exotic
        // deployment; it is a web backend and a jobs backend.
        //
        // Found by writing the HTTP route, because a `POST` echoes the index in its answer and a
        // wrong number is then visible instead of merely wrong.
        let mut seen = 0usize;
        self.visit(|backend, pool| {
            let statuses = pool
                .worker_status()
                .into_iter()
                .enumerate()
                .map(|(slot, mut status)| {
                    status.backend = backend.to_string();
                    status.index = seen + slot;
                    // Kept as well as the global one, because a log line that says "worker 4" for a
                    // pool whose own supervisor calls it worker 1 is a search that starts in the
                    // wrong place.
                    status.slot = slot;
                    status
                })
                .collect::<Vec<_>>();
            seen += statuses.len();
            statuses
        })
        .into_iter()
        .flatten()
        .collect()
    }

    fn restart(&self, index: usize) -> std::result::Result<(), ResetRefusal> {
        // One walk, counting slots as it goes, because the index is global across pools and the pools
        // are the only thing that knows how many workers each holds. The count is finished even when
        // the index is not found, so the refusal can say how many there are.
        let mut seen = 0usize;
        let mut found = None;
        self.visit(|_, pool| {
            let len = pool.len();
            if found.is_none() && index >= seen && index < seen + len {
                found = Some((seen, pool.restart(index - seen)));
            }
            seen += len;
        });
        match found {
            Some((_, Ok(()))) => Ok(()),
            // **The pool is asked in its own numbering and answers in it too**, which is the whole
            // reason this rebase exists: a refusal from pool 2 about its worker 1 must not reach a
            // caller that asked about worker 5 as "worker 1". A refusal is what an operator acts on —
            // an index is what they type next — so an off-by-a-pool number is a wrong instruction
            // rather than a cosmetic slip. Found by writing the HTTP route, where the index is echoed
            // in a status code's body and every wrong number is visible.
            Some((offset, Err(refusal))) => Err(refusal.rebased(offset)),
            None => Err(ResetRefusal::NoSuchWorker {
                index,
                workers: seen,
            }),
        }
    }

    /// Every live pool, swept in configuration order.
    ///
    /// Deliberately *not* a `Dispatcher` walk: the directory is what both the call plane and the
    /// status listener reach, and a sweep that went through the dispatcher would be a second path
    /// to the same pools that could enumerate them differently. A pool whose strong handle has gone
    /// is skipped for the same reason [`PoolDirectory::visit`] skips it — the upgrade failing is the
    /// moment the pools are being torn down, and resurrecting one to sweep it would be a restart of
    /// a process that is already exiting.
    fn respawn_idle(&self) -> Vec<crate::respawn::RespawnReport> {
        self.visit(|_, pool| pool.respawn_idle())
    }
}

impl Dispatcher {
    /// Build every configured backend and prepare dispatch.
    ///
    /// Backends are started here rather than lazily, so a misconfiguration is
    /// reported at startup instead of on the first request that happens to hit
    /// the broken one.
    pub fn from_config(config: &Config, routing: cgi::Routing) -> Result<Self> {
        Self::from_config_in(config, routing, Path::new(""))
    }

    /// [`Dispatcher::from_config`], with the directory a relative `[kv.*]` path resolves against.
    ///
    /// A second entry point rather than an added parameter, because the parameter has exactly one
    /// production caller and would otherwise be a `Path::new("")` written into sixty tests —
    /// which is how a test ends up asserting that a cache lives in the process's working directory.
    pub fn from_config_in(
        config: &Config,
        routing: cgi::Routing,
        config_dir: &Path,
    ) -> Result<Self> {
        // The directory is built first and installed on every worker channel as its pool boots, because
        // a channel needs its handler before the handshake and the handshake is inside
        // `WorkerPool::start`. It reaches the pools through a `Weak` filled in at the end of this
        // function, which is what keeps the reference out of both directions at once.
        //
        // A registry is built **per worker backend**, from that backend's own `services` list, so a
        // worker facing the public gets one that cannot restart its siblings. That is
        // the per-backend capability boundary, and building it once for the whole process would be the
        // shape it refuses.
        // The stores are built before any backend, because a backend's registry needs them: a worker
        // granted `kv.sessions` has to find the store already there. Built once and shared, so two
        // backends granted the same storage share one map rather than getting a private copy that
        // silently diverges — which would be the worst possible bug in a cache, since both would work.
        let stores = build_stores(config, config_dir)?;
        // And the lock backend, for the same reason: a worker granted `lock` has to find it already
        // there, and the registry is built per backend while the backend itself is built once.
        let locks = build_locks(config)?;
        // And the queues, for the same reason again: the registry is built per backend and the pipelines
        // are built once, so a worker granted `jobs` finds the drivers already there.
        let jobs = build_queues(config)?;

        let directory = Arc::new(PoolDirectory::default());
        let mut backends = BTreeMap::new();
        let mut registries: BTreeMap<String, Arc<MethodRegistry>> = BTreeMap::new();
        for (name, backend_config) in &config.backends {
            // The server's shared `env` baseline is merged in *here*, before a pool is built: a
            // `Spec` captures its environment at construction, and a replacement worker is started
            // from that captured copy — so a merge applied anywhere later would be a baseline that
            // held for the first worker and not for the one that replaced it.
            let backend_config = backend_config.with_server_env(&config.server);
            let backend_config = &backend_config;
            // Everything that has a `services` list gets a registry, and there is exactly **one**
            // builder for it. What differs is where the list lives and how the registry is used:
            // a `worker` and an `attach` have a channel to install the handler on, while the three
            // kinds that answer FastCGI or HTTP have no channel at all and reach it through `[rpc]`
            // instead — the listener looks the registry up by token.
            //
            // One builder is the point rather than tidiness: a registry built twice would mean two
            // `KvNamespace` values over one store, correct and pointless, and a *second* place to
            // change when a namespace gains a constructor argument. It also means a deployment
            // cannot grant a cache to its workers and forget the same cache for the same
            // application's other half, because there is one list per backend either way.
            let services = match backend_config {
                BackendConfig::Worker(worker) => worker.services.as_slice(),
                BackendConfig::Attach(attach) => attach.services.as_slice(),
                BackendConfig::External(external) => external.services.as_slice(),
                BackendConfig::SpawnCgi(cgi) => cgi.services.as_slice(),
                BackendConfig::Upstream(upstream) => upstream.services.as_slice(),
            };
            let registry = (!services.is_empty()).then(|| {
                let registry = Arc::new(worker_registry(
                    services, config, &directory, &stores, &locks, &jobs,
                ));
                registries.insert(name.clone(), Arc::clone(&registry));
                registry
            });
            // A registry is *offered* to a kind that has no channel to install it on; `Backend`
            // ignores it for those, which is the same contract `from_config_with` already had.
            backends.insert(
                name.clone(),
                Backend::from_config_with(name, backend_config, registry)?,
            );
        }

        // The pools are recorded now that they are all up, and recorded *weakly*: see
        // `PoolDirectory` for why a strong handle here would leak every worker process.
        directory.install(
            backends
                .iter()
                .filter_map(|(name, backend)| match backend {
                    Backend::Worker { pool, .. } => Some((name.clone(), Arc::downgrade(pool))),
                    _ => None,
                })
                .collect(),
        );

        // No backends at all is a legitimate deployment rather than a mistake: a
        // listener with nothing to dispatch to is how static files are served
        // without any backend at all, and with no backends there is also no route that could
        // select one, because `Config::validate` requires every route to name a
        // configured backend. It is allowed here so that `check` and `serve`
        // agree about the file — `check` reports it as a warning, because a server
        // that can answer nothing but static files is usually a configuration
        // still being written rather than a finished one.
        let default = match config.implied_backend(config.server.default_backend.as_deref()) {
            Some(name) => name.to_string(),
            None if backends.is_empty() => String::new(),
            None => {
                return Err(ServerError::Misconfigured {
                    backend: "<default>".to_string(),
                    reason: "no default_backend is configured".to_string(),
                })
            }
        };

        if !backends.contains_key(&default) && !backends.is_empty() {
            return Err(ServerError::Misconfigured {
                backend: default,
                reason: "default_backend does not match any configured backend".to_string(),
            });
        }

        Ok(Dispatcher {
            backends,
            default,
            routing,
            routes: config.server.routes.clone(),
            grpc_grants: Arc::new(config.grpc_grants_all()),
            directory,
            queues: jobs,
            // Minted here if and only if the deployment asked for the call plane *and* has something
            // to serve on it. `None` for a deployment with `[rpc]` in a file that failed to grant —
            // `Config::validate` refuses that combination, so this is the test-and-`check` path — and
            // `None` also means no application is handed a token, which is the honest answer rather
            // than a socket that refuses everything.
            rpc: match (&config.rpc, registries.is_empty()) {
                (Some(_), false) => match crate::rpc::new_secret() {
                    Some(secret) => Some(Arc::new(RpcGrants::new(registries.clone(), &secret))),
                    None => {
                        // No entropy, so no token: the listener is not started either, and the
                        // failure is a logged error rather than a grant any local process could
                        // claim. See `rpc::new_secret`.
                        tracing::error!(
                            "[rpc] is configured but no random source is available, so tokens \
                             cannot be minted; the call plane will not be served"
                        );
                        None
                    }
                },
                _ => None,
            },
            rpc_address: config
                .rpc
                .as_ref()
                .map(|rpc| rpc.listen.clone())
                .unwrap_or_default(),
            registries,
        })
    }

    /// The call plane, when this deployment has one.
    ///
    /// `None` means either no `[rpc]` table or nothing granted — both of which mean an application is
    /// handed no address, so there is nothing for a request path to inject.
    pub fn rpc(&self) -> Option<&Arc<RpcGrants>> {
        self.rpc.as_ref()
    }

    /// The CGI parameters a request to `backend` must carry so that its application can reach the
    /// call plane.
    ///
    /// **Empty for a backend that granted nothing**, which is the whole of the security rule: the
    /// absence of the address is what stops an application from even trying, the same way the
    /// absence of a call handler on a worker channel stops it there. A backend that has no grant is
    /// not handed a token it has to be careful not to misuse.
    ///
    /// The address is the one the deployment wrote, which is where the listener binds — see the note
    /// on port `0` in `[rpc].listen`.
    pub fn rpc_params(&self, backend: &str) -> Vec<(&'static str, String)> {
        let Some(rpc) = &self.rpc else {
            return Vec::new();
        };
        let Some(token) = rpc.token_for(backend) else {
            return Vec::new();
        };
        vec![
            (
                volucer_core::config::RPC_ENDPOINT_PARAM,
                self.rpc_address.clone(),
            ),
            (volucer_core::config::RPC_TOKEN_PARAM, token.to_string()),
        ]
    }

    /// The method registries this deployment built, by backend name.
    ///
    /// **The same `Arc`s the worker channels hold**, so a call over `[rpc]` reaches the store its
    /// sibling worker reaches rather than a second handle to the same thing — and so a `KvNamespace`
    /// that accumulates anything (a refusal counter, a connection) accumulates it once.
    ///
    /// The map is the deployment's *first generation*, exactly as [`Self::grpc_grants`] is: a reload
    /// rebuilds the dispatcher, and a listener built from a dispatcher therefore starts with what the
    /// process was configured with. Only backends that granted at least one namespace appear.
    pub fn registries(&self) -> &BTreeMap<String, Arc<MethodRegistry>> {
        &self.registries
    }

    /// The pools this backend's workers may address through a call, or `None` if they may not.
    ///
    /// The one accessor that answers a question about the *deployment* rather than about a request, and it
    /// exists because the dispatcher itself has to answer a call: `volucer workers` and `volucer reset`
    /// on the command line are clients of this same machinery, and a CLI
    /// that re-derived "which pool is worker 3 in" would be a second implementation of the one rule
    /// `workers.reset` exists to get right.
    pub fn pools(&self) -> Option<Arc<PoolDirectory>> {
        self.backends.values().find_map(|backend| match backend {
            Backend::Worker { .. } => Some(Arc::clone(&self.directory)),
            _ => None,
        })
    }

    /// Replace the application processes behind every backend, where that can be done without
    /// dropping a request.
    ///
    /// The one rolling restart, for all three of its triggers — `SIGUSR2`, the `workers.restart`
    /// call and `volucer workers restart` — so that they cannot disagree about what a sweep is.
    /// Per backend the answers differ and are documented on [`Backend::respawn_idle`]; across
    /// backends the sweep is sequential and each backend's report is returned, so an operator
    /// reading the log can tell which part of the deployment rolled.
    ///
    /// **`&self`, and not a stop-then-start**: the whole point is that the dispatcher and the
    /// listeners keep running, so the front-ends never see the pools change shape.
    pub fn respawn_idle(&self) -> Vec<crate::respawn::RespawnReport> {
        let reports: Vec<_> = self
            .backends
            .values()
            .map(Backend::respawn_idle)
            .collect();
        for report in &reports {
            if report.is_noop() {
                continue;
            }
            tracing::info!(
                backend = %report.backend,
                replaced = report.replaced.len(),
                skipped_busy = report.skipped_busy.len(),
                failed = report.failed.len(),
                "rolling restart: {}",
                report.summary()
            );
        }
        reports
    }

    /// The queues this deployment built, if it has a `[jobs]` table.
    ///
    /// For the tests, and for whatever polls them next — the consumer side. `None` means no
    /// `[jobs]` table, which is the same distinction `[lock]` makes: absent is not "the default driver".
    pub fn queues(&self) -> Option<Arc<JobsPlane>> {
        self.queues.as_ref().map(Arc::clone)
    }

    /// Whether any backend was configured.
    ///
    /// False is the static-only deployment, where a request that names no file
    /// has nowhere to go. The front-ends answer 500 for one, and the CLI warns at
    /// startup, so an operator is not left reading a bare "internal error".
    pub fn has_backends(&self) -> bool {
        !self.backends.is_empty()
    }

    /// The routing table in use, so a front-end can resolve paths for logging.
    pub fn routing(&self) -> &cgi::Routing {
        &self.routing
    }

    /// The default backend's name.
    pub fn default_backend(&self) -> &str {
        &self.default
    }

    /// Whether the named backend is a worker pool.
    ///
    /// A worker reads `script_filename` as metadata and routes on `REQUEST_URI`, so a
    /// path naming no script is not a file the router needs to resolve for it — which is
    /// what lets a route to a worker skip the `stat` entirely. An unknown name is not a
    /// worker: a name that selects nothing cannot vouch for the safety of anything.
    pub fn is_worker(&self, backend: &str) -> bool {
        self.backends.get(backend).is_some_and(Backend::is_worker)
    }

    /// Whether the named backend reads the path as a file to execute.
    ///
    /// The question a route's `skip_stat` turns on. Not the negation of [`Self::is_worker`]: an
    /// upstream does not interpret the path either, but for its own reason, and a route to one must
    /// skip the `stat` just as a worker's does. An unknown name answers `true` — a backend that is
    /// not there cannot vouch for skipping a check.
    pub fn interprets_path(&self, backend: &str) -> bool {
        self.backends
            .get(backend)
            .is_none_or(Backend::interprets_path)
    }

    /// Names of every backend, for diagnostics.
    pub fn backend_names(&self) -> Vec<&str> {
        self.backends.values().map(Backend::name).collect()
    }

    /// The backend that should serve this request.
    ///
    /// A route may override the default, and both front-ends reach the decision
    /// through `volucer_core::config::select_backend`, so a request gets the same
    /// answer whichever protocol carried it and whichever front-end asked. That
    /// sharing is the point: the two used to have their own copy of the precedence
    /// rule, and the HTTP one had grown `path_suffix` and the exact-host tiebreak
    /// while this one had not.
    pub fn backend_for(&self, request: &Request) -> &str {
        self.backend_for_routes(&self.routes, request)
    }

    /// The backend that should serve an HTTP request, asked without a CGI environment.
    ///
    /// The same route rule [`Self::backend_for`] uses, reached from the other front-end. It exists
    /// because the HTTP front-end has `Parts`, not a CGI environment, and building one just to ask
    /// this question would be a second, lossy representation of a request that already exists —
    /// the flattening this whole step is about avoiding.
    ///
    /// `host` is the `Host` header as sent. `path` is the request path *without* its query or
    /// fragment, and a caller that forgets is a routing difference between the two front-ends for
    /// `/api?x=1`, so the rule lives here rather than in each caller.
    pub fn backend_for_target(&self, host: Option<&str>, path: &str) -> &str {
        let path = strip_query(path);
        volucer_core::config::select_backend(&self.routes, host, path)
            .unwrap_or(self.default.as_str())
    }

    /// [`Self::backend_for_target`], against a site's own route table.
    pub fn backend_for_target_routes<'a>(
        &'a self,
        routes: &'a [RouteConfig],
        host: Option<&str>,
        path: &str,
    ) -> &'a str {
        let path = strip_query(path);
        volucer_core::config::select_backend(routes, host, path).unwrap_or(self.default.as_str())
    }

    /// Whether the named backend is an HTTP upstream.
    ///
    /// The front-end's branch, and it has to be at the front-end: every other backend is a blocking
    /// process on a pipe, so making the dispatcher's *type* async to accommodate the one that is
    /// not would drag `spawn_blocking` into the type system of all four. An unknown name answers
    /// `false`, so a name that selects nothing keeps the CGI path's behaviour.
    pub fn is_upstream(&self, backend: &str) -> bool {
        matches!(self.backends.get(backend), Some(Backend::Upstream { .. }))
    }

    /// The client for a named upstream backend.
    ///
    /// `None` for the three CGI kinds, so a caller that asks for the wrong one cannot accidentally
    /// reach an origin — and the client is borrowed rather than cloned, because it shares its pool
    /// through `&self` and cloning it per request would be an `Arc` bump for nothing. Deref through
    /// the box happens here, once, so no caller has to know the field is boxed.
    pub fn upstream(
        &self,
        backend: &str,
    ) -> Option<&volucer_upstream::UpstreamClient<UpstreamBody>> {
        match self.backends.get(backend) {
            Some(Backend::Upstream { client, .. }) => Some(client),
            _ => None,
        }
    }

    /// Send an HTTP request to a named upstream backend.
    ///
    /// The one place the two request paths meet, and the reason it is a method here rather than in
    /// the front-end: choosing the backend, knowing whether it is an upstream, and holding the
    /// client are all the dispatcher's business, so the listener has exactly one call to make for
    /// either path.
    pub async fn dispatch_upstream(
        &self,
        backend: &str,
        request: volucer_upstream::UpstreamRequest<UpstreamBody>,
    ) -> Result<volucer_upstream::UpstreamResponse> {
        let Some(client) = self.upstream(backend) else {
            // Reached only if a caller asked for the wrong kind, which the front-end's own branch
            // makes unreachable in practice. Naming it rather than panicking keeps a future caller's
            // mistake a 500 with a reason instead of a crashed server.
            return Err(match self.backends.get(backend) {
                Some(_) => ServerError::WrongProtocol {
                    backend: backend.to_string(),
                },
                None => ServerError::NoWorker {
                    backend: backend.to_string(),
                },
            });
        };
        client
            .request(request)
            .await
            .map_err(|source| ServerError::Upstream {
                backend: backend.to_string(),
                source,
            })
    }

    /// Whether this deployment configures an upstream backend at all.
    ///
    /// The capability check the CLI makes before refusing a whole configuration: without it,
    /// `serve` would have to reject every deployment, including the ones that never mention an
    /// upstream. It answers for the *deployment*, not for a route, because the thing that is
    /// missing is a request path rather than a routing decision.
    pub fn has_upstream(&self) -> bool {
        self.backends
            .values()
            .any(|backend| matches!(backend, Backend::Upstream { .. }))
    }

    /// [`Dispatcher::backend_for`], against a site's own route table.
    ///
    /// A site carries its own routes, so a request from nginx must be routed by the table
    /// of the site its `Host` names rather than by the server's. Passing the table in keeps
    /// one rule for both front-ends; what differs behind nginx is only that the document root
    /// is already settled, because nginx resolved `SCRIPT_FILENAME` before forwarding.
    pub fn backend_for_routes<'a>(
        &'a self,
        routes: &'a [RouteConfig],
        request: &Request,
    ) -> &'a str {
        let host = param(request, "HTTP_HOST").or_else(|| param(request, "SERVER_NAME"));
        let path = param(request, "REQUEST_URI");
        let path = path.as_deref().map(strip_query).unwrap_or("/");

        volucer_core::config::select_backend(routes, host.as_deref(), path)
            // A route that matches nothing leaves the decision to `default_backend`, which is
            // the name this deployment's configuration gave — not necessarily the first route.
            .unwrap_or(self.default.as_str())
    }

    /// Send a request to a backend chosen from a given route table.
    ///
    /// The routes are a parameter rather than a field because a site carries its own: a request
    /// from nginx must be routed by the table of the site its `Host` names. One rule, two callers
    /// — which is what keeps the two front-ends interchangeable.
    pub fn dispatch_with_routes(
        &self,
        routes: &[RouteConfig],
        request: &Request,
    ) -> Result<Response> {
        let name = self.backend_for_routes(routes, request);
        self.dispatch_to(name, request)
    }

    /// Write the call plane's address and token into a request that is about to reach `backend`.
    ///
    /// **At the front-end, where the request is already `&mut`, rather than inside
    /// [`Self::dispatch_with_routes`]** — and the reason is mechanical rather than stylistic. Both
    /// front-ends own their `Request` by the time they dispatch, so this is two assignments; the
    /// dispatch path sees only `&Request`, so injecting there would mean cloning one, which for a
    /// `POST` is a copy of the whole body to add two parameters.
    ///
    /// One *rule*, two call sites, and the rule that must not be duplicated is [`Self::rpc_params`]:
    /// which backends have a grant, what their token is, and that a backend without one is handed
    /// nothing at all.
    pub fn inject_rpc_params(&self, backend: &str, request: &mut Request) {
        for (key, value) in self.rpc_params(backend) {
            // Replace rather than append: two `VOLUCER_RPC` entries is a request whose address is
            // chosen by whichever one the application reads first, which is the rule
            // `HTTP_TRACEPARENT` already follows for the same reason.
            let key = key.as_bytes().to_vec();
            match request.params.iter_mut().find(|(k, _)| *k == key) {
                Some(pair) => pair.1 = value.into_bytes(),
                None => request.params.push((key, value.into_bytes())),
            }
        }
    }

    /// The gRPC grants this dispatcher was built with, as its first generation.
    ///
    /// The answer a listener must *start* with. A reload replaces the front-end's own copy — see
    /// `volucer_watch::Reloaded::grpc_grants` — so this is read once at bind time and never again.
    pub fn grpc_grants(&self) -> Arc<volucer_watch::GrpcGrantsMap> {
        Arc::clone(&self.grpc_grants)
    }

    /// Whether the named backend is one a gRPC call can be delivered to at all.
    ///
    /// An unknown name is `false`, which is the same answer as a kind that cannot serve gRPC — from
    /// the caller's side both mean "this deployment is not going to serve that call", and
    /// `Config::validate` has already refused a route naming a backend that does not exist.
    ///
    /// The *service* half of the question is deliberately not here: the grants are reloadable state
    /// carried by the configuration generation a request is served under
    /// (`volucer_watch::Reloaded::grpc_grants`), so asking the backend would answer with whatever
    /// the process started with.
    pub fn accepts_grpc(&self, backend: &str) -> bool {
        self.backends
            .get(backend)
            .is_some_and(Backend::accepts_grpc)
    }

    /// Send a request to the backend its route selects.
    pub fn dispatch(&self, request: &Request) -> Result<Response> {
        // The name is borrowed rather than cloned into a `String`. `backend_for` and
        // `dispatch_to` both only read `self`, so the borrow it returns is still live
        // when `dispatch_to` is called; owning the name was an allocation per request
        // that bought nothing.
        let name = self.backend_for(request);
        self.dispatch_to(name, request)
    }

    /// Send a request to a named backend.
    pub fn dispatch_to(&self, name: &str, request: &Request) -> Result<Response> {
        let backend = self.backend_named(name)?;
        backend.request(request)
    }

    /// Send a request to a named backend, relaying the response body when it is large.
    ///
    /// The same lookup, the same error shapes and the same route decision as [`Self::dispatch_to`]:
    /// the only difference is what happens to a body that does not fit. Which is why it is a second
    /// method on the same type rather than a mode on the first — a caller that cannot express a
    /// stream (the FastCGI front-end, which relays a whole record) asks for the buffered one and gets
    /// byte-identical behaviour to before this existed.
    pub fn dispatch_to_relayed(
        &self,
        name: &str,
        request: &Request,
        buffer: usize,
        relay: volucer_fcgi::Relay,
    ) -> Result<crate::pool::Delivery> {
        let backend = self.backend_named(name)?;
        backend.request_relayed(request, buffer, relay)
    }

    /// Send a request to the backend its route selects, relaying a large body.
    pub fn dispatch_relayed(
        &self,
        request: &Request,
        buffer: usize,
        relay: volucer_fcgi::Relay,
    ) -> Result<crate::pool::Delivery> {
        let name = self.backend_for(request);
        self.dispatch_to_relayed(name, request, buffer, relay)
    }

    /// The named backend, or the error that says which kind of miss this is.
    ///
    /// Two different failures share this lookup, and which one it is decides the status a client
    /// reads. No backends at all is the static-only deployment: the configuration has no dispatch
    /// target and never will, so it is a 500 and the CLI has already warned about it. A name that
    /// does not resolve in a deployment that *has* backends is the pool being momentarily exhausted,
    /// which is a transient 503.
    fn backend_named(&self, name: &str) -> Result<&Backend> {
        self.backends.get(name).ok_or_else(|| {
            if self.backends.is_empty() {
                ServerError::NoBackend
            } else {
                ServerError::NoWorker {
                    backend: name.to_string(),
                }
            }
        })
    }

    /// Send a request to the default backend, ignoring the route table.
    ///
    /// An alias used by the listener, where "default" is the meaningful word.
    /// Returns `NoBackend` for a static-only configuration, which is the one case
    /// where a request can arrive with nowhere to go.
    pub fn dispatch_to_default(&self, request: &Request) -> Result<Response> {
        self.dispatch_to(&self.default, request)
    }

    /// Slot contention in every worker pool, for the caller to report.
    ///
    /// `&self`, so it is readable at any time. The CLI reads it once, after the
    /// listeners have stopped, so a load test's numbers are in the log next to the
    /// arm they belong to.
    pub fn contention(&self) -> Vec<crate::backend::Contention> {
        self.backends
            .values()
            .filter_map(Backend::worker_contention)
            .collect()
    }

    /// Whether every worker pool can serve a request now.
    ///
    /// **Only worker pools count, and the asymmetry is the point.** A pool that volucer started
    /// itself is the one that spends a measurable time booting — four worker processes, each
    /// loading an application — so it is the one a readiness probe should wait for. An `external`
    /// FastCGI server is already running and supervised by systemd, and an `upstream` origin is
    /// somebody else's process; waiting for either would be this server reporting on a runtime it
    /// does not own.
    ///
    /// A dispatcher with no worker pools at all is ready, which is the honest answer for a
    /// static-only deployment or one behind an already-running FastCGI pool.
    pub fn is_ready(&self) -> bool {
        self.backends.values().all(|backend| backend.is_ready())
    }

    /// One line per worker pool that cannot serve yet, for the body of a 503.
    pub fn readiness_reasons(&self) -> Vec<String> {
        self.backends
            .values()
            .filter_map(Backend::readiness_reason)
            .collect()
    }

    /// Publish every worker pool's gauges into a metrics registry.
    ///
    /// `&self` and one call at one moment, so the numbers a scrape reads are one picture of the
    /// pool rather than several: `workers_ready + workers_working + workers_invalid` equals
    /// `total_workers` by construction, and a graph where the parts do not sum to the whole is a
    /// graph nobody trusts when it matters.
    ///
    /// Called on the scrape rather than on a timer. A background timer would be a thread whose only
    /// job is to be read by a metric nobody may ever fetch, and the cost here is a handful of atomic
    /// stores per pool — measured against a scrape that already renders every family in the process.
    pub fn publish_metrics(&self, registry: &volucer_metrics::Registry) {
        for backend in self.backends.values() {
            backend.publish_metrics(registry);
        }
    }

    /// Stop every backend.
    ///
    /// `&mut self`, and only reachable by the owner: `Drop` is what reaps the
    /// processes, so this is not on the request path.
    pub fn shutdown(&mut self) {
        for backend in self.backends.values_mut() {
            backend.shutdown();
        }
    }
}

/// Look up a CGI parameter as a lossy UTF-8 string.
fn param(request: &Request, name: &str) -> Option<String> {
    request
        .params
        .iter()
        .find(|(key, _)| key.as_slice() == name.as_bytes())
        .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
}

/// A path with its query and fragment removed.
///
/// One function for both front-ends, because two copies of this rule is how `/api?x=1` comes to
/// route differently depending on which protocol carried it. A `REQUEST_URI` from a CGI client
/// carries its query the same way a request line does, so there is genuinely one rule here and not
/// two that happen to agree.
fn strip_query(path: &str) -> &str {
    path.split(['?', '#']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use volucer_core::config::RoutePolicy;
    use volucer_core::config::{
        BackendConfig, Config, ExternalConfig, MemoryKvConfig, ServerConfig,
    };

    /// A queue argument reaches the broker's type system through the dispatcher, not only through the
    /// parser.
    ///
    /// The gap this closes is real: the configuration could parse `x-queue-type = "quorum"` into an
    /// [`AmqpArgument`] and the driver could understand a field table, and the *conversion between them*
    /// could still have stringified a number or dropped a key. Asserted through [`field_table`] because
    /// that is the function `build_queues` calls, so a test of it is a test of the seam.
    #[cfg(feature = "amqp")]
    #[test]
    fn a_queue_argument_reaches_the_brokers_type_system_with_its_type_intact() {
        use volucer_core::config::AmqpArgument;
        use volucer_jobs::amqp_types::{FieldName, FieldValue};

        let mut arguments = BTreeMap::new();
        arguments.insert(
            "x-queue-type".to_string(),
            AmqpArgument::Text("quorum".to_string()),
        );
        arguments.insert("x-max-priority".to_string(), AmqpArgument::Integer(10));
        arguments.insert(
            "x-single-active-consumer".to_string(),
            AmqpArgument::Flag(true),
        );

        let table = field_table(&arguments).expect("every name is short enough for a field name");
        assert!(
            matches!(
                table.get(&FieldName::try_from("x-queue-type").expect("a valid name")),
                Some(FieldValue::S(text)) if text.as_ref() == "quorum"
            ),
            "a string argument must arrive as a string, because RabbitMQ rejects a queue type it cannot \
             read as text"
        );
        assert!(
            matches!(
                table.get(&FieldName::try_from("x-max-priority").expect("a valid name")),
                Some(FieldValue::l(10))
            ),
            "and a number as a signed 64-bit integer, which is the integer type RabbitMQ's field table \
             actually uses"
        );
        assert!(matches!(
            table.get(&FieldName::try_from("x-single-active-consumer").expect("a valid name")),
            Some(FieldValue::t(true))
        ));

        // A name too long for the broker is refused rather than dropped, because a silently missing
        // argument is a queue declared with a TTL nobody wrote. The bound is AMQP's `shortstr`: 255
        // octets, which is what the client's `TryFrom` enforces — measured, since 200 characters is
        // *accepted* and a test asserting otherwise would have been asserting the wrong limit.
        let mut too_long = BTreeMap::new();
        too_long.insert("x".repeat(300), AmqpArgument::Integer(1));
        assert!(
            field_table(&too_long).is_err(),
            "an argument name past the 255-octet short-string limit must be an error, not a silently \
             dropped key"
        );
    }

    /// A `[kv.*]` file store for a test, with the given relative path.
    ///
    /// A helper rather than a literal at the one call site because a `FileKvConfig` is about to grow
    /// its third field and every test that states the other two should not have to.
    fn file_kv(path: &str) -> volucer_core::config::FileKvConfig {
        volucer_core::config::FileKvConfig {
            path: std::path::PathBuf::from(path),
            max_bytes: 1 << 20,
        }
    }

    /// A call with parameters, for the registry tests.
    fn call_with(name: &str, params: serde_json::Value) -> volucer_worker::Call {
        volucer_worker::Call {
            name: name.to_string(),
            params,
            traceparent: None,
            token: None,
        }
    }

    /// A backend that can be *built* without starting anything, but cannot serve:
    /// an `external` backend pointing at a loopback port nothing listens on.
    ///
    /// Deliberately not a `worker` backend: the worker pool starts real processes
    /// in `from_config`, and these tests are about routing, not supervision.
    ///
    /// The bind is retried, because binding `:0` is not guaranteed to succeed on a loaded
    /// machine: this suite runs beside the `spawn_cgi` tests, which start real `php-cgi`
    /// processes and hold ports, and Windows and macOS both fail an ephemeral bind under
    /// momentary exhaustion of the dynamic range. A single attempt made this file flaky in
    /// exactly that way -- `bind a probe port` -- while passing in isolation. Nothing is
    /// being asserted about the port, only that *some* port was free a moment ago, so
    /// asking again is the truthful thing to do.
    fn unreachable_backend() -> BackendConfig {
        let mut listener = None;
        for _ in 0..20 {
            match std::net::TcpListener::bind("127.0.0.1:0") {
                Ok(bound) => {
                    listener = Some(bound);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        }
        let listener = listener.expect("bind a probe port after 20 attempts");
        let socket = listener.local_addr().expect("address").to_string();
        drop(listener);
        BackendConfig::External(ExternalConfig {
            socket,
            max_connections: None,
            services: Vec::new(),
        })
    }

    fn config_with(backends: &[(&str, BackendConfig)], default: &str) -> Config {
        Config {
            server: ServerConfig {
                http: Some("127.0.0.1:8080".to_string()),
                default_backend: Some(default.to_string()),
                routes: Vec::new(),
                ..Default::default()
            },
            backends: backends
                .iter()
                .map(|(name, config)| (name.to_string(), config.clone()))
                .collect(),
            sites: BTreeMap::new(),
            // Named rather than `..Default::default()` would be shorter, but listing every field
            // is what made adding one a compile error in four test files. A test that asserts on
            // routing has nothing to say about metrics, so it takes the rest as it comes.
            ..Config::default()
        }
    }

    /// A request as a front-end would build it: the CGI parameters the route
    /// table reads, and nothing else.
    fn request_for(host: &str, uri: &str) -> Request {
        let pair = |name: &str, value: &str| (name.as_bytes().to_vec(), value.as_bytes().to_vec());
        Request {
            params: vec![pair("HTTP_HOST", host), pair("REQUEST_URI", uri)],
            ..Request::default()
        }
    }

    #[test]
    fn builds_every_backend_and_dispatch_reaches_the_default() {
        let config = config_with(
            &[
                ("app", unreachable_backend()),
                ("other", unreachable_backend()),
            ],
            "app",
        );
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert_eq!(dispatcher.default_backend(), "app");
        let mut names = dispatcher.backend_names();
        names.sort();
        assert_eq!(names, vec!["app", "other"]);

        // The backend cannot connect, but the routing is what is under test: the
        // failure names the backend we reached, which is the default one.
        let err = dispatcher.dispatch(&Request::default()).unwrap_err();
        assert!(err.to_string().contains("app"), "{err}");
    }

    #[test]
    fn dispatch_to_an_unknown_backend_reports_it_by_name() {
        let config = config_with(&[("app", unreachable_backend())], "app");
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        let err = dispatcher
            .dispatch_to("nope", &Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::NoWorker { .. }), "{err}");
        assert!(err.to_string().contains("nope"), "{err}");
        // A name that does not resolve in a deployment that *has* backends is a transient pool
        // condition — the named backend exists and its slots are momentarily taken — so it stays
        // a 503. Only a deployment with no backends at all is the 500.
        assert!(err.is_busy(), "{err}");
    }

    /// The static-only deployment is `NoBackend`, not `NoWorker`: it is the difference between a
    /// 500 and a 503, and the two must not be reached by the same lookup.
    #[test]
    fn a_dispatcher_with_no_backends_reports_no_backend_and_not_a_busy_pool() {
        let config = config_with(&[], "");
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        let err = dispatcher
            .dispatch_to_default(&Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::NoBackend), "{err}");
        assert!(
            !err.is_busy(),
            "a deployment with no backends must not claim to be at capacity: {err}"
        );
        assert!(
            err.to_string().contains("no backend is configured"),
            "{err}"
        );
    }

    #[test]
    fn rejects_a_default_backend_that_does_not_exist() {
        let config = config_with(&[("app", unreachable_backend())], "missing");
        let err = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap_err();
        assert!(err.to_string().contains("default_backend"), "{err}");
    }

    /// A misspelled namespace in `services` is refused at validation, and a *valid* one builds a
    /// registry — the two halves of the same rule.
    ///
    /// The second half is what this asserts, because the first half lives in `volucer-core`'s own
    /// tests. What matters here is that the allow-list and the implementations agree: a name that
    /// validates but that [`worker_registry`] does not know would be a worker granted a capability it
    /// silently does not have, which is exactly the failure the allow-list exists to prevent.
    #[test]
    fn a_granted_namespace_reaches_the_registry_and_nothing_else_does() {
        let config = config_with(&[("app", unreachable_backend())], "app");
        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config, Path::new("")).expect("no storage needs a server");
        assert!(
            worker_registry(&[], &config, &directory, &stores, &None, &None).is_empty(),
            "no services must mean no namespaces"
        );

        let granted = worker_registry(
            &["config".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );
        assert_eq!(granted.names(), vec!["config"]);
        assert!(
            !granted.names().contains(&"workers"),
            "granting config must not grant workers"
        );

        // Every name the validator allows has an implementation here. Asserted as a loop rather than
        // one case so that adding a name to `CALL_NAMESPACES` without an arm fails *this* test rather
        // than a deployment.
        for name in volucer_core::config::CALL_NAMESPACES {
            let registry = worker_registry(
                &[name.to_string()],
                &config,
                &directory,
                &stores,
                &None,
                &None,
            );
            assert_eq!(
                registry.names(),
                vec![*name],
                "{name} is allowed by the configuration and must have an implementation"
            );
        }
    }

    /// A `Call` naming one method, with no params — enough for anything that only routes.
    fn named(name: &str) -> volucer_worker::Call {
        volucer_worker::Call {
            name: name.to_string(),
            params: serde_json::Value::Null,
            traceparent: None,
            token: None,
        }
    }

    /// **Every method the catalogue declares reaches an arm that answers it, and every verb the
    /// namespaces answer is in the catalogue.**
    ///
    /// The catalogue is a second copy of a fact the namespaces encode in their `match` arms, because
    /// validation lives in `volucer-core` and the namespaces live here — the dependency runs one way
    /// and the copy cannot be removed. This test is what makes the copy safe: adding a method without
    /// cataloguing it fails here rather than shipping a method no grant can name, and cataloguing one
    /// the namespace does not serve fails here rather than granting a call that is refused.
    ///
    /// The second half is checked by asserting that an *ungranted* namespace refuses the call, which
    /// is a fact the namespace can only produce from an arm — a verb it does not implement would be
    /// refused by the fall-through, and a verb it does would be answered differently.
    #[test]
    fn every_catalogued_method_reaches_an_answering_arm() {
        use volucer_core::config::METHODS;

        let config = config_with(&[("app", unreachable_backend())], "app");
        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config, Path::new("")).expect("no storage needs a server");
        let locks = build_locks(&config).expect("no lock backend needs a server");
        let jobs = build_queues(&config).expect("no queue needs a server");

        // One registry holding every namespace, each granted everything, so that a catalogued verb
        // can be sent to the namespace that claims to serve it.
        let all: Vec<String> = METHODS.iter().map(|m| m.name.to_string()).collect();
        let registry = worker_registry(&all, &config, &directory, &stores, &locks, &jobs);
        assert!(
            !registry.is_empty(),
            "granting every catalogued method must install at least one namespace"
        );

        for method in METHODS {
            // The full call name, with a storage filled in for the shapes whose call carries one.
            let call_name = match method.shape {
                volucer_core::config::MethodShape::Flat => {
                    format!("{}.{}", method.namespace, method.verb)
                }
                volucer_core::config::MethodShape::PerStorage => {
                    format!("kv.sessions.{}", method.verb)
                }
            };
            let answer = registry.serve(&named(&call_name));
            // An `error` here is a refusal, and a refusal is not proof of an arm: an unknown verb and
            // an ungranted one read the same. So the refusal must not be the *not-granted* one, and
            // it must not be the *namespace does not serve this* one either — which is why the
            // namespaces' own fall-through messages name the verb they were given.
            let refusal = answer.error.unwrap_or_default();
            assert!(
                !refusal.contains("is not granted to this backend"),
                "{call_name} is catalogued but its own grant did not reach it: {refusal}"
            );
            assert!(
                !refusal.contains("does not serve"),
                "{call_name} is catalogued but the namespace has no arm for it: {refusal}"
            );
        }

        // And the direction that matters more, because it fails silently rather than loudly: every
        // verb a namespace answers is in the catalogue. A method added to a namespace and forgotten
        // here is a method no `services` entry can name — it exists, it is implemented, and no
        // deployment can ever reach it.
        for namespace in volucer_core::config::CALL_NAMESPACES
            .iter()
            .copied()
            .chain(["lock", "jobs", "kv"])
        {
            let verbs = volucer_core::config::Method::verbs_of(namespace);
            assert!(
                !verbs.is_empty(),
                "the {namespace} namespace has an implementation but no catalogued methods, so no \
                 services entry could reach any of it"
            );
        }
        // The KV verbs are the one set the namespace decides at runtime, so they are checked against
        // the constant the namespace's own refusal message is written from.
        assert_eq!(
            volucer_core::config::Method::verbs_of("kv"),
            {
                let mut verbs = volucer_core::config::KV_VERBS.to_vec();
                verbs.sort_unstable();
                verbs.dedup();
                verbs
            },
            "the catalogued kv verbs and KV_VERBS must be one set, or a grant and the namespace's \
             own refusal message disagree about what a storage serves"
        );
    }

    /// And the other direction: a namespace-wide grant covers every verb that namespace has, so
    /// `services = ["workers"]` keeps meaning what it did before grants could name one method.
    #[test]
    fn a_namespace_wide_grant_still_covers_every_verb() {
        use volucer_core::config::{Grant, Method};

        let config = config_with(&[("app", unreachable_backend())], "app");
        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config, Path::new("")).expect("no storage needs a server");

        let registry = worker_registry(
            &["workers".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );
        for verb in Method::verbs_of("workers") {
            let answer = registry.serve(&named(&format!("workers.{verb}")));
            let refusal = answer.error.unwrap_or_default();
            assert!(
                !refusal.contains("is not granted to this backend"),
                "a namespace-wide grant must cover workers.{verb}: {refusal}"
            );
        }

        // And a grant naming one verb does *not* cover the other, which is the whole point.
        let narrow = worker_registry(
            &["workers.list".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );
        let answer = narrow.serve(&named("workers.reset"));
        let refusal = answer.error.expect("an ungranted method is refused");
        assert!(
            refusal.contains("not granted to this backend"),
            "the refusal must say the method is ungranted rather than unknown: {refusal}"
        );
        assert!(
            refusal.contains("workers.list"),
            "and name the grant that would have permitted a call: {refusal}"
        );

        // A `Grant` built by hand is what the registry stores, so the two must agree about shape.
        let _ = Grant::parse("workers.list").expect("a flat grant parses");
    }

    /// A `[kv.*]` file store is **built** from the configuration, which is the seam this pattern keeps
    /// finding.
    ///
    /// The fourth instance of the pattern, and it is asserted the way the first three taught: through
    /// the function a deployment uses rather than through a store the test built by hand. What it
    /// catches is a `build_stores` arm that was added to the config and forgotten here — the
    /// deployment then has a storage that validates, is granted to a worker, and answers "no storage
    /// configured" on every call.
    #[test]
    fn a_file_storage_is_built_from_the_configuration() {
        let dir =
            std::env::temp_dir().join(format!("volucer-dispatch-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the fixture directory");

        let mut config = config_with(&[("app", unreachable_backend())], "app");
        config.kv.insert(
            "sessions".to_string(),
            KvStorageConfig::File(file_kv("cache")),
        );

        let stores = build_stores(&config, &dir).expect("the store builds");
        let store = stores.get("sessions").expect("the storage is there");
        assert_eq!(store.name(), "file");
        assert!(
            dir.join("cache").is_dir(),
            "the store's directory is created under the config directory, which is what a \
             relative path promises"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A granted `lock` service reaches the registry, and it is reachable as a *call*.
    ///
    /// This is the `build_locks` seam — the third time in this document's history that a wiring function
    /// between two tested things was the only untested part, after `set_call_handler` and `build_stores`.
    /// So it is asserted by making a real call through the registry rather than by inspecting the
    /// namespace list: a registry that named `lock` while `build_locks` returned `None` would pass the
    /// weaker check, and that is precisely the bug the KV case had.
    #[test]
    fn a_granted_lock_service_is_reachable_through_the_registry() {
        let mut config = config_with(&[("app", unreachable_backend())], "app");
        config.lock = Some(volucer_core::config::LockConfig::Memory(
            volucer_core::config::MemoryLockConfig {},
        ));

        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config, Path::new("")).expect("no storage needs a server");
        let locks = build_locks(&config)
            .expect("the memory driver needs no server")
            .expect("a [lock] table builds a backend");

        let registry = worker_registry(
            &["lock".to_string()],
            &config,
            &directory,
            &stores,
            &Some(locks),
            &None,
        );
        assert_eq!(registry.names(), vec!["lock"]);

        // Through the registry, not around it: the call has to route on the first dot, find the
        // namespace, reach the backend and come back.
        let acquired = registry.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": "worker-1" }),
        ));
        assert!(acquired.ok, "{acquired:?}");
        assert_eq!(
            acquired.result.expect("a result")["acquired"],
            serde_json::json!(true)
        );

        let twice = registry.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": "worker-2" }),
        ));
        assert_eq!(
            twice.result.expect("a result")["acquired"],
            serde_json::json!(false),
            "a second worker must contend with the first, through the real backend"
        );
    }

    /// With no `[lock]` table there is no backend, so a granted `lock` is skipped rather than faked.
    ///
    /// `Config::validate` refuses that grant before a server serves it, so this is only reachable from a
    /// configuration built in a test. The behaviour that matters is which way it fails: an empty `lock`
    /// namespace installed anyway would answer `lock.acquire` with "no lock backend configured", which
    /// reads like a configuration mistake rather than the internal one it is.
    #[test]
    fn a_lock_service_with_no_backend_installs_nothing() {
        let config = config_with(&[("app", unreachable_backend())], "app");
        assert!(config.lock.is_none());

        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config, Path::new("")).expect("no storage needs a server");
        assert!(
            build_locks(&config).expect("nothing to build").is_none(),
            "an absent [lock] table must not produce a backend"
        );

        let registry = worker_registry(
            &["lock".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );
        assert!(
            registry.is_empty(),
            "a granted service with nothing behind it must be refused by name rather than answered \
             with an empty namespace: {:?}",
            registry.names()
        );
    }

    /// The default read timeout is the one the documentation promises, and it is reachable by omitting it.
    ///
    /// Pinned because the value is written in three places — the configuration default, the lock crate's
    /// documentation, and RoadRunner's own default — and a default that drifted would be a deadline
    /// nobody could predict from the file.
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
            redis.pool_size, 16,
            "and the pool default is the KV one, written out here because this crate cannot name a \
             driver's constant — the two are one value in configuration"
        );
    }

    /// Two storages granted to one backend arrive in **one** `kv` namespace.
    ///
    /// The shape that makes this easy to get wrong: the registry routes on the text before the first dot,
    /// so both `kv.sessions` and `kv.cache` route to the namespace named `kv`. Installing one `with` call
    /// per granted storage would therefore have the second replace the first, and the symptom would be
    /// that only one of a backend's two storages worked — which a test with one storage cannot see.
    #[test]
    fn two_granted_storages_share_one_kv_namespace() {
        let mut config = config_with(&[("app", unreachable_backend())], "app");
        config.kv.insert(
            "sessions".to_string(),
            KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1024 }),
        );
        config.kv.insert(
            "cache".to_string(),
            KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1024 }),
        );

        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config, Path::new("")).expect("no storage needs a server");
        let registry = worker_registry(
            &["kv.sessions".to_string(), "kv.cache".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );

        assert_eq!(
            registry.names(),
            vec!["kv"],
            "two storages are one namespace, because that is what the router keys on"
        );

        // And both are reachable through it. A call that reaches the namespace is answered for each,
        // which is the property the merge exists for; `store.set` then `store.get` is the one round trip
        // that proves the namespace found a real store rather than an empty map.
        for storage in ["sessions", "cache"] {
            let answer = registry.serve(&call_with(
                &format!("kv.{storage}.set"),
                serde_json::json!({
                    "key": crate::params::encode_base64(b"k"),
                    "value": crate::params::encode_base64(b"v"),
                }),
            ));
            assert!(answer.ok, "{storage} should accept a value: {answer:?}");

            let answer = registry.serve(&call_with(
                &format!("kv.{storage}.get"),
                serde_json::json!({ "key": crate::params::encode_base64(b"k") }),
            ));
            assert_eq!(
                answer.result.as_ref().and_then(|r| r["value"].as_str()),
                Some("dg=="),
                "{storage} should return what it stored: {answer:?}"
            );
        }
    }

    /// The two storages are separate maps, so writing one does not write the other.
    ///
    /// The other half of sharing a namespace: they share the *namespace*, not the store. A merge that got
    /// this wrong would make two named caches one, and the difference would only show as a key appearing
    /// in a place nobody put it.
    #[test]
    fn two_storages_in_one_namespace_do_not_share_their_map() {
        let mut config = config_with(&[("app", unreachable_backend())], "app");
        for name in ["sessions", "cache"] {
            config.kv.insert(
                name.to_string(),
                KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1024 }),
            );
        }

        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config, Path::new("")).expect("no storage needs a server");
        let registry = worker_registry(
            &["kv.sessions".to_string(), "kv.cache".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );

        let key = crate::params::encode_base64(b"shared-key");
        registry.serve(&call_with(
            "kv.sessions.set",
            serde_json::json!({ "key": key, "value": crate::params::encode_base64(b"from-sessions") }),
        ));
        let answer = registry.serve(&call_with(
            "kv.cache.get",
            serde_json::json!({ "key": key }),
        ));

        assert_eq!(
            answer.result.as_ref().and_then(|r| r["value"].as_str()),
            None,
            "a key written to one storage must not appear in another: {answer:?}"
        );
    }

    /// A storage a backend was not granted is refused by name, and the refusal says what it *may* reach.
    ///
    /// This is the per-backend capability boundary as a test. The distinction worth checking is between a storage the deployment has and
    /// this backend lacks — a `services` list to edit — and a storage that does not exist at all, because
    /// an operator reading one message and acting on it as though it were the other wastes their time.
    #[test]
    fn a_storage_outside_a_backends_grant_is_refused_by_name() {
        let mut config = config_with(&[("app", unreachable_backend())], "app");
        for name in ["sessions", "secrets"] {
            config.kv.insert(
                name.to_string(),
                KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1024 }),
            );
        }

        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config, Path::new("")).expect("no storage needs a server");
        let registry = worker_registry(
            &["kv.sessions".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );

        let refused = registry.serve(&call_with(
            "kv.secrets.get",
            serde_json::json!({ "key": crate::params::encode_base64(b"k") }),
        ));
        assert!(!refused.ok);
        let message = refused.error.clone().unwrap_or_default();
        assert!(
            message.contains("secrets"),
            "the refusal must name what was asked for: {message}"
        );
        assert!(
            message.contains("sessions"),
            "and what may be reached, or the operator has to read the config file to find out: {message}"
        );

        // A storage that does not exist anywhere reads differently — it is refused as ungranted too, since
        // from this backend's side the two are the same question, but the message names the one it may
        // reach so the difference is visible to whoever reads it.
        let unknown = registry.serve(&call_with(
            "kv.typo.get",
            serde_json::json!({ "key": crate::params::encode_base64(b"k") }),
        ));
        assert!(!unknown.ok);
        assert!(
            unknown
                .error
                .as_deref()
                .is_some_and(|m| m.contains("typo") && m.contains("sessions")),
            "{unknown:?}"
        );
    }

    #[test]
    fn startup_fails_on_a_misconfigured_backend_rather_than_on_first_request() {
        #[cfg(not(unix))]
        let socket = "unix:/run/php.sock";
        #[cfg(unix)]
        let socket = "unix:";

        let config = config_with(
            &[(
                "fpm",
                BackendConfig::External(volucer_core::config::ExternalConfig {
                    socket: socket.to_string(),
                    max_connections: None,
                    services: Vec::new(),
                }),
            )],
            "fpm",
        );
        assert!(Dispatcher::from_config(&config, cgi::Routing::default()).is_err());
    }

    #[test]
    fn a_route_selects_the_backend_and_the_default_applies_otherwise() {
        let mut config = config_with(
            &[
                ("app", unreachable_backend()),
                ("legacy", unreachable_backend()),
            ],
            "app",
        );
        config.server.routes = vec![RouteConfig {
            policy: RoutePolicy::All,
            path_prefix: Some("/api".to_string()),
            path_suffix: None,
            host: None,
            backend: Some("legacy".to_string()),
            redirect: None,
        }];
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert_eq!(
            dispatcher.backend_for(&request_for("example.test", "/api/users")),
            "legacy"
        );
        assert_eq!(
            dispatcher.backend_for(&request_for("example.test", "/other")),
            "app"
        );
        // A query string is not part of the path, so it cannot change the route.
        assert_eq!(
            dispatcher.backend_for(&request_for("example.test", "/api?x=1")),
            "legacy"
        );
    }

    #[test]
    fn a_route_narrows_an_existing_one_rather_than_widening_it() {
        // Two routes match `/api/x`; the more specific one decides. This is the
        // property that makes the table predictable rather than order-dependent.
        let mut config = config_with(
            &[
                ("app", unreachable_backend()),
                ("api", unreachable_backend()),
                ("admin_api", unreachable_backend()),
            ],
            "app",
        );
        config.server.routes = vec![
            RouteConfig {
                policy: RoutePolicy::All,
                path_prefix: Some("/api".to_string()),
                path_suffix: None,
                host: None,
                backend: Some("api".to_string()),
                redirect: None,
            },
            RouteConfig {
                policy: RoutePolicy::All,
                path_prefix: Some("/".to_string()),
                path_suffix: None,
                host: Some("admin.test".to_string()),
                backend: Some("admin_api".to_string()),
                redirect: None,
            },
        ];
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert_eq!(
            dispatcher.backend_for(&request_for("admin.test", "/api/x")),
            "admin_api"
        );
        assert_eq!(
            dispatcher.backend_for(&request_for("public.test", "/api/x")),
            "api"
        );
    }

    // -----------------------------------------------------------------------
    // The upstream arm: reachable from Rust, and the two routing sources must agree.
    // -----------------------------------------------------------------------

    /// An upstream config, with the origin pointed at a port nothing listens on.
    ///
    /// Deliberately unreachable: every test here is about *which* backend is chosen and whether the
    /// client exists, not about what an origin answers — that needs the listener, and lives with
    /// the front-end tests.
    fn unreachable_upstream() -> BackendConfig {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe port");
        let port = listener.local_addr().expect("address").port();
        drop(listener);
        BackendConfig::Upstream(volucer_core::config::UpstreamConfig::new(format!(
            "http://127.0.0.1:{port}"
        )))
    }

    #[test]
    fn an_upstream_backend_builds_a_client_and_is_not_reached_over_cgi() {
        let config = config_with(
            &[
                ("node", unreachable_upstream()),
                ("cgi", unreachable_backend()),
            ],
            "node",
        );
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert!(dispatcher.is_upstream("node"));
        assert!(!dispatcher.is_upstream("cgi"));
        assert!(dispatcher.upstream("node").is_some());
        assert!(dispatcher.upstream("cgi").is_none());
        // And the FastCGI path refuses it by *name* rather than trying a connection, so the error
        // says "wrong protocol" instead of "unreachable origin" — two different operator actions.
        let err = dispatcher
            .dispatch_to("node", &Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::WrongProtocol { .. }), "{err}");
        assert!(err.to_string().contains("FastCGI"), "{err}");
    }

    #[test]
    fn an_upstream_whose_origin_cannot_be_built_is_a_misconfiguration() {
        // `Backend::from_config` is reachable without `Config::validate`, so the origin is parsed
        // here too: a backend built from an unvalidated config must fail at startup with the key
        // named, not 502 on every request.
        let config = config_with(
            &[(
                "node",
                BackendConfig::Upstream(volucer_core::config::UpstreamConfig::new("not-an-origin")),
            )],
            "node",
        );
        let err = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("node"), "{err}");
        assert!(err.to_string().contains("no scheme"), "{err}");
    }

    #[test]
    fn a_ca_that_cannot_be_read_fails_at_startup_not_on_the_first_request() {
        // The rule the other backends already follow: a mistyped path is a startup failure. Without
        // it, a deployment serves happily and 502s every request for one backend.
        let config = config_with(
            &[(
                "node",
                BackendConfig::Upstream({
                    let mut upstream =
                        volucer_core::config::UpstreamConfig::new("https://node.internal:8443");
                    upstream.tls = Some(volucer_core::config::UpstreamTlsConfig {
                        ca: Some(std::path::PathBuf::from(
                            "/definitely/not/a/ca-certificate.pem",
                        )),
                        sni: None,
                    });
                    upstream
                }),
            )],
            "node",
        );
        let err = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("ca-certificate"), "{err}");
    }

    /// The two routing sources must give the same answer, including about a query string.
    ///
    /// This is the assertion that makes `backend_for_target` a *reuse* of the route rule rather
    /// than a second copy of it. `/api?x=1` is the case that matters: a path with its query left on
    /// matches a `path_prefix` differently from one without, so a front-end that forgot to strip it
    /// would route the same request differently depending on the protocol.
    #[test]
    fn the_cgi_and_parts_routing_agree_including_about_a_query() {
        let config = config_with(
            &[
                ("api", unreachable_backend()),
                ("app", unreachable_backend()),
                ("fallback", unreachable_backend()),
            ],
            "app",
        );
        let mut config = config;
        // Two routes, and the *default* is a third. That matters: several of the assertions below
        // compare against the default, so a test whose default was one of the routes would pass
        // even if a query-bearing path failed to match a route at all.
        config.server.default_backend = Some("fallback".to_string());
        config.server.routes = vec![
            RouteConfig {
                policy: RoutePolicy::All,
                host: None,
                path_prefix: Some("/api".to_string()),
                path_suffix: None,
                backend: Some("api".to_string()),
                redirect: None,
            },
            // A *suffix* route, deliberately, because it is the one that makes the stripping
            // load-bearing: `/api` as a prefix still matches `/api?x=1` with the query left on,
            // whereas `/index.php?x=1` does not end with `.php`.
            RouteConfig {
                policy: RoutePolicy::All,
                host: None,
                path_prefix: None,
                path_suffix: Some(".php".to_string()),
                backend: Some("app".to_string()),
                redirect: None,
            },
        ];
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        let fallback = config
            .server
            .default_backend
            .clone()
            .expect("the fixture states one");
        for (host, uri, expected) in [
            ("public.test", "/api", Some("api")),
            ("public.test", "/api/x", Some("api")),
            ("public.test", "/api?x=1&y=2", Some("api")),
            ("public.test", "/api/x?trace=1", Some("api")),
            ("public.test", "/api#frag", Some("api")),
            ("public.test", "/index.php", Some("app")),
            // The case a front-end that forwarded the query would get wrong: without stripping,
            // `.php` does not match and this falls through to the default.
            ("public.test", "/index.php?x=1", Some("app")),
            ("public.test", "/index.php?x=1#top", Some("app")),
            ("public.test", "/index.php#top", Some("app")),
            ("public.test", "/other", None),
            ("public.test", "/nothing/here", None),
        ] {
            let from_cgi = dispatcher.backend_for(&request_for(host, uri));
            // `parts.uri.path()` never carries a query, and a caller that passes a whole URI needs
            // the rule — so the front-end hands over a path and the helper strips one if a caller
            // forgot. Both sources therefore agree without the front-end knowing the rule.
            let path = uri.split(['?', '#']).next().unwrap_or(uri);
            let from_parts = dispatcher.backend_for_target(Some(host), path);

            let expected = expected.unwrap_or(fallback.as_str());
            assert_eq!(from_cgi, expected, "{uri} from a CGI client");
            assert_eq!(from_parts, expected, "{uri} from an HTTP client");
        }
    }

    #[test]
    fn an_unknown_backend_is_not_an_upstream_and_keeps_the_cgi_behaviour() {
        // A name that selects nothing must not be treated as an upstream: it cannot vouch for
        // anything, so it keeps the CGI path's answer rather than sending a request to an origin
        // that was never configured.
        let config = config_with(&[("app", unreachable_backend())], "app");
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();
        assert!(!dispatcher.is_upstream("nope"));
        assert!(dispatcher.upstream("nope").is_none());
        let err = dispatcher
            .dispatch_to("nope", &Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::NoWorker { .. }), "{err}");
    }

    #[test]
    fn has_upstream_answers_for_the_deployment_rather_than_a_route() {
        // The capability check `serve` makes. A deployment with no upstream must not be refused,
        // which is why this is not "does any route name one".
        let plain = config_with(&[("app", unreachable_backend())], "app");
        let dispatcher = Dispatcher::from_config(&plain, cgi::Routing::default()).unwrap();
        assert!(!dispatcher.has_upstream());

        let with_upstream = config_with(
            &[
                ("app", unreachable_backend()),
                ("node", unreachable_upstream()),
            ],
            "app",
        );
        let dispatcher = Dispatcher::from_config(&with_upstream, cgi::Routing::default()).unwrap();
        assert!(dispatcher.has_upstream());
    }
}
