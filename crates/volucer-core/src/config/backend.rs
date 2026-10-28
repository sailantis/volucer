//! The backend table: the five kinds, and what each says about request state.

use super::*;

/// A backend is one of five shapes, tagged by `kind` in TOML.
///
/// There was a fourth, `spawn_fpm`, which volucer would have started itself. It was
/// never implemented — `Dispatcher` refused it — while `volucer version` advertised it
/// and the preflight check warned that it would not start, so the key looked supported
/// and could not work. It is gone rather than built: the project does not
/// reimplement what php-fpm already does, and `external` attaches to the php-fpm that
/// systemd already supervises, which is the better version of the same feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendConfig {
    /// Persistent PHP process speaking volucer's own framed protocol over pipes.
    Worker(WorkerConfig),
    /// A worker pool whose processes somebody else started, reached over a `unix:` relay.
    ///
    /// The one thing a `worker` backend cannot do is share memory between its workers,
    /// because it *executes* each one: an `exec`ed process maps its own OPcache segment, so N
    /// workers pay the interpreter and the preloaded code N times. `fork()` shares all of it
    /// copy-on-write, which measured **89% less resident memory** for the same 16 workers —
    /// but the parent of a fork must be a PHP process, and a PHP process cannot hand a file
    /// descriptor to another process (`ext/sockets` has no `SCM_RIGHTS`). So the side that
    /// owns the socket has to be the side that owns the fork.
    ///
    /// This is the shape where nothing has to give. A supervisor boots the application once,
    /// forks its children, and they serve connections on a relay it listens on; volucer
    /// **dials that relay** and does not start a process at all. The protocol, the handshake
    /// and the per-slot exchange are exactly a `worker`'s, because a child that connected to
    /// us speaks the same frames — the only difference is which side opened the socket.
    ///
    /// It exists because the alternative is worse. A `spawn_cgi` supervisor would work today,
    /// but it is FastCGI: a cold interpreter per request and no channel, measured at **8,292
    /// rps where the warm pipe reached 24,336**. Attaching keeps the warm pipe and changes
    /// only who calls `connect`.
    Attach(AttachConfig),
    /// An already-running php-fpm (systemd-managed). Linux/macOS default.
    External(ExternalConfig),
    /// N `php-cgi` processes on N ports. The only option on Windows.
    SpawnCgi(SpawnCgiConfig),
    /// An HTTP origin volucer proxies to, for a runtime that owns its own HTTP loop.
    ///
    /// Named by role rather than by language: a Node, Python or Go application served by an
    /// application server of its own is the same thing to volucer, which only has to hand it an
    /// HTTP request and relay the answer. This is a *fifth* shape rather than a variation on the
    /// four above because it shares none of their machinery: there is no CGI environment, no
    /// FastCGI record and no volucer protocol, only hyper's client.
    Upstream(UpstreamConfig),
}

/// Whether a backend resets state between requests in the application's own runtime.
///
/// Three answers, not two, and the third is the one that would be got wrong by a boolean: an
/// upstream is neither a CGI process resetting per request nor an application-scoped worker — it
/// is somebody else's runtime, whose request scope is its own business and not something this
/// side can describe. `check` prints exactly that rather than claiming one of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestScope {
    /// State is reset for every request, by the backend itself.
    ///
    /// Both FastCGI shapes: `php-fpm` resets it through PHP's SAPI, and a `spawn_cgi` slot is
    /// either a `php-cgi` process or whatever CGI framework the configured `command` runs. The
    /// *mechanism* differs and volucer does not need to know which one it is — only that the
    /// application is not expected to clear its own state, which is the fact a reader of `check`
    /// is looking for. It was named `PhpSapi` while both backends could only run PHP.
    PerRequest,
    /// The application lives across requests and clears its own (the worker protocol).
    ApplicationScoped,
    /// A runtime of its own, which volucer does not reach into.
    OriginScoped,
}

impl RequestScope {
    /// The phrase `volucer check` prints for this answer.
    ///
    /// Owned by the type rather than written at the call site, so the answer and its description
    /// cannot drift apart — and so the three cases are one value instead of a `bool` that would
    /// have to be reinterpreted wherever it is printed.
    pub fn describe(self) -> &'static str {
        match self {
            RequestScope::PerRequest => "yes (reset by the backend, per request)",
            RequestScope::ApplicationScoped => "no (application-scoped worker)",
            RequestScope::OriginScoped => "not applicable (the origin owns its request scope)",
        }
    }
}

impl BackendConfig {
    /// This backend's configuration with the server's shared `env` baseline merged underneath it.
    ///
    /// The merge has to happen before a pool is built, because a `Spec` captures its environment at
    /// construction and a *replacement* worker must be started on the same terms as the one it
    /// replaces — see [`crate::config::WorkerConfig::env`]. Returning an owned copy keeps the
    /// original configuration describable as written, which is what `config.describe` reports.
    ///
    /// `attach` and `external` have nothing to merge: volucer starts neither process, and an `env`
    /// key on them would describe a process nobody here spawns.
    #[must_use]
    pub fn with_server_env(&self, server: &ServerConfig) -> Self {
        let mut merged = self.clone();
        match &mut merged {
            BackendConfig::Worker(worker) => worker.env = server.env_for(&worker.env),
            BackendConfig::SpawnCgi(cgi) => cgi.env = server.env_for(&cgi.env),
            BackendConfig::Upstream(upstream) => upstream.env = server.env_for(&upstream.env),
            BackendConfig::Attach(_) | BackendConfig::External(_) => {}
        }
        merged
    }

    pub fn kind(&self) -> &'static str {
        match self {
            BackendConfig::Worker(_) => "worker",
            BackendConfig::Attach(_) => "attach",
            BackendConfig::External(_) => "external",
            BackendConfig::SpawnCgi(_) => "spawn_cgi",
            BackendConfig::Upstream(_) => "upstream",
        }
    }

    /// Every spelling of `kind` a configuration file may use, canonically.
    ///
    /// Exists because `default_backend` accepts a kind where it used to accept only a name, and that
    /// means the *parser* has to know the set of kinds in order to tell "a kind that selects nothing"
    /// from "a name that matches nothing" — two different mistakes with two different sentences.
    /// A `const` list beside the `match` above would be a second place to forget, so this returns
    /// the same strings by construction and a test pins the two against each other.
    pub fn kinds() -> &'static [&'static str] {
        &["worker", "attach", "external", "spawn_cgi", "upstream"]
    }

    /// Whether `stated` names a backend kind, accepting the hyphenated spelling `check` prints.
    pub fn is_kind(stated: &str) -> bool {
        let normalized = stated.replace('-', "_").to_ascii_lowercase();
        Self::kinds().contains(&normalized.as_str())
    }

    /// True when the backend resets state per request.
    pub fn resets_per_request(&self) -> bool {
        matches!(self.request_scope(), RequestScope::PerRequest)
    }

    /// Where a request's state is scoped for this backend.
    pub fn request_scope(&self) -> RequestScope {
        match self {
            // An attached worker is a worker: somebody else started it, but it is still a
            // persistent PHP process that clears its own request scope, which is the fact this
            // answers. Who called `fork()` has no bearing on it.
            BackendConfig::Worker(_) | BackendConfig::Attach(_) => RequestScope::ApplicationScoped,
            BackendConfig::External(_) | BackendConfig::SpawnCgi(_) => RequestScope::PerRequest,
            BackendConfig::Upstream(_) => RequestScope::OriginScoped,
        }
    }

    /// Whether this backend keeps a persistent application alive across requests.
    ///
    /// The answer for the two kinds that speak volucer's own protocol, whichever side opened the
    /// socket. Stated as a method rather than as `matches!(kind)` at each call site because it is
    /// the property the server asks about — gRPC delivery, calls, warm start — and it must not be
    /// possible for one of them to be updated and the other forgotten.
    pub fn is_worker(&self) -> bool {
        matches!(self, BackendConfig::Worker(_) | BackendConfig::Attach(_))
    }

    /// How many requests this backend can run at once, as a phrase `volucer check` prints.
    ///
    /// A *description* rather than a number, because for `external` there is no number: the pool
    /// belongs to php-fpm or a preforking `php-cgi`, and its size lives in configuration volucer
    /// cannot read. Printing an invented figure would be worse than printing the rule — and the
    /// rule is what an operator actually needs, because it says why there is no `workers` key to
    /// tune. See [`ExternalConfig`] for the measurement behind it.
    pub fn concurrency(&self) -> &'static str {
        match self {
            // `socket` changes the address, not the count: one connection per worker either way,
            // so this line stays true whether the pool binds loopback ports or unix sockets.
            BackendConfig::Worker(_) | BackendConfig::Attach(_) => "`workers` connections",
            // One FastCGI connection carries one request, so there is nothing to size: the far
            // side's accept loop is the limit, and one connection per request in flight is what
            // reaches it.
            BackendConfig::External(_) => {
                "one connection per request in flight; the pool's own max_children is the limit"
            }
            BackendConfig::SpawnCgi(cgi) if cgi.is_php_cgi() => {
                "one slot per listening socket, `children` requests each"
            }
            BackendConfig::SpawnCgi(_) => {
                "one slot per listening socket, `concurrency` requests each"
            }
            BackendConfig::Upstream(_) => "the origin's own loop",
        }
    }

    /// Whether this backend interprets the path as a file to execute.
    ///
    /// The question a route's `skip_stat` turns on, and it is *not* the complement of
    /// [`Self::resets_per_request`]: a worker does not reset per request and an upstream does not
    /// either, but only the worker is reached through a CGI environment for which the path is
    /// metadata. An upstream is handed the path as the request's URL, and does its own routing with
    /// it, so volucer stat-ing it first would check a file nothing reads.
    pub fn interprets_path(&self) -> bool {
        matches!(
            self,
            BackendConfig::External(_) | BackendConfig::SpawnCgi(_)
        )
    }

    /// The parsed origin, for an `upstream` backend.
    ///
    /// `None` for the four CGI-and-worker-shaped kinds. Parsed rather than stored because the
    /// configuration model deserialises straight from TOML: a field holding a parsed value would have to be
    /// filled in by hand after every load, and a reload reads the file again. This is called at
    /// startup, at `check`, and once per backend when its client is built — never per request.
    ///
    /// The error is returned rather than logged so the caller decides: `validate_backend` turns it
    /// into a configuration error, `preflight` into a warning.
    pub fn origin(&self) -> Option<std::result::Result<OriginSpec, String>> {
        match self {
            BackendConfig::Upstream(upstream) => Some(OriginSpec::parse_with(
                &upstream.origin,
                upstream.tls.clone(),
                upstream.protocol,
            )),
            _ => None,
        }
    }
}
