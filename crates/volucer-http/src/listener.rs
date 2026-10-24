//! The HTTP listener: accept connections, translate to CGI, dispatch, answer.
//!
//! Both front-ends do the same job — read a client protocol, produce the CGI
//! environment, hand it to the one dispatcher, translate the answer back. The
//! difference is the transport and the dialect. That is why this module's real
//! work is two conversions, [`to_cgi`] and [`from_cgi`], with the network
//! machinery around them being as thin as the protocols allow.
//!
//! # The async boundary
//!
//! Every backend is blocking: a worker slot is one process on a pipe serving one
//! request at a time, and `php-cgi`/`php-fpm` process requests sequentially. So
//! the dispatcher is reached through `tokio::task::spawn_blocking`, never
//! directly. This is the single line that keeps the two worlds apart, and the
//! rule not to break is: **no blocking call in an async context**.
//!
//! # Request body handling
//!
//! The body is read into memory before dispatch, up to
//! [`HttpLimits::max_body_bytes`]. That is a deliberate limit rather than a
//! shortcut: the CGI protocol carries a body as one `STDIN` stream preceded by
//! its length, and `php-cgi` needs `CONTENT_LENGTH` before it will read any of
//! it, so a streaming pass-through is not available. Multipart parsing is not
//! done here either — the bytes go to PHP, which owns `$_FILES` and
//! `is_uploaded_file()`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Incoming};
use hyper::header::{
    HeaderName, HeaderValue, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST,
    STRICT_TRANSPORT_SECURITY, VARY,
};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};

use volucer_core::sites::Sites;
use volucer_core::SiteProtocol;
use volucer_fcgi::{cgi, RequestInfo, Route, Routing};
use volucer_server::{Dispatcher, ServerError};
use volucer_watch::{RegisteredFrontEnd, Registry, SharedState, Swappable};

use crate::error::{HttpError, Result};
use crate::static_files;

/// The body type every response uses.
///
/// A backend answer is a `Vec` that is already in memory, while a static file is
/// a stream, so the two have to meet somewhere. Boxing is that meeting point: one
/// allocation per response, against a static file that would otherwise have to be
/// buffered whole to be expressible.
///
/// **`Unsync`, and that is the whole point of the name.** The upstream path's body is
/// `hyper::body::Incoming`, which wraps `h2::RecvStream` and is therefore `Send` but **not**
/// `Sync`; `BoxBody` requires both, so an origin's response could not be expressed in it at
/// all. `hyper`'s server is what makes the looser bound the right one rather than a
/// compromise: it accepts any `B: Body + 'static` with no `Send` or `Sync`, and every
/// existing producer here — a few bytes, a `FileBody` — is `Send` regardless. So this is
/// one word, and the alternative would be two response types for no gain.
pub type ResponseBody = UnsyncBoxBody<Bytes, std::io::Error>;

/// A body made of bytes that are already in memory.
pub fn body_of(bytes: Bytes) -> ResponseBody {
    Full::new(bytes)
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed_unsync()
}

/// How long an idle keep-alive connection may sit before it is closed.
///
/// The same fifteen seconds the FastCGI listener uses. Without it, an idle
/// connection holds one of the capped slots for ever.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long `accept` waits before the shutdown flag is checked again.
///
/// `hyper` and `tokio` between them need no runtime configuration, and adding a
/// signal stream just to interrupt an `accept` would be the only reason to pull
/// one in. Polling matches the FastCGI listener, and 200 ms is far below the time
/// an operator notices.
const ACCEPT_POLL: Duration = Duration::from_millis(200);

/// Limits on the HTTP listener.
#[derive(Debug, Clone, Copy)]
pub struct HttpLimits {
    /// Maximum concurrent connections. One accepted connection holds a slot for
    /// as long as it is open, keep-alive included.
    pub max_connections: usize,
    /// Maximum request body, in bytes.
    ///
    /// The body is buffered so its length can be declared to the backend. PHP's
    /// own `post_max_size` is a separate, later limit; this one exists so an
    /// unauthenticated client cannot make the server allocate without bound.
    pub max_body_bytes: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        HttpLimits {
            max_connections: 512,
            // The one constant, rather than the same literal spelled twice. The FastCGI front-end's
            // `Limits::default()` reads it too, which is what makes "32 MiB" a single decision
            // instead of two that happen to agree.
            max_body_bytes: volucer_core::config::DEFAULT_MAX_BODY_BYTES,
        }
    }
}

/// What the listener reports about its own behaviour.
#[derive(Debug, Default)]
pub struct HttpStats {
    pub connections: AtomicUsize,
    pub requests: AtomicUsize,
    pub rejected: AtomicUsize,
    pub failed: AtomicUsize,
    /// Handshakes that completed. Zero on a deployment without TLS.
    pub handshakes: AtomicUsize,
    /// Handshakes that failed.
    ///
    /// Counted rather than only logged because a TLS misconfiguration is the one failure that
    /// is invisible from the client's side: the client sees a handshake error and never reaches
    /// the server's logs. A non-zero count on a port that should be idle is the signal.
    pub handshake_failures: AtomicUsize,
    /// Cleartext connections answered on a TLS port.
    ///
    /// Only possible with `[server.tls].plain`, which is off by default and logged at startup.
    /// Counted so a deployment can see it happening rather than inferring it.
    pub plaintext_on_tls_port: AtomicUsize,
}

impl HttpStats {
    /// The counters as a tuple, for logging at shutdown.
    pub fn snapshot(&self) -> (usize, usize, usize, usize) {
        (
            self.connections.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }

    /// The TLS counters, for logging at shutdown.
    pub fn tls_snapshot(&self) -> (usize, usize, usize) {
        (
            self.handshakes.load(Ordering::Relaxed),
            self.handshake_failures.load(Ordering::Relaxed),
            self.plaintext_on_tls_port.load(Ordering::Relaxed),
        )
    }
}

/// TLS settings for a listener, borrowed from the configuration.
#[derive(Clone, Copy)]
pub struct TlsSettings<'a> {
    /// The validated configuration, read for `[server.tls]` and the per-site tables.
    pub config: &'a volucer_core::config::Config,
    /// Where a relative `certificate`/`key` resolves.
    pub config_dir: &'a std::path::Path,
}

/// How one listener is set up: whether *it* speaks TLS, and whether it redirects.
///
/// The two are deliberately separate, because they come apart in the shape a real deployment
/// uses. With `http` on :80 and `https` on :443, the plaintext listener has **no TLS of its
/// own** — it must not build an acceptor — and still carries the redirect. Deriving the
/// redirect from the presence of a certificate would have made that listener a TLS port, which
/// is a mistake that only shows up as a client that cannot connect at all.
#[derive(Clone, Copy, Default)]
pub struct ListenerSettings<'a> {
    /// TLS for this listener, or `None` for a plaintext one.
    pub tls: Option<TlsSettings<'a>>,
    /// The port a redirect should name, when it is not this listener's own.
    ///
    /// **Whether** to redirect is not a listener setting: it is a site's, resolved from
    /// `redirect_to_https` on the site or on `[server]`. A listener only contributes the facts a
    /// redirect needs — this port, and whether the connection arrived over TLS — because those are
    /// properties of the socket rather than of any site.
    pub https_port: Option<u16>,
}

/// Stops the listener from another thread.
///
/// A plain flag, exactly as the FastCGI listener uses, so the Ctrl-C handler
/// lives on an ordinary thread and no runtime handle has to be passed around.
#[derive(Clone)]
pub struct HttpShutdown {
    flag: Arc<AtomicBool>,
}

impl HttpShutdown {
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// Everything one connection needs, shared across tasks.
struct FrontEnd {
    /// Shared without a lock of its own: the dispatcher and its backends
    /// synchronise internally, per backend slot, so requests to different
    /// workers genuinely run at the same time.
    dispatcher: Arc<Dispatcher>,
    /// The process's one registry, which a reload applies through.
    ///
    /// Held so this listener's [`ReloadHandle`] can move *every* front-end rather than only its
    /// own snapshot-holder. Without it a reload would be correct for one protocol and wrong for
    /// the other, which no single-protocol test can see.
    registry: Arc<Registry>,
    /// Everything this listener derives from the table, behind **one** pointer.
    ///
    /// One handle rather than one per field, and that is the whole correctness of a reload
    /// under load: three separate swaps would let a request take the new site table and the
    /// old routing map, which is two answers to one question. The failure would not look
    /// like a race — it would look like a site serving the wrong document root, once, under
    /// load, and be blamed on the filesystem.
    ///
    /// See [`Reloaded`] for what is in it and [`FrontEnd::state`] for how the request path
    /// reads it.
    state: Arc<Swappable<Reloaded>>,
    /// TLS, when this listener speaks it.
    ///
    /// The **resolver**, not the acceptor: rustls owns the acceptor and the
    /// [`rustls::ServerConfig`] inside it for the life of the listener, so those cannot be
    /// replaced. The resolver can, and it is what decides which certificate a handshake is
    /// served — so a certificate that arrives while the process is running is presented on the
    /// next handshake rather than the next restart. See [`crate::tls::SniResolver`].
    tls: Option<Arc<crate::tls::SniResolver>>,
    limits: HttpLimits,
    stats: Arc<HttpStats>,
    shutdown: Arc<AtomicBool>,
    /// The port this listener is bound to, for `SERVER_PORT` when the client
    /// sends no `Host`.
    server_port: u16,
    /// The port a redirect should name, when it differs from this listener's own.
    ///
    /// Here rather than resolved per request because it is a property of the socket: a redirect
    /// must name the endpoint that actually speaks TLS, and only the listener knows where that is.
    /// Whether any given request *is* redirected is a site's answer, not this one's.
    https_port: Option<u16>,
    /// Request identity and the access log, shared with the FastCGI front-end.
    ///
    /// One `Arc` for the process rather than one per listener, for the reason the site table is:
    /// a deployment behind nginx runs both front-ends, and two access logs would mean the same
    /// application's requests appear in two files depending on which front-end received them.
    observe: Arc<volucer_observe::Observability>,
    /// Whose forwarded headers are believed. Off resolves every request to its own peer.
    ///
    /// Beside `observe` because it is the same kind of thing — a process-wide decision both
    /// front-ends share — and because the two are read at the same point in the request path: the
    /// address that is logged is the address the policy resolved.
    client_ip: Arc<volucer_core::ClientIpPolicy>,
    /// The rate limit, when the deployment asked for one.
    ///
    /// **One instance for the process**, shared with the FastCGI front-end: two limiters would give
    /// a client twice the configured allowance depending on which protocol took the request. It is
    /// `None` — not a policy that allows everything — when no `[server.rate_limit]` table exists, so
    /// the request path does one `Option` check and touches no map, no clock and no lock.
    rate_limit: Option<Arc<volucer_core::RateLimitPolicy>>,
}

impl FrontEnd {
    /// A snapshot of the reloadable state, taken once per request.
    ///
    /// The single load is the point: everything downstream — site resolution, the routing
    /// map, the static layer — reads from the value this returns, so a request is served
    /// under exactly one configuration even if a reload lands while it is in flight. A
    /// reload therefore affects the *next* request, which is the only ordering that can be
    /// explained to an operator.
    fn state(&self) -> Arc<Reloaded> {
        self.state.load()
    }
}

/// `Reloaded`, `Resolved` and the site table's derived maps live in `volucer-watch`, because both
/// front-ends build them and a reload has to be able to. Re-exported here because this is where a
/// reader looks for them.
pub use volucer_watch::{Reloaded, Resolved};

/// What one front-end needs in hand to serve a request.
///
/// Built by `serve` and handed to whichever listener binds, rather than constructed by each: the
/// registry is the process's single reload path, and a listener that built its own would be a
/// front-end no `SIGHUP` reaches.
#[derive(Clone)]
pub struct ReloadHandles {
    /// The live site table, shared by every front-end.
    pub shared: SharedState,
    /// The process's one registry, which a reload applies through.
    pub registry: Arc<Registry>,
    /// TLS, when a listener speaks it, so a certificate can be replaced too.
    ///
    /// Separate from the registry because it *has* to be: the resolver is installed into a
    /// `rustls::ServerConfig` that rustls owns, so the resolver is the stable part and its table
    /// is what moves. It is `None` on a deployment with no `[server.tls]`, and a plaintext
    /// listener therefore carries no TLS state at all.
    pub tls: Option<Arc<crate::tls::SniResolver>>,
    /// Request identity and the access log, shared by **every** front-end.
    ///
    /// Here rather than on the listener, and here rather than as another argument, because this
    /// struct already means "what a front-end must be handed that a reload may also move". A
    /// deployment behind nginx runs the HTTP listener *and* the FastCGI one, so a request id that
    /// only one assigned, or an access log only one wrote, would make the same application's
    /// requests appear differently depending on which front-end received them — the same
    /// one-front-end-only failure the shared site table exists to prevent.
    ///
    /// Defaults to disabled, so `ReloadHandles::new` keeps costing nothing.
    pub observe: Arc<volucer_observe::Observability>,
    /// Whose forwarded headers are believed, when a reverse proxy is in front.
    ///
    /// Here for the same reason `observe` is: it is a process-wide decision both front-ends have to
    /// agree on, and it is not reloadable. **Restart-only**, unlike the site table — the same
    /// treatment `[server.tls]`'s `alpn` gets and for the same kind of reason: it is a statement
    /// about the deployment's topology rather than about a request, and a reload that appeared to
    /// apply it while a running connection kept the old answer would be worse than one that
    /// refused. `serve`'s `restart_only` list names it on every reload.
    ///
    /// Defaults to off, so every existing caller keeps resolving to the peer address.
    pub client_ip: Arc<volucer_core::ClientIpPolicy>,
    /// The rate limit, when the deployment asked for one.
    ///
    /// Here for the same reason `client_ip` is: it is a process-wide decision both front-ends must
    /// share. **Not reloadable**, and that is forced rather than chosen — the buckets are live
    /// state, so replacing the policy would silently reset every caller's allowance on a `SIGHUP`,
    /// which is a limit that can be escaped by asking the operator to reload. `restart_only` names
    /// it.
    pub rate_limit: Option<Arc<volucer_core::RateLimitPolicy>>,
}

impl ReloadHandles {
    /// Handles built around a site table, with no TLS and no observability.
    pub fn new(registry: Arc<Registry>) -> Self {
        ReloadHandles {
            shared: registry.shared().clone(),
            registry,
            tls: None,
            observe: Arc::new(volucer_observe::Observability::disabled()),
            client_ip: Arc::new(volucer_core::ClientIpPolicy::off()),
            rate_limit: None,
        }
    }

    /// The same, with request identity and an access log.
    #[must_use]
    pub fn with_observability(mut self, observe: Arc<volucer_observe::Observability>) -> Self {
        self.observe = observe;
        self
    }

    /// The same, with a trust list for the peers whose forwarded headers are believed.
    ///
    /// A builder beside [`ReloadHandles::with_observability`] rather than an argument on `new`, for
    /// the reason that one is: every existing caller and every test constructs these handles without
    /// a policy, and those must keep compiling and keep resolving to the peer address.
    #[must_use]
    pub fn with_client_ip(mut self, client_ip: Arc<volucer_core::ClientIpPolicy>) -> Self {
        self.client_ip = client_ip;
        self
    }

    /// The same, with a rate limit both front-ends share.
    ///
    /// A builder beside [`ReloadHandles::with_client_ip`] for the same reason: every existing caller
    /// constructs these handles without a limiter and must keep costing nothing.
    #[must_use]
    pub fn with_rate_limit(mut self, rate_limit: Arc<volucer_core::RateLimitPolicy>) -> Self {
        self.rate_limit = Some(rate_limit);
        self
    }

    /// The same, for a listener that speaks TLS.
    #[must_use]
    pub fn with_tls(mut self, resolver: Arc<crate::tls::SniResolver>) -> Self {
        self.tls = Some(resolver);
        self
    }
}

impl std::fmt::Debug for ReloadHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadHandles")
            .field("tls", &self.tls.is_some())
            .field("rate_limit", &self.rate_limit.is_some())
            .finish_non_exhaustive()
    }
}

/// A handle that replaces a running listener's reloadable state.
///
/// Cloned into whatever watches for a reload — a signal handler, a filesystem watcher, or a
/// test — so the listener itself does not have to know where the trigger comes from. That
/// separation is deliberate: the mechanism for *noticing* a change is a platform question
/// (there is no `SIGHUP` on Windows), while the mechanism for *applying* one is not.
///
/// Applying is whole-or-nothing by construction. There is no setter for a single field,
/// because a reload that changed half of what the file says would be worse than one that
/// refused the file: the operator would have no way to tell which half took effect.
#[derive(Clone)]
pub struct ReloadHandle {
    state: Arc<Swappable<Reloaded>>,
    /// The process-wide registry: the shared table and every other front-end.
    ///
    /// Held so [`ReloadHandle::apply`] moves *all* the front-ends rather than only this one, which
    /// is the difference between one server and one protocol.
    registry: Arc<Registry>,
    /// TLS, when this listener speaks it, so a certificate can be replaced too.
    ///
    /// Independent of the registry, so a reload that changes sites and certificates applies both
    /// and a reload that changes neither installs nothing.
    tls: Option<Arc<crate::tls::SniResolver>>,
}

impl ReloadHandle {
    /// The state being served right now.
    ///
    /// For diagnostics and for tests: what a request made at this instant would see.
    pub fn current(&self) -> Arc<Reloaded> {
        self.state.load()
    }

    /// The site table being served right now, shared across front-ends.
    pub fn shared(&self) -> SharedState {
        self.registry.shared().clone()
    }

    /// The process-wide registry, so a caller can see how many reloads have been applied.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// The certificate resolver, when this listener speaks TLS.
    ///
    /// The handle a certificate reload goes through — see
    /// [`SniResolver::refresh`](crate::tls::SniResolver::refresh) for a pair on disk and
    /// [`SniResolver::replace`](crate::tls::SniResolver::replace) for one already in memory.
    /// `None` on a plaintext listener.
    pub fn tls(&self) -> Option<&Arc<crate::tls::SniResolver>> {
        self.tls.as_ref()
    }

    /// Replace the shared site table, rebuilding every front-end's derived maps from it.
    ///
    /// The table is replaced through the [`Registry`], so **every** front-end moves at once. This
    /// listener's own state is the one the registry rebuilt for it, rather than a second rebuild
    /// here — two rebuilds of one table is how the two front-ends would come to disagree.
    ///
    /// Returns the state that was replaced, so a caller can report what changed rather than
    /// only that something did — the difference between "site `api` document_root changed"
    /// and "configuration reloaded", after which an operator has to go and diff the file.
    ///
    /// In-flight requests are unaffected: each holds a snapshot of the state it started
    /// under, which is what makes this safe to call while the listener is serving.
    #[must_use]
    pub fn apply(&self, sites: Sites) -> Arc<Reloaded> {
        self.registry.apply(sites).unwrap_or_else(|| {
            // Unreachable while `bind` registers this listener before handing out a handle, and
            // expressed as a fresh snapshot rather than a panic so removing that could never
            // become a crash in a signal handler.
            self.state.load()
        })
    }
}

/// The key the default site's routing is stored under.
///
/// A name that cannot collide with a site's own, because a site name comes from a TOML
/// table key and `<>` is not valid in a bare TOML key.
pub const DEFAULT_SITE_KEY: &str = "<default>";

/// A running HTTP listener.
pub struct HttpListener {
    listener: tokio::net::TcpListener,
    front_end: Arc<FrontEnd>,
    /// TLS, when `[server.tls]` is configured. `None` is a plaintext listener, and that is
    /// the whole of the cost when TLS is off: no accept wrapping, no peek, no TLS type built.
    tls: Option<Arc<tokio_rustls::TlsAcceptor>>,
    /// Also answer cleartext on the TLS port (`[server.tls].plain`).
    ///
    /// Only ever true with `tls` set. The first byte decides which of the two paths a
    /// connection takes, which is what nginx's `ssl_preread` does — a TLS record always begins
    /// `0x16`, a plaintext request begins an ASCII method letter.
    accept_plain: bool,
}

/// A running HTTP listener on a dedicated runtime thread.
///
/// This is how a blocking caller — `volucer serve`, whose FastCGI listener owns
/// the main thread — runs an HTTP front-end: the runtime is created here, lives on
/// a thread that exists only for it, and is torn down by [`RunningHttp::stop`].
/// The alternative would be an async `main`, which drags the whole program into
/// the runtime instead of leaving it at this boundary.
///
/// The runtime is deliberately *not* stored: it is moved onto the listener's
/// thread and dropped there, which is what the runtime requires. The handle kept
/// here is the shutdown flag and the thread, which is all a caller needs.
pub struct RunningHttp {
    shutdown: HttpShutdown,
    thread: Option<std::thread::JoinHandle<()>>,
    address: SocketAddr,
    stats: Arc<HttpStats>,
    /// What a reload replaces, taken before the listener moved onto its thread.
    reload: ReloadHandle,
}

impl RunningHttp {
    /// Bind a listener and start it on a dedicated runtime thread.
    ///
    /// Returns once the listener is bound and the thread is accepting, so a port
    /// conflict is reported to the caller rather than to a thread.
    ///
    /// A listener on its own still gets a registry of its own, because that is what a reload
    /// applies through — a reload is a property of the process, not of having two listeners.
    pub fn start(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        sites: Arc<Sites>,
        limits: HttpLimits,
    ) -> Result<Self> {
        let registry = Arc::new(Registry::new(SharedState::new((*sites).clone())));
        Self::start_with_tls(
            address,
            dispatcher,
            ReloadHandles::new(registry),
            limits,
            ListenerSettings::default(),
        )
    }

    /// [`RunningHttp::start`], with TLS and/or a redirect.
    ///
    /// The handles are the process's reload path, passed in rather than built here: a listener
    /// that built its own registry would be one no reload reaches.
    pub fn start_with_tls(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        handles: ReloadHandles,
        limits: HttpLimits,
        settings: ListenerSettings<'_>,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(HttpError::Runtime)?;

        let listener = runtime.block_on(HttpListener::bind_with_resolver(
            address, dispatcher, handles, limits, settings,
        ))?;
        let bound = listener.local_addr()?;
        let shutdown = listener.shutdown_handle();
        let stats = listener.stats();
        // Before `run` consumes the listener, and that is the only moment it can be taken.
        let reload = listener.reload_handle();

        let thread = std::thread::spawn(move || {
            // The runtime is owned by this thread and dropped when the listener
            // stops, which is the order `tokio` requires.
            if let Err(err) = runtime.block_on(listener.run()) {
                tracing::warn!(%err, "http listener stopped with an error");
            }
        });

        Ok(RunningHttp {
            shutdown,
            thread: Some(thread),
            address: bound,
            stats,
            reload,
        })
    }

    /// The address actually bound.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// A handle that replaces what this listener is serving, without a restart.
    ///
    /// What a signal handler holds. Cloned rather than borrowed because the caller usually
    /// wants to move it into a closure that outlives this struct's use as a listener.
    pub fn reload_handle(&self) -> ReloadHandle {
        self.reload.clone()
    }

    /// Statistics, for logging at shutdown.
    pub fn stats(&self) -> Arc<HttpStats> {
        Arc::clone(&self.stats)
    }

    /// Stop the listener and wait for its thread to finish.
    ///
    /// Waiting matters: the dispatcher lives inside the listener, and the worker
    /// pool reaps its processes when it is dropped. Returning before that would
    /// leave PHP processes behind.
    pub fn stop(self) {
        drop(self);
    }

    fn stop_inner(&mut self) {
        self.shutdown.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RunningHttp {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

impl HttpListener {
    /// Bind a listener.
    ///
    /// An async function because a `tokio` socket can only be created inside a
    /// runtime — which is the visible tip of the boundary this crate exists to
    /// draw. Binding happens before `run` so a port conflict is reported before
    /// any backend is reached.
    ///
    /// The site table is wrapped in its own [`SharedState`], so a caller with a single listener
    /// has nothing extra to build. A caller with more than one front-end must use
    /// [`HttpListener::bind_with_resolver`] with handles it shares, or a reload would move only
    /// the HTTP listener's copy.
    ///
    /// A listener on its own still gets a registry of its own, because that is what a reload
    /// applies through — a reload is a property of the process, not of having two listeners.
    pub async fn bind(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        sites: Arc<Sites>,
        limits: HttpLimits,
    ) -> Result<Self> {
        Self::bind_with_tls(
            address,
            dispatcher,
            sites,
            limits,
            ListenerSettings::default(),
        )
        .await
    }

    /// [`HttpListener::bind`], with the TLS settings and the directory certificate paths
    /// resolve against.
    ///
    /// The resolver is built here rather than per connection, and a certificate that cannot
    /// load fails this call — before the port is bound — so a misconfigured deployment is
    /// reported once rather than as a listener that accepts and then fails every handshake.
    pub async fn bind_with_tls(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        sites: Arc<Sites>,
        limits: HttpLimits,
        settings: ListenerSettings<'_>,
    ) -> Result<Self> {
        // A listener on its own still needs a registry, because that is what a reload applies
        // through — and a reload is a property of the process, not of having several front-ends.
        let registry = Arc::new(Registry::new(SharedState::new((*sites).clone())));
        let mut handles = ReloadHandles::new(registry);
        if let Some(tls) = settings.tls {
            handles = handles.with_tls(Arc::new(
                crate::tls::SniResolver::from_config(tls.config, tls.config_dir).map_err(
                    |source| HttpError::Tls {
                        reason: source.to_string(),
                    },
                )?,
            ));
        }
        Self::bind_with_resolver(address, dispatcher, handles, limits, settings).await
    }

    /// [`HttpListener::bind_with_tls`], around a site table and resolver another listener shares.
    ///
    /// The table arrives as a [`SharedState`] because it is **shared**, not owned: a deployment
    /// runs an HTTP listener and possibly a FastCGI one, and a reload that moved only one of them
    /// would take effect on one protocol and not the other. A listener that built its own table
    /// from the config would be exactly that one.
    ///
    /// The resolver comes from the handle rather than as a separate argument, because the resolver
    /// and the registry have to travel together: they are the two things a reload replaces, and a
    /// listener that took one from the caller and built the other itself would be half-reachable.
    ///
    /// The resolver's own documentation covers the certificate side: the table this starts with is
    /// the one passed in, and **no certificate file is read**. That is what lets a deployment whose
    /// certificate does not exist yet — ACME — bind its port, and the resolver is then the handle a
    /// renewal goes through.
    ///
    /// `settings.tls` is still read: `min_version` and `alpn` are properties of the
    /// [`rustls::ServerConfig`], not of a certificate, and they are restart-only (§ the
    /// [`crate::tls`] module doc). A resolver with a `settings.tls` of `None` is refused rather
    /// than ignored, because a resolver nothing consults is a certificate table the caller
    /// believes is serving.
    pub async fn bind_with_resolver(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        handles: ReloadHandles,
        limits: HttpLimits,
        settings: ListenerSettings<'_>,
    ) -> Result<Self> {
        // The redirect *target* comes from the listener's own settings, so a plaintext listener in
        // a split :80/:443 deployment can name the `https` port with no TLS of its own — which is
        // exactly why this is separate from `tls` rather than derived from it.
        let requested_https_port = settings.https_port;
        let resolver = handles.tls.clone();

        if resolver.is_some() && settings.tls.is_none() {
            return Err(HttpError::Tls {
                reason: "a certificate resolver was given to a listener with no TLS settings, so \
                         nothing would consult it"
                    .to_string(),
            });
        }

        // The acceptor is built before the port is bound, and its failure is reported as a
        // configuration error: a certificate that cannot be read must not become a listener.
        //
        // The order is the point rather than a detail. Binding first and loading afterwards fails
        // safely but *late*, and it fails after `serve` has already printed the listener lines —
        // so an operator on a `:80`/`:443` deployment reads that both ports came up and then reads
        // an error, and a service manager that restarts on failure takes and releases the port in
        // a loop, with every client arriving in the window getting nothing. Nothing is bound here
        // until the certificate table that would serve it is in place.
        //
        // The resolver is kept as well as the acceptor, and that pair is not redundant: the
        // acceptor holds a `ServerConfig` that rustls will not let us rebuild, while the
        // resolver behind it is what a reload replaces. Keeping only the acceptor would make a
        // renewed certificate wait for a restart.
        let (acceptor, resolver, accept_plain) = match (settings.tls, resolver) {
            (Some(tls), Some(resolver)) => {
                let server =
                    crate::tls::server_config(tls.config, tls.config_dir, Arc::clone(&resolver))
                        .map_err(|source| HttpError::Tls {
                            reason: source.to_string(),
                        })?;
                tracing::info!(
                    named_certificates = resolver.named_certificates(),
                    alpn = ?server.alpn_protocols.iter()
                        .map(|p| String::from_utf8_lossy(p).into_owned())
                        .collect::<Vec<_>>(),
                    plaintext_accepted = tls.config.server.tls.as_ref()
                        .is_some_and(|t| t.plain),
                    "TLS enabled"
                );
                (
                    Some(Arc::new(tokio_rustls::TlsAcceptor::from(server))),
                    Some(resolver),
                    tls.config.server.tls.as_ref().is_some_and(|t| t.plain),
                )
            }
            // Refused above, so this cannot be reached; kept as an explicit arm rather than an
            // `unreachable!()` so that removing that check could never become a panic in a
            // listener.
            (None, Some(_)) => {
                return Err(HttpError::Tls {
                    reason: "a certificate resolver was given to a listener with no TLS settings"
                        .to_string(),
                })
            }
            (None, None) => (None, None, false),
            // TLS settings with no resolver: the caller wanted TLS and supplied no certificate
            // table. Building one here would mean reading a file, which is the call that refuses
            // a certificate that is not there *yet*.
            (Some(_), None) => {
                return Err(HttpError::Tls {
                    reason: "TLS is configured but no certificate table was supplied; use \
                             `bind_with_tls` to load one from the configuration"
                        .to_string(),
                })
            }
        };

        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|source| HttpError::Bind {
                address: address.to_string(),
                source,
            })?;
        let bound = listener.local_addr().map_err(|source| HttpError::Bind {
            address: address.to_string(),
            source,
        })?;

        // The bound port, which matters when port 0 was requested. The port is needed
        // here rather than by the caller because only the listener knows what it got.
        let port = bound.port();
        // A redirect names the `https` port when there is a separate one, and this listener's
        // own port when TLS shares it. `unwrap_or(port)` covers both, because `bind` sets
        // `https_port` only for the split case — and with TLS on this port, redirecting
        // cleartext to `https://host:this-port` is a real redirect to a real endpoint, not a
        // loop, since the client comes back speaking TLS.
        let https_port = requested_https_port.unwrap_or(port);

        // Which sites redirect is the *site table's* answer, so the report is derived from the
        // same table the request path reads rather than from a listener flag. A listener only
        // knows whether it can redirect at all: it must be able to receive cleartext.
        let table = handles.shared.sites();
        let redirecting: Vec<&str> = std::iter::once(table.default_site())
            .chain(table.named().values())
            .filter(|site| site.redirect_to_https)
            .map(|site| site.name.as_str())
            .collect();
        if accept_plain && !redirecting.is_empty() {
            tracing::info!(
                https_port,
                sites = %redirecting.join(", "),
                "cleartext requests will be answered with a 301 to https for these sites"
            );
        }

        // This listener's maps, built once here from a snapshot of the shared table. `port` is the
        // bound port, which a reload cannot change — the socket is already listening on it.
        //
        // The snapshot is built from the shared handle's table, and the holder is *registered*, so
        // a reload reaches this listener as well as every other one. Registering is the step that
        // makes a reload reach a listener at all: a snapshot-holder nobody registered is a
        // front-end the process's own reload path cannot see, which is how a reload comes to take
        // effect on one protocol and not another.
        let state = Arc::new(Swappable::new(Reloaded::from_sites(
            handles.shared.sites(),
            port,
        )));
        handles
            .registry
            .register(RegisteredFrontEnd::new(Arc::clone(&state), port));

        Ok(HttpListener {
            listener,
            front_end: Arc::new(FrontEnd {
                dispatcher,
                registry: Arc::clone(&handles.registry),
                state,
                tls: resolver,
                limits,
                stats: Arc::new(HttpStats::default()),
                shutdown: Arc::new(AtomicBool::new(false)),
                server_port: port,
                https_port: Some(https_port),
                observe: Arc::clone(&handles.observe),
                client_ip: Arc::clone(&handles.client_ip),
                rate_limit: handles.rate_limit.clone(),
            }),
            tls: acceptor,
            accept_plain,
        })
    }

    /// A handle that replaces this listener's reloadable state.
    ///
    /// The listener's own state is the handle's `Arc`, so a replacement made through it is
    /// immediately visible to the accept loop and to every request after the one in flight.
    pub fn reload_handle(&self) -> ReloadHandle {
        ReloadHandle {
            state: Arc::clone(&self.front_end.state),
            registry: Arc::clone(&self.front_end.registry),
            tls: self.front_end.tls.clone(),
        }
    }

    /// The certificate resolver this listener is deciding handshakes with.
    ///
    /// `None` on a listener with no TLS, which is the visible half of "TLS costs nothing when
    /// it is off": there is no resolver to refresh. A caller that wants to install a
    /// certificate has to go through this, so a plaintext deployment cannot be handed one by
    /// mistake.
    pub fn resolver(&self) -> Option<Arc<crate::tls::SniResolver>> {
        self.front_end.tls.clone()
    }

    /// The address actually bound, which matters when port 0 was requested.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .map_err(|source| HttpError::Bind {
                address: "<listener>".to_string(),
                source,
            })
    }

    /// A handle that stops the listener from another thread.
    pub fn shutdown_handle(&self) -> HttpShutdown {
        HttpShutdown {
            flag: Arc::clone(&self.front_end.shutdown),
        }
    }

    /// Statistics, for logging.
    pub fn stats(&self) -> Arc<HttpStats> {
        Arc::clone(&self.front_end.stats)
    }

    /// Accept and serve connections until the shutdown handle is used.
    ///
    /// A connection is a task rather than a thread, which is the whole reason
    /// this front-end is async: thousands of mostly-idle connections share a
    /// handful of threads, where the FastCGI listener's thread-per-connection
    /// model costs a kernel stack each. The number of connections that can be
    /// *doing work* is still the number of backend slots — async does not change
    /// that, and does not pretend to.
    pub async fn run(self) -> Result<()> {
        let HttpListener {
            listener,
            front_end,
            tls,
            accept_plain,
        } = self;

        while !front_end.shutdown.load(Ordering::Relaxed) {
            match tokio::time::timeout(ACCEPT_POLL, listener.accept()).await {
                Ok(Ok((stream, peer))) => {
                    let active = front_end.stats.connections.fetch_add(1, Ordering::Relaxed) + 1;
                    if active > front_end.limits.max_connections {
                        front_end.stats.rejected.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(%peer, active, "refusing an HTTP connection: at the limit");
                        drop(stream);
                        continue;
                    }

                    // **The local address, read from the accepted socket.** This is the address the
                    // client actually reached, which is what a site's `addresses` binding compares
                    // against. The configured address is not a substitute: a listener on
                    // `0.0.0.0:8080` reports the wildcard, and the whole point of the binding is to
                    // tell `135.181.172.123` from `192.168.9.123` on that same socket. Reading it
                    // here rather than in `prepare` keeps it off the request path — it is one
                    // `getsockname` per *connection*, and keep-alive means many requests each.
                    let local = stream.local_addr().ok().map(|address| address.ip());

                    // Nagle's algorithm is disabled on the raw socket, which is the one place
                    // it can be: the stream is wrapped by TLS from here on, and a `TlsStream`
                    // has no `set_nodelay`.
                    let _ = stream.set_nodelay(true);
                    let front_end = Arc::clone(&front_end);
                    let tls = tls.clone();
                    tokio::spawn(async move {
                        serve_accepted(stream, tls, accept_plain, front_end.clone(), peer, local)
                            .await;
                        front_end.stats.connections.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                Ok(Err(source)) => {
                    // A transient accept failure must not stop the listener,
                    // which is what the FastCGI listener does too.
                    tracing::warn!(%source, "http accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                // The poll elapsed with no connection: loop so the shutdown flag
                // is read again.
                Err(_elapsed) => {}
            }
        }

        let (connections, requests, rejected, failed) = front_end.stats.snapshot();
        tracing::info!(
            connections,
            requests,
            rejected,
            failed,
            "http listener stopped"
        );
        Ok(())
    }
}

/// The first byte of a TLS record header: `ContentType: handshake`.
///
/// A plaintext HTTP request cannot begin with this byte — it begins with an ASCII method letter
/// (`G` = `0x47`, `P` = `0x50`, …) — so one byte separates the two protocols unambiguously.
/// This is the same test nginx's `ssl_preread` applies.
const TLS_HANDSHAKE_BYTE: u8 = 0x16;

/// Decide the protocol for one accepted connection, then serve it.
///
/// With no TLS configured this is `serve_connection` on the raw stream — no peek, no branch,
/// no TLS value constructed, which is the whole of the claim that TLS costs nothing when it is
/// off.
///
/// With TLS configured and `plain` off, every connection is a TLS handshake: a client that
/// sends cleartext gets a handshake failure, which is what a TLS-only port should do.
///
/// With `plain` on, the first byte decides. That is a **detection, not a downgrade**: a
/// connection goes down one path or the other, and a client that offers TLS and fails the
/// handshake still fails. There is no retry as cleartext.
async fn serve_accepted(
    stream: tokio::net::TcpStream,
    tls: Option<Arc<tokio_rustls::TlsAcceptor>>,
    accept_plain: bool,
    front_end: Arc<FrontEnd>,
    peer: SocketAddr,
    local: Option<std::net::IpAddr>,
) {
    let Some(acceptor) = tls else {
        if let Err(err) = serve_connection(stream, front_end, peer, local, true).await {
            tracing::debug!(%peer, %err, "http connection ended");
        }
        return;
    };

    if accept_plain && !starts_with_tls_handshake(&stream).await {
        front_end
            .stats
            .plaintext_on_tls_port
            .fetch_add(1, Ordering::Relaxed);
        if let Err(err) = serve_connection(stream, front_end, peer, local, true).await {
            tracing::debug!(%peer, %err, "http connection ended");
        }
        return;
    }

    match acceptor.accept(stream).await {
        Ok(tls_stream) => {
            front_end.stats.handshakes.fetch_add(1, Ordering::Relaxed);
            // `false`: this connection is already encrypted, so a redirect would loop it.
            if let Err(err) = serve_connection(tls_stream, front_end, peer, local, false).await {
                tracing::debug!(%peer, %err, "https connection ended");
            }
        }
        // A failed handshake is not an error worth propagating: it is a client that could not
        // negotiate, and it is counted so a port that nothing can connect to is visible rather
        // than silent.
        Err(err) => {
            front_end
                .stats
                .handshake_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(%peer, %err, "tls handshake failed");
        }
    }
}

/// Whether a connection begins with a TLS handshake.
///
/// `peek` reads without consuming, so the bytes are still there for whichever path runs next.
/// A read that fails or returns nothing is *not* a TLS handshake: there is no request to serve
/// either way, and treating a dead connection as TLS would only turn a clean close into a
/// handshake error.
async fn starts_with_tls_handshake(stream: &tokio::net::TcpStream) -> bool {
    let mut first = [0u8; 1];
    matches!(stream.peek(&mut first).await, Ok(1) if first[0] == TLS_HANDSHAKE_BYTE)
}

/// Serve every request on one connection until it closes.
///
/// Generic over the stream so that one code path serves both protocols. The TLS handshake is
/// the caller's business: by the time a stream arrives here it is already decrypted (or was
/// never encrypted), and everything below — the request loop, dispatch, site resolution, the
/// static layer — is identical. `hyper`'s auto-detection still picks HTTP/2 from the ALPN
/// result, which it reads off the `TlsStream` through the `TokioIo` wrapper.
async fn serve_connection<S>(
    stream: S,
    front_end: Arc<FrontEnd>,
    peer: SocketAddr,
    local: Option<std::net::IpAddr>,
    cleartext: bool,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // The peer address as the client's, which is the answer whenever no trust list is configured —
    // and the answer for a peer that is not on the list, whatever the list says. Only the IP is kept
    // and not the port: the policy is about the *host*, and the client's port is not sent to the
    // backend at all, so a `REMOTE_PORT` that was really the proxy's is a reader this deliberately
    // does not create.
    let peer_ip = peer.ip();
    let service = service_fn(move |request: Request<Incoming>| {
        let front_end = Arc::clone(&front_end);
        // Infallible because a failure is turned into an error page, not
        // propagated: a 502 the client can read beats a dropped connection.
        async move {
            // **Resolved here, once, from the request's own headers.** Two readers depend on the
            // answer — `REMOTE_ADDR` for the backend and the access log's `peer` — and resolving them
            // separately is how the two come to disagree, which is a log that contradicts what the
            // application was told. It is inside the closure rather than outside because the headers
            // are what it reads.
            let resolved = front_end
                .client_ip
                .resolve(peer_ip, &HttpHeaders(request.headers()));
            let remote_addr = resolved.to_string();
            Ok::<_, Infallible>(
                handle(request, front_end, remote_addr, resolved, local, cleartext).await,
            )
        }
    });

    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    // A connection that sends a partial request and stops must not hold a slot
    // for ever. This is the timeout `hyper` offers for HTTP/1; a fully idle
    // keep-alive connection is bounded by `max_connections` instead, because
    // `hyper` has no per-connection idle timeout and inventing one would mean
    // wrapping the IO stream rather than configuring the server.
    //
    // The timer is not decoration: once any deadline is configured `hyper` panics
    // with "no timer" rather than silently not applying it, so the timer is what
    // makes the timeout real.
    builder
        .http1()
        .timer(TokioTimer::new())
        .keep_alive(true)
        .header_read_timeout(Some(KEEPALIVE_TIMEOUT));

    builder
        .serve_connection(TokioIo::new(stream), service)
        .await
        .map_err(|source| HttpError::BadGateway(format!("connection failed: {source}")))
}

/// Serve one request, mapping any failure to an error page.
async fn handle(
    request: Request<Incoming>,
    front_end: Arc<FrontEnd>,
    remote_addr: String,
    resolved_peer: std::net::IpAddr,
    local: Option<std::net::IpAddr>,
    cleartext: bool,
) -> Response<ResponseBody> {
    // Identity first, before anything can fail. A request that is rejected, redirected or 404s
    // still needs an id — those are precisely the requests an operator correlates against a
    // client's report — so this is taken before `prepare`, not inside it.
    let context = front_end.observe.begin(
        header_str(request.headers(), volucer_observe::REQUEST_ID_HEADER),
        header_str(request.headers(), volucer_observe::TRACEPARENT_HEADER),
    );
    let started = Instant::now();
    // The request parts are needed for the record afterwards, so they are taken now rather than
    // inside `prepare`: consuming the request there would leave nothing to log.
    let method = request.method().as_str().to_owned();
    let target = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), |pq| pq.as_str().to_string());
    let protocol = format!("{:?}", request.version());
    let host = header_str(request.headers(), "host").map(str::to_owned);
    let user_agent = header_str(request.headers(), "user-agent").map(str::to_owned);
    let referer = header_str(request.headers(), "referer").map(str::to_owned);
    let peer: Option<std::net::IpAddr> = remote_addr.parse().ok();

    let mut hsts: Option<volucer_core::config::HstsPolicy> = None;
    let mut response = match prepare(
        request,
        &front_end,
        remote_addr,
        resolved_peer,
        local,
        cleartext,
        &mut hsts,
    )
    .await
    {
        Ok(response) => {
            front_end.stats.requests.fetch_add(1, Ordering::Relaxed);
            response
        }
        Err(err) => {
            front_end.stats.failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(status = err.status(), %err, "http request failed");
            error_response(&err)
        }
    };

    // **The id is echoed on every answer, including error pages.** A client that sends an id and
    // cannot find it on the response has no way to quote it, and an error is the case where they
    // most need to. Written after the handler so it cannot be overwritten by a backend that
    // happened to send its own.
    if let Some(name) = front_end.observe.id_header() {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            if let Ok(value) = HeaderValue::from_str(&context.id.as_str()) {
                response.headers_mut().insert(name, value);
            }
        }
    }

    // **`Strict-Transport-Security`, on every answer over TLS.** Here rather than in `prepare`
    // because this is the one point every response passes through — including the ones
    // `error_response` built from an `Err`, which matters: a policy belongs to the *name*, so a 404
    // or a 500 has as much business telling the browser about it as a 200 does. `nginx` spells this
    // `add_header … always`, and it is the behaviour an operator migrating from it expects.
    //
    // `hsts` is `None` for a cleartext connection, which is not a missing case but the rule: RFC
    // 6797 §7.2 forbids the header over a non-secure transport. It is also `None` for a site with no
    // policy, and for the redirect answer, which returns before `prepare` resolves anything — also
    // correct, since a redirect is cleartext by definition.
    if let Some(policy) = &hsts {
        if let Ok(value) = HeaderValue::from_str(&policy.header_value()) {
            response
                .headers_mut()
                .insert(STRICT_TRANSPORT_SECURITY, value);
        }
    }

    // The server span, when a `[telemetry]` table asked for one. **After the id echo and before the
    // access log**, so the exported span's status is the one the client sees — the same value the
    // log records — rather than the status a backend returned before the front-end rewrote it.
    //
    // The path is logged, not the query: a query string is unbounded and often a secret, and this is
    // a *trace* attribute that a backend indexes.
    if let Some(path) = target.split('?').next() {
        front_end.observe.record_server_span(
            &context,
            context.started_at,
            std::time::SystemTime::now(),
            volucer_observe::ServerSpanAttributes {
                method: &method,
                path,
                host: host.as_deref(),
                status: response.status().as_u16(),
                name: "http.request",
                status_message: None,
                extra: &[],
            },
        );
    }

    // `is_active` and not `is_enabled`: the two sinks are separately configured, and a deployment
    // with `[telemetry]` and no `[server.access_log]` — the ordinary way to try tracing first — must
    // still export spans. The access log keeps its own guard below.
    if front_end.observe.is_active() {
        // The body's exact length when the body knows it, and `0` when it does not. A `Full<Bytes>`
        // answer from a backend knows it; a streamed static file or an origin's `Incoming` does not,
        // and the count only exists after the write. Wrapping every response body in a counter to
        // get the real figure would put a task on the hot path for one log field, so this reports
        // what is known and the README says which bodies report zero.
        let bytes_sent = response.body().size_hint().exact().unwrap_or(0);
        let id = context.id.as_str();
        if front_end.observe.is_enabled() {
            front_end
                .observe
                .access_log()
                .record(&volucer_observe::access_log::Record {
                    id: &id,
                    method: &method,
                    target: &target,
                    protocol: &protocol,
                    peer,
                    host: host.as_deref(),
                    user_agent: user_agent.as_deref(),
                    referer: referer.as_deref(),
                    status: response.status().as_u16(),
                    total_seconds: started.elapsed().as_secs_f64(),
                    origin_seconds: None,
                    bytes_sent,
                });
        }
    }
    response
}

/// One request header as text, if it is present and is valid UTF-8.
///
/// A header with an invalid byte is not a reason to refuse the request: it simply cannot be a
/// request id or a trace context, so `None` is the honest answer.
fn header_str<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The HTTP side of [`volucer_core::HeaderLookup`].
///
/// A newtype rather than an `impl` on `HeaderMap`, because `HeaderMap` is `hyper`'s type and the
/// trait is ours: the orphan rule would refuse it, and wrapping keeps the resolution rule in
/// `volucer-core` with one set of tests covering both front-ends.
struct HttpHeaders<'a>(&'a hyper::HeaderMap);

impl volucer_core::HeaderLookup for HttpHeaders<'_> {
    fn header(&self, name: &str) -> Option<&str> {
        // `HeaderMap`'s own lookup is case-insensitive, so the canonicalisation the policy did is
        // for deduplication and for the FastCGI side rather than for correctness here.
        header_str(self.0, name)
    }
}

/// The authority a redirect should name.
///
/// The `Host` header carries the *client's* spelling, including whatever port it reached us on —
/// and a redirect must not send the client to `https://host:80`, which is the port it just used.
/// So the port is replaced rather than kept: the target's own port when one is configured, the
/// listener's own when TLS shares this port, and none at all when it is the default 443 (a bare
/// authority is what a browser expects, and `:443` in a URL is noise).
fn redirect_authority(host: &str, https_port: Option<u16>, own_port: u16) -> String {
    // Everything before the port, so an IPv6 literal's brackets and colons survive.
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            name
        }
        _ => host,
    };
    let name = if name.is_empty() { "localhost" } else { name };

    let port = https_port.unwrap_or(own_port);
    if port == 443 {
        name.to_string()
    } else {
        format!("{name}:{port}")
    }
}

/// Why a CGI dispatch produced no response.
///
/// **Two kinds, because they need different answers and only one of them is a backend's fault.** A
/// request that never became a CGI request — a path naming no script, one the configuration forbids —
/// is answered `404`/`403` whatever the protocol, because there is no call to report a gRPC status
/// about: nothing was dispatched, so nothing failed. A backend that was asked and failed is where the
/// gRPC mapping applies.
///
/// Collapsing them would mean either answering a missing script with a `grpc-status`, which claims a
/// service was consulted where none was, or answering a dead worker with a `404`, which hides a
/// failure an operator needs.
enum BackendFailure {
    /// The request never became a CGI request; nothing reached a backend.
    Request(HttpError),
    /// A backend was asked, and did not answer.
    Backend(volucer_server::ServerError),
}

impl From<HttpError> for BackendFailure {
    fn from(err: HttpError) -> Self {
        BackendFailure::Request(err)
    }
}

/// Read the body, build the CGI environment, dispatch, and translate the answer.
async fn prepare(
    request: Request<Incoming>,
    front_end: &Arc<FrontEnd>,
    remote_addr: String,
    resolved_peer: std::net::IpAddr,
    local: Option<std::net::IpAddr>,
    cleartext: bool,
    hsts: &mut Option<volucer_core::config::HstsPolicy>,
) -> Result<Response<ResponseBody>> {
    let (parts, body) = request.into_parts();

    // **The rate limit is the first thing consulted, and before anything that costs work.**
    // Before the redirect, before the site table, before the static layer's `stat`, and above all
    // before the body is read: a limited request must cost a map lookup and a clock read, not a
    // file lookup or a buffered body. The whole purpose of the feature is to stop a caller
    // spending the server's work, so a check that ran after some of that work would be a limit on
    // the *remainder*.
    //
    // `resolved_peer` and not `remote_addr`: the identity is the address `[server.proxy]`
    // resolved, which is why that table was built before this one. A limit keyed on the socket peer
    // behind nginx would throttle the proxy and could not tell two clients apart.
    if let Some(limit) = &front_end.rate_limit {
        let identity = match limit.key() {
            volucer_core::KeyMode::Global => volucer_core::Identity::Global,
            volucer_core::KeyMode::Ip => volucer_core::Identity::Address(resolved_peer),
            volucer_core::KeyMode::Header => volucer_core::Identity::Header(
                limit
                    .header()
                    .and_then(|name| header_str(&parts.headers, name))
                    .unwrap_or_default(),
            ),
        };
        if let volucer_core::Verdict::Limited { retry_after, .. } =
            limit.check(identity, Instant::now())
        {
            // **A limited gRPC call is refused as a gRPC failure, not as an HTTP 429.** The protocol
            // is known here and cannot be recovered later — `error_response` sees only the error,
            // and the request has been consumed by this point — so the branch is decided while the
            // headers are still in hand. `RESOURCE_EXHAUSTED` is the specification's retryable
            // code, which is the same instruction the HTTP side's `Retry-After` carries; a 429 is
            // unreadable to a gRPC client, which reports "unexpected HTTP status" and never retries.
            //
            // `Retry-After` has no gRPC equivalent and is not lost: it is written to the log with
            // the rest of the failure, where the operator reads it.
            if volucer_upstream::is_grpc(&parts.headers) {
                return Err(HttpError::Grpc {
                    status: volucer_upstream::GrpcStatus::ResourceExhausted,
                    reason: format!(
                        "the {} rate limit refused this call; retry in {}s",
                        limit.key().as_str(),
                        retry_after.as_secs().max(1)
                    ),
                });
            }
            return Err(HttpError::RateLimited {
                key: limit.key(),
                retry_after,
            });
        }
    }

    // The site is resolved before the redirect, because **the redirect is per site**: a site bound
    // to an internal address with `protocols = ["http"]` is deliberately cleartext, and bouncing it
    // to `https` would send its clients to a scheme it does not serve. `[server.redirect_to_https]`
    // is what a site that states nothing inherits, so the deployment-wide key keeps its meaning and
    // a site that wants to differ says so.
    //
    // Still before anything that costs work — one map probe plus a comparison, with nothing read
    // from the body. `cleartext` is what makes it safe when TLS shares the port: a connection that
    // arrived over TLS has already done what the redirect asks for, so redirecting it would loop
    // the client for ever. That flag is per connection, not per listener, because with
    // `plain = true` one listener carries both.
    let host = parts
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok());
    let state = front_end.state();
    // The protocol is decided by the **connection**, not by a header: `cleartext` is the listener's
    // own fact, and `X-Forwarded-Proto` is a client's claim. A site restricted to `https` must not
    // be selectable by a cleartext request that says otherwise — the same argument that makes the
    // local address the trustworthy half of the binding.
    let protocol = if cleartext {
        SiteProtocol::Http
    } else {
        SiteProtocol::Https
    };
    let resolved = state.resolve_request(host, local, protocol);
    let site = resolved.site;
    let (routing, statics, site_compression) =
        (resolved.routing, resolved.statics, resolved.compression);

    if cleartext && site.redirect_to_https {
        let authority = redirect_authority(
            host.unwrap_or_default(),
            front_end.https_port,
            front_end.server_port,
        );
        let target = format!(
            "https://{authority}{}",
            parts
                .uri
                .path_and_query()
                .map(|pq| pq.as_str())
                .unwrap_or("/")
        );
        // The redirect is a cleartext answer, so it carries no `Strict-Transport-Security`: RFC 6797
        // §7.2 forbids the header on a non-secure transport, and the client learns the policy on the
        // TLS response that follows instead.
        return Ok(Response::builder()
            .status(301)
            .header(hyper::header::LOCATION, target)
            .header(hyper::header::CONTENT_LENGTH, "0")
            .body(body_of(Bytes::new()))
            .expect("a 301 with a location is a valid response"));
    }

    // The policy travels out of `prepare` as a **value** rather than being applied here, because
    // `handle` is where every answer is stamped — including the error pages this function returns
    // as `Err`. A policy on a 404 matters as much as one on a 200: both tell the browser what this
    // name is. `resolved` borrows the snapshot, so it cannot be returned.
    *hsts = if cleartext { None } else { site.hsts.clone() };

    // Static content is decided before the body is read: a `GET` for a file has
    // no body worth buffering, and refusing to buffer one it will never use is
    // the point of a limit.
    //
    // `serve_only` decides whether this happens at all. A site that serves only a
    // backend skips it, which is the difference between "static files first" and "no
    // static layer": the first still stats on every request to discover there is no
    // file, and for an application host there never is one.
    // The route is selected *before* the static decision.
    // It used to be selected after, so a route's `policy` could not affect the layering at all
    // and the two layers could disagree about which subtree they were serving. One selection,
    // used by both, is the same rule the `skip_stat` work established: two answers to one
    // question is the drift.
    let route = volucer_core::config::select_route(&site.routes, host, parts.uri.path());

    // A route's policy is the site's `serve_only` for one subtree -- the only place a
    // deployment can say "/assets is files, /api is the application" without splitting the
    // site in two. A route that states nothing gets the site's behaviour, because `policy`
    // defaults to `all`.
    let policy = route.map(|route| route.policy).unwrap_or_default();
    let serves_files = policy.serves_files() && site.serve_only.serves_files();
    let serves_backend = policy.serves_backend() && site.serve_only.serves_backend();

    if serves_files {
        let static_request = static_files::Request {
            method: &parts.method,
            path: parts.uri.path(),
            headers: &parts.headers,
            host,
        };
        if let Some(answer) = static_files::respond(static_request, statics).await {
            return answer;
        }
    }

    // A site that serves only files has no backend to fall through to, and the honest
    // answer for a path naming no file is then 404 — not a dispatch, and not a 500 from
    // a backend the deployment never configured.
    if !serves_backend {
        return Err(HttpError::NotFound(parts.uri.path().to_string()));
    }

    let backend = route
        .and_then(|route| route.backend.clone())
        .unwrap_or_else(|| front_end.dispatcher.default_backend().to_string());

    // A gRPC call leaves this function twice as often as a plain request does, so the one answer is
    // decided here rather than at each of them.
    let is_grpc = volucer_upstream::is_grpc(&parts.headers);

    // The one branch between the two request paths, and it is here rather than inside the
    // dispatcher on purpose: every other backend is a blocking process on a pipe, so making the
    // dispatcher's *type* async to accommodate the one that is not would drag `spawn_blocking`
    // into the type system of all four.
    //
    // It is *before* `read_body`, which is the whole difference between the paths: the CGI body
    // has to be a `Vec` because the environment carries its length, and an origin's must not be
    // touched at all.
    if front_end.dispatcher.is_upstream(&backend) {
        return upstream(front_end, &backend, parts, body, &remote_addr, cleartext).await;
    }

    // **A gRPC call this backend was not granted is refused here**, and every part of that sentence
    // is load-bearing.
    //
    // *Before the body is read*: a gRPC call is delivered to an application, and this server is not
    // one. Reading a request body for a call that will not be dispatched is work an attacker asked
    // for, on a path whose whole purpose is refusing.
    //
    // *Before any backend is reached*: `services` is what a worker may call, `grpc_services` is what
    // it may be asked to do, and both are per-backend because a worker facing the public and one
    // serving an internal tool are different trust boundaries. Checking after dispatch would mean
    // the boundary was advisory.
    //
    // *`UNIMPLEMENTED`, not a 404 or a 501*: gRPC's own code for "this server does not implement that
    // service", and the code a client already handles for every other non-reflective server. It is
    // also not transient, so a client does not retry a call that will never be served.
    //
    // The reason names the grant that would have been needed and the one that was missing, because
    // the operator reading the log is the person who has to fix the file — and `parse_grpc_path` is
    // asked directly rather than through the grant check so that "not a method path" and "the wrong
    // service" are told apart in the sentence rather than both reading as a refusal.
    if is_grpc
        && front_end
            .dispatcher
            .granted_grpc_call(&backend, parts.uri.path())
            .is_none()
    {
        let path = parts.uri.path();
        let reason = match volucer_core::config::parse_grpc_path(path) {
            None => format!(
                "{path:?} is not a gRPC method path; a call addresses its service as \
                 `/pkg.Service/Method`"
            ),
            Some(call) => format!(
                "backend {backend:?} does not list the service {:?} in its grpc_services, so this \
                 call was not delivered to it",
                call.service
            ),
        };
        return Err(grpc_refusal(
            volucer_upstream::GrpcStatus::Unimplemented,
            reason,
        ));
    }

    let body = read_body(body, front_end.limits.max_body_bytes).await?;

    // A route that proves its pattern names a script may skip the router's own `stat`:
    let skips_stat = route.is_some_and(|route| {
        // A route with no backend cannot reach one, so `interprets_path` has nothing to
        // answer about: `true` here means "does not interpret", which would skip the stat that
        // is the only thing resolving a files-only subtree.
        let interprets = route
            .backend
            .as_deref()
            .is_some_and(|name| front_end.dispatcher.interprets_path(name));
        volucer_core::config::skip_stat(route, interprets, &site.script_extensions)
    });

    // Everything from here on blocks: routing stats the filesystem to resolve
    // SCRIPT_FILENAME, and the backend reads and writes pipes. Both must happen
    // off the runtime's threads.
    let front_end = Arc::clone(front_end);
    // Taken before `parts` moves into the blocking task: the compression decision needs the
    // request's own `Accept-Encoding`, and a header map is a cheap clone beside the body copy
    // that already happens on this path.
    let request_headers = parts.headers.clone();
    let routing = routing.clone();
    let routes = site.routes.clone();
    let blocking =
        tokio::task::spawn_blocking(move || -> std::result::Result<_, BackendFailure> {
            let cgi_request = if skips_stat {
                to_cgi_routed(&parts, body, &routing, front_end.server_port, &remote_addr)?
            } else {
                to_cgi(&parts, body, &routing, front_end.server_port, &remote_addr)?
            };
            // The site's own table, so the routing decision above and the dispatch agree.
            //
            // Mapped rather than mapped to an HTTP status here, because *which* mapping applies depends
            // on the protocol and the protocol is not known on this side of the thread: a gRPC failure is
            // a 200 with a status in the trailers, and a `502` is unreadable to a gRPC client.
            front_end
                .dispatcher
                .dispatch_with_routes(&routes, &cgi_request)
                .map_err(BackendFailure::Backend)
        });

    let response = match blocking.await {
        Ok(Ok(response)) => response,
        // A request that never reached a backend: a 404 and a 403 mean what they always meant,
        // because there is no call whose outcome a gRPC status could describe.
        Ok(Err(BackendFailure::Request(err))) => return Err(err),
        // A backend that was asked and failed. For a gRPC call the outcome travels in the trailers —
        // the failure is "the call did not happen", which is a *protocol* fact rather than a
        // component one, so the mapping is chosen by protocol before it is chosen by component.
        Ok(Err(BackendFailure::Backend(err))) if is_grpc => return Err(grpc_failure(err)),
        Ok(Err(BackendFailure::Backend(err))) => return Err(map_server_error(err)),
        Err(err) => {
            return Err(HttpError::BadGateway(format!(
                "the dispatch task failed: {err}"
            )))
        }
    };

    if !response.stderr.is_empty() {
        // Where a developer looks first when PHP complains, exactly as in the
        // FastCGI front-end.
        tracing::debug!(
            stderr = %String::from_utf8_lossy(&response.stderr).trim_end(),
            "backend wrote to STDERR"
        );
    }

    if is_grpc {
        return grpc_answer(response);
    }
    from_cgi(response, site_compression, &request_headers)
}

/// Send a request to an HTTP origin and turn its answer into this front-end's response.
///
/// **Nothing here blocks and nothing is buffered.** The request body is `Incoming`, passed straight
/// through to the client, and the response body comes back unread — which is what makes a large
/// upload, a streamed download and an SSE feed work without a byte of either being held. The CGI
/// path is the opposite on both counts by necessity, so the two must not share a body type and do
/// not.
///
/// The body cap is deliberately *not* applied. An upstream is strictly more capable than worker
/// mode, and a reverse proxy silently imposing a limit its own client never set is a bug report
/// waiting; the limit to configure is the origin's. That is a real change in this server's exposure
/// and it is stated in the README beside the limit rather than only here.
async fn upstream(
    front_end: &Arc<FrontEnd>,
    backend: &str,
    parts: hyper::http::request::Parts,
    body: Incoming,
    remote_addr: &str,
    cleartext: bool,
) -> Result<Response<ResponseBody>> {
    let uri = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().parse::<hyper::Uri>())
        .unwrap_or_else(|| "/".parse())
        .map_err(|err| HttpError::BadRequest(format!("the request path is not a URI: {err}")))?;

    // The listener carries the peer's *address* — `remote_addr` is an IP with no port, and the
    // port of an incoming connection is not part of what this header carries anyway.
    let peer = remote_addr.parse::<std::net::IpAddr>().ok();
    let request = volucer_upstream::prepare_request(
        parts.headers,
        parts.method,
        uri,
        parts.version,
        // Boxed as the *upstream* crate's body type, not `http_body_util::BodyExt::boxed`, which
        // would give `BoxBody<Bytes, hyper::Error>` — a different type from the one the dispatcher's
        // client accepts, and one that could not be `Sync` anyway.
        volucer_upstream::incoming_body(body),
        peer,
        cleartext,
    );

    // A gRPC call, so a failure has to be reported the way gRPC reports one: HTTP 200 with a status
    // in the trailers, never an HTTP error status. See `grpc_failure`.
    let is_grpc = volucer_upstream::is_grpc(&request.headers);

    let answer = match front_end
        .dispatcher
        .dispatch_upstream(backend, request)
        .await
    {
        Ok(answer) => answer,
        Err(err) => {
            return Err(if is_grpc {
                grpc_failure(err)
            } else {
                map_server_error(err)
            })
        }
    };

    let answer = volucer_upstream::translate_response(answer);

    let mut outgoing = Response::builder().status(answer.status);
    if let Some(headers) = outgoing.headers_mut() {
        *headers = answer.headers;
    }
    // `UnsyncBoxBody` is the reason this needs no conversion: an origin's body is exactly what the
    // response type is now.
    outgoing
        .body(answer.body)
        .map_err(|err| HttpError::BadGateway(err.to_string()))
}

/// Read the request body, refusing anything over the limit.
///
/// The frames are counted as they arrive rather than checked afterwards, so an
/// oversized upload is refused without being buffered first.
async fn read_body(body: Incoming, limit: usize) -> Result<Vec<u8>> {
    let mut body = body;
    let mut out: Vec<u8> = Vec::new();

    while let Some(frame) = body.frame().await {
        let frame = frame
            .map_err(|err| HttpError::BadRequest(format!("reading the request body: {err}")))?;
        if let Some(data) = frame.data_ref() {
            if out.len().saturating_add(data.len()) > limit {
                return Err(HttpError::BodyTooLarge { limit });
            }
            out.extend_from_slice(data);
        }
    }

    Ok(out)
}

/// Build the CGI request a backend understands.
///
/// The request is expressed as the CGI environment plus a body, which is what
/// every backend in the configuration already speaks. Doing it here rather than
/// teaching each front-end its own dialect is what makes `dispatch` one path.
///
/// Public, with `Parts` in the signature, so that the allocation cost of translating one
/// request can be measured in a test binary of its own — a `#[global_allocator]` is
/// per-binary, and this is the only place the translation can be driven from outside.
#[doc(hidden)]
pub fn to_cgi(
    parts: &hyper::http::request::Parts,
    body: Vec<u8>,
    routing: &Routing,
    server_port: u16,
    remote_addr: &str,
) -> Result<volucer_fcgi::Request> {
    to_cgi_with(parts, body, routing, server_port, remote_addr, false)
}

/// Build the CGI request for a path a route has already claimed.
///
/// Identical to [`to_cgi`] except that a path naming no script is handed to the backend
/// rather than answered 404 here — see [`cgi::route_or_dispatch`] for why that is safe.
/// Called only when a route's `skip_stat` says so, because skipping the check
/// unconditionally would send every unmatched path to a backend.
#[doc(hidden)]
pub fn to_cgi_routed(
    parts: &hyper::http::request::Parts,
    body: Vec<u8>,
    routing: &Routing,
    server_port: u16,
    remote_addr: &str,
) -> Result<volucer_fcgi::Request> {
    to_cgi_with(parts, body, routing, server_port, remote_addr, true)
}

fn to_cgi_with(
    parts: &hyper::http::request::Parts,
    body: Vec<u8>,
    routing: &Routing,
    server_port: u16,
    remote_addr: &str,
    allow_no_script: bool,
) -> Result<volucer_fcgi::Request> {
    let path = parts.uri.path();
    let target = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let query = parts.uri.query().unwrap_or("");

    // Resolved once, before building the environment, so a missing or forbidden path
    // can be told apart from a backend failure. `build_params` would report both as a
    // bad request, which is right for FastCGI and wrong for HTTP: a path that names no
    // script is a 404.
    //
    // The answer is then handed to `build_params_for`, which does not route again. Doing
    // it twice walked the path and called `is_file` on its candidates a second time on
    // every request, for a result already in hand.
    let route = match if allow_no_script {
        cgi::route_or_dispatch(path, routing)
    } else {
        cgi::route(path, routing)
    } {
        Route::NotFound => return Err(HttpError::NotFound(path.to_string())),
        Route::Forbidden => return Err(HttpError::Forbidden(path.to_string())),
        resolved => resolved,
    };

    let host = parts
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok());
    let (server_name, server_port) = split_host(host, server_port);

    let body_len = body.len();
    let headers = parts
        .headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name.as_str()))
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();

    let info = RequestInfo {
        method: parts.method.as_str().to_string(),
        uri: target.to_string(),
        query_string: query.to_string(),
        protocol: protocol_name(parts.version).to_string(),
        headers,
        remote_addr: remote_addr.to_string(),
        server_name,
        server_port,
        body_len,
        // Nothing can arrive here over HTTPS yet.
        https: false,
    };

    let params = cgi::build_params_for(&info, routing, &route).map_err(map_cgi_error)?;

    Ok(volucer_fcgi::Request {
        params,
        body,
        // The front-end decides keep-alive from its own protocol, so the
        // backend is always asked to stay up.
        keep_conn: true,
    })
}

/// Turn a backend response into an HTTP response.
///
/// `Content-Length` is always set from the body that is actually sent, and an
/// application-supplied one is dropped. The two cannot both be right, and only
/// one of them can break framing: declaring more than is sent desynchronises the
/// connection, while declaring less is merely conservative. `HEAD` is handled by
/// `hyper`, which suppresses the body and keeps the length — which is exactly
/// what `HEAD` is for.
fn from_cgi(
    response: volucer_fcgi::Response,
    compression: &volucer_core::config::CompressionSetting,
    request_headers: &hyper::header::HeaderMap,
) -> Result<Response<ResponseBody>> {
    let settings = crate::compress::Settings::from(compression);
    let (status, headers, body) = response.parse();
    let body = body.to_vec();

    // The application's headers, kept as an **ordered list of pairs** because repeats are
    // meaningful: two `Set-Cookie` headers are two cookies, and folding them into one corrupts
    // both. A `HeaderMap` is used only for the *decision* below, where collapsing is what the
    // question needs; the response is built from this list.
    let mut outgoing: Vec<(HeaderName, HeaderValue)> = Vec::with_capacity(headers.len());
    for (name, value) in &headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            // An unusable header from the application used to panic, because
            // `Builder::header` panics on a name or value `hyper` rejects: the connection was
            // dropped with no response at all. Skipping is what a gateway should do ? the rest
            // of the response is still sendable.
            continue;
        };
        if is_hop_by_hop(name.as_str()) || name == CONTENT_LENGTH {
            continue;
        }
        let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) else {
            continue;
        };
        outgoing.push((name, value));
    }

    // Compression is decided from what the application sent, before `Body` is built: the
    // encoding, the length and `Vary` are header facts, and a body handed to `hyper` cannot be
    // replaced afterwards.
    let original = Bytes::from(body);
    let decision = {
        let view = {
            let mut map = hyper::header::HeaderMap::new();
            for (name, value) in &outgoing {
                map.append(name.clone(), value.clone());
            }
            map
        };
        let content_type = view
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        crate::compress::compress(
            &settings,
            request_headers,
            &view,
            content_type.as_deref(),
            original.clone(),
        )
    };

    let body = match decision {
        crate::compress::Outcome::Compressed(compressed, encoding) => {
            outgoing.retain(|(name, _)| *name != CONTENT_ENCODING && *name != VARY);
            outgoing.push((
                CONTENT_ENCODING,
                HeaderValue::from_static(encoding.as_str()),
            ));
            if let Some(vary) = crate::compress::vary_value(&outgoing) {
                outgoing.push((VARY, vary));
            }
            compressed
        }
        // The clone is only live on the path that keeps the original, so a pass-through costs
        // one `Bytes` refcount and no copy.
        crate::compress::Outcome::Unchanged => original,
    };

    // The length of the body that is actually sent. An application-supplied `Content-Length`
    // was dropped above: the two cannot both be right, and declaring more than is sent
    // desynchronises the connection while declaring less is merely conservative.
    outgoing.push((
        CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string()).expect("a decimal length"),
    ));

    let mut builder = Response::builder().status(status);
    for (name, value) in outgoing {
        builder = builder.header(name, value);
    }

    builder
        .body(body_of(body))
        .map_err(|err| HttpError::BadGateway(format!("the backend sent an unusable header: {err}")))
}

/// A worker's answer to a gRPC call, put on the wire the way gRPC requires.
///
/// This is tier B's construction side, and it is the same job [`error_response`] does for a
/// *failure* this server produces: take what the application said and move the outcome into the
/// trailer section, where a gRPC client reads it. The difference is that this one carries the
/// application's bytes too — a successful call has a message.
///
/// The application expresses its outcome as two ordinary response headers, because the worker
/// protocol's `RESPONSE` head has nowhere else to put them; `volucer_upstream::grpc_reply` moves
/// them. Everything that can go wrong with that is explained there; what matters here is that the
/// application is never asked to know gRPC's wire format, only to name a status and a message.
///
/// **A refusal to even try.** The body has to be a `Vec` rather than the borrowed slice `from_cgi`
/// relays, because an HTTP/1.1 body with a trailer section is chunked and hyper needs it as frames;
/// and a gRPC message is small enough that the copy is not the interesting cost. A *streamed* gRPC
/// reply — a server-streaming method — is not supported yet, and it is where this type would have to
/// change.
fn grpc_answer(response: volucer_fcgi::Response) -> Result<Response<ResponseBody>> {
    let (status, headers, body) = response.parse();

    if status != 200 {
        // The application answered a gRPC call with a non-200 HTTP status. `grpc_reply` below forces
        // 200 and would therefore *hide* the application's own status line — so the one case where
        // the application said something this server cannot represent is reported rather than
        // silently rewritten. It is almost always a PHP error page, which is a bug in the
        // application, and a 200 with an `INTERNAL` status is the honest translation of it.
        tracing::warn!(
            status,
            "an application answered a gRPC call with an HTTP status other than 200; \
                      reporting it as INTERNAL in the trailers, because a gRPC client reads the \
                      outcome there and has no HTTP status to branch on"
        );
        return Err(grpc_refusal(
            volucer_upstream::GrpcStatus::Internal,
            format!(
                "the application answered a gRPC call with HTTP {status} rather than a gRPC status \
                 in the trailers"
            ),
        ));
    }

    let owned: Vec<(String, String)> = headers
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let (status, headers, body) = volucer_upstream::grpc_reply(owned, body.to_vec());

    let mut builder = Response::builder().status(status);
    for (name, value) in headers.iter() {
        builder = builder.header(name, value);
    }
    builder.body(ResponseBody::new(body)).map_err(|err| {
        HttpError::BadGateway(format!("the application sent an unusable header: {err}"))
    })
}

/// A plain-text error page for a failure that never reached the application.
///
/// **Except for a gRPC call, whose failure must not be an error page at all.** A gRPC client reads the
/// outcome from the trailer section, so an HTML or plain-text body under a `502` is unreadable to it
/// — it reports "unexpected HTTP status" and the failure this server is trying to report is lost. So
/// the one case where the status is in the trailers produces a `200` with a trailer-carrying body,
/// and everything else produces the page it always did.
fn error_response(err: &HttpError) -> Response<ResponseBody> {
    if let HttpError::Grpc { status, .. } = err {
        let (code, headers, body) = volucer_upstream::grpc_error(*status);
        let mut builder = Response::builder().status(code);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        // The body yields no data frames and then the trailers — which is why it has to be boxed as
        // an `UnsyncBoxBody` like every other response body here rather than assembled from bytes.
        return builder
            .body(ResponseBody::new(body))
            .expect("a trailer-carrying gRPC failure is a valid response");
    }

    let body = format!("{}\n", err);
    let mut builder = Response::builder()
        .status(err.status())
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        // **`no-store`, for the same reason RoadRunner sets it.** A 429 is the one response a cache
        // must never keep: a shared proxy that stored it would serve the refusal to every other
        // client of the same URL, which turns one caller's limit into an outage.
        .header(hyper::header::CACHE_CONTROL, "no-store")
        .header(CONTENT_LENGTH, body.len());
    // `Retry-After` in whole seconds, which is the only form RFC 9110 §10.2.3 defines for a delay.
    // Carried from the limiter's own arithmetic so the number the client is told is the number the
    // bucket will honour.
    if let HttpError::RateLimited { retry_after, .. } = err {
        builder = builder.header(hyper::header::RETRY_AFTER, retry_after.as_secs().max(1));
    }
    builder
        .body(body_of(Bytes::from(body)))
        .expect("a plain-text error page is a valid response")
}

/// Which HTTP status a dispatch failure deserves.
///
/// The distinction a gateway exists to make: a backend that is unreachable is a
/// 502, one that is reachable but silent is a 504, and one that has no capacity
/// is a 503. Collapsing them into one status throws away the only information
/// the operator gets.
fn map_server_error(err: ServerError) -> HttpError {
    if err.is_timeout() {
        HttpError::GatewayTimeout(err.to_string())
    } else if err.is_busy() {
        HttpError::Overloaded
    } else {
        HttpError::BadGateway(err.to_string())
    }
}

/// A failed gRPC call, mapped to the status a gRPC client can actually read.
///
/// The mapping is a *refusal* rather than a translation, and that is the whole point: a gRPC client
/// reads the outcome from the trailer section, so every one of `map_server_error`'s statuses —
/// 502, 503, 504 — is unreadable to it. It would report "unexpected HTTP status 502" and the
/// operator would have no way to tell a dead origin from a slow one. So the status travels where the
/// client looks for it, and the HTTP status is a plain 200.
///
/// **A proxy can only determine a few of the sixteen codes**, which is why [`GrpcStatus`] has the
/// variants it has and no more:
///
/// - An origin that could not be **reached**, and one that was reached but did not answer in time,
///   are both [`GrpcStatus::Unavailable`]. That is the code the specification defines as *transient*
///   and the one a client is permitted to retry, and it is the honest answer: the call did not fail,
///   the transport did. `DEADLINE_EXCEEDED` would be wrong — that code is reserved for a deadline the
///   *caller* set, and this server imposes none on an upstream of its own.
/// - A full pool is also `UNAVAILABLE`, for the same reason: transient, and a retry is the correct
///   response. `RESOURCE_EXHAUSTED` is the other candidate and is wrong, because that code is about
///   the *caller's* quota.
/// - A route that reaches no HTTP origin at all — a gRPC client sent to a `worker` or CGI backend —
///   is [`GrpcStatus::Unimplemented`], which is what a gRPC client expects from a server that does
///   not serve the method it asked for. Not transient, so a client does not retry it.
/// - Anything else is [`GrpcStatus::Internal`]: the origin answered and the answer was the problem,
///   so a retry would get the same thing.
///
/// The distinction is deliberately *not* the 502/504 one. That split exists to tell an operator which
/// component to look at; this one exists to tell a client whether retrying can help.
fn grpc_failure(err: ServerError) -> HttpError {
    let status = match &err {
        // A route with no HTTP path at all — a gRPC client hitting a `worker` or CGI backend. That is
        // literally what gRPC's `UNIMPLEMENTED` means: this server does not implement the service
        // asked for. It is the code a client expects from a server that does not serve a method, and
        // it is *not* transient, so a client does not retry it.
        ServerError::WrongProtocol { .. } => volucer_upstream::GrpcStatus::Unimplemented,
        // Transient failures, and the three the specification gives a client permission to retry.
        _ if err.is_timeout() || err.is_busy() || err.is_unreachable_upstream() => {
            volucer_upstream::GrpcStatus::Unavailable
        }
        // The origin answered, and the answer was the problem.
        _ => volucer_upstream::GrpcStatus::Internal,
    };
    grpc_refusal(status, map_server_error(err).to_string())
}

/// A gRPC call this server refused, as the client has to be told about it.
///
/// One constructor for both refusals, so the log line cannot disagree with the wire: the status is
/// what the client reads and the reason is what the operator reads, and they are written together
/// here or not at all.
///
/// `reason` names what happened in the deployment's vocabulary. It deliberately does **not** reach
/// the wire — `grpc-message` carries [`GrpcStatus::message`], which is a fixed sentence a client
/// could act on. The reason's home is the log, which is the one place a full explanation survives.
fn grpc_refusal(status: volucer_upstream::GrpcStatus, reason: String) -> HttpError {
    tracing::warn!(status = status.name(), %reason, "a gRPC call was refused");
    HttpError::Grpc { status, reason }
}

/// A CGI-environment failure, in HTTP terms.
fn map_cgi_error(err: volucer_fcgi::FcgiError) -> HttpError {
    match err {
        volucer_fcgi::FcgiError::BadRequest(reason) => HttpError::BadRequest(reason),
        other => HttpError::BadGateway(other.to_string()),
    }
}

/// Split a `Host` header into a name and a port.
///
/// A `Host` without a port is the common case, and an IPv6 literal needs its
/// brackets stripped before the port can be found, so the naive `rsplit_once`
/// is not enough on its own.
fn split_host(host: Option<&str>, fallback_port: u16) -> (String, u16) {
    let Some(host) = host else {
        return ("localhost".to_string(), fallback_port);
    };
    let host = host.trim();
    if host.is_empty() {
        return ("localhost".to_string(), fallback_port);
    }

    if let Some(rest) = host.strip_prefix('[') {
        // `[::1]:8080` or `[::1]`
        if let Some((address, tail)) = rest.split_once(']') {
            let port = tail
                .strip_prefix(':')
                .and_then(|port| port.parse().ok())
                .unwrap_or(fallback_port);
            return (address.to_string(), port);
        }
        return (host.to_string(), fallback_port);
    }

    match host.rsplit_once(':') {
        Some((name, port)) if !name.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (name.to_string(), port.parse().unwrap_or(fallback_port))
        }
        _ => (host.to_string(), fallback_port),
    }
}

/// The CGI name for an HTTP version.
fn protocol_name(version: hyper::Version) -> &'static str {
    match version {
        hyper::Version::HTTP_09 => "HTTP/0.9",
        hyper::Version::HTTP_10 => "HTTP/1.0",
        hyper::Version::HTTP_2 => "HTTP/2.0",
        hyper::Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/1.1",
    }
}

/// Headers that belong to one hop and must not be forwarded.
///
/// `Host` is deliberately absent: it becomes `HTTP_HOST`, which is what PHP reads
/// and what the route table matches on. `Content-Type` is absent too — it becomes
/// `CONTENT_TYPE`, which `build_params` does.
fn is_hop_by_hop(name: &str) -> bool {
    const HOP_BY_HOP: [&str; 7] = [
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
    ];
    HOP_BY_HOP.iter().any(|hop| name.eq_ignore_ascii_case(hop))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn routing() -> Routing {
        Routing {
            document_root: PathBuf::from("."),
            front_controller: Some("index.php".to_string()),
            forbidden_extensions: vec![".htaccess".to_string()],
            ..Routing::default()
        }
    }

    /// `Parts` as a front-end would produce them, with no `FrontEnd` involved:
    /// none of these tests should start a backend.
    fn parts(method: &str, target: &str, host: Option<&str>) -> hyper::http::request::Parts {
        let mut builder = Request::builder().method(method).uri(target);
        if let Some(host) = host {
            builder = builder.header(HOST, host);
        }
        builder.body(()).expect("a valid request").into_parts().0
    }

    fn param<'a>(request: &'a volucer_fcgi::Request, name: &str) -> &'a [u8] {
        request
            .params
            .iter()
            .find(|(key, _)| key.as_slice() == name.as_bytes())
            .map(|(_, value)| value.as_slice())
            .unwrap_or_else(|| panic!("{name} is missing from the environment"))
    }

    #[test]
    fn the_environment_carries_what_php_reads() {
        let request = to_cgi(
            &parts("GET", "/about?x=1", Some("example.test")),
            Vec::new(),
            &routing(),
            8080,
            "10.0.0.5",
        )
        .expect("a request that routes");

        assert_eq!(param(&request, "REQUEST_METHOD"), b"GET");
        assert_eq!(param(&request, "REQUEST_URI"), b"/about?x=1");
        assert_eq!(param(&request, "QUERY_STRING"), b"x=1");
        assert_eq!(param(&request, "SERVER_PROTOCOL"), b"HTTP/1.1");
        assert_eq!(param(&request, "REMOTE_ADDR"), b"10.0.0.5");
        assert_eq!(param(&request, "SERVER_NAME"), b"example.test");
        assert_eq!(param(&request, "SERVER_PORT"), b"8080");
        assert_eq!(param(&request, "HTTP_HOST"), b"example.test");
        assert_eq!(param(&request, "SCRIPT_NAME"), b"/index.php");
        assert_eq!(param(&request, "PATH_INFO"), b"/about");
        assert_eq!(param(&request, "GATEWAY_INTERFACE"), b"FastCGI/1.0");
        // The front controller is what serves an unrouted path, so PHP must be
        // told to run it rather than to 404 in the SAPI.
        assert_eq!(param(&request, "REDIRECT_STATUS"), b"200");
    }

    #[test]
    fn a_body_is_declared_to_the_backend() {
        let request = to_cgi(
            &parts("POST", "/submit", Some("example.test")),
            b"a=1&b=2".to_vec(),
            &routing(),
            80,
            "127.0.0.1",
        )
        .expect("a request that routes");

        // Without CONTENT_LENGTH PHP leaves php://input empty and never runs its
        // body parser, so this is the parameter that makes a POST work at all.
        assert_eq!(param(&request, "CONTENT_LENGTH"), b"7");
        assert_eq!(request.body, b"a=1&b=2");
    }

    #[test]
    fn a_request_without_a_host_falls_back_to_the_bound_port() {
        let request = to_cgi(
            &parts("GET", "/", None),
            Vec::new(),
            &routing(),
            9090,
            "127.0.0.1",
        )
        .expect("a request that routes");

        assert_eq!(param(&request, "SERVER_NAME"), b"localhost");
        assert_eq!(param(&request, "SERVER_PORT"), b"9090");
    }

    #[test]
    fn a_host_with_a_port_is_split_into_name_and_port() {
        assert_eq!(
            split_host(Some("example.test:8443"), 80),
            ("example.test".to_string(), 8443)
        );
        assert_eq!(
            split_host(Some("example.test"), 80),
            ("example.test".to_string(), 80)
        );
        assert_eq!(
            split_host(Some("[::1]:8443"), 80),
            ("::1".to_string(), 8443)
        );
        assert_eq!(split_host(Some("[::1]"), 80), ("::1".to_string(), 80));
        assert_eq!(split_host(None, 80), ("localhost".to_string(), 80));
        assert_eq!(split_host(Some(""), 80), ("localhost".to_string(), 80));
    }

    #[test]
    fn hop_by_hop_headers_are_not_forwarded() {
        let request = Request::builder()
            .method("POST")
            .uri("/submit")
            .header(HOST, "example.test")
            .header("Connection", "keep-alive")
            .header("Transfer-Encoding", "chunked")
            .header(CONTENT_TYPE, "application/json")
            .header("X-Custom", "yes")
            .body(())
            .expect("a valid request")
            .into_parts()
            .0;

        let built = to_cgi(&request, b"{}".to_vec(), &routing(), 80, "127.0.0.1")
            .expect("a request that routes");

        assert_eq!(param(&built, "HTTP_X_CUSTOM"), b"yes");
        // Content-Type keeps its CGI name; it is not a hop-by-hop header.
        assert_eq!(param(&built, "CONTENT_TYPE"), b"application/json");
        let names: Vec<&[u8]> = built.params.iter().map(|(key, _)| key.as_slice()).collect();
        assert!(
            !names.contains(&b"HTTP_CONNECTION".as_slice()),
            "Connection is hop-by-hop"
        );
        assert!(
            !names.contains(&b"HTTP_TRANSFER_ENCODING".as_slice()),
            "Transfer-Encoding is hop-by-hop"
        );
    }

    #[test]
    fn a_missing_script_is_reported_as_not_found() {
        let routing = Routing {
            document_root: PathBuf::from("a-directory-that-does-not-exist"),
            front_controller: None,
            forbidden_extensions: Vec::new(),
            ..Routing::default()
        };
        let err = to_cgi(
            &parts("GET", "/nope.php", Some("example.test")),
            Vec::new(),
            &routing,
            80,
            "127.0.0.1",
        )
        .expect_err("no script exists under that root");

        assert_eq!(err.status(), 404);
    }

    #[test]
    fn a_forbidden_path_is_reported_as_forbidden() {
        let routing = Routing {
            document_root: PathBuf::from("a-directory-that-does-not-exist"),
            front_controller: Some("index.php".to_string()),
            forbidden_extensions: vec![".htaccess".to_string()],
            ..Routing::default()
        };
        let err = to_cgi(
            &parts("GET", "/.htaccess", Some("example.test")),
            Vec::new(),
            &routing,
            80,
            "127.0.0.1",
        )
        .expect_err("the path names a forbidden extension");

        assert_eq!(err.status(), 403);
    }

    #[test]
    fn a_status_header_becomes_the_http_status_and_the_body_survives() {
        let response = volucer_fcgi::Response {
            stdout: b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nmissing\n".to_vec(),
            ..volucer_fcgi::Response::default()
        };
        let built = from_cgi(
            response,
            &volucer_core::config::CompressionSetting::default(),
            &hyper::header::HeaderMap::new(),
        )
        .expect("a well-formed response");

        assert_eq!(built.status(), 404);
        assert_eq!(
            built
                .headers()
                .get(CONTENT_TYPE)
                .expect("content type is kept"),
            "text/plain"
        );
    }

    #[test]
    fn the_declared_length_is_the_body_that_is_sent() {
        // An application that declares a length it does not send would
        // desynchronise the connection, so the front-end's own count wins.
        let response = volucer_fcgi::Response {
            stdout: b"Content-Length: 9999\r\n\r\nshort".to_vec(),
            ..volucer_fcgi::Response::default()
        };
        let built = from_cgi(
            response,
            &volucer_core::config::CompressionSetting::default(),
            &hyper::header::HeaderMap::new(),
        )
        .expect("a well-formed response");

        assert_eq!(built.headers().get(CONTENT_LENGTH).expect("a length"), "5");
        assert_eq!(built.status(), 200, "no Status header means 200");
    }

    #[test]
    fn a_repeated_header_is_not_collapsed() {
        // `Set-Cookie` is the reason: two cookies are two headers, and folding
        // them into one corrupts both.
        let response = volucer_fcgi::Response {
            stdout: b"Set-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n".to_vec(),
            ..volucer_fcgi::Response::default()
        };
        let built = from_cgi(
            response,
            &volucer_core::config::CompressionSetting::default(),
            &hyper::header::HeaderMap::new(),
        )
        .expect("a well-formed response");

        let cookies: Vec<&str> = built
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|value| value.to_str().expect("valid UTF-8"))
            .collect();
        assert_eq!(cookies, vec!["a=1", "b=2"]);
    }

    #[test]
    fn a_backend_timeout_is_a_504_and_an_unreachable_backend_a_502() {
        let timeout = ServerError::WorkerProtocol {
            backend: "app".to_string(),
            source: volucer_worker::WorkerError::Timeout(Duration::from_secs(30)),
        };
        assert_eq!(map_server_error(timeout).status(), 504);

        let unreachable = ServerError::Misconfigured {
            backend: "fpm".to_string(),
            reason: "cannot connect".to_string(),
        };
        assert_eq!(map_server_error(unreachable).status(), 502);

        let busy = ServerError::NoWorker {
            backend: "app".to_string(),
        };
        assert_eq!(map_server_error(busy).status(), 503);
    }

    #[test]
    fn an_error_page_names_its_own_status() {
        let response = error_response(&HttpError::BadGateway("worker died".to_string()));
        assert_eq!(response.status(), 502);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).expect("content type"),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn the_protocol_name_matches_the_cgi_spelling() {
        assert_eq!(protocol_name(hyper::Version::HTTP_10), "HTTP/1.0");
        assert_eq!(protocol_name(hyper::Version::HTTP_11), "HTTP/1.1");
        assert_eq!(protocol_name(hyper::Version::HTTP_2), "HTTP/2.0");
    }
}

#[test]
fn the_two_front_ends_default_to_the_same_body_limit() {
    // The property the config key exists for: one answer, whichever protocol a request arrived on.
    // These are two independent `Default` impls in two crates, so nothing but this assertion stops
    // them drifting — and a drift would be invisible, because it would only show as one protocol
    // accepting an upload the other refuses.
    assert_eq!(
        HttpLimits::default().max_body_bytes,
        volucer_core::config::DEFAULT_MAX_BODY_BYTES
    );
    assert_eq!(
        volucer_core::config::DEFAULT_MAX_BODY_BYTES,
        32 * 1024 * 1024
    );
}
