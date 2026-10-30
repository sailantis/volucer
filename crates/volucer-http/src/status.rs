//! The metrics and status listener: a second HTTP endpoint that is not the request path.
//!
//! # Why this is in `volucer-http` rather than in a crate of its own
//!
//! It is a `hyper` server, and this crate is where the `tokio` runtime lives — the crate document
//! says so: *"This is the **only** place in volucer where async code runs, and it is separated on
//! purpose."* A second runtime elsewhere would be a second answer to "where does tokio live", and
//! the reason the boundary exists is that every backend is blocking by construction.
//!
//! # It shares no code with the request path, on purpose
//!
//! No sites, no dispatcher, no CGI, no TLS, no reload. What it serves is a registry and a readiness
//! table, both of which are `Arc`s handed in at construction. That is deliberate rather than a
//! simplification: the request path's HTTP listener may be TLS-only and faces the public, and an
//! endpoint that inherited any of its configuration would be one misconfiguration away from being
//! unreachable — or public.
//!
//! # Two endpoints, and the difference is the one an orchestrator acts on
//!
//! `/health` answers 200 while the process is running, even if nothing is ready yet: it is the
//! answer to "should this container be restarted", and restarting a server that is merely still
//! booting turns a slow start into a crash loop.
//!
//! `/ready` answers the configured unavailable status until every service reports ready. It is the
//! answer to "should traffic be sent here", and a probe that returned 200 while the pools were
//! still booting is how a rolling deploy sends traffic to a server that answers 502.
//!
//! Every other path is a 404, on both, so neither endpoint becomes a second way to ask whether the
//! server is up — and so a scrape endpoint does not become a surface for anything else.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use volucer_metrics::Registry;

use crate::error::{HttpError, Result};
use volucer_watch::Swappable;
/// What the worker-control routes can do, without the listener knowing what a worker pool is.
///
/// The three things the routes need are the three things `workers.list` and `workers.reset` already
/// need, which is why there is no new machinery behind this: `crate::PoolLookup` is the same
/// implementation the call plane uses, so the HTTP route and a worker's own call are one answer with
/// two callers rather than two implementations of "which pool is worker 3 in".
///
/// A separate trait rather than `PoolLookup` itself, because of the last method. A status listener
/// whose `restart` may block for as long as a PHP process takes to boot has a choice to make about
/// *where*, and this is the one method that says out loud that the answer is not the listener's
/// thread — see [`RunningStatus`].
pub trait WorkerControl: Send + Sync {
    /// One entry per worker, across every pool, in a stable order.
    fn workers(&self) -> Vec<volucer_server::WorkerStatus>;

    /// Restart one worker by its index in [`WorkerControl::workers`].
    ///
    /// Blocking by nature, which is why the listener calls it through `spawn_blocking` rather than
    /// inline. The bound is the worker protocol's own handshake deadline, so "how long may this
    /// take" is already answered by the pool and is not this trait's question.
    fn restart(&self, index: usize) -> std::result::Result<(), volucer_server::ResetRefusal>;

    /// Replace every application process that is not serving a request.
    ///
    /// The rolling restart. Blocking for the same reason `restart` is — and for longer, since it
    /// may boot several workers — so it crosses the same `spawn_blocking` boundary.
    ///
    /// A default implementation returning an empty sweep rather than a required method: this trait
    /// has fixtures with no pools at all, and "no pools were swept" is the honest answer for them.
    fn respawn_idle(&self) -> Vec<volucer_server::RespawnReport> {
        Vec::new()
    }
}

/// The routes the worker-control surface adds, and what they answer.
///
/// Declared as constants rather than assembled at each use, so a test and the router cannot disagree
/// about a path — which is the defect a hand-written router is prone to and a `match` on a literal
/// is not.
struct WorkerRoutes;

impl WorkerRoutes {
    /// `GET` returns every worker; nothing else is allowed here.
    const PATH: &'static str = "/workers";

    /// `POST /workers/restart`, the rolling restart.
    ///
    /// A literal rather than the `{index}` shape below, and it is checked *first* so that
    /// `restart` is never read as an index. `reset_index` would refuse it — every byte has to be a
    /// digit — but relying on that would make the route's existence a consequence of a parser's
    /// strictness rather than of a declaration.
    const RESTART: &'static str = "/workers/restart";

    /// `POST /workers/{index}/reset`, whose prefix and suffix bracket the index.
    const RESET_PREFIX: &'static str = "/workers/";
    const RESET_SUFFIX: &'static str = "/reset";

    /// The index in `POST /workers/{index}/reset`, if the path is one.
    ///
    /// `None` for anything that is not exactly that shape, which includes `/workers/3/` and
    /// `/workers//reset`: a router that accepted a trailing slash here would answer two paths for one
    /// route, and the one thing this surface must not do is exist in more forms than documented.
    fn reset_index(path: &str) -> Option<usize> {
        let inner = path
            .strip_prefix(Self::RESET_PREFIX)?
            .strip_suffix(Self::RESET_SUFFIX)?;
        // **Every byte has to be an ASCII digit**, rather than "let `usize::parse` decide".
        //
        // `"+3".parse::<usize>()` succeeds, and so does `"+0"` — Rust's integer parser accepts an
        // explicit sign — so the one-line version of this function accepted `/workers/+3/reset` as
        // worker 3. Found by the test below rather than by reading, and it matters because the whole
        // point of pinning the shape is that a caller cannot name an index the pool's own list does
        // not print. An empty segment is refused too, which `parse` would refuse anyway but which is
        // clearer stated than depended upon.
        if inner.is_empty() || !inner.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        inner.parse::<usize>().ok()
    }
}

/// What the status endpoints report, and how they are configured.
///
/// A trait rather than a table of booleans, because the answer changes while the process runs: a
/// worker pool becomes ready when its first worker finishes the handshake, and an ACME engine is
/// ready whether or not a certificate has been issued yet. A snapshot taken at startup would be a
/// readiness probe that always says the same thing.
pub trait Readiness: Send + Sync {
    /// Whether the process can serve traffic now.
    fn is_ready(&self) -> bool;

    /// One line per thing that is not ready, for the body of a 503.
    ///
    /// Empty when ready. The point of the body is that a probe's *log* contains the reason: a 503
    /// with an empty body tells an on-call engineer that something is wrong and nothing else, and
    /// the server that knows is the only place that can say what.
    fn reasons(&self) -> Vec<String>;
}

/// Always ready, for a deployment with no services to wait for.
///
/// The default, and the honest one: a server with no worker pools and no ACME can serve the moment
/// it is listening, so a readiness probe that waited for something would be waiting for a condition
/// that never arrives.
pub struct AlwaysReady;

impl Readiness for AlwaysReady {
    fn is_ready(&self) -> bool {
        true
    }

    fn reasons(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Something a scrape can read.
///
/// A trait rather than a bare [`Registry`], because a scrape has to *produce* the numbers it renders:
/// the pool gauges are a reading of live state, so something has to take that reading at the moment
/// of the request. Handing the listener a `Registry` alone would render whatever happened to be in it
/// — which, with no background timer, is nothing.
///
/// Implemented for [`Registry`] itself so a test can serve one directly, and by the CLI over the
/// dispatcher, which is the implementation that publishes before it renders.
pub trait MetricsSource: Send + Sync {
    /// Take a fresh reading, before the render below.
    ///
    /// Called on every scrape and expected to be cheap: the CLI's implementation walks the worker
    /// pools and does a handful of atomic stores each. A slow implementation here is a slow scrape,
    /// which is why it is a separate method rather than something the caller cannot see.
    fn refresh(&self) {}

    /// Render the exposition.
    fn render(&self) -> String;

    /// The path this is served at.
    fn path(&self) -> &str;

    /// A prefix prepended to every metric name, or `None`.
    fn prefix(&self) -> Option<&str>;
}

/// A plain registry, served as it stands.
///
/// What a test uses, and what `refresh` being a no-op means for anything that has no live state to
/// read: every series it has was written by whoever wrote it, and there is nothing more to collect.
impl MetricsSource for Registry {
    fn render(&self) -> String {
        Registry::render(self)
    }

    fn path(&self) -> &str {
        "/metrics"
    }

    fn prefix(&self) -> Option<&str> {
        None
    }
}

/// What the listener needs that is not configuration.
pub struct StatusServices {
    /// The exposition behind the metrics path, or `None` to serve no metrics endpoint.
    pub metrics: Option<Arc<dyn MetricsSource>>,
    /// Readiness, behind `/health` and `/ready`, or `None` to serve neither.
    pub readiness: Option<Arc<dyn Readiness>>,
    /// What to answer when not ready.
    ///
    /// A `u16` rather than a `hyper::StatusCode`, so that a caller which only *configures* this
    /// listener does not have to depend on `hyper` to say a number. Validation has already refused
    /// anything outside `100..=599`, and the conversion below is where that promise is spent.
    pub unavailable_status: u16,
    /// The worker-control routes' implementation, or `None` to serve none of them.
    ///
    /// This is `None` exactly when the process has no worker pool: there is nothing to list, and
    /// `/workers` would be an empty array that looks like a pool with no workers.
    ///
    /// **`None` does not disable `POST /workers/restart`**, and that asymmetry is deliberate. The
    /// list and the reset act on a pool slot by index, so without a pool they are meaningless; a
    /// sweep is defined for every backend kind — a `spawn_cgi` pool is swept and a supervised
    /// `upstream` origin is restarted, and neither holds a `WorkerPool`. Gating the whole surface on
    /// this handle answered `405` to the sweep on exactly the deployments whose code it can roll.
    ///
    /// Whether a grant of `[status.workers]` reaches any route is [`StatusServices::reset_gate`]'s
    /// answer, asked per request — this field answers "is there a pool to control", the gate answers
    /// "may a caller reach it now".
    pub workers: Option<Arc<dyn WorkerControl>>,
    /// Which worker-control routes answer.
    ///
    /// Separated from [`StatusServices::workers`] so `[status.workers] allow` can be changed by a
    /// reload without dropping the pool handle — and turned on only for a deployment that has a
    /// pool, which `workers` already encodes. `None` means the gate is permanently shut, which is what
    /// a listener built by a caller that never mentioned it gets.
    pub reset_gate: Option<Arc<WorkerResetGate>>,
}

impl Default for StatusServices {
    /// Every endpoint absent. Not a useful listener — it answers 404 to everything — but the honest
    /// starting point for one built field by field, and what the tests that care about one endpoint
    /// use so that adding a fourth does not edit seven fixtures.
    fn default() -> Self {
        StatusServices {
            metrics: None,
            readiness: None,
            unavailable_status: 503,
            workers: None,
            reset_gate: None,
        }
    }
}

impl std::fmt::Debug for StatusServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `Arc<dyn Readiness>` is not `Debug`, and every other struct in this
        // crate is. A derived one would force the traits to carry a `Debug` bound they have no other
        // use for.
        f.debug_struct("StatusServices")
            .field("metrics", &self.metrics.is_some())
            .field("readiness", &self.readiness.is_some())
            .field("unavailable_status", &self.unavailable_status)
            .field("workers", &self.workers.is_some())
            .field(
                "reset_gate",
                &self.reset_gate.as_ref().map(|gate| gate.routes()),
            )
            .finish()
    }
}

/// Whether each worker-control route answers at all.
///
/// **A live gate rather than a construction-time `Option`**, so `[status.workers] allow` can be
/// changed by a reload. The safety property is unchanged and still the point: while a method is not
/// granted its route *does not exist* — `/workers` is a 404 and a `POST /workers/{i}/reset` is a 405,
/// not a 403 — so an endpoint never appears just because somebody edited a file and signalled. What
/// the gate adds is the other direction: an operator who granted a reset and then wants it gone no
/// longer has to restart a serving process to close an unauthenticated mutation surface.
///
/// **Two methods rather than one boolean**, because the routes are not equally dangerous. A list
/// is a fact about this process; a reset is a lever on it. A deployment that wants to *see* a pool
/// should not have to arm the recycle to do it, and `[status.workers] allow = ["workers.list"]` says
/// exactly that. The third, `restart`, is the rolling deploy — a sweep of every idle slot rather than
/// a kill switch for one worker — and is separable for the same reason: a pipeline wants the sweep,
/// not the ability to pick a worker off.
///
/// Deliberately **not** a field of [`StatusServices::workers`]: that is `None` when the deployment
/// configured no worker pool at all, which is a different answer and must stay a different answer — a
/// gate cannot conjure a pool. The two are asked in that order, pool first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkerGrant {
    /// `GET /workers`.
    pub list: bool,
    /// `POST /workers/{index}/reset`.
    pub reset: bool,
    /// `POST /workers/restart` — the rolling restart of application code.
    ///
    /// A third method rather than an alias for `reset` because it is a different operation with a
    /// different contract: a reset names one worker and either replaces it or is refused, while a
    /// restart sweeps every idle slot and is *expected* to be incomplete. Separating them is what
    /// lets a deployment grant the rolling form while withholding a per-worker kill switch, and it
    /// is the direction the two are usually wanted in — a deploy pipeline wants the sweep, not the
    /// ability to pick a worker off.
    pub restart: bool,
}

impl WorkerGrant {
    /// Every route, which is what a grant of the whole namespace means.
    pub const ALL: Self = WorkerGrant {
        list: true,
        reset: true,
        restart: true,
    };

    /// None, which is what a deployment that never wrote `[status.workers]` gets.
    pub const NONE: Self = WorkerGrant {
        list: false,
        reset: false,
        restart: false,
    };

    /// Whether any route exists, which is the question the listener asks about the surface as a whole.
    pub fn any(self) -> bool {
        self.list || self.reset || self.restart
    }
}

/// The live [`WorkerGrant`], in a form a reload can replace.
#[derive(Debug)]
pub struct WorkerResetGate {
    open: Swappable<WorkerGrant>,
}

impl WorkerResetGate {
    /// A gate with these routes open — the deployment asked for them at startup.
    pub fn open(routes: WorkerGrant) -> Self {
        WorkerResetGate {
            open: Swappable::new(routes),
        }
    }

    /// A gate with no route, which is every deployment until one asks.
    pub fn closed() -> Self {
        WorkerResetGate {
            open: Swappable::new(WorkerGrant::NONE),
        }
    }

    /// Which routes answer right now.
    pub fn routes(&self) -> WorkerGrant {
        *self.open.load()
    }

    /// Replace the granted routes, returning what they were.
    ///
    /// Whole-or-nothing, and the caller has already validated the configuration it came from — which
    /// includes the refusal of a wildcard `[status] listen` beside `workers.reset`. That refusal is
    /// what makes the *open* direction safe to reach through a reload.
    #[must_use]
    pub fn set(&self, routes: WorkerGrant) -> WorkerGrant {
        *self.open.replace(routes)
    }
}

/// The status listener, running on its own thread.
pub struct RunningStatus {
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    address: SocketAddr,
}

impl RunningStatus {
    /// Bind and serve, on a thread of its own.
    pub fn start(address: &str, services: StatusServices) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            // Two threads rather than the default, which would be one per core. The listener serves
            // two things — a scrape and a probe — and a bigger pool is not free: it is idle threads
            // parked per process, and it is what made a *test* of the `spawn_blocking` boundary pass
            // while the boundary was absent. With two threads, one blocked inline reset leaves one
            // thread for everything else, which is the smallest pool that can still serve a probe
            // during a recycle — so a regression to inline work shows up as a timeout rather than as
            // a slow answer nobody measures.
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(HttpError::Runtime)?;

        // Bound *before* the thread starts, so a bad address is an error from `start` rather than a
        // listener that reports a port and then fails to appear. The same reason `RunningHttp`
        // binds inside `block_on` before spawning.
        let listener = std::net::TcpListener::bind(address).map_err(|source| HttpError::Bind {
            address: address.to_string(),
            source,
        })?;
        let bound = listener.local_addr().map_err(|source| HttpError::Bind {
            address: address.to_string(),
            source,
        })?;
        listener.set_nonblocking(true).map_err(HttpError::Io)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let services = Arc::new(services);

        let thread = std::thread::Builder::new()
            .name("volucer-status".to_string())
            .spawn(move || {
                if let Err(err) = runtime.block_on(serve(listener, services, flag)) {
                    tracing::warn!(%err, "the metrics listener stopped with an error");
                }
            })
            .map_err(|source| HttpError::Runtime(std::io::Error::other(source)))?;

        Ok(RunningStatus {
            shutdown,
            thread: Some(thread),
            address: bound,
        })
    }

    /// The address actually bound, which matters when port 0 was requested.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// A handle that stops the listener from another thread.
    pub fn shutdown_handle(&self) -> StatusShutdown {
        StatusShutdown {
            flag: Arc::clone(&self.shutdown),
        }
    }

    /// Stop the listener and wait for its thread to finish.
    ///
    /// Joining rather than dropping: the runtime is owned by the thread, and `tokio` requires the
    /// runtime to be dropped *after* everything that borrowed it. Returning early would also make
    /// "the port is free" a race the next bind in a test could lose.
    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A handle that stops a [`RunningStatus`] from another thread.
#[derive(Clone)]
pub struct StatusShutdown {
    flag: Arc<AtomicBool>,
}

impl StatusShutdown {
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for StatusShutdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatusShutdown").finish()
    }
}

/// Accept connections until the flag is set.
async fn serve(
    listener: std::net::TcpListener,
    services: Arc<StatusServices>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::from_std(listener).map_err(HttpError::Io)?;
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    while !shutdown.load(Ordering::Relaxed) {
        // A timeout rather than a bare `accept`, because the shutdown flag has to be noticed and a
        // blocking accept has nothing to interrupt it. The same shape the FastCGI listener uses,
        // and for the same reason: there is no portable way to wake an `accept` on a flag.
        match tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept()).await {
            Ok(Ok((stream, _peer))) => {
                let services = Arc::clone(&services);
                tasks.retain(|task| !task.is_finished());
                tasks.push(tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let services = Arc::clone(&services);
                        async move { Ok::<_, Infallible>(answer(request, &services).await) }
                    });
                    // HTTP/1 only, and `hyper`'s auto builder is *not* used here. A scraper and an
                    // orchestrator both speak HTTP/1.1, so h2 would be a negotiation to maintain for
                    // no caller — and the request path's listener is where protocol support belongs.
                    if let Err(err) = hyper::server::conn::http1::Builder::new()
                        .timer(hyper_util::rt::TokioTimer::new())
                        .serve_connection(io, service)
                        .await
                    {
                        // A scraper that disconnects mid-response is not an operational event.
                        tracing::debug!(%err, "metrics connection ended");
                    }
                }));
            }
            Ok(Err(err)) => {
                tracing::warn!(%err, "the metrics listener could not accept");
            }
            // The timeout: loop and re-check the flag.
            Err(_) => {}
        }
    }
    Ok(())
}

/// Build the answer for one request.
///
/// `async` for **one** reason: `POST /workers/{i}/reset` boots a worker process under the protocol's
/// own handshake deadline, and doing that on the listener's runtime thread would block every other
/// connection the listener is serving — including the orchestrator's readiness probe, which would
/// then time out and take the container out of rotation because somebody recycled a worker. Every
/// other route here is synchronous work that happens to be polled once; the cost of that is nothing.
async fn answer(
    request: Request<hyper::body::Incoming>,
    services: &StatusServices,
) -> Response<Full<Bytes>> {
    // The path only, so `/metrics?x=1` is the same endpoint. A query string is how a scraper adds a
    // cache-buster, and treating it as a different path would answer 404 to a legitimate scrape.
    let path = request.uri().path();

    // The worker routes are checked **before** the metrics path and before readiness, and only when
    // they were configured. Order rather than precedence: the three sets of paths do not overlap, but
    // a `POST` has to reach the router at all, and the read-only routes below would answer a `POST` to
    // `/metrics` with the exposition — a body nobody asked for and a status code that says it worked.
    //
    // The gate is consulted per request rather than baked into `workers`, so `[status.workers] allow`
    // can be changed by a reload. A method the file does not grant does not exist, which is the same
    // answer a deployment that never asked for it gives.
    //
    // **A deployment with no worker pool is still consulted for the sweep**, and that is not a
    // widening of the surface: `GET /workers` and a reset act on a pool slot by index, so they are
    // meaningless without one, while a sweep is defined for every backend kind — a `spawn_cgi` pool
    // is swept, and a supervised `upstream` origin is restarted. Gating the whole surface on
    // `services.workers` made `POST /workers/restart` a `405` on exactly those deployments, which
    // was found by running it against one.
    let grant = services
        .reset_gate
        .as_ref()
        .map_or(WorkerGrant::NONE, |gate| gate.routes());
    if grant.any() {
        if let Some(response) = worker_route(
            request.method(),
            path,
            services.workers.as_ref(),
            grant,
        )
        .await
        {
            return response;
        }
    }

    // **The method guard is here, before any read-only route, rather than beside the 404 below.**
    // It was written in the latter place first, and a `POST /metrics` answered `200` with the whole
    // exposition — a request that appears to have worked, from a test that caught it. A guard that
    // runs after the first `return` in the function does not guard the paths that return.

    if !matches!(request.method(), &hyper::Method::GET | &hyper::Method::HEAD) {
        return method_not_allowed();
    }

    if let Some(metrics) = &services.metrics {
        if path == metrics.path() {
            // Taken *before* the render, and that ordering is the whole reason this is a trait:
            // the pool gauges are a reading of live state, so a scrape that rendered first would
            // report whatever the previous scrape left behind — or nothing at all, on the first one.
            metrics.refresh();
            let rendered = metrics.render();
            let body = match metrics.prefix() {
                Some(prefix) => prefix_metrics(&rendered, prefix),
                None => rendered,
            };
            return text(
                StatusCode::OK,
                body,
                "text/plain; version=0.0.4; charset=utf-8",
            );
        }
    }

    // A method other than `GET`/`HEAD` on a read-only path was answered above, before the paths that
    // return; the 404 is the last word for a path this listener does not serve.

    match (path, &services.readiness) {
        ("/health", Some(_)) => text(
            StatusCode::OK,
            "ok\n".to_string(),
            "text/plain; charset=utf-8",
        ),
        ("/ready", Some(readiness)) => {
            if readiness.is_ready() {
                text(
                    StatusCode::OK,
                    "ready\n".to_string(),
                    "text/plain; charset=utf-8",
                )
            } else {
                let mut body = String::from("not ready\n");
                for reason in readiness.reasons() {
                    body.push_str(&reason);
                    body.push('\n');
                }
                text(
                    // The configured code, and `SERVICE_UNAVAILABLE` if the number is somehow not one
                    // — validation refuses anything outside `100..=599`, so the fallback is unreachable
                    // from configuration and exists because a `StatusCode` cannot be built from an
                    // arbitrary `u16` without a decision being made somewhere.
                    StatusCode::from_u16(services.unavailable_status)
                        .unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                    body,
                    "text/plain; charset=utf-8",
                )
            }
        }
        _ => not_found(),
    }
}

/// The worker-control routes, for the methods the deployment granted.
///
/// `None` means "this path is not mine", and the caller falls through to the read-only routes. That
/// is deliberately *not* a 404 from here: `/health` and `/metrics` are still served by the same
/// listener, so an unmatched path has to be the last word rather than this function's.
///
/// **A path this function owns but was not granted a method for is still answered**, and that is the
/// point of taking [`WorkerGrant`] rather than assuming all three. `/workers` with only
/// `workers.reset` granted is a 404 on `GET` — the route does not exist — but the path is this
/// listener's own, so it does not silently fall through to a readiness lookup that would answer the
/// same 404 for a different reason. `/workers/{i}/reset` with only `workers.list` granted answers
/// 405, matching what an ungranted `POST` has always answered.
///
/// **`workers` is `None` for a deployment with no worker pool, and that is not the same as "no
/// route".** The list and the reset act on a pool slot by index, so without one they are meaningless
/// and answer 404 — the answer the surface has always given for a deployment that cannot honour it.
/// The sweep is different in kind: a `spawn_cgi` pool and a supervised `upstream` origin are both
/// swept, and neither holds a [`WorkerPool`](volucer_server::WorkerPool). Gating the sweep on the
/// pool handle made it a 405 on exactly those deployments, which is how this was found.
async fn worker_route(
    method: &hyper::Method,
    path: &str,
    workers: Option<&Arc<dyn WorkerControl>>,
    grant: WorkerGrant,
) -> Option<Response<Full<Bytes>>> {
    let Some(workers) = workers else {
        // No pool: the sweep is still answered — it is a question about the *deployment* rather than
        // about a pool slot — and everything else is the 404 the surface always gave here.
        if path == WorkerRoutes::RESTART {
            if !grant.restart {
                return Some(not_found());
            }
            return Some(sweep_response(method, None).await);
        }
        if path == WorkerRoutes::PATH || WorkerRoutes::reset_index(path).is_some() {
            return Some(not_found());
        }
        return None;
    };

    if path == WorkerRoutes::PATH {
        if !grant.list {
            return Some(not_found());
        }
        return Some(match *method {
            hyper::Method::GET => match serde_json::to_value(workers.workers()) {
                Ok(list) => json(StatusCode::OK, &list),
                // Unreachable from a `WorkerStatus`, which is six scalars and two strings. Reported
                // rather than unwrapped because the encode is the *only* thing between the pool and
                // a caller, and a panic here would take down a listener whose whole job is to say
                // whether the process is healthy.
                Err(err) => json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &serde_json::json!({ "error": format!("the worker list could not be encoded: {err}") }),
                ),
            },
            // `HEAD` is what a monitoring check does, and answering it as `GET` is the same bytes
            // `hyper` drops — but the body would still be rendered, so it is answered as the method
            // it is.
            hyper::Method::HEAD => text(
                StatusCode::OK,
                String::new(),
                "application/json; charset=utf-8",
            ),
            _ => method_not_allowed(),
        });
    }

    // **Before the index route**, so `restart` is a declared path rather than a parse that happens to
    // fail. An ungranted restart is a 404 for the same reason an ungranted reset is: a route the file
    // did not grant does not exist, and a 405 would tell a caller it does.
    if path == WorkerRoutes::RESTART {
        if !grant.restart {
            return Some(not_found());
        }
        return Some(sweep_response(method, Some(workers)).await);
    }

    let index = WorkerRoutes::reset_index(path)?;

    // A path this listener owns but was not granted a reset for answers 404, and the same answer the
    // whole route gave before it existed. Decided before the method, so a `GET` to an ungranted reset
    // path is a 404 rather than the 405 that would tell a caller the route is there.
    if !grant.reset {
        return Some(not_found());
    }

    // A `GET` on the reset path is a 405 rather than a 404: the path *is* routed, and the difference
    // is what tells a caller who typed `curl` instead of `curl -XPOST` what they did wrong.
    if *method != hyper::Method::POST {
        return Some(method_not_allowed());
    }

    // **`spawn_blocking`, and the reason is the whole point of this route's shape.** `restart` boots
    // a process and waits for its handshake under a 30-second deadline. Run inline it would own a
    // runtime worker for that long; enough concurrent resets would own all of them, and the readiness
    // probe this listener exists to answer would time out — turning an operator's maintenance action
    // into an outage the orchestrator acts on. The same boundary the request path already crosses.
    let workers = Arc::clone(workers);
    let outcome = tokio::task::spawn_blocking(move || workers.restart(index)).await;
    let refusal = match outcome {
        Ok(Ok(())) => {
            return Some(json(
                StatusCode::OK,
                &serde_json::json!({ "restarted": index }),
            ))
        }
        Ok(Err(refusal)) => refusal,
        // A panic inside `restart` — which would be a bug in the pool, not a refusal it can name —
        // arrives here as a `JoinError`. Reported as a 500 with the reason rather than as a dropped
        // connection, because a caller scripting this has to be able to tell "the server crashed"
        // from "the reset was refused".
        Err(err) => {
            return Some(json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({ "error": format!("the reset task failed: {err}") }),
            ))
        }
    };

    // **The refusal is a status code, and the body only explains it.** `curl -f` has to fail on the
    // first two without the caller parsing a document: an index that does not exist is the caller's
    // mistake and retrying it changes nothing (404), while a busy slot is a temporary condition worth
    // retrying (409). A worker that could not be booted is the deployment's problem (500) and the
    // slot is empty afterwards, so it is not a `409` — a client told to retry would retry against a
    // pool that has a hole in it.
    let status = match refusal {
        volucer_server::ResetRefusal::NoSuchWorker { .. } => StatusCode::NOT_FOUND,
        volucer_server::ResetRefusal::Busy { .. } => StatusCode::CONFLICT,
        volucer_server::ResetRefusal::Failed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // The readiness number is deliberately not reused here even when the deployment configured one:
    // that number is a statement about what "not serving traffic" means to *its* load balancer, and a
    // busy worker is not that. Mixing them would make a retryable `409` arrive as a `503` that takes
    // the whole node out of rotation because one slot was mid-request.
    Some(json(
        status,
        &serde_json::json!({ "error": refusal.to_string(), "index": refusal.index() }),
    ))
}

/// The sweep, answered for the grant the deployment made.
///
/// A function of its own rather than two arms in [`worker_route`], because the sweep is the one route
/// that must answer when there is no worker pool — so it is reached from two places and has to mean
/// the same thing in both.
///
/// **`reports` is what the caller resolved**, rather than the control being called here: a deployment
/// with no worker backend has an empty report, and an empty report is deliberately *complete*. The
/// distinction `is_complete` draws is "was anything left behind", not "did a backend exist" — a sweep
/// over nothing left nothing behind, and calling that `202` would make a static-only deployment's
/// route report a roll in progress for ever.
///
/// **`spawn_blocking` for the reason `reset` uses it, and more so**: one sweep may boot every worker
/// in every pool, and a supervised `upstream` origin is waited out for its own readiness gate, which
/// is the longest thing this listener can be asked to do.
async fn sweep_response(
    method: &hyper::Method,
    workers: Option<&Arc<dyn WorkerControl>>,
) -> Response<Full<Bytes>> {
    if *method != hyper::Method::POST {
        return method_not_allowed();
    }
    let reports = match workers {
        Some(workers) => {
            // Cloned because `spawn_blocking` needs a `'static` handle; `Arc::clone` of the trait
            // object, so this is the same implementation rather than a second one.
            let workers = Arc::clone(workers);
            match tokio::task::spawn_blocking(move || workers.respawn_idle()).await {
                Ok(reports) => reports,
                // A panic in the pool, reported rather than dropped, for the reason `reset` reports
                // one.
                Err(err) => {
                    return json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &serde_json::json!({ "error": format!("the restart task failed: {err}") }),
                    )
                }
            }
        }
        // No pool: nothing to roll, and nothing left behind either.
        None => Vec::new(),
    };

    let complete = reports.iter().all(volucer_server::RespawnReport::is_complete);
    // **`200` for a complete sweep and `202` for one that is not**, which is the honest pair rather
    // than one code for both. A sweep that skipped a busy slot has not deployed everything yet;
    // answering `200` would tell a pipeline the rollout is done and it would stop sweeping, leaving
    // the old code in front of traffic until the next unrelated deploy. `curl -f` still succeeds on
    // both, because it was accepted and it did work.
    let status = if complete {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    json(
        status,
        &serde_json::json!({
            "complete": complete,
            "backends": reports,
        }),
    )
}

/// The listener's own 404, so an ungranted route and an unknown path read the same.
fn not_found() -> Response<Full<Bytes>> {
    text(
        StatusCode::NOT_FOUND,
        "not found\n".to_string(),
        "text/plain; charset=utf-8",
    )
}

/// A JSON answer, with the length that comes from the body.
fn json(status: StatusCode, value: &serde_json::Value) -> Response<Full<Bytes>> {
    // Rendered rather than unwrapped: a `serde_json::Value` cannot fail to encode, so the `expect`
    // would be sound — but a listener that panics instead of answering is the failure mode this route
    // exists to make survivable, and an `expect` here would be the one place the rule was skipped.
    let body = serde_json::to_string(value)
        .unwrap_or_else(|err| format!("{{\"error\":\"the answer could not be encoded: {err}\"}}"));
    text(status, body, "application/json; charset=utf-8")
}

/// `405`, with the `Allow` header RFC 9110 requires on one.
fn method_not_allowed() -> Response<Full<Bytes>> {
    let mut response = text(
        StatusCode::METHOD_NOT_ALLOWED,
        "method not allowed\n".to_string(),
        "text/plain; charset=utf-8",
    );
    response.headers_mut().insert(
        hyper::header::ALLOW,
        hyper::header::HeaderValue::from_static("GET, HEAD"),
    );
    response
}

/// A `text/plain` answer with a length that is always right, because it is the body's own.
fn text(status: StatusCode, body: String, content_type: &str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    // Set through `HeaderValue::from_static`-style construction rather than parsed, because these
    // are two constants in this file and a parse that could fail would be a branch nothing can take.
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_str(content_type).expect("a constant content type"),
    );
    response
}

/// Prepend a prefix to every metric name, leaving `# HELP`/`# TYPE` comments and their names alone
/// in step with the lines they describe.
///
/// Done on the rendered text rather than inside the registry, so the registry's names stay the ones
/// RoadRunner documents and a prefix is a presentation choice. `prefix = "myapp_"` therefore yields
/// `myapp_rr_http_request_total`, which is what RoadRunner's own `prefix` does.
///
/// The awkward part is the *derived* suffixes: a histogram's series are `name_bucket`, `name_sum`
/// and `name_count`, so prefixing the family name in a `# TYPE` line and the series lines
/// separately has to agree. Prefixing every line that is not a comment, at the position the name
/// starts, does agree — because `# HELP x` and `# TYPE x` and `x{...}` and `x_bucket{...}` all begin
/// with the name.
fn prefix_metrics(rendered: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(rendered.len() + rendered.len() / 8);
    for line in rendered.lines() {
        if let Some(rest) = line.strip_prefix("# HELP ") {
            out.push_str("# HELP ");
            out.push_str(prefix);
            out.push_str(rest);
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            out.push_str("# TYPE ");
            out.push_str(prefix);
            out.push_str(rest);
        } else {
            out.push_str(prefix);
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// The instant the process started, for uptime.
///
/// A function rather than a constant, because a `static` initialiser that called `Instant::now()`
/// would be lazily evaluated on first use — which for a metric read on the first scrape is
/// indistinguishable from the process's start, and would silently become wrong the moment something
/// else touched it earlier.
#[must_use]
pub fn process_start() -> Instant {
    Instant::now()
}

#[cfg(test)]
mod tests {
    use super::WorkerRoutes;

    /// The path router accepts exactly one shape, and the shapes it might have accepted are the ones
    /// worth pinning.
    ///
    /// A route parser that also accepted `/workers/3/` would give one route two spellings, and one
    /// that accepted `+3` or a `%203` would give a caller a way to name an index the pool's own list
    /// does not print. The interesting half is the refusals: each of them is a shape a hand-written
    /// router gets wrong by using `split('/')` and taking the second-to-last segment.
    #[test]
    fn only_the_exact_reset_path_names_an_index() {
        assert_eq!(WorkerRoutes::reset_index("/workers/0/reset"), Some(0));
        assert_eq!(WorkerRoutes::reset_index("/workers/12/reset"), Some(12));

        for refused in [
            // Not that route at all.
            "/workers",
            "/workers/",
            "/",
            // The right route without its index.
            "/workers//reset",
            "/workers/reset",
            "/workers/3",
            "/workers/3/reset/",
            "/workers/3/reset/extra",
            // An index with something in it a caller could have got from a browser's escaping.
            "/workers/+3/reset",
            "/workers/%203/reset",
            "/workers/ 3/reset",
            "/workers/-1/reset",
            "/workers/3.0/reset",
            // A second segment, which `split` would happily read as one index.
            "/workers/3/extra/reset",
        ] {
            assert_eq!(
                WorkerRoutes::reset_index(refused),
                None,
                "{refused:?} must not name a worker"
            );
        }
    }

    /// A number too large for the platform is not an index, and `usize::parse` is what says so rather
    /// than a truncating cast that would reset worker 0 for a caller who asked about 2^64.
    #[test]
    fn an_index_that_does_not_fit_is_not_a_worker() {
        // `usize::MAX` is 20 digits on a 64-bit host and 20 nines does not fit either way; the test
        // asserts the refusal rather than the platform's width.
        assert_eq!(
            WorkerRoutes::reset_index(&format!("/workers/{}/reset", "9".repeat(40))),
            None
        );
    }
}
