//! `[rpc]`: the listener a FastCGI application reaches the call plane on.
//!
//! # Why this table exists at all
//!
//! A worker reaches `kv`, `lock` and `jobs` over the channel that carries its requests: `CALL` and
//! `RETURN` are two more frame types on a stream the server already owns. A FastCGI application has
//! no such stream. FastCGI is server-driven and half-duplex — the process exists for the duration of
//! one request and can only answer the frame it was handed — so there is nowhere on that protocol for
//! the application to ask a question.
//!
//! So the call plane gets a socket of its own, which the application dials. That is what RoadRunner's
//! `rpc.listen` is, and the shape is worth copying because the alternative is not: a FastCGI
//! application that wants a cache has to either talk to Redis itself — reimplementing the lock's Lua
//! reservation scripts and the queue's, outside the server that owns them — or go without.
//!
//! # Why a separate listener rather than a route
//!
//! The same argument [`StatusConfig`](super::StatusConfig) makes, only more so. `[server]`'s
//! listener may be TLS-only, may redirect to HTTPS, and faces the public. What travels on this one is
//! a **mutation** surface — `kv.set`, `lock.release`, `jobs.push`, `workers.reset` — so it is a
//! separate address, `unix:` by default, and a TCP address has to be loopback.
//!
//! # Enabling it
//!
//! **The presence of this table is what turns the listener on**, matching `[status]` and
//! `[telemetry]`. It is not enough on its own, though: a backend also has to grant something, or the
//! listener would be a socket that refuses every call. [`Config::validate`] refuses that combination
//! by name rather than leaving an operator with a live socket and an application that can reach
//! nothing.

use super::*;

/// Where the call plane listens for FastCGI applications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcConfig {
    /// The address to listen on, e.g. `unix:/run/volucer/rpc.sock` or `127.0.0.1:6001`.
    ///
    /// Required, and with no default, for the reason `[status].listen` has none: a default TCP
    /// port is either occupied by something else or reachable by somebody it should not be.
    pub listen: String,

    /// How many connections may be open at once.
    ///
    /// **Unset means no cap**, which is the right default for a CGI pool: a `php-fpm` with 32
    /// children can have 32 connections open at the busiest possible moment and no more, so a cap
    /// here adds a refusal without removing a risk. It exists for the deployment that has measured a
    /// problem — a front-end that opens a connection per request and never closes one, which is a
    /// file-descriptor leak that would otherwise present as the server running out of them.
    #[serde(default)]
    pub max_connections: Option<usize>,

    /// How long a connection may sit idle before the server closes it.
    ///
    /// Defaults to five minutes. Idle is the ordinary state of this socket: a FastCGI application
    /// opens a connection during a request and may then serve no request for hours. Closing on idle
    /// is what keeps a pool of 32 children from holding 32 descriptors through a quiet night — and
    /// the application's next call reconnects, which costs one `connect`.
    ///
    /// `0` is refused rather than read as "never", which `Option` would express if it were one; a
    /// connection that never times out is a descriptor held until either process dies.
    #[serde(default = "default_rpc_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,

    /// How long one call may take before the server writes a refusal and closes the connection.
    ///
    /// **A refusal rather than a silent drop**, because a caller waiting for a `RETURN` that will
    /// never come is a request that hangs until PHP's own `max_execution_time` kills it — and that
    /// failure names a `max_execution_time` rather than the store that did not answer.
    ///
    /// Defaults to ten seconds, which is deliberately longer than any store operation should take
    /// and shorter than a PHP request's own limit, so the timeout an operator meets is this one.
    #[serde(default = "default_rpc_call_timeout_seconds")]
    pub call_timeout_seconds: u64,
}

fn default_rpc_idle_timeout_seconds() -> u64 {
    DEFAULT_RPC_IDLE_TIMEOUT_SECONDS
}

fn default_rpc_call_timeout_seconds() -> u64 {
    DEFAULT_RPC_CALL_TIMEOUT_SECONDS
}

/// Five minutes: long enough that a quiet period does not churn connections, short enough that a
/// pool's descriptors are returned.
pub const DEFAULT_RPC_IDLE_TIMEOUT_SECONDS: u64 = 300;

/// Ten seconds: longer than a store operation should ever take, shorter than PHP's default
/// `max_execution_time`, so this deadline is the one a caller meets.
pub const DEFAULT_RPC_CALL_TIMEOUT_SECONDS: u64 = 10;

/// The CGI parameter that carries the address to dial.
///
/// Injected into every request a granted backend serves, so an application reads it from `$_SERVER`
/// and needs no configuration of its own — the deployment states the address once, in one file, and
/// both sides read it. That is the same reasoning `VOLUCER_WORKER_SOCKET` follows, and the same
/// reason: two copies of one address with nothing keeping them equal is a deployment that starts and
/// then fails on the first call.
pub const RPC_ENDPOINT_PARAM: &str = "VOLUCER_RPC";

/// The CGI parameter that carries the backend's identity.
///
/// **The token is the grant.** A call arrives with no idea which backend made it — there is no
/// request to attach it to, which is the whole difference from the worker channel — so the
/// identity has to travel in the call. Deriving it from the backend's name rather than adding a
/// second grant table keeps `[backends.*].services` the single statement of what a backend may
/// reach; see [`crate::config::Config::rpc_grants`].
pub const RPC_TOKEN_PARAM: &str = "VOLUCER_RPC_TOKEN";

impl RpcConfig {
    /// The parsed listening endpoint, for a caller that has already validated.
    pub fn endpoint(&self) -> std::result::Result<Listener, String> {
        parse_listener(&self.listen)
    }

    /// Reject a table that cannot work.
    ///
    /// Everything here is about *this* table alone; whether any backend grants anything is a
    /// question about the backends, and [`Config::validate`] asks it there.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let listener = parse_listener(&self.listen).map_err(|err| format!("rpc.listen: {err}"))?;

        // A TCP listener has to be loopback, and the rule is the one `[status.workers]`
        // already follows — with a stronger reason, because every path on this socket mutates
        // something. A unix socket needs no such check: its permissions are the operator's access
        // control, and they set them with `chmod` on a path they chose, which is a decision this
        // file cannot make for them.
        //
        // A wildcard bind is refused rather than warned about because it is never what an operator
        // meant: `0.0.0.0:6001` exposes an unauthenticated call plane to the network, and the token
        // in the params is a *correlation* mechanism, not a credential against a host that can read
        // another host's traffic.
        if let Listener::Tcp(address) = &listener {
            if address.ip().is_unspecified() {
                return Err(format!(
                    "rpc.listen is {}, which binds every interface; this socket answers calls that \
                     write to the cache, take locks and push jobs, and it has no authentication \
                     beyond a token the application is handed. Give it a loopback address, or a \
                     unix socket path",
                    self.listen
                ));
            }
        }

        if self.max_connections == Some(0) {
            return Err(
                "rpc.max_connections is 0, so no call could ever be served; remove the key to \
                 leave the listener uncapped, which is the default"
                    .to_string(),
            );
        }
        if self.idle_timeout_seconds == 0 {
            return Err(
                "rpc.idle_timeout_seconds is 0, so a connection would never be closed; a FastCGI \
                 application opens one during a request and may be quiet for hours, so give a \
                 number of seconds"
                    .to_string(),
            );
        }
        if self.call_timeout_seconds == 0 {
            return Err(
                "rpc.call_timeout_seconds is 0, so every call would be abandoned the moment it was \
                 sent; give a number of seconds, e.g. 10"
                    .to_string(),
            );
        }

        Ok(())
    }

    /// The idle deadline, as a [`Duration`](std::time::Duration).
    pub fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_timeout_seconds)
    }

    /// The per-call deadline.
    pub fn call_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.call_timeout_seconds)
    }
}

impl Config {
    /// The backends that grant at least one call namespace, and the names they grant.
    ///
    /// **The one statement of what an application may reach**, which is why it reads
    /// `[backends.*].services` rather than a table of its own. A worker's grant and a FastCGI
    /// application's grant are then the same declaration, and there is no way for the two to
    /// disagree — which the alternative (a `[rpc.clients.*]` table naming its own list) would make
    /// possible, in the shape where it is hardest to see: one backend's cache working and its lock
    /// calls refused, because the file said so twice and meant it once.
    ///
    /// An `attach` backend is excluded, and the exclusion is the interesting one. Its workers reach
    /// the call plane over their own channel, which is connected and authenticated by construction,
    /// so a token for them would be a second way in to the same thing that nothing needs. A
    /// `worker` backend is excluded for the same reason.
    ///
    /// The order is the `BTreeMap`'s, so it is deterministic — which matters because the grants are
    /// published to the listener once at bind time and an order that changed between runs would make
    /// a diagnostic differ from itself.
    pub fn rpc_grants(&self) -> BTreeMap<String, Vec<String>> {
        self.backends
            .iter()
            .filter_map(|(name, backend)| {
                let services = match backend {
                    // The two CGI-shaped kinds, which is what this feature is for.
                    BackendConfig::External(external) => &external.services,
                    BackendConfig::SpawnCgi(cgi) => &cgi.services,
                    // An upstream receives the token as a header, and its grants are the same
                    // list; it is included so a Node or Go application served over HTTP has the
                    // same reach as a PHP one over FastCGI.
                    BackendConfig::Upstream(upstream) => &upstream.services,
                    // A worker and an attach already have a channel with a registry on it.
                    BackendConfig::Worker(_) | BackendConfig::Attach(_) => return None,
                };
                (!services.is_empty()).then(|| (name.clone(), services.clone()))
            })
            .collect()
    }

    /// Whether this deployment has anything for a `[rpc]` listener to serve.
    pub fn serves_rpc(&self) -> bool {
        !self.rpc_grants().is_empty()
    }
}
