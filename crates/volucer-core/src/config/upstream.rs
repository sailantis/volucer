//! The `upstream` backend: an HTTP origin reached over the network or a unix socket.

use super::*;

/// An HTTP origin, reached over the network or a unix socket.
///
/// Parsed by [`OriginSpec::parse`], which lives here so that the *configuration model* can
/// describe and check an origin without the client crate's parser — the same reason
/// [`ChecksumMode`] mirrors the worker protocol's policy instead of importing it. `volucer-core`
/// is the leaf every other crate depends on, so it cannot depend on `volucer-upstream`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Where the origin is: `http://host:port`, `https://host:port`, or `unix:/path/to.sock`.
    pub origin: String,

    /// How to trust an `https:` origin. An error beside any other scheme, since there is no
    /// handshake for it to describe.
    #[serde(default)]
    pub tls: Option<UpstreamTlsConfig>,

    /// That a plaintext or `unix:` origin speaks HTTP/2 with **prior knowledge** (h2c).
    ///
    /// Absent — the default — means HTTP/1.1, which is what every ordinary HTTP origin speaks and
    /// what this client spoke before the key existed. `true` makes the connector answer
    /// `negotiated_h2()` for this origin and nothing else, which is the whole mechanism: measured in
    /// `crates/volucer-upstream/tests/h2c.rs`, making that one claim is what makes hyper open with
    /// the HTTP/2 connection preface instead of an HTTP/1.1 request line.
    ///
    /// # Why this is a key rather than something inferred
    ///
    /// gRPC's transport *is* HTTP/2, so a plaintext or unix-socket gRPC backend needs h2c — there is
    /// no ALPN to negotiate it on a transport without a handshake. The alternative considered and
    /// refused was inferring it from the route that selected the backend:
    /// that needs no key, but it makes an origin's *protocol* depend
    /// on where a request came from, which is the kind of implicit coupling this project refuses
    /// elsewhere. Stated here, it is one fact about one origin that an operator can read.
    ///
    /// # What it refuses
    ///
    /// Beside `https://`, by `validate`: TLS negotiates HTTP/2 through ALPN (RFC 7301), and a claim
    /// of prior knowledge there would be a second answer to a question the handshake already
    /// answered — and the wrong one when the origin offers `http/1.1` only. `http://` and `unix:`
    /// are not two settings for one thing either; they are one setting for two transports, since
    /// there is no handshake on either.
    ///
    /// # The failure if the origin does not actually speak h2
    ///
    /// A connection error or a reset rather than a silent downgrade, because a client's first bytes
    /// are the HTTP/2 preface and an HTTP/1.1 peer answers them with a 400. That is a loud failure
    /// and the right one: an origin that does not speak h2 is a deployment mistake, and guessing
    /// HTTP/1.1 for a gRPC route would surface as a protocol error somewhere far less obvious.
    #[serde(default)]
    pub protocol: Option<UpstreamProtocol>,

    /// Command line that starts the origin, e.g.
    /// `gunicorn myapp.wsgi --bind unix:/run/app.sock`.
    ///
    /// **Absent means the origin is somebody else's process**, which is what an upstream was
    /// before this key existed and remains the common case: nginx, php-fpm, a Node service under
    /// systemd. When it is present volucer starts the process, waits for the origin to answer,
    /// restarts it if it dies, and makes sure it does not outlive the server.
    ///
    /// This is the whole of what a runtime like Django or Rails needs to run behind volucer: a
    /// process volucer starts, waits for, restarts and cleans up after. It is deliberately **not** a
    /// `kind = "python"` backend: HTTP
    /// framing is a real cost and it is the cost every Rails and Django deployment already pays,
    /// so there is nothing for a new backend kind to do that this key does not.
    ///
    /// Split and resolved by the same tokenizer a worker's `command` uses
    /// ([`crate::program::split_command`]), which is deliberately not a shell: no variables, no
    /// globs, no operators. A deployment that needs shell syntax writes a script and names it.
    #[serde(default)]
    pub command: Option<String>,

    /// Working directory for the started process.
    ///
    /// Defaults to the process's own, the same rule [`WorkerConfig::working_dir`] follows. A
    /// Django or Rails app almost always needs this set — `manage.py` and `config.ru` are found
    /// relative to it — so a supervisor that guessed would start the process somewhere the
    /// application cannot boot, with an error the operator would read as a volucer bug.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,

    /// Extra environment variables for the started process.
    ///
    /// The same key a worker has, and it is how a Django `DJANGO_SETTINGS_MODULE` or a Rails
    /// `RAILS_ENV` is passed without a wrapper script. An `origin` naming a unix socket is
    /// **not** overridden here: the socket path is the configuration's, and letting the
    /// environment move it would point the supervisor at one address and the client at another.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// When to restart the started process after it exits.
    ///
    /// **An `Option` rather than a defaulted enum, and this is the one place in this struct where
    /// that is deliberate.** A default of [`UpstreamRestart::OnFailure`] would make an origin with
    /// *no* command — the common case, somebody else's process — indistinguishable from one that
    /// asked to be restarted, so `validate` could not tell "this key does nothing" from "this key is
    /// set". Absent means `on_failure` for a supervised origin and is refused outright for an
    /// unsupervised one; [`UpstreamConfig::restart_policy`] is the resolution, in one place.
    #[serde(default)]
    pub restart: Option<UpstreamRestart>,

    /// How many consecutive restarts to attempt before giving up, per start attempt.
    ///
    /// **A limit rather than a loop, because a command that cannot start must not become a busy
    /// loop.** An origin whose command exits instantly — a typo in an import, a missing
    /// virtualenv — fails in milliseconds, and an unbounded restarter would spin a core and fill
    /// the log while the backend answers nothing. `0` means never restart, which is
    /// [`UpstreamRestart::Never`] said with a number.
    #[serde(default = "default_upstream_restart_limit")]
    pub restart_limit: u32,

    /// How long to wait before each restart, and it is a *base* rather than a fixed delay.
    ///
    /// It doubles per consecutive failure, up to [`UPSTREAM_RESTART_BACKOFF_CAP_MS`]. A fixed
    /// delay is either short enough to hammer a service that is genuinely broken or long enough
    /// to turn a one-second blip into ten seconds of 502s; doubling is the shape that is neither.
    #[serde(default = "default_upstream_restart_backoff_ms")]
    pub restart_backoff_ms: u64,

    /// How long the origin has to answer before startup gives up.
    ///
    /// The readiness gate, and the reason this matters more for a supervised origin than for any
    /// other backend: a Django app under `gunicorn` is not listening the instant it is spawned —
    /// it imports, it connects to its database, it may migrate. Without this the first requests
    /// after a deploy get 502s from an origin that is merely still booting, which is the failure
    /// the plan calls out by name.
    ///
    /// Longer than the CGI pool's ten seconds because the reason it exists is a boot that takes
    /// seconds rather than milliseconds: Django and Rails both do real work before they listen.
    #[serde(default = "default_upstream_ready_timeout_ms")]
    pub ready_timeout_ms: u64,
}

/// Which HTTP version a plaintext or unix-socket origin is spoken to in.
///
/// One variant, deliberately. HTTP/1.1 is the absence of this key rather than a value of it — the
/// same reason [`UpstreamConfig::restart`] is an `Option` — so there is no `Http1` to write and no
/// way to state a setting that changes nothing. Adding a second variant later is a one-line change
/// here and a match arm in the connector, which is the shape that keeps "absent" and "explicitly
/// HTTP/1.1" from becoming two different things to test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamProtocol {
    /// HTTP/2 over cleartext, with prior knowledge — no ALPN, no upgrade, no `h2c` header dance.
    ///
    /// That is what a gRPC client does: it opens a connection and sends the HTTP/2 preface. The name
    /// `h2c` is the IETF's for the cleartext form (`RFC 9113` §3.2 covers the prior-knowledge form
    /// too); there is deliberately no `"h2c-upgrade"`, because an upgrade is a mechanism for a client
    /// that must assume HTTP/1.1 first and gRPC clients do not.
    H2c,
}

/// When a supervised upstream origin is started again after it exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamRestart {
    /// Never. The process is started once, and if it exits the backend answers 502 from then on.
    Never,
    /// Restart only after a failure, which is any exit status other than a clean zero.
    ///
    /// The default for a supervised origin, and deliberately *not* "always": a `gunicorn` that
    /// received `SIGTERM` and exited 0 has been shut down by somebody, and starting it again would
    /// fight the operator. A process that died on an uncaught exception exited non-zero and is
    /// exactly the case this is for.
    OnFailure,
    /// Restart whatever the exit status.
    Always,
}

impl UpstreamRestart {
    /// The name this is written as in configuration.
    ///
    /// So a message about it reads as the key the operator typed rather than as Rust's `Debug`:
    /// `restart = "on_failure"` and `OnFailure` are the same value and only one of them is
    /// searchable in the file that set it.
    pub fn as_str(self) -> &'static str {
        match self {
            UpstreamRestart::Never => "never",
            UpstreamRestart::OnFailure => "on_failure",
            UpstreamRestart::Always => "always",
        }
    }
}

impl std::fmt::Display for UpstreamRestart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl UpstreamConfig {
    /// An origin that is somebody else's process, with no supervisor keys set.
    ///
    /// The shape a programmatic configuration wants and the shape every fixture in this tree builds:
    /// an origin, and nothing that describes a process nobody starts. `Config::from_toml` reaches it
    /// too — every supervisor key is absent, which is what `validate` requires without a `command` —
    /// so there is one answer to "what does an unsupervised origin look like" rather than one per
    /// call site.
    pub fn new(origin: impl Into<String>) -> Self {
        UpstreamConfig {
            origin: origin.into(),
            tls: None,
            protocol: None,
            command: None,
            working_dir: None,
            env: BTreeMap::new(),
            restart: None,
            restart_limit: default_upstream_restart_limit(),
            restart_backoff_ms: default_upstream_restart_backoff_ms(),
            ready_timeout_ms: default_upstream_ready_timeout_ms(),
        }
    }

    /// When this origin's process is restarted.
    ///
    /// The one resolution of [`UpstreamConfig::restart`]'s `None`, so the supervisor and `check`
    /// cannot disagree about what an absent key means. Only meaningful when `command` is set, which
    /// `Config::validate` enforces.
    pub fn restart_policy(&self) -> UpstreamRestart {
        self.restart.unwrap_or(UpstreamRestart::OnFailure)
    }
}

pub(super) fn default_upstream_restart_limit() -> u32 {
    3
}

pub(super) fn default_upstream_restart_backoff_ms() -> u64 {
    1_000
}

pub(super) fn default_upstream_ready_timeout_ms() -> u64 {
    30_000
}

/// The longest a restart backoff grows to.
///
/// A cap rather than unbounded doubling because the point of the backoff is to stop *hammering*,
/// not to stop *recovering*: a twenty-minute delay turns a transient failure into an outage
/// nobody connects to it. One minute is longer than any restart that is going to work and shorter
/// than an operator's patience.
pub const UPSTREAM_RESTART_BACKOFF_CAP_MS: u64 = 60_000;

/// How an `https:` origin is verified.
///
/// Two knobs and no third. There is deliberately **no** `verify = false`: an unverified TLS hop is
/// strictly worse than a plaintext one, because it looks encrypted and checks nothing, and this
/// project already refuses the equivalent shape on the serving side (`[server.tls]` will not put a
/// self-signed certificate on a wire "for a certificate nothing ever trusted"). A deployment that
/// wants that shape writes `http://` on a loopback port and says so out loud.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamTlsConfig {
    /// PEM file holding the roots to verify the origin against. Absent means the public roots.
    #[serde(default)]
    pub ca: Option<PathBuf>,

    /// The name to verify against, when it is not the host in `origin`. The common case is
    /// dialling an IP or a service address while the certificate names a host.
    #[serde(default)]
    pub sni: Option<String>,
}

/// A parsed `origin`, in the configuration model's own terms.
///
/// `volucer-upstream`'s `Origin` is hyper-typed — a `Uri`, an `Authority` — and a configuration
/// model cannot carry one without depending on `hyper`, which this crate deliberately does not.
/// The reverse is fine and is the direction used: this parses, validates and describes, and the
/// client crate builds its `Uri` from it once, at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginSpec {
    /// `host:port` over the network, plaintext or TLS.
    ///
    /// Three fields rather than one string because the three have different rules and the
    /// difference is not reconstructible from the whole: `sni` is what the certificate is verified
    /// against, `host` is what the `Host` header carries, and the two are *meant* to differ when a
    /// deployment dials a service address.
    Network {
        /// `"http"` or `"https"`.
        scheme: &'static str,
        /// The host name or bracketed IPv6 literal, without the port.
        host: String,
        /// The port, defaulted from the scheme when the origin names none.
        port: u16,
        /// How to verify an `https:` origin. Always `Some` for `https`, always `None` for `http`.
        tls: Option<UpstreamTlsConfig>,
        /// That this origin speaks h2c — HTTP/2 with prior knowledge. Never set for `https:`.
        ///
        /// On the *spec* rather than passed beside it because it describes the same thing the other
        /// four fields do: how this address is reached. A caller that got an `OriginSpec` therefore
        /// has the protocol, which is what makes "`protocol` beside a handshake was refused" a
        /// property of the type rather than of `validate` having run.
        protocol: Option<UpstreamProtocol>,
    },
    /// A unix socket on this host.
    ///
    /// Only produced on a platform that has one. On Windows the parse refuses `unix:` by name
    /// rather than accepting a path nothing can dial — the same rule `Endpoint` uses.
    #[cfg(unix)]
    Socket {
        /// The socket to dial.
        path: PathBuf,
        /// That this socket speaks h2c. See [`OriginSpec::protocol`].
        ///
        /// A field rather than something inferred from "it is a socket", which would be wrong: the
        /// ordinary unix-socket origin is `php-fpm` or gunicorn speaking HTTP/1.1, and assuming h2
        /// for every socket would break them.
        protocol: Option<UpstreamProtocol>,
    },
}

impl OriginSpec {
    /// Parse an `origin` string, together with the `[backends.<n>.tls]` table that belongs to it.
    ///
    /// One constructor rather than two, and the TLS table is a *parameter* rather than something
    /// attached afterwards, because the two are only meaningful together: a `tls` table beside a
    /// plaintext origin describes a handshake that will never happen, so an origin parsed without
    /// it could not be checked for that at all.
    ///
    /// Strict, and every refusal is a shape that would otherwise be *silently* read as something
    /// else: a bare `host:port` would be a guess between a path and an address, a base path and
    /// credentials would look honoured when nothing consumes them, an unknown scheme would be
    /// treated as plaintext, and a `tls` table beside `http` would look like a verified hop.
    pub fn parse(spec: &str, tls: Option<UpstreamTlsConfig>) -> std::result::Result<Self, String> {
        Self::parse_with(spec, tls, None)
    }

    /// Parse an origin that may also be declared to speak h2c.
    ///
    /// [`OriginSpec::parse`] is this with no protocol, which keeps the four call sites that have no
    /// `protocol` key — a diagnostic, a test, the exporter's endpoint — from inventing one.
    pub fn parse_with(
        spec: &str,
        tls: Option<UpstreamTlsConfig>,
        protocol: Option<UpstreamProtocol>,
    ) -> std::result::Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("it is empty".to_string());
        }

        if let Some(path) = spec.strip_prefix("unix:") {
            if path.is_empty() {
                return Err("the unix socket path is empty".to_string());
            }
            if tls.is_some() {
                return Err(
                    "a [backends.<n>.tls] table is set beside a `unix:` origin, which has no \
                     handshake for it to describe; remove it"
                        .to_string(),
                );
            }
            #[cfg(unix)]
            {
                if path.contains('\0') {
                    return Err("the unix socket path contains a NUL byte".to_string());
                }
                if path.len() > UNIX_SOCKET_PATH_MAX {
                    return Err(format!(
                        "the unix socket path is {} bytes and the platform allows \
                         {UNIX_SOCKET_PATH_MAX}",
                        path.len()
                    ));
                }
                return Ok(OriginSpec::Socket {
                    path: PathBuf::from(path),
                    protocol,
                });
            }
            #[cfg(not(unix))]
            {
                return Err(
                    "unix sockets are not available on this platform, and a Windows named \
                            pipe cannot be used here: the HTTP client has no pipe transport, so an \
                            origin must be reached over TCP on Windows"
                        .to_string(),
                );
            }
        }

        let (scheme, rest) = spec.split_once("://").ok_or_else(|| {
            format!(
                "{spec:?} has no scheme; write `http://127.0.0.1:3000` (a bare `host:port` would \
                 be neither plaintext nor TLS for certain, so it is not guessed at)"
            )
        })?;
        let scheme: &'static str = match scheme {
            "http" => "http",
            "https" => "https",
            other => {
                return Err(format!(
                    "{other:?} is not a scheme an origin can be reached with; use `http`, `https` \
                     or `unix`"
                ))
            }
        };

        // Everything before the first `/` is authority. A path, a query or a fragment after it
        // would have to be joined with the request's own, and a prefix joined without saying so is
        // worse than one that is refused — a route prefix belongs in the route table, where it is.
        let (authority, tail) = match rest.find(['/', '?', '#']) {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        // Checked before the path, so `http:///api` is reported as having no host rather than as
        // carrying a path — the missing host is the mistake that makes the rest unreadable.
        if authority.is_empty() {
            return Err("it has no host; write `http://127.0.0.1:3000`".to_string());
        }
        if !tail.is_empty() && tail != "/" {
            return Err(format!(
                "it carries the path {tail:?}; an origin is only an address, and a route prefix \
                 belongs in the route table (`path_prefix = ...`)"
            ));
        }
        if authority.contains('@') {
            return Err(
                "it carries credentials, which an origin has no use for: volucer forwards \
                        the client's credentials, never an origin's own"
                    .to_string(),
            );
        }

        let (host, port) = split_authority(authority, scheme)?;
        match scheme {
            // Absent means the public roots, which is the right default for a publicly-trusted
            // origin and the wrong one for an internal service — which is exactly why it is the
            // documented default rather than something inferred.
            "https" => Ok(OriginSpec::Network {
                scheme,
                host,
                port,
                tls: Some(tls.unwrap_or_default()),
                protocol: None,
            }),
            _ => {
                if tls.is_some() {
                    return Err(format!(
                        "a [backends.<n>.tls] table is set beside the `{scheme}` origin {spec:?}, \
                         which has no handshake for it to describe; remove it, or make the origin \
                         `https://`"
                    ));
                }
                Ok(OriginSpec::Network {
                    scheme,
                    host,
                    port,
                    tls: None,
                    protocol,
                })
            }
        }
    }

    /// The scheme, as it is written in configuration.
    pub fn scheme(&self) -> &'static str {
        match self {
            OriginSpec::Network { scheme, .. } => scheme,
            #[cfg(unix)]
            OriginSpec::Socket { .. } => "unix",
        }
    }

    /// The `host:port` the origin is dialled at, when there is one.
    ///
    /// The port is the resolved one, so a caller reporting "nothing is listening on
    /// 127.0.0.1:80" from `http://127.0.0.1` names the port that was actually chosen rather than
    /// the empty string the file contains.
    pub fn host_port(&self) -> Option<(String, u16)> {
        match self {
            OriginSpec::Network { host, port, .. } => Some((host.clone(), *port)),
            #[cfg(unix)]
            OriginSpec::Socket { .. } => None,
        }
    }

    /// The socket path, for a `unix:` origin.
    pub fn socket_path(&self) -> Option<&Path> {
        match self {
            OriginSpec::Network { .. } => None,
            #[cfg(unix)]
            OriginSpec::Socket { path, .. } => Some(path),
        }
    }

    /// How to verify this origin.
    ///
    /// `None` for plaintext and for a socket, `Some` for `https` — and the field is *not* optional
    /// in the `https` case, which is what makes "verification is not optional" a property of the
    /// type rather than a rule somebody has to remember.
    pub fn tls(&self) -> Option<&UpstreamTlsConfig> {
        match self {
            OriginSpec::Network { tls, .. } => tls.as_ref(),
            #[cfg(unix)]
            OriginSpec::Socket { .. } => None,
        }
    }

    /// Whether this origin is reached over TLS.
    pub fn is_tls(&self) -> bool {
        self.tls().is_some()
    }

    /// Whether this origin was declared to speak h2c, on either transport.
    ///
    /// Read by the client's connector to answer `negotiated_h2()` for a plaintext or socket origin.
    /// Never `true` for `https:`, and that is structural rather than checked here: the `https` arm of
    /// the parser writes `protocol: None` unconditionally, so a spec that came from an `https://`
    /// string cannot carry a claim even if one was passed in.
    ///
    /// One accessor over both variants rather than one per transport, because the *question* is one
    /// question — "does this address speak HTTP/2 with prior knowledge?" — and a unix-socket gRPC
    /// backend is the shape this key exists for. A second accessor for sockets would be an invitation
    /// to forget the case that matters.
    pub fn protocol(&self) -> Option<UpstreamProtocol> {
        match self {
            OriginSpec::Network { protocol, .. } => *protocol,
            #[cfg(unix)]
            OriginSpec::Socket { protocol, .. } => *protocol,
        }
    }

    /// Whether the origin is on this host rather than across a network.
    ///
    /// Used to decide whether plaintext is worth warning about: a loopback origin is this
    /// deployment talking to itself and has nothing to protect, while a plaintext hop to another
    /// host is the one place an operator may not have realised there is no encryption.
    pub fn is_local(&self) -> bool {
        match self {
            OriginSpec::Network { host, .. } => {
                let bare = host.trim_start_matches('[').trim_end_matches(']');
                bare.eq_ignore_ascii_case("localhost")
                    || bare == "::1"
                    || bare
                        .parse::<std::net::Ipv4Addr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }
            #[cfg(unix)]
            OriginSpec::Socket { .. } => true,
        }
    }
}

/// Split `host[:port]`, refusing the shapes that cannot be dialled.
///
/// A bracketed IPv6 literal keeps its brackets: they are what distinguishes the colons in the
/// address from the one before the port, so stripping them here would make the address ambiguous
/// to everything downstream. Everything else is `SocketAddr`'s rule, taken from `parse_listener`
/// so an origin and a listener cannot disagree about what a `host:port` is.
fn split_authority(authority: &str, scheme: &str) -> std::result::Result<(String, u16), String> {
    let default_port = if scheme == "https" { 443 } else { 80 };

    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("{authority:?} opens an IPv6 literal without closing it"))?;
        host.parse::<std::net::Ipv6Addr>()
            .map_err(|source| format!("{host:?} is not an IPv6 address: {source}"))?;
        let port = match after {
            "" => default_port,
            _ => after
                .strip_prefix(':')
                .ok_or_else(|| {
                    format!(
                        "{authority:?} has something after the IPv6 literal that is \
                                        not a `:port`"
                    )
                })?
                .parse::<u16>()
                .map_err(|source| format!("{authority:?} has no usable port: {source}"))?,
        };
        return Ok((format!("[{host}]"), port));
    }

    match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return Err(format!("{authority:?} names a port with no host"));
            }
            if host.contains(':') {
                return Err(format!(
                    "{authority:?} has a bare IPv6 address; write it in brackets, e.g. \
                     `[{host}]:{port}`"
                ));
            }
            let port = port
                .parse::<u16>()
                .map_err(|source| format!("{authority:?} has no usable port: {source}"))?;
            Ok((host.to_string(), port))
        }
        None => Ok((authority.to_string(), default_port)),
    }
}
