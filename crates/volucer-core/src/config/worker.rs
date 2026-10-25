//! The `worker` backend: a pool of application processes this server drives.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    /// Command line that starts the persistent worker, e.g. `php worker.php`.
    pub command: String,

    /// Number of worker processes. `0` means one per logical CPU.
    #[serde(default = "default_workers")]
    pub workers: u32,

    /// Recycle a worker once its resident set exceeds this many megabytes.
    #[serde(default)]
    pub memory_limit_mb: Option<u64>,

    /// Recycle a worker after this many requests.
    ///
    /// **`0` means "never recycle"**, matching php-fpm's `pm.max_requests = 0` and
    /// RoadRunner's `max_jobs: 0`. That is the opposite of what a bare
    /// `served >= max` comparison would do, and the difference is not academic: a
    /// php-fpm pool config carrying `pm.max_requests = 0` is the common production
    /// case, and copying it here used to destroy the worker after every request —
    /// one application boot per request, which is the deployment model this backend
    /// exists to avoid. Absent and zero therefore both mean "no limit".
    #[serde(default)]
    pub max_requests: Option<u64>,

    /// Working directory for the worker process.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,

    /// Extra environment variables for the worker process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Which control namespaces this backend's workers may call.
    ///
    /// **Absent means none, and that is the whole point of the key.** A worker's channel is a
    /// remote-execution surface, and the only thing protecting it
    /// is which methods the server agreed to answer. So a backend that faces the public gets a
    /// registry without `workers`, and therefore cannot restart its siblings, while an operator's
    /// worker gets one that can. A process-wide registry would give every worker every
    /// capability, which is the shape the design refuses.
    ///
    /// Named rather than spelled as method names: the unit of capability here is the namespace,
    /// because that is the unit a handler is written and reviewed as, and because `["kv"]` is a
    /// one-line diff where a list of verbs is a diff per verb that no reviewer reads.
    ///
    /// Only a `worker` backend has this key at all: the other three have no channel to install a
    /// handler on, and their configuration tables refuse it rather than accept a key that
    /// advertises a surface which does not exist.
    #[serde(default)]
    pub services: Vec<String>,

    /// Which gRPC services this backend's workers may be asked to handle.
    ///
    /// **The opposite direction from `services`, and that is why it is a second key.** `services`
    /// decides what the application may *call*; this decides what may be *delivered* to it. A
    /// gRPC call arriving at `/greeter.Greeter/SayHello` is matched against this list by service
    /// name, and a request naming a service that is not here never reaches a worker: it is answered
    /// `UNIMPLEMENTED` by the server itself, which is what a gRPC client expects from a server that
    /// does not implement a method.
    ///
    /// **Absent means none**, exactly as for `services`, and for the same reason: the list is the
    /// whole of what a route may reach, so the safe default has to be the empty one. A backend that
    /// names no gRPC service answers `UNIMPLEMENTED` to every gRPC call, which is also the right
    /// answer for a worker that was never meant to speak gRPC at all — including a plain PHP
    /// application that a misconfigured client happens to send `content-type: application/grpc` to.
    ///
    /// Only a `worker` or an `attach` backend has the key. Both have a channel the server agreed to
    /// install a handler on and a process that outlives a request; `external` and `spawn_cgi` reset
    /// per request and have no such contract, and an `upstream` speaks gRPC itself and needs no
    /// permission from here — the gRPC tiers are: the `upstream` backend relays to an origin that
    /// already speaks gRPC, and the `worker` backend is the application serving the service itself.
    ///
    /// **Service names, not paths and not methods.** `"greeter.Greeter"` is a service; a method
    /// underneath it (`SayHello`) is the application's business, because a method the application
    /// does not implement is a method it can answer `UNIMPLEMENTED` to itself. Listing methods here
    /// would mean editing the server's configuration on every application deploy, which is the
    /// coupling the whole design exists to avoid.
    #[serde(default)]
    pub grpc_services: Vec<String>,

    /// Serve **any** gRPC service this backend's workers are sent, without naming them.
    ///
    /// The opt-out from [`WorkerConfig::grpc_services`], and it is a *deliberate* one rather than
    /// the lack of a list: absent `grpc_services` grants nothing, which is the safe default, so
    /// "serve whatever arrives" has to be something an operator wrote down.
    ///
    /// **This gives up the boundary the list exists for.** `grpc_services` is checked before the
    /// request body is read, so a caller cannot spend a body copy or a worker slot on a service the
    /// deployment does not serve; with this set, every well-formed `/pkg.Service/Method` is delivered
    /// and the application answers `UNIMPLEMENTED` itself after decoding. It is right for a backend
    /// whose service set is genuinely dynamic — one that is discovered at runtime rather than
    /// written down — and [`Self::grpc_proto`] is the answer for the much more common case of a set
    /// that is large or churning but *declared* somewhere.
    ///
    /// It is legal **only on a `worker` or `attach` backend**, and only when [`Self::grpc_services`]
    /// and [`Self::grpc_proto`] are both empty: two statements about one thing that can disagree is
    /// refused everywhere else in this file. A startup warning names the backend, so a
    /// public-facing worker granted everything says so in the log rather than only in the file.
    #[serde(default)]
    pub grpc_serve_any: bool,

    /// Read the gRPC services from `.proto` files instead of naming them.
    ///
    /// **The same grant as [`Self::grpc_services`], from the source that cannot go stale.** A
    /// deployment whose application already has `.proto` files has exactly one reliable statement of
    /// which services exist, and a hand-maintained copy of it in this file is a second one that will
    /// eventually disagree — in the silent direction, because a service that was added and not
    /// listed is answered `UNIMPLEMENTED` with no log line saying why.
    ///
    /// Each entry is a `.proto` file or a directory that is searched recursively, resolved against
    /// the directory holding this configuration file (the rule `[server.tls] certificate` and
    /// [`Config::kv`]'s paths already follow). Only the `package` and the top-level `service`
    /// declarations are read: an `import` is not followed, because a service's name depends on
    /// nothing an import resolves.
    ///
    /// **Read when the configuration is loaded**, which is what makes it reloadable: the resolved
    /// names become [`Self::grpc_services`] before anything is served, so a `SIGHUP` re-reads the
    /// files and a changed set takes effect on the next request with no restart. A file that cannot
    /// be read, or a set that resolves to no service at all, refuses the load — the same
    /// whole-or-nothing rule the rest of the file follows, because a backend silently granted
    /// nothing is the failure this key exists to remove.
    ///
    /// It is **mutually exclusive** with [`Self::grpc_services`] and with
    /// [`Self::grpc_serve_any`]: they are three statements about one set, and every way of resolving
    /// them silently ignores one. `volucer check` prints the files and the names they produced, so a
    /// directory that brings in more services than intended is visible rather than inferred.
    #[serde(default)]
    pub grpc_proto: Vec<PathBuf>,

    /// The names [`Self::grpc_proto`] resolved, or empty.
    ///
    /// **Not a statement in the file, and deliberately not deserializable**: `skip` means a file that
    /// names this key is refused as an unknown field, so the three ways to say which services this
    /// backend serves stay three. This is the *result* of one of them.
    ///
    /// It is a separate field rather than writing the resolved names back into
    /// [`Self::grpc_services`] because those two keys are mutually exclusive: an expansion that
    /// filled the list would produce a configuration its own validator refuses, which is a trap
    /// whose only symptom would be a `check` on an already-loaded file.
    #[serde(skip)]
    pub grpc_proto_services: Vec<String>,

    /// Whether the per-frame `crc32c` is verified.
    ///
    /// Defaults to [`ChecksumMode::Required`], which is what every volucer release
    /// before this option did, so a configuration that does not mention it is unchanged on
    /// the wire. Setting `optional` lets a worker that also agrees omit the checksum; the
    /// negotiation is in the handshake, so a worker that does not understand the offer
    /// simply keeps checksumming — see [`ChecksumMode`].
    #[serde(default)]
    pub checksum: ChecksumMode,
    // `services` is declared above, before the transport docs, because it is the key an
    // operator reads first: it is the one that decides what the application may ask for.
    /// How the server and a worker are connected.

    /// How the server and a worker are connected.
    ///
    /// Defaults to the platform's better transport: [`WorkerTransport::Socket`] on Windows
    /// and [`WorkerTransport::Pipe`] everywhere else. The split is measured rather than
    /// assumed. PHP's stream layer takes **~8 ms median (15 ms p95)** to return from a
    /// blocking read on a Windows pipe, against **~0.09 ms** for the same read on a socket;
    /// on a request path that was **21× on the whole `/items` route** (15.24 ms vs 0.72 ms
    /// median, 9 runs of 1000 requests each). A Rust process reading the identical pipe
    /// returns in 0.010 ms, and RoadRunner — a different server with a different protocol —
    /// measured the same 15 ms, so the cost is PHP's pipe read on Windows rather than
    /// anything volucer does. On Linux the ordering is the other way round and much
    /// smaller, so the pipe stays the default there.
    #[serde(default)]
    pub transport: WorkerTransport,

    /// Where the server listens for the [`WorkerTransport::Socket`] transport: a unix
    /// socket path **prefix**, not a finished path.
    ///
    /// **`transport` decides the kind of transport; this decides the address.** Omitting it
    /// keeps the loopback ephemeral port this backend has always used, so no existing
    /// configuration changes meaning — which is why this is a second key rather than a third
    /// [`WorkerTransport`] variant. A variant would have had to describe an address too, and
    /// `socket = "unix:/run/x"` already reads the same way here as it does for `external`,
    /// `attach` and `spawn_cgi`.
    ///
    /// The value is a prefix because the pool appends `.<slot>.sock`, for the reason
    /// [`SpawnCgiConfig::socket`] gives at length: one socket per worker is what lets an
    /// individual worker be reached, recycled and reasoned about on its own. A single shared
    /// socket would be one queue, and `workers` would stop meaning anything.
    ///
    /// `unix:` is required and a bare path is refused. A bare path is a *relative* path, so
    /// the same string copied from an `external` backend's `socket` would bind a file
    /// literally named `unix:/run/php/php8.3-fpm.sock` in the working directory rather than
    /// failing — the one mistake this key exists to make impossible.
    ///
    /// Refused beside [`WorkerTransport::Pipe`] rather than ignored: a key that is read by
    /// nothing looks supported and is not, which is why `spawn_fpm` was removed.
    #[serde(default)]
    pub socket: Option<String>,
}

/// How a worker process is connected to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTransport {
    /// The worker's own `stdin`/`stdout`. Portable, needs nothing from the worker, and
    /// the fastest option on Linux and macOS.
    Pipe,
    /// A loopback TCP socket, for Windows, where PHP's pipe reads are slow. The server
    /// listens on an ephemeral loopback port and passes it to the worker as
    /// `VOLUCER_WORKER_SOCKET`; the worker connects back. The transport is
    /// indistinguishable to the protocol, which is why nothing above this setting changes.
    Socket,
}

impl Default for WorkerTransport {
    /// Windows gets a socket, everything else gets a pipe.
    ///
    /// A hand-written `Default` rather than `#[default]` on a variant, because the better
    /// transport is a property of the platform and not of the enum. Getting this wrong is
    /// expensive in the quiet way: a Windows deployment that leaves the setting out used to
    /// pay 15 ms of scheduler latency per request and look healthy doing it — every request
    /// answered, every status 200, no log line anywhere.
    fn default() -> Self {
        if cfg!(windows) {
            WorkerTransport::Socket
        } else {
            WorkerTransport::Pipe
        }
    }
}

impl WorkerTransport {
    /// True when the transport is the worker's own stdio.
    pub fn is_pipe(self) -> bool {
        matches!(self, WorkerTransport::Pipe)
    }

    /// The name as it is written in configuration, for logs and diagnostics.
    ///
    /// Worth logging even though it is usually the default: the transport is invisible in
    /// the protocol, so a deployment that ends up on the wrong one behaves identically
    /// except for how fast it is.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerTransport::Pipe => "pipe",
            WorkerTransport::Socket => "socket",
        }
    }
}

impl std::fmt::Display for WorkerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The environment variable carrying the address a socket-transport worker must connect to.
///
/// The server listens before starting the worker, so the address is known up front and the
/// worker is told rather than having to discover it.
pub const WORKER_SOCKET_ENV: &str = "VOLUCER_WORKER_SOCKET";

/// The environment variable naming the relay an `attach` backend dials, for a supervisor that
/// would rather read it than take it as an argument.
///
/// **The one address both halves of an `attach` deployment must agree on, and until this existed
/// they agreed by being written twice.** `VOLUCER_WORKER_SOCKET` cannot do this job: there is no
/// spawned child to inherit it, so a supervisor that read it would be reading whatever an
/// enclosing environment happened to leave there. This is the variable the *operator* sets, in
/// the unit file or the container's environment, and both the server and the SDK read it — so
/// `relay` may be omitted from the configuration entirely and the supervisor needs no argument.
///
/// Named `_SOCKET` to match [`WORKER_SOCKET_ENV`], and it is a naming convention rather than a
/// claim about the transport: a relay may legitimately be `host:port` for a supervisor in another
/// container, and that variable's own value is a TCP address too.
pub const RELAY_SOCKET_ENV: &str = "VOLUCER_RELAY_SOCKET";

/// The environment variable carrying the address a `spawn_cgi` slot must bind, and the
/// placeholder a slot's `command` may use in place of it.
///
/// A slot's address is decided by the pool — one per process, so that a process can be
/// recycled, counted and reasoned about on its own — which means the program cannot choose it
/// and has to be told. Two forms, because both exist in the wild: `command = "flup-fcgi
/// {address}"` for a program that takes it as an argument, and this variable for one that reads
/// its bind address from the environment.
///
/// The same value is in both, so a deployment may use either, or both. Both are written *after*
/// a configured `env` table, so a table cannot point the program at an address the pool is not
/// connecting to.
pub const FCGI_ADDRESS_ENV: &str = "VOLUCER_FCGI_ADDRESS";

/// The placeholder a `spawn_cgi` `command` uses for the address its slot must bind.
pub const FCGI_ADDRESS_PLACEHOLDER: &str = "{address}";

/// The placeholder a `spawn_cgi` `command` uses for the number of requests its slot may serve at
/// once — the resolved `concurrency`, which is the same number the pool hands that address as
/// permits.
///
/// **Here because the same number otherwise has to be written twice.** A program that forks its
/// own workers is told how many by the same `command` string that is told the address, so
/// `command = "gunicorn app.wsgi --workers 4 --bind {address}"` beside `concurrency = 4` states
/// one fact in two places with nothing checking that they agree. Raising `concurrency` to `8`
/// and leaving the `--workers 4` behind is a pool that holds eight connections against a program
/// with four workers, where the surplus waits in the socket backlog and nothing counts it. With
/// `--workers {concurrency}` there is one statement and nothing to drift.
///
/// **Substituted with the resolved value, not the configured one**, because `concurrency = 0`
/// means one: a program handed `0` where it expects a worker count is a program that starts
/// nothing, and the pool would be admitting one request to it. The substituted number is
/// [`crate::config::SpawnCgiConfig::slot_concurrency`], so it is by construction the number of
/// permits the pool holds for that address.
///
/// **`{workers}` deliberately has no counterpart.** A slot's command is started once per slot
/// with its own address, so "how many siblings do you have" is a fact about a different layer,
/// and a program told it would be invited to divide work or ports the pool has already divided.
/// It is refused by name rather than passed through as a literal, because a command line carrying
/// the text `{workers}` fails inside the program with a message that names neither volucer nor
/// the key.
pub const FCGI_CONCURRENCY_PLACEHOLDER: &str = "{concurrency}";

/// The placeholder that deliberately does **not** exist, named so it can be refused with an
/// explanation instead of reaching the program.
///
/// A slot's command is started once per slot with its own address, so the number of slots is not
/// a fact about that process — it is a fact about how many of it volucer starts. A program told
/// `{workers}` would be invited to divide work, ports or a shared resource by a number the pool
/// has already divided, which is the failure this placeholder's absence prevents. Left to the
/// program, the text arrives as an argument, its argument parser rejects it (or reads it as a
/// name), and the operator sees an error that names neither volucer nor `workers`.
pub const WORKERS_PLACEHOLDER: &str = "{workers}";

/// The call namespaces a worker backend may be granted, and the fixed half of the allow-list
/// `WorkerConfig::services` is checked against.
///
/// **An allow-list rather than "anything the registry will accept", because a misspelled
/// namespace must be a startup failure rather than a silent refusal at call time.** A worker
/// granted `["workes"]` has no capability at all, and the only symptom would be an application
/// that reports "the server refused the call" — which reads as a server bug, not a typo. The
/// same argument `#[serde(deny_unknown_fields)]` makes for a misspelled *key* applies to a
/// misspelled *value* here.
///
/// The list is the namespaces `volucer-server` implements that need no configuration of their own,
/// and it lives in `volucer-core` because validation does. A service that *is* configuration — a KV
/// storage, whose name is chosen by the deployment — is not here: it is checked against the tables
/// in the same file, by [`Config::is_a_call_service`]. Adding a fixed service means adding a name
/// here, which is the one place a reviewer has to look to see the whole surface a worker can reach.
pub const CALL_NAMESPACES: &[&str] = &[
    // The deployment's own description, redacted. The pools: `workers.list` and `worker.reset`.
    "config", "workers",
];

/// How strictly the worker protocol's per-frame `crc32c` is enforced.
///
/// A configuration-level mirror of the protocol's `Checksum` policy. It lives here
/// rather than importing the worker crate's enum so that the configuration model stays
/// free of the protocol crate, which is what lets `volucer-core` be the leaf that every
/// other crate depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumMode {
    /// Every frame must carry a verified checksum, and one that claims otherwise is
    /// refused. The default, and the only mode an older worker can be driven in.
    #[default]
    Required,
    /// The handshake may agree to omit the checksum. A frame that still carries one is
    /// verified; only a frame that declares it has none is read without hashing it.
    Optional,
}

impl ChecksumMode {
    /// True when the checksum is enforced, for logging and tests.
    pub fn is_required(self) -> bool {
        matches!(self, ChecksumMode::Required)
    }
}
