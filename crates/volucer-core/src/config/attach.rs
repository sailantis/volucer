//! The two backends volucer does not start: `external`, which names an address, and `attach`,
//! which dials a relay a supervisor owns.

use super::*;

/// An already-running FastCGI server — php-fpm, or a preforking `php-cgi`.
///
/// **There is no pool size to discover and none to enforce**, which is what this type's shape
/// says. The pool is somebody else's: its size lives in `pm.max_children` or `PHP_FCGI_CHILDREN`,
/// in configuration volucer has no access to.
///
/// `FCGI_GET_VALUES` is the protocol's capacity channel and it is worth knowing what each server
/// does with it, because the two PHP ones disagree — measured, not assumed:
///
/// | server | asked for `FCGI_MAX_CONNS` / `FCGI_MAX_REQS` |
/// | --- | --- |
/// | preforking `php-cgi` | answered, and correct: `4` for `PHP_FCGI_CHILDREN=4` |
/// | `php-fpm` | answered with **nothing** — a present but empty result |
///
/// Nor does either *enforce* the figure: a `php-cgi` advertising 4 served 32 simultaneous requests
/// across exactly 4 children with zero refusals, queuing the excess in the children's backlog. So
/// the number is a hint about the pool's width, never a limit a client is held to.
///
/// Which is why volucer does not query it at runtime. There is a `get_values` client and it is
/// deliberately not wired into `serve`: a discovery round trip on the accept path, against a
/// server that answers `max_connections` with an empty result and needs 1.5 s after start before
/// it answers anything at all, would trade a reliable queue for an unreliable gauge. It is
/// available in `volucer check`, where a human is waiting, and only on request.
///
/// What volucer does instead is [`ExternalConfig::max_connections`]: an operator-supplied ceiling,
/// off by default, for the case where a misbehaving front-end would otherwise pile up connections
/// faster than the pool can drain them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalConfig {
    /// php-fpm listener: `unix:/run/php/php8.3-fpm.sock` or `127.0.0.1:9000`.
    ///
    /// Also accepts a preforking `php-cgi -b`, which is the same FastCGI protocol and the same
    /// fork model as php-fpm. It is reachable this way with no extra mode; what it does not offer
    /// is the warm channel, so it measures like `spawn_cgi` rather than like `worker`.
    pub socket: String,

    /// How many requests may be in flight against this backend at once. **Unset means no cap.**
    ///
    /// This is a *back-pressure* control, not a pool-size declaration: volucer still does not know
    /// how many children the far side has, and setting this above that number changes nothing.
    /// Setting it below is what makes it useful — a request beyond the cap waits for a slot
    /// instead of opening another connection, which turns an unbounded pile-up into a queue the
    /// front-end's own limits can drain.
    ///
    /// Unset is the default because the far side already queues: a `php-cgi` advertising 4 served
    /// 32 simultaneous requests across exactly 4 children with no refusals. A cap therefore adds a
    /// wait without removing anything a correct deployment would notice, and on a *wrong* one (a
    /// front-end opening thousands of connections) it is the only thing that helps.
    ///
    /// `0` is refused rather than read as "no cap". `Option::None` already means that, and a zero
    /// here would otherwise be a backend that can never serve a request, discovered on the first
    /// one rather than at startup.
    #[serde(default)]
    pub max_connections: Option<usize>,
}

/// A worker pool volucer connects to rather than starts.
///
/// See [`BackendConfig::Attach`] for why this exists. The short version: it is the only shape
/// that lets a PHP process `fork()` its workers — and therefore share one OPcache and one booted
/// container instead of N — while keeping volucer's warm framed protocol rather than dropping to
/// FastCGI per request.
///
/// **The workers' own configuration is not here.** A supervisor that forked its children has
/// already decided their count, their command line and their environment; repeating any of it
/// would be a number volucer prints that nothing enforces. What is here is only what volucer
/// genuinely owns: where to connect, how many connections to hold, when to recycle one, and what
/// the attached workers may call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachConfig {
    /// The relay the attached workers are reached on, e.g. `unix:/run/app/workers.sock`.
    ///
    /// **A path, not an address list.** The supervisor listens; volucer dials it `workers` times
    /// and holds each connection for the life of a slot. `unix:` is the form that matters — an
    /// `AF_UNIX` socket has no Nagle/delayed-ACK interaction, and a loopback TCP relay measured
    /// **38.96 ms per request against 0.06 ms** for the same protocol until `TCP_NODELAY` was set
    /// on both ends. A `host:port` is accepted because a supervisor may live in another
    /// container, and it is not the documented default for that reason.
    ///
    /// **Omittable, in which case [`RELAY_SOCKET_ENV`] supplies it.** The address is the one fact
    /// the configuration and the supervisor both have to know, and before this they stated it
    /// twice with nothing keeping the two copies equal — a mismatch starts, then fails on the
    /// first request. An environment variable read by both is the one form that lets a deployment
    /// write it once. Writing it here still wins, so a file that names a relay keeps meaning
    /// exactly what it says and a stale variable in the operator's shell cannot move it.
    #[serde(default)]
    pub relay: Option<String>,

    /// How many connections to the relay to hold, which is how many requests run at once.
    ///
    /// **No default, and `0` is refused.** `worker`'s `workers = 0` means "one per logical CPU",
    /// and that is exactly the wrong answer here: the supervisor already forked a specific
    /// number of children, so a volucer-side number that differed from it would either leave
    /// children unused or queue requests behind connections that have no child to answer them.
    /// The honest value is the number the supervisor forked, and only the operator knows it.
    pub workers: u32,

    /// Recycle a slot after this many requests, by closing the connection and dialling again.
    ///
    /// **`0` means never**, as it does for `max_requests` on a `worker` backend and
    /// `pm.max_requests` in php-fpm. Recycling here is cheap — the supervisor's child is already
    /// forked, so a reconnect costs a `connect` and a handshake rather than a framework boot —
    /// which makes it the natural place to bound an application leak the supervisor cannot see.
    #[serde(default)]
    pub max_requests: Option<u64>,

    /// Whether the per-frame `crc32c` is verified. Defaults to required, as for a `worker`.
    #[serde(default)]
    pub checksum: ChecksumMode,

    /// Which control namespaces the attached workers may call.
    ///
    /// The same allow-list and the same reasoning as [`WorkerConfig::services`]: a connection is
    /// a remote-execution surface, and the safe default is the empty one.
    #[serde(default)]
    pub services: Vec<String>,

    /// Which gRPC services the attached workers may be asked to handle.
    ///
    /// The same allow-list and the same reasoning as [`WorkerConfig::grpc_services`].
    #[serde(default)]
    pub grpc_services: Vec<String>,

    /// Serve any gRPC service the attached workers are sent.
    ///
    /// The same opt-out as [`WorkerConfig::grpc_serve_any`], with the same rule that it is refused
    /// beside a non-empty [`Self::grpc_services`].
    #[serde(default)]
    pub grpc_serve_any: bool,

    /// Read the gRPC services from `.proto` files instead of naming them.
    ///
    /// The same key and the same reasoning as [`WorkerConfig::grpc_proto`], including the rule that
    /// it is refused beside both [`Self::grpc_services`] and [`Self::grpc_serve_any`].
    #[serde(default)]
    pub grpc_proto: Vec<PathBuf>,

    /// The names [`Self::grpc_proto`] resolved. See [`WorkerConfig::grpc_proto_services`].
    #[serde(skip)]
    pub grpc_proto_services: Vec<String>,
}

impl AttachConfig {
    /// The relay to connect to: the configured one, or the environment's, or an error saying both
    /// are missing.
    ///
    /// One read per call rather than a `resolve()` step that writes the answer back into the
    /// configuration, because this is what [`validate`] and the pool both call and neither should
    /// have to know which of them ran first. It is also why a *reload* cannot leave a stale
    /// address behind: the pool is built once, and a reload does not rebuild backends.
    ///
    /// The configured value wins over the variable, and the trimming rule is the same on both —
    /// a stray `=` in a unit file produces an empty value, which must mean "unset" rather than
    /// "connect to nothing".
    ///
    /// [`validate`]: AttachConfig::validate
    pub fn relay(&self) -> std::result::Result<Cow<'_, str>, String> {
        if let Some(relay) = self
            .relay
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            return Ok(Cow::Borrowed(relay));
        }

        match std::env::var(RELAY_SOCKET_ENV) {
            Ok(address) if !address.trim().is_empty() => {
                // Owned only in this branch, and only the trimmed copy: the caller builds a `Spec`
                // that outlives this call, and a borrowed slice into the process environment would
                // not. The configured branch borrows, which is the one a test can assert on
                // without touching the environment at all.
                Ok(Cow::Owned(address.trim().to_string()))
            }
            _ => Err(format!(
                "backends.relay is not set, and {RELAY_SOCKET_ENV} is not in the environment; \
                 give the path the supervisor listens on, e.g. \"unix:/run/app/workers.sock\", \
                 or export {RELAY_SOCKET_ENV} to have both sides read one value"
            )),
        }
    }

    /// Refuse a table whose values cannot mean what they look like.
    ///
    /// Checked at configuration load so that a bad relay is a startup error naming the key,
    /// rather than a pool of slots that all fail to connect on the first request.
    pub fn validate(&self, backend: &str) -> std::result::Result<(), String> {
        if self.workers == 0 {
            return Err(format!(
                "backends.{backend}.workers must be at least 1: a supervisor that forked its \
                 children already decided how many there are, and a connection count of zero \
                 would leave every one of them unreachable"
            ));
        }
        // The same parser the `external` and `spawn_cgi` sockets use, because it is the same
        // question: is this a `unix:` path or a `host:port`, and is the path absolute?
        let relay = self
            .relay()
            .map_err(|reason| format!("backends.{backend}: {reason}"))?;
        parse_listener(relay.as_ref()).map_err(|err| format!("backends.{backend}.relay: {err}"))?;

        // The same two checks a `worker` backend's grants get, and duplicated here rather than
        // shared because an `attach` table has no `WorkerConfig` to borrow: a name that is not a
        // protobuf service is a grant that can admit nothing, and `grpc_serve_any` beside a list is
        // two statements about one thing.
        let mut seen = std::collections::BTreeSet::new();
        for service in &self.grpc_services {
            if !is_valid_grpc_service(service) {
                return Err(format!(
                    "backends.{backend}.grpc_services contains {service:?}, which is not a \
                     protobuf service name; a service is identifiers joined by dots, e.g. \
                     \"greeter.Greeter\" — check the name in the application's `.proto`, with its \
                     package"
                ));
            }
            if !seen.insert(service) {
                return Err(format!(
                    "backends.{backend}.grpc_services names {service:?} twice"
                ));
            }
        }
        if self.grpc_serve_any && !self.grpc_services.is_empty() {
            return Err(format!(
                "backends.{backend}.grpc_serve_any is set beside a non-empty grpc_services; they \
                 are two statements about what this backend may be asked to serve. Remove the \
                 list, or remove grpc_serve_any to keep the allow-list"
            ));
        }
        // The third statement about the same set, refused beside both of the others for the reason
        // the `worker` table refuses it: only one of the three can be the answer.
        if !self.grpc_proto.is_empty() {
            if !self.grpc_services.is_empty() {
                return Err(format!(
                    "backends.{backend}.grpc_proto is set beside a non-empty grpc_services; both \
                     name the services this backend may be asked to serve, and only one of them \
                     can be the answer. Remove grpc_services to let the `.proto` files decide, or \
                     remove grpc_proto to keep the list"
                ));
            }
            if self.grpc_serve_any {
                return Err(format!(
                    "backends.{backend}.grpc_proto is set beside grpc_serve_any; serving every \
                     service makes reading the `.proto` files pointless. Remove one of them"
                ));
            }
            if let Some(empty) = self
                .grpc_proto
                .iter()
                .find(|path| path.as_os_str().is_empty())
            {
                return Err(format!(
                    "backends.{backend}.grpc_proto contains {empty:?}, which names no file; give a \
                     `.proto` path or a directory, relative to the configuration file"
                ));
            }
        }
        Ok(())
    }
}
