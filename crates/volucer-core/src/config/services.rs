//! The named side-services: trace export, key-value storages, locks and queues.
//!
//! Grouped together because they answer one question for a worker — which call namespaces the
//! deployment provides. A table here is what makes `["kv.sessions"]`, `["lock"]` or `["jobs"]` a
//! name a backend may put in its `services` list.

use super::*;

/// `[telemetry]`: where spans go, and what this process calls itself.
///
/// # Why this is not `[metrics]` with more keys
///
/// They are two protocols with different transports and different failure modes. Prometheus
/// *pulls* on a scrape, so a scrape endpoint that is unreachable costs nothing; OTLP *pushes*, so an
/// exporter that cannot reach its collector is a thread doing failed requests for as long as the
/// deployment serves traffic. Merging them into one table would make "I want metrics but not
/// traces" expressible only by leaving keys unset, which is the two-statements-one-thing this
/// project's `[server.tls]` documentation already refuses.
///
/// # The `traceparent` this exports is the one already sent to PHP
///
/// Worth stating because it is the reason the export is driven directly rather than through a
/// `tracing` subscriber layer: `volucer-observe` mints a trace id and a span id per request and puts
/// *that* header on the request it hands a backend, so PHP's spans — and any origin's — name this
/// server's span as their parent. An exporter that minted its own span id would export a parent no
/// child points at, and the waterfall would break at exactly the seam it exists to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// The collector's OTLP/HTTP endpoint: where `/v1/traces` is appended.
    ///
    /// `http://host:port` — **with no path**, because the path is the protocol's and not the
    /// operator's: the specification fixes it at `/v1/traces`, and a deployment that wrote a
    /// complete URL including it would end up posting to `/v1/traces/v1/traces`. A value that
    /// already ends in `/v1/traces` is therefore *accepted and normalised* rather than refused,
    /// since that is what most collector documentation quotes verbatim.
    ///
    /// `https:` is supported — the exporter reuses the `upstream` client, which already speaks TLS
    /// against a named CA or the public roots.
    pub endpoint: String,

    /// The `service.name` resource attribute. Defaults to `volucer`.
    ///
    /// A resource attribute rather than a label, and it is the one every backend groups by. It
    /// defaults rather than being required because a single-service deployment should not have to
    /// invent a name for the only thing it runs.
    #[serde(default = "default_service_name")]
    pub service_name: String,

    /// Headers sent with every export, e.g. an API key for a hosted collector.
    ///
    /// A table, because a collector's authentication is a header and different vendors spell it
    /// differently. **Every value here is a credential to `config.describe`**, which elides the whole
    /// map's values by name and keeps the header names — the same rule `[server.env]` follows, and the
    /// one that makes `${...}` in this table safe.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,

    /// How long one export request may take before it is abandoned. Defaults to 10s.
    ///
    /// A request deadline rather than a total budget: a collector that accepts a connection and
    /// never answers must not hold the exporter's thread for ever, and abandoned spans are dropped
    /// rather than queued without bound — the same rule the access log follows for a full disk.
    #[serde(default = "default_telemetry_timeout_ms")]
    pub timeout_ms: u64,

    /// How many spans are held before a batch is sent. Defaults to 512.
    ///
    /// A batch is a compromise and both ends of it are real: one export per span is a request per
    /// request, which is the load this feature exists to avoid adding, and an unbounded buffer is a
    /// memory leak with a slow collector as its trigger. At the default, a batch of finished spans
    /// is around 100 KB — measured from the encoder's own output — which is a request a collector
    /// handles without noticing.
    #[serde(default = "default_telemetry_batch_size")]
    pub batch_size: usize,

    /// How long a partly-filled batch waits before it is sent anyway. Defaults to 5s.
    ///
    /// Without this, a deployment serving one request a minute would export nothing until the
    /// batch filled, which on a quiet service is the difference between a trace and no trace. The
    /// timeout is what makes the feature usable below the batch size.
    #[serde(default = "default_telemetry_interval_ms")]
    pub interval_ms: u64,
}

fn default_service_name() -> String {
    "volucer".to_string()
}

fn default_telemetry_timeout_ms() -> u64 {
    10_000
}

fn default_telemetry_batch_size() -> usize {
    512
}

fn default_telemetry_interval_ms() -> u64 {
    5_000
}

/// The path the OTLP/HTTP trace service is fixed at by the specification.
///
/// A constant rather than a configuration key, because it is the protocol's rather than the
/// deployment's: a collector that served traces elsewhere would not be an OTLP collector, and an
/// operator who needs a path prefix has a reverse proxy for exactly that.
pub const OTLP_TRACES_PATH: &str = "/v1/traces";

impl TelemetryConfig {
    /// The endpoint as an *origin* — scheme and authority, no path.
    ///
    /// # Why this exists rather than the exporter trimming it
    ///
    /// **One normaliser, because two would diverge and the divergence is invisible.** The first
    /// version had the exporter strip a trailing `/v1/traces` while `validate` checked the raw string
    /// with `OriginSpec::parse` — which refuses any path. That combination rejected a value the
    /// documentation said was accepted, and a *user* pasting the complete URL their collector's
    /// documentation quotes would have hit a configuration error naming a rule that reads as
    /// arbitrary.
    ///
    /// Accepting the complete URL is the right side to fix on: it is what collector documentation
    /// quotes verbatim, it is unambiguous, and refusing it teaches nothing. The path is the
    /// protocol's, so it is removed here and appended by the exporter — which is what stops
    /// `/v1/traces/v1/traces`, the failure this normalisation exists to prevent.
    #[must_use]
    pub fn base_endpoint(&self) -> &str {
        let trimmed = self.endpoint.trim().trim_end_matches('/');
        let base = trimmed.strip_suffix(OTLP_TRACES_PATH).unwrap_or(trimmed);
        // Trailing slashes again, because `http://host:4318/v1/traces/` is a shape somebody writes.
        base.trim_end_matches('/')
    }
}

/// One `[kv.<name>]` storage: which driver, and that driver's own settings.
///
/// # Why a tagged enum and not `driver` plus an untyped table
///
/// This is the one place the document says outright that it diverges from RoadRunner's shape,
/// and the reason is the same one [`BackendConfig`] gives: RR's
/// `driver = "redis"` plus a free-form `config` table means `adress` is a key that parses, is
/// ignored, and leaves an operator with a Redis client pointed at the default port and no message
/// saying why. A tag and a typed arm make every one of those a parse error naming the key.
///
/// It is *flatter* for the same reason it is typed — `max_bytes` sits beside `driver` rather than
/// inside a `config` table — because a nested table for one key would be a level of indirection
/// that exists only to look like somebody else's file format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "driver", rename_all = "snake_case", deny_unknown_fields)]
pub enum KvStorageConfig {
    /// In this process's memory. Sharded, bounded by bytes, with per-item expiry.
    ///
    /// The default for a single-node deployment, and the one that needs nothing installed. See
    /// `volucer-kv` for what the bound means and why it is bytes rather than entries.
    Memory(MemoryKvConfig),

    /// In Redis. Shared between processes and survives a restart of this one.
    ///
    /// Behind the `redis` cargo feature, so a build without it refuses this driver at *parse* time rather
    /// than accepting a table whose store cannot be built. See `RedisKvConfig` for why that is a feature
    /// and not a plugin.
    #[cfg(feature = "redis")]
    Redis(RedisKvConfig),

    /// A directory of files, one per key. Survives a restart, needs no server.
    ///
    /// The driver for a single node that has a disk and no Redis, which is the deployment whose session
    /// store would otherwise be emptied by every deploy. See `volucer-kv`'s `file` module for what a
    /// directory costs and what it deliberately does not do — no fsync, no compaction, no cross-process
    /// coordination.
    File(FileKvConfig),
}

/// `[kv.<name>] driver = "redis"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisKvConfig {
    /// The server, as a URL: `redis://127.0.0.1:6379`, optionally with a username, password and database.
    ///
    /// **May carry a credential, and `config.describe` removes the userinfo from it** — the host and the
    /// database survive, because the address is the part an operator is checking. See the redaction's own
    /// documentation in `volucer-server`'s `calls` module for the three rules. A URL rather than separate
    /// `host`/`port`/`password` keys because that is
    /// what every other Redis client takes and what an operator has in their environment already.
    ///
    /// A `unix:` URL — `redis:///path/to/socket` or `unix:/run/redis.sock` — is not accepted here even where
    /// the client supports it, because a socket path in a URL is spelled differently by every client and an
    /// operator copying one would get a connection error rather than a message about the spelling.
    pub url: String,

    /// How many connections to keep.
    ///
    /// One per worker is the natural size, since a call blocks the worker that asked. Defaults to
    /// [`volucer_kv::DEFAULT_POOL_SIZE`]'s value, written out here because this crate cannot depend on a
    /// driver to name a default.
    #[serde(default = "default_redis_pool_size")]
    pub pool_size: usize,
}

/// `[kv.<name>] driver = "memory"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryKvConfig {
    /// How many bytes of stored values to hold before refusing to store more.
    ///
    /// Defaults to 64 MiB. **Deliberately not an entry count**, and deliberately not the process's
    /// heap either: this is the sum of the values' own lengths, and the map holding them costs
    /// roughly two to three times as much again. The module documentation in `volucer-kv` states
    /// the multiplier rather than letting the setting imply something it does not mean.
    #[serde(default = "default_kv_max_bytes")]
    pub max_bytes: usize,
}

/// `[kv.<name>] driver = "file"`.
///
/// Unlike [`RedisKvConfig`], the path is **not** a credential, so `config.describe` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileKvConfig {
    /// The directory the store owns.
    ///
    /// **Required, rather than defaulting to somewhere under `/tmp`.** A cache that writes to a
    /// temporary location by default is one that silently loses its contents on the reboot an
    /// operator least expected, and a default is exactly the kind of value nobody reads. A relative
    /// path resolves against the configuration file's directory, like `[server.access_log]`'s does
    /// and for the same reason: what it is relative to should be a property of the deployment, not
    /// of wherever somebody happened to run the binary.
    pub path: std::path::PathBuf,

    /// How many bytes of stored values the directory may hold, before an insert is refused.
    ///
    /// Defaults to 64 MiB, the same number and the same meaning as [`MemoryKvConfig::max_bytes`] —
    /// one setting an operator learns once. Counted from what is already on disk when the store
    /// opens, so a restart does not double the space a cache uses.
    #[serde(default = "default_kv_max_bytes")]
    pub max_bytes: usize,
}

/// The environment variable carrying the volucer version, set for **every** process this server
/// starts.
///
/// The same fact `volucer version` prints and the same one `HelloAck` carries, made available to a
/// process that has no channel to ask on: a `php-cgi` per-request process, a supervised origin, a
/// job consumer. The SDK does not read it — a worker's channel already reports its version — so
/// this is for the application and for a bug report that has to say which server it ran on.
///
/// **Set after the configured `env`, so it always wins**, exactly as `VOLUCER_WORKER_SOCKET` does:
/// a value an operator could shadow would be a variable that sometimes names the wrong server.
pub const VERSION_ENV: &str = "VOLUCER_VERSION";

pub(super) fn default_kv_max_bytes() -> usize {
    // Written out rather than referring to `volucer-kv`'s constant, because this crate is the leaf
    // that everything depends on and taking a dependency on a *driver* to name a default would
    // invert that. The two are asserted equal by a test in `volucer-kv`, so they cannot drift
    // silently — the alternative, a second value here, is what that test exists to prevent.
    64 * 1024 * 1024
}

impl KvStorageConfig {
    /// The driver's name, as it is written in configuration.
    ///
    /// For logs and for `config.describe`. Matches the serde tag, which a test asserts.
    pub fn driver(&self) -> &'static str {
        match self {
            KvStorageConfig::Memory(_) => "memory",
            #[cfg(feature = "redis")]
            KvStorageConfig::Redis(_) => "redis",
            KvStorageConfig::File(_) => "file",
        }
    }
}

/// `[lock]`: which driver coordinates resource locks, and that driver's own settings.
///
/// # Why this is one table and not a `[lock.<name>]` map
///
/// The shape KV has, and the reason it does not apply here: see [`Config::lock`]. A lock is named by its
/// resource, so two tables would be two namespaces that do not see each other.
///
/// # Why a tagged enum, as `KvStorageConfig` is
///
/// Same argument as that one's own documentation: RR's `driver = "redis"` plus a free-form `config` table
/// means a misspelled key parses, is ignored, and leaves an operator with a client pointed at a default
/// and nothing in the log. Here every unknown key is a parse error naming the key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "driver", rename_all = "snake_case", deny_unknown_fields)]
pub enum LockConfig {
    /// Locks held in this process. The default, and the one that needs nothing installed.
    Memory(MemoryLockConfig),

    /// Locks held in Redis, and therefore shared between processes and across a restart of this one.
    ///
    /// Behind the `redis` cargo feature, so a build without it refuses this driver at *parse* time rather
    /// than accepting a table whose backend cannot be built.
    #[cfg(feature = "redis")]
    Redis(RedisLockConfig),
}

/// `[lock] driver = "memory"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryLockConfig {}

/// `[lock] driver = "redis"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisLockConfig {
    /// The server, as a URL: `redis://127.0.0.1:6379`, optionally with a username, password and database.
    ///
    /// The same key, the same shape and the same validation as `[kv.<name>]`'s `url`, deliberately: an
    /// operator pointing two features at one server should be able to copy a line, and a second URL
    /// spelling for one client is a second thing to get wrong. `config.describe` redacts it for the
    /// same reason it redacts the KV one — it may carry a credential.
    pub url: String,

    /// How many connections to keep.
    ///
    /// One per worker is the natural size, since a call blocks the worker that asked.
    #[serde(default = "default_redis_pool_size")]
    pub pool_size: usize,

    /// How long one command may take before it is considered failed.
    ///
    /// **The deadline that turns a hung Redis into an `Unknown` rather than a stuck worker**, and the
    /// reason it is configuration rather than a constant: a deployment with a replicating Redis over a
    /// slow link needs a longer one, and the failure mode of too short a value is a lock whose state is
    /// permanently unknown to its holder.
    ///
    /// The client's own default when absent. Millisecond-resolution because the value ends up as a socket
    /// option, and a whole-second granularity would make sub-second deadlines unexpressible.
    #[serde(default = "default_lock_read_timeout_ms")]
    pub read_timeout_ms: u64,
}

/// How many connections a Redis client holds unless configured otherwise.
///
/// One per worker is the natural size for every Redis driver here — the KV store's, the lock backend's and
/// a queue's — and the reason is the same in all three: each operation blocks the caller that issued it,
/// so a pool smaller than the number of concurrent callers is a pool that queues behind itself.
pub(super) fn default_redis_pool_size() -> usize {
    16
}

fn default_lock_read_timeout_ms() -> u64 {
    // Five seconds, which is what RoadRunner's own Redis lock configuration defaults to and what the
    // worker protocol's request timeout already is — so a lock command cannot outlive the request that
    // issued it by much, and a worker killed for exceeding its deadline is not killed while a lock
    // command it cannot see the end of is still running.
    5000
}

impl LockConfig {
    /// The driver's name, as it is written in configuration.
    pub fn driver(&self) -> &'static str {
        match self {
            LockConfig::Memory(_) => "memory",
            #[cfg(feature = "redis")]
            LockConfig::Redis(_) => "redis",
        }
    }
}

/// `[jobs]`: the queues, and which of them are consumed here.
///
/// # Why a table of pipelines where `[kv.*]` had to be named and `[lock]` had to be one
///
/// All three shapes are in this file and the difference is what the *thing* is. A KV storage is named
/// because a worker has to say which cache it means. A lock is not, because a lock is named by its
/// resource and two lock backends would not see each other. A queue **is** named, and for a reason
/// neither of the others has: a job's pipeline is part of what the job *is* — it is the choice of which
/// consumer runs it — so a producer selects one by name on every push.
///
/// # The `consume` list, and why it is not "consume everything"
///
/// A pipeline absent from `consume` is **producer-only**: it is configured, can be pushed to, and is not
/// polled by this server. That is RoadRunner's rule and the reason is a deployment one — a web node that
/// pushes mail jobs and a worker node that consumes them run the same configuration file, and the only
/// difference between them is one line here. Consuming by default would make every web node also a
/// consumer, which is how a job gets handled twice by two servers that both meant to be producers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobsConfig {
    /// Pipelines this server **polls**. Empty means it consumes nothing and only produces.
    ///
    /// Checked against `pipelines` at validation: a name here that is not a pipeline is a typo, and the
    /// symptom without that check is a consumer that starts, finds nothing, and reports no error — the
    /// same silent failure the `services` allow-list exists to prevent, one layer up.
    #[serde(default)]
    pub consume: Vec<String>,

    /// The pipelines, by name, each with its own driver.
    #[serde(default)]
    pub pipelines: BTreeMap<String, PipelineConfig>,

    /// The consumers' worker pool: command, worker count, recycling — every knob a backend has.
    ///
    /// **Required when `consume` is non-empty**, and refused rather than defaulted when it is: there is no
    /// sensible command to default to, and a consumer configured without one would be a server that
    /// started, could not start a worker, and reported a pool of zero.
    ///
    /// It is a [`JobsPoolConfig`] rather than a [`WorkerConfig`] because two of that type's fields mean
    /// something different here — see that type's own documentation — and one is absent entirely.
    #[serde(default)]
    pub pool: Option<JobsPoolConfig>,
}

/// One `[jobs.pipelines.<name>]`: which driver, and that driver's own settings.
///
/// A tagged enum, like [`KvStorageConfig`] and [`LockConfig`], so that a misspelled key is a parse error
/// naming it rather than a key that parses, is ignored, and leaves an operator with a queue on the wrong
/// driver and nothing in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "driver", rename_all = "snake_case", deny_unknown_fields)]
pub enum PipelineConfig {
    /// Jobs in this process's memory. Lost on restart — see the driver's documentation.
    Memory(MemoryPipelineConfig),

    /// Jobs in Redis, shared between processes and surviving a restart of this one.
    ///
    /// The honest multi-node default, and the same client the KV store and the lock backend use. Its
    /// `pool_size` and `read_timeout_ms` are the lock backend's settings under the same names, because
    /// the constraint is the same one: a command that blocks a poller.
    #[cfg(feature = "redis")]
    Redis(RedisPipelineConfig),
}

/// `[jobs.pipelines.<name>] driver = "memory"`.
///
/// Every memory pipeline is its own queue — the driver is built per pipeline — so there is nothing to
/// configure here. `driver` is the whole table, and the type exists so that a second key is a parse error
/// naming it rather than a key that is accepted and ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPipelineConfig {}

/// `[jobs.pipelines.<name>] driver = "redis"`.
#[cfg(feature = "redis")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisPipelineConfig {
    /// The Redis URL, e.g. `redis://127.0.0.1:6379`.
    pub url: String,

    /// How many connections to hold. One per worker is the natural size, and the driver takes them
    /// round-robin.
    #[serde(default = "default_redis_pool_size")]
    pub pool_size: usize,

    /// How long one command may take before it is treated as failed, in milliseconds.
    ///
    /// Defaults to the same five seconds the lock backend uses, and for the same reason: a driver command
    /// is on a *poll* path, where this bounds how long a stalled poller stalls rather than how long a
    /// request may take.
    #[serde(default = "default_lock_read_timeout_ms")]
    pub read_timeout_ms: u64,
}

impl PipelineConfig {
    /// The driver's name, as it is written in configuration.
    ///
    /// An associated function rather than a field, because the tag *is* the driver: a value that could
    /// disagree with the tag it parsed from would be a `config.describe` that lied about what is running.
    pub fn driver(&self) -> &'static str {
        match self {
            PipelineConfig::Memory(_) => "memory",
            #[cfg(feature = "redis")]
            PipelineConfig::Redis(_) => "redis",
        }
    }
}

/// `[jobs.pool]`: the consumers' worker settings.
///
/// # Why this is not a [`WorkerConfig`]
///
/// It has that type's shape and three deliberate differences, each of which would be a silent
/// misconfiguration if the field were inherited:
///
/// - **`services` is absent.** A consumer's channel carries `JOB`, `ACK` and `NACK`, not `CALL`, so there
///   is nothing for a `services` list to grant. Accepting the key would advertise a call surface a
///   consumer does not have — the shape `BackendConfig::External` refuses for the same reason.
/// - **`max_requests` is absent**, which is a *renaming* rather than an omission: RoadRunner's jobs pool
///   calls this `max_jobs`, and it means "recycle a worker after this many jobs". `max_requests` on this
///   table would be a request count on a pool that serves no requests, and copying it across would make
///   an operator's `max_requests` mean something it does not.
/// - **`command` is required**, where a backend's is too — but the failure without it is different here,
///   because a jobs pool with no command is a server that consumes a queue with nobody to hand jobs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobsPoolConfig {
    /// Command line that starts a consumer, e.g. `php consumer.php`.
    pub command: String,

    /// How many consumers to keep. `0` means one per logical CPU, matching [`WorkerConfig::workers`].
    #[serde(default = "default_workers")]
    pub workers: u32,

    /// Recycle a consumer once its resident set exceeds this many megabytes.
    #[serde(default)]
    pub memory_limit_mb: Option<u64>,

    /// Whether the per-frame `crc32c` is verified.
    ///
    /// The same negotiation a backend offers, and here for the same reason it exists there: a
    /// consumer's channel is the same channel, so a deployment that accepted the trade for its
    /// request workers has already accepted it for its consumers. Defaulting to the *same*
    /// [`ChecksumMode`] is what keeps that one decision rather than two.
    #[serde(default)]
    pub checksum: ChecksumMode,

    /// How the server and a consumer are connected.
    ///
    /// Likewise inherited rather than re-decided: the socket transport exists because PHP's pipe
    /// reads are slow on Windows, and a consumer process reads its pipe exactly as a request worker
    /// does. Leaving it out would have made the default — a socket on Windows — *unavailable* to
    /// consumers, so the one platform that needs it would be the one that could not have it.
    #[serde(default)]
    pub transport: WorkerTransport,

    /// Where the server listens for the socket transport, when this consumer is not on a pipe.
    ///
    /// The same key, with the same meaning, as [`WorkerConfig::socket`]: a `unix:` path prefix
    /// with a per-consumer suffix appended. Kept in step deliberately, because the transport
    /// default is shared and a consumer that could not say where to listen would be forced onto
    /// the pipe on the platform where the pipe is the slow one.
    #[serde(default)]
    pub socket: Option<String>,

    /// Working directory for the consumer process.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,

    /// Extra environment variables for the consumer process.
    ///
    /// Worth having on a consumer and not only on a backend: a consumer is the process that talks to
    /// the application's *queue*, so a deployment that runs its consumers against a different database
    /// or broker than its web nodes is the ordinary case rather than an exotic one.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Recycle a consumer after this many **jobs**.
    ///
    /// `0` means never, the same rule [`WorkerConfig::max_requests`] documents at length. Named `max_jobs`
    /// because that is what RoadRunner's jobs pool calls it and because a request count is not a thing
    /// this pool has.
    #[serde(default)]
    pub max_jobs: Option<u64>,

    /// How long a consumer's reservation of a job lasts, in seconds.
    ///
    /// **The one setting that must be longer than a job takes**, and the reason it is configuration rather
    /// than a constant: a reservation that expires while a job is still running means a second consumer
    /// picks it up and the work happens twice. RoadRunner's equivalent is the pipeline's `timeout` plus the
    /// broker's own visibility timeout; here it is one number, and the documentation says plainly which
    /// direction is safe to be wrong in.
    ///
    /// Defaults to 300 seconds. A deployment whose jobs take longer must raise it — there is no way to
    /// auto-tune it, because the server cannot know how long an application's work takes.
    #[serde(default = "default_job_lease_seconds")]
    pub job_lease_seconds: u64,

    /// How many jobs to ask a pipeline for at once.
    ///
    /// One is the honest default for a *worker* pool, because a consumer that reserves five jobs and then
    /// handles them one at a time holds four of them idle — and their leases are running down while it
    /// does. Raising it is worth doing only for a consumer that handles them concurrently, which this pool
    /// does not: one consumer, one job at a time.
    #[serde(default = "default_job_prefetch")]
    pub prefetch: usize,
}

fn default_job_lease_seconds() -> u64 {
    // Five minutes: long enough for anything an HTTP request would not have been allowed to do, short
    // enough that a consumer which died does not leave work stuck for an hour. Deliberately *not* the
    // request timeout, because the work is deliberately different work.
    300
}

fn default_job_prefetch() -> usize {
    1
}

impl JobsConfig {
    /// Whether this server polls the named pipeline.
    pub fn consumes(&self, pipeline: &str) -> bool {
        self.consume.iter().any(|name| name == pipeline)
    }
}

/// The Redis URL and pool rules, applied identically to every table that names a Redis.
///
/// **One function, because there are now two tables and there will be three** — a jobs driver wants
/// the same client and the same rules. A second copy of these checks is a second place for the
/// credential rule to be written slightly differently — and the credential rule is the one where a
/// divergence is a password on the wire in the clear.
#[cfg(feature = "redis")]
pub(super) fn validate_redis(
    what: &str,
    url: &str,
    pool_size: usize,
) -> std::result::Result<(), Error> {
    // Checked for the shape a client will accept, not by connecting: `volucer check` must not require the
    // server to be up, and a URL that parses is all the configuration can vouch for. A server that is not
    // there is reported by `serve` when it connects.
    if !url.starts_with("redis://") && !url.starts_with("rediss://") {
        return Err(Error::Config(format!(
            "{what}.url {url:?} is not a Redis URL; it must start with \"redis://\" or \"rediss://\", \
             e.g. \"redis://127.0.0.1:6379\""
        )));
    }
    if url.contains('@') && !url.contains("rediss://") {
        // A password over a plaintext connection. An error rather than a warning: a *configuration* is
        // the one place where nobody is watching a log at the moment the decision is made, and
        // `rediss://` is the fix. The message never repeats the URL, because the URL is the credential.
        return Err(Error::Config(format!(
            "{what}.url carries a credential (it has an \"@\") over a plaintext connection; use \
             \"rediss://\" to encrypt it, or accept that the password crosses the network in the clear \
             by making the connection local"
        )));
    }
    if pool_size == 0 {
        return Err(Error::Config(format!(
            "{what}.pool_size is 0, so no call could ever be served; remove the key for the {default} \
             default",
            default = default_redis_pool_size()
        )));
    }
    Ok(())
}

impl Config {
    /// Whether a name in a backend's `services` list is one this configuration can honour.
    ///
    /// Two shapes, and the second is the one worth explaining: a fixed namespace is always
    /// available (the server's own machinery, [`CALL_NAMESPACES`]), while a KV storage is
    /// **named** — `"kv.sessions"` — because a deployment may have several and a worker has to say
    /// which. That is checked against this file's own `[kv.*]` tables rather than against a list of
    /// drivers, so `services = ["kv.sessions"]` with no `[kv.sessions]` table is refused at startup
    /// instead of becoming a worker that boots, serves traffic, and cannot reach a cache it was
    /// told it had.
    ///
    /// A bare `"kv"` is refused for that reason rather than silently resolving to "the only one":
    /// a deployment with a second storage added later would find the first worker's calls quietly
    /// going somewhere else.
    ///
    /// A lock backend is the third shape, and it is the fixed half: `[lock]` is a single table, so
    /// `"lock"` is a name that means the same thing to every worker. What it *is* checked against is the
    /// presence of the table, which is why it is not in [`CALL_NAMESPACES`] — a backend granted `"lock"`
    /// with no `[lock]` is a worker that boots, serves traffic, and cannot reach a lock it was told it
    /// had, which is exactly the silent failure the KV case is checked for.
    pub fn is_a_call_service(&self, service: &str) -> bool {
        if CALL_NAMESPACES.contains(&service) {
            return true;
        }
        if service == "lock" {
            return self.lock.is_some();
        }
        // `jobs` is a fixed name — a producer names the *pipeline* in the call, not the service — so it is
        // checked against the table's presence for the same reason `lock` is: a worker granted a queue the
        // deployment never configured would boot, serve traffic, and be unable to push anything.
        if service == "jobs" {
            return self.jobs.is_some();
        }
        match service.strip_prefix("kv.") {
            Some(storage) => self.kv.contains_key(storage),
            None => false,
        }
    }
}
