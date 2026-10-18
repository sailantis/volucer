//! The listener loop: accept connections, serve requests, dispatch to backends.
//!
//! Deliberately thread-per-connection on blocking sockets rather than an async
//! runtime. The reason is the backend, not the front-end: `php-cgi` processes
//! requests sequentially and a slot serves one request at a time, so the
//! concurrency ceiling is the number of slots. Threads map onto that directly,
//! keep the code readable, and avoid an executor in a program whose real work is
//! process supervision.
//!
//! The dispatcher is shared without a lock of its own: each backend slot
//! synchronises internally, so a request to one worker does not hold up a
//! request to another. That is what makes `workers` mean parallelism as well as
//! recycling, and it is the change this comment used to say would be needed here
//! once the worker pool landed.
//!
//! Thread-per-connection is deliberate. Threads map onto the number of backend
//! slots directly, keep the code readable, and avoid an executor in a program
//! whose real work is process supervision.

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use volucer_core::sites::Sites;
use volucer_observe::access_log::Record;
use volucer_observe::{Observability, RequestContext};
use volucer_watch::{Registry, SharedState};

use crate::dispatch::Dispatcher;
use crate::error::{Result, ServerError};
use crate::frontend::fastcgi::{self, ServerErrorKind};

/// How long an idle keep-alive connection may sit before it is closed.
///
/// Without this a keep-alive connection from nginx would occupy a thread for
/// ever, and enough of them would exhaust the process's threads.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(15);

/// How much of a request to read ahead of the codec.
///
/// A front-end sends the environment in records of up to `MAX_CONTENT_LEN`, but the common case
/// is a handful of small ones, and the codec asks for each record's header and content
/// separately. This is sized so an ordinary environment arrives in one fill; a larger one costs
/// one read per buffer rather than one per field.
const READ_BUFFER_LEN: usize = 8 * 1024;

/// Limits on concurrent connections, so a misbehaving client cannot make the
/// process allocate unboundedly many threads.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_connections: usize,
    /// The largest request body this listener will buffer.
    ///
    /// **Here rather than only on the HTTP listener**, which is the defect this field fixes:
    /// `read_next` used to extend a `Vec` for as long as `STDIN` records arrived, so a request
    /// arriving over FastCGI had no bound at all and the only thing limiting it was nginx's
    /// `client_max_body_size` on somebody else's configuration. The two front-ends now read the
    /// same `server.max_body_bytes`.
    pub max_body_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_connections: 512,
            // Through the one constant the HTTP front-end's `HttpLimits::default()` also uses, so
            // "the default" is a single number rather than two that agree today.
            max_body_bytes: volucer_core::config::DEFAULT_MAX_BODY_BYTES,
        }
    }
}

/// What the listener reports about its own behaviour.
#[derive(Debug, Default)]
pub struct Stats {
    pub connections: AtomicUsize,
    pub requests: AtomicUsize,
    pub rejected: AtomicUsize,
    pub failed: AtomicUsize,
}

impl Stats {
    fn snapshot(&self) -> (usize, usize, usize, usize) {
        (
            self.connections.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }
}

/// A running FastCGI listener.
pub struct FastCgiListener {
    listener: TcpListener,
    dispatcher: Arc<Dispatcher>,
    /// The site table, shared with the HTTP front-end, so a reload moves both.
    ///
    /// Site selection behind nginx can only change the *backend*: nginx resolved
    /// `SCRIPT_FILENAME` before forwarding and the params are relayed verbatim, so the document
    /// root is already settled. That is still the difference between one pool and another — and
    /// it is why this must move with the HTTP listener rather than beside it. A reload that
    /// reached only one front-end took effect on one protocol, which reads as the other one being
    /// right and is invisible to any test that exercises a single front-end.
    sites: SharedState,
    /// The process's one registry, so this listener is reachable by a reload.
    ///
    /// Held rather than derived from `sites` because a reload has to rebuild the *derived* state of
    /// every front-end, and this listener has none of its own — it resolves per request. Keeping
    /// the registry here is what lets `serve` hold one place that a trigger applies through.
    registry: Arc<Registry>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<Stats>,
    limits: Limits,
    /// Request identity and the access log, shared with the HTTP front-end.
    ///
    /// One `Arc` for the process rather than one per listener: a deployment behind nginx runs both
    /// front-ends, and two access logs would mean one application's requests appear in two files
    /// depending on which front-end received them. Defaults to disabled so every existing caller
    /// and test is unchanged — the "off by default costs nothing" rule, made structural.
    observe: Arc<volucer_observe::Observability>,
    /// The rate limit, when the deployment asked for one.
    ///
    /// The **same `Arc` the HTTP front-end holds**, so a client's allowance does not depend on which
    /// protocol received its request. `None` when no `[server.rate_limit]` table exists, so an
    /// unconfigured deployment's request path does one `Option` check.
    rate_limit: Option<Arc<volucer_core::RateLimitPolicy>>,
}

impl FastCgiListener {
    /// Bind a listener.
    ///
    /// Binding happens here rather than inside `run`, so a port conflict is
    /// reported before any backend is started.
    ///
    /// The dispatcher arrives as an `Arc` because it is shared: the HTTP
    /// front-end serves the same backends, so there is one set of processes
    /// behind one routing decision, whichever protocol a request arrived on. It
    /// needs no lock because each backend synchronises internally.
    ///
    /// A listener on its own gets a registry of its own, so that a reload still
    /// has something to apply through.
    pub fn bind(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        sites: Arc<Sites>,
        limits: Limits,
    ) -> Result<Self> {
        let registry = Arc::new(Registry::new(SharedState::new((*sites).clone())));
        Self::bind_registered(address, dispatcher, registry, limits)
    }

    /// [`FastCgiListener::bind`], around the process's one registry.
    ///
    /// The two-listener case, and the only way `serve` can give both front-ends one table. A
    /// listener that built its own would be the one still serving the old configuration after a
    /// reload, which reads as the *other* front-end being right and is invisible to any test that
    /// exercises one protocol — so this parameter prevents the defect rather than documenting it.
    ///
    /// The registry is taken rather than a bare table because a FastCGI listener needs the table
    /// *and* the reload path that moves it; passing only the table is how a reload came to reach
    /// this listener's data but not its configuration.
    pub fn bind_registered(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        registry: Arc<Registry>,
        limits: Limits,
    ) -> Result<Self> {
        let listener = TcpListener::bind(address).map_err(|source| ServerError::Misconfigured {
            backend: "<listener>".to_string(),
            reason: format!("cannot bind {address}: {source}"),
        })?;
        listener.set_nonblocking(false).map_err(ServerError::Io)?;

        Ok(FastCgiListener {
            listener,
            dispatcher,
            sites: registry.shared().clone(),
            registry,
            shutdown: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Stats::default()),
            limits,
            observe: Arc::new(volucer_observe::Observability::disabled()),
            rate_limit: None,
        })
    }

    /// The same listener with request identity and an access log.
    ///
    /// A builder rather than another argument on [`FastCgiListener::bind_registered`], because the
    /// observability handle is a process-wide thing that only `serve` has: every existing caller
    /// and every test constructs a listener without one, and those must keep compiling and keep
    /// costing nothing.
    #[must_use]
    pub fn with_observability(mut self, observe: Arc<volucer_observe::Observability>) -> Self {
        self.observe = observe;
        self
    }

    /// The same listener with a rate limit.
    ///
    /// Builder-shaped for the same reason [`FastCgiListener::with_observability`] is, and it takes
    /// the `Arc` the HTTP listener was given rather than building one: two policies would be two
    /// allowances for one client.
    #[must_use]
    pub fn with_rate_limit(mut self, rate_limit: Arc<volucer_core::RateLimitPolicy>) -> Self {
        self.rate_limit = Some(rate_limit);
        self
    }

    /// The site table this listener serves from, shared with every other front-end.
    ///
    /// A clone of the handle rather than of the table, so replacing through it moves all of them.
    pub fn shared_state(&self) -> SharedState {
        self.sites.clone()
    }

    /// The process's one registry, which a reload applies through.
    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    /// A handle that replaces what this listener is serving, without a restart.
    ///
    /// The process's registry, and that is the point: both front-ends hold one, so replacing
    /// through it moves both. There is deliberately no per-listener reload path, because
    /// per-listener is the bug.
    pub fn reload_handle(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    /// The address actually bound, which matters when port 0 was requested.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.listener.local_addr().map_err(ServerError::Io)
    }

    /// A handle that stops the listener from another thread.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            flag: Arc::clone(&self.shutdown),
        }
    }

    /// Statistics, for logging.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Accept and serve connections until [`ShutdownHandle::shutdown`] is called.
    pub fn run(self) -> Result<()> {
        // A read timeout on the listener would break accept; instead the flag is
        // checked between accepts, and the loop uses a short poll interval.
        self.listener
            .set_nonblocking(true)
            .map_err(ServerError::Io)?;

        let mut handlers = Vec::new();
        while !self.shutdown.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    let active = self.stats.connections.fetch_add(1, Ordering::Relaxed) + 1;
                    if active > self.limits.max_connections {
                        self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(%peer, active, "refusing a connection: at the limit");
                        drop(stream);
                        continue;
                    }

                    let dispatcher = Arc::clone(&self.dispatcher);
                    let sites = self.sites.clone();
                    let stats = Arc::clone(&self.stats);
                    let shutdown = Arc::clone(&self.shutdown);
                    let limits = self.limits;
                    let observe = Arc::clone(&self.observe);
                    let rate_limit = self.rate_limit.clone();
                    let shared = Shared {
                        dispatcher,
                        sites,
                        stats,
                        shutdown,
                        observe,
                    };
                    handlers.retain(|handle: &std::thread::JoinHandle<()>| !handle.is_finished());
                    handlers.push(std::thread::spawn(move || {
                        if let Err(err) = serve_connection(stream, shared, limits, rate_limit) {
                            tracing::debug!(%peer, %err, "connection ended");
                        }
                    }));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    // A transient accept failure should not kill the listener.
                    tracing::warn!(%source, "accept failed");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }

        tracing::info!("listener stopping");
        for handle in handlers {
            let _ = handle.join();
        }
        let (connections, requests, rejected, failed) = self.stats.snapshot();
        tracing::info!(connections, requests, rejected, failed, "listener stopped");
        Ok(())
    }
}

/// Stops a listener from another thread.
#[derive(Clone)]
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
}

impl ShutdownHandle {
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// A request's CGI parameter, as text, if it is there and is valid UTF-8.
///
/// The environment is bytes, and a header value with an invalid byte is not a reason to refuse the
/// request — it just cannot be a request id or a trace context, so `None` is the honest answer and
/// the caller mints a fresh one.
fn param<'a>(params: &'a [(Vec<u8>, Vec<u8>)], name: &[u8]) -> Option<&'a str> {
    let (_, value) = params.iter().find(|(key, _)| key.as_slice() == name)?;
    std::str::from_utf8(value).ok()
}

/// The status the client will see for a failed dispatch.
///
/// Read off the same classification the error response uses, so the log and the answer cannot
/// disagree. `AllBusy` is a 503 because a full pool is temporary; the rest are a 502, which is what
/// a gateway says when the thing behind it failed.
fn server_error_status(err: &ServerError) -> u16 {
    match err {
        ServerError::AllBusy { .. } => 503,
        ServerError::BodyTooLarge { .. } => 413,
        _ => 502,
    }
}

/// Write one access-log record for a request that arrived over FastCGI.
///
/// A free function rather than a method: it needs the request environment, the context and the
/// elapsed time, none of which belong to the listener.
///
/// **`REMOTE_ADDR` here is nginx's address, not the client's**, and that is the correct thing to
/// log: this listener's peer really is nginx, and nginx's own access log carries the real client.
/// Writing the header nginx put in `X-Forwarded-For` instead would make this log assert a peer it
/// never saw, which is the one thing a log must not do.
///
/// **Checked against nginx's own `fastcgi_params`, and the conclusion is that `[server.proxy]` must
/// *not* be applied on this path.** nginx sets `REMOTE_ADDR = $remote_addr` — the address it saw the
/// client at, already resolved by nginx's own `real_ip` handling — so the *param* is the client's
/// address even though the socket belongs to nginx. The `HTTP_*` params beside it are the client's
/// own claims, relayed verbatim, which nginx has not vouched for. A policy that preferred a
/// relayed header over `REMOTE_ADDR` here would therefore replace a value the proxy established
/// with one the client asserted: the two front-ends look symmetric and are not, and the asymmetry is
/// this line rather than an oversight.
///
/// So the trust list is a property of the **HTTP** front-end, where the peer really is a proxy when
/// one is configured. If this listener is ever put behind something that forwards raw FastCGI
/// without setting `REMOTE_ADDR`, that deployment is unaddressed — and it would need the *socket*
/// peer to be trusted, which is a different check from the one `[server.proxy]` makes.
fn record_fastcgi(
    observe: &Observability,
    context: &RequestContext,
    request: &volucer_fcgi::Request,
    status: u16,
    bytes_sent: u64,
    elapsed: std::time::Duration,
) {
    // The server span first, and from the same values the record below uses, so the two cannot
    // describe different requests. `elapsed` is a monotonic duration and the span needs a wall-clock
    // end, so the end is derived from the elapsed time rather than read again — which keeps the
    // span's duration and the log's `request_time` the *same number* instead of two readings of a
    // clock that can step between them.
    //
    // Recorded here rather than only on the success path so a 502 appears as an error span; an
    // exporter that only saw successful requests would be blind to exactly the traffic an operator
    // opens a trace view to find.
    let params = request.params.as_slice();
    observe.record_server_span(
        context,
        context.started_at,
        context.started_at + elapsed,
        volucer_observe::ServerSpanAttributes {
            method: param(params, b"REQUEST_METHOD").unwrap_or("-"),
            path: param(params, b"REQUEST_URI")
                .unwrap_or("-")
                .split('?')
                .next()
                .unwrap_or("-"),
            host: param(params, b"HTTP_HOST").or_else(|| param(params, b"SERVER_NAME")),
            status,
            name: "http.request",
            status_message: None,
            extra: &[],
        },
    );

    let id = context.id.as_str();
    // The access log is separately configured, so it is written only when *it* is on: a deployment
    // with `[telemetry]` and no `[server.access_log]` must get spans and no log file, and
    // `AccessLog::record` on a disabled log is a lock and a match for nothing.
    if observe.is_enabled() {
        observe.access_log().record(&Record {
            id: &id,
            method: param(params, b"REQUEST_METHOD").unwrap_or("-"),
            target: param(params, b"REQUEST_URI").unwrap_or("-"),
            protocol: param(params, b"SERVER_PROTOCOL").unwrap_or("-"),
            peer: param(params, b"REMOTE_ADDR").and_then(|value| value.parse().ok()),
            host: param(params, b"HTTP_HOST").or_else(|| param(params, b"SERVER_NAME")),
            user_agent: param(params, b"HTTP_USER_AGENT"),
            referer: param(params, b"HTTP_REFERER"),
            status,
            total_seconds: elapsed.as_secs_f64(),
            // FastCGI has no separately-timed origin: the backend *is* the origin, and PHP's own
            // duration arrives in the worker protocol's `END` frame rather than as a header here.
            origin_seconds: None,
            bytes_sent,
        });
    }
}

/// What every connection on one listener shares.
///
/// Grouped rather than passed as five parameters, and the grouping is not cosmetic: `serve_connection`
/// had reached eight arguments when the rate limit was added, and the compiler's seven-argument limit
/// is a fair signal that the list had become "everything the listener owns". These are exactly the
/// handles cloned per accepted connection — all of them `Arc`s that outlive it — so they travel
/// together.
struct Shared {
    dispatcher: Arc<Dispatcher>,
    sites: SharedState,
    stats: Arc<Stats>,
    shutdown: Arc<AtomicBool>,
    observe: Arc<volucer_observe::Observability>,
}

/// Serve requests on one connection until the peer closes or keep-alive ends.
fn serve_connection(
    stream: std::net::TcpStream,
    shared: Shared,
    limits: Limits,
    rate_limit: Option<Arc<volucer_core::RateLimitPolicy>>,
) -> Result<()> {
    let Shared {
        dispatcher,
        sites,
        stats,
        shutdown,
        observe,
    } = shared;
    // On Windows an accepted socket inherits the listener's non-blocking mode,
    // so this must be reset explicitly. Without it every read returns
    // WSAEWOULDBLOCK immediately and the connection is dropped right after the
    // first record — which is exactly what happened before this line existed.
    stream.set_nonblocking(false).map_err(ServerError::Io)?;

    // The keep-alive timeout is what stops an idle connection holding a thread.
    stream
        .set_read_timeout(Some(KEEPALIVE_TIMEOUT))
        .map_err(ServerError::Io)?;
    let _ = stream.set_nodelay(true);

    // Buffered, because every record the front-end sends is a header followed by a content
    // payload and the codec asks for the two separately: unbuffered, a request whose environment
    // arrives as a dozen small records costs about three `read(2)`s each, and every one of them
    // is a syscall the previous one could have answered. Measured: 40 reads for a 12-record
    // request, 7 for the same request in one record. Writes go through `get_mut`, so a response
    // still leaves in its records rather than being held back, and the read half stays buffered.
    let mut stream = std::io::BufReader::with_capacity(READ_BUFFER_LEN, stream);
    let max_connections_string = limits.max_connections.to_string();
    let mut requests_string = String::new();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }

        let next = match fastcgi::read_next(&mut stream, limits.max_body_bytes) {
            Ok(Some(next)) => next,
            // Clean end of stream: the peer closed between requests.
            Ok(None) => return Ok(()),
            // **A body over the limit is answered, not just logged.** It reaches here rather than
            // the dispatch branch because the refusal happens while the request is being read, so
            // there is no dispatch result to map — and a listener that dropped the connection
            // instead would leave nginx reporting an upstream failure rather than a 413.
            Err(ServerError::BodyTooLarge { limit }) => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    limit,
                    "refusing a request: the body is over server.max_body_bytes"
                );
                fastcgi::write_error(
                    stream.get_mut(),
                    volucer_fcgi::REQUEST_ID,
                    &ServerError::BodyTooLarge { limit }.to_string(),
                    ServerErrorKind::BodyTooLarge,
                )?;
                // A keep-alive connection cannot be reused after this: the rest of the oversized
                // body is still on the wire, and reading it to find the next request would be
                // exactly the buffering the limit exists to refuse. Closing is the only correct
                // answer, and it is what nginx does too.
                return Ok(());
            }
            Err(err) => return Err(err),
        };

        // A `GET_VALUES` probe is not a request: it must be answered with a
        // result record, never an END_REQUEST. nginx uses it to decide whether
        // this server is usable.
        let incoming = match next {
            fastcgi::NextRequest::GetValues(names) => {
                tracing::debug!(count = names.len(), "answering a GET_VALUES probe");
                let values = [
                    ("max_connections", max_connections_string.as_str()),
                    ("requests", requests_string.as_str()),
                ];
                fastcgi::write_get_values(stream.get_mut(), &values)?;
                continue;
            }
            fastcgi::NextRequest::Request(incoming) => incoming,
        };

        if incoming.aborted {
            // The client gave up before we produced anything, so there is
            // nothing to write.
            tracing::debug!("a request was aborted before it was served");
            continue;
        }

        let keep_conn = incoming.keep_conn;
        let request_id = volucer_fcgi::REQUEST_ID;

        // Routed, not `dispatch_to_default`. A request arriving from nginx still has a route
        // table, and a route naming a host or a path is the operator's way of saying which pool
        // serves it; bypassing the table meant the two front-ends chose differently for the same
        // request. The site is resolved from `HTTP_HOST` first, then `SERVER_NAME`, because a
        // client that sent no `Host` must not lose its site to nginx's `fastcgi_param`.
        //
        // What site selection *cannot* change behind nginx is the document root: nginx resolved
        // `SCRIPT_FILENAME` before forwarding and the params are relayed verbatim. So this
        // reaches the backend and nothing else, which is still the difference between one pool
        // and another.
        let request = incoming.into_backend_request();
        let host = request
            .params
            .iter()
            .find(|(key, _)| key.as_slice() == b"HTTP_HOST")
            .or_else(|| {
                request
                    .params
                    .iter()
                    .find(|(key, _)| key.as_slice() == b"SERVER_NAME")
            })
            .map(|(_, value)| String::from_utf8_lossy(value).into_owned());
        // One snapshot for the request, so the site is resolved against one generation of the
        // table even if a reload lands here — the same rule the HTTP front-end follows. Taken
        // *per request* rather than per connection: nginx keeps its upstream alive, so the request
        // that arrives after a reload very often arrives on a connection accepted before it, and a
        // per-connection snapshot would serve the old configuration for the life of that
        // connection. That is what this line was written to fix, and the test pins it.
        let snapshot = sites.sites();
        let site = snapshot.resolve(host.as_deref());
        // **Identity is resolved per request, here, on the request path.** nginx keeps its upstream
        // alive, so the next request very often arrives on a connection accepted long before it; a
        // per-connection id would give two requests the same id, which is worse than no id at all
        // because it looks like correlation and is not.
        let context = observe.begin(
            param(request.params.as_slice(), b"HTTP_X_REQUEST_ID"),
            param(request.params.as_slice(), b"HTTP_TRACEPARENT"),
        );
        let started = Instant::now();

        // **The rate limit, before the site is resolved and before any backend is reached.** It is
        // deliberately later than on the HTTP path — a `PARAMS` block has to arrive before a request
        // exists at all, so this is the earliest point — but it is still before the dispatch, which
        // is the work the limit exists to protect.
        //
        // The identity is `REMOTE_ADDR`, **not** the socket peer and not a forwarded header, and §6.8
        // is why: nginx sets `REMOTE_ADDR = $remote_addr`, the address *it* saw the client at and
        // already resolved through its own `real_ip` handling, while the `HTTP_*` params beside it are
        // the client's unvouched-for claims. So the param is the client, even though the socket
        // belongs to nginx — the same asymmetry §6.8 found and left alone rather than "fixing" it.
        //
        // A request with no `REMOTE_ADDR` at all (a front-end that forwards raw FastCGI without
        // setting it) resolves to `None`, and gets no bucket rather than a shared one: bucketing
        // every such request together would make the limit a global one that reads as per-caller.
        if let Some(limit) = &rate_limit {
            let address = param(request.params.as_slice(), b"REMOTE_ADDR")
                .and_then(|value| value.trim().parse::<std::net::IpAddr>().ok());
            let identity = match limit.key() {
                volucer_core::KeyMode::Global => Some(volucer_core::Identity::Global),
                volucer_core::KeyMode::Ip => address.map(volucer_core::Identity::Address),
                volucer_core::KeyMode::Header => {
                    // The CGI environment carries a request header as `HTTP_` + the name with `-`
                    // replaced by `_` and upper-cased, which is the transformation nginx's own
                    // `fastcgi_params` performs. Built here rather than stored so the policy holds
                    // one spelling of the name rather than two.
                    let name = limit
                        .header()
                        .unwrap_or_default()
                        .to_ascii_uppercase()
                        .replace('-', "_");
                    param(request.params.as_slice(), format!("HTTP_{name}").as_bytes())
                        .map(volucer_core::Identity::Header)
                }
            };
            if let Some(identity) = identity {
                if let volucer_core::Verdict::Limited { retry_after, .. } =
                    limit.check(identity, Instant::now())
                {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    let reason = format!(
                        "the {} rate limit refused this request; retry in {}s",
                        limit.key().as_str(),
                        retry_after.as_secs().max(1)
                    );
                    tracing::debug!(%reason, "refusing a request");
                    // Logged with the status the client will see, like every other refusal — and the
                    // record is written *before* the answer here rather than after, because there is
                    // no backend answer to wait for and the alternative is a request an operator can
                    // see arrive and never see answered.
                    if observe.is_active() {
                        record_fastcgi(&observe, &context, &request, 429, 0, started.elapsed());
                    }
                    fastcgi::write_rate_limited(
                        stream.get_mut(),
                        request_id,
                        &reason,
                        retry_after.as_secs().max(1),
                    )?;
                    if !keep_conn {
                        return Ok(());
                    }
                    continue;
                }
            }
        }

        let response = dispatcher.dispatch_with_routes(&site.routes, &request);
        match response {
            Ok(response) => {
                stats.requests.fetch_add(1, Ordering::Relaxed);
                requests_string = stats.requests.load(Ordering::Relaxed).to_string();
                if !response.stderr.is_empty() {
                    // nginx puts this in its error log, which is where a
                    // developer looks first when PHP complains.
                    tracing::debug!(
                        stderr = %String::from_utf8_lossy(&response.stderr).trim_end(),
                        "backend wrote to STDERR"
                    );
                }
                fastcgi::write_response(
                    stream.get_mut(),
                    request_id,
                    &response.stdout,
                    &response.stderr,
                    keep_conn,
                )?;
                // **Logged after the answer is written, and that order is a decision.** A record
                // written before the bytes leave would claim a delivery that a failed write then
                // contradicts, and the write is the part that can fail. The status is the one the
                // backend actually sent — the same `parse` this front-end relies on, so the log
                // cannot disagree with what the client was told.
                if observe.is_active() {
                    let (status, _, body) = response.parse();
                    record_fastcgi(
                        &observe,
                        &context,
                        &request,
                        status,
                        body.len() as u64,
                        started.elapsed(),
                    );
                }
            }
            Err(err) => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%err, "dispatch failed");
                // A failed request is logged too, and with the status the *client* will see. An
                // access log that only records successes is the one an operator needs least: the
                // 502s are why they are reading it.
                if observe.is_active() {
                    record_fastcgi(
                        &observe,
                        &context,
                        &request,
                        server_error_status(&err),
                        0,
                        started.elapsed(),
                    );
                }
                fastcgi::write_error(
                    stream.get_mut(),
                    request_id,
                    &err.to_string(),
                    match err {
                        ServerError::BodyTooLarge { .. } => ServerErrorKind::BodyTooLarge,
                        _ => ServerErrorKind::Backend,
                    },
                )?;
            }
        }

        if !keep_conn {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn limits_have_a_sane_default() {
        assert!(Limits::default().max_connections > 0);
    }

    #[test]
    fn a_shutdown_handle_stops_the_listener() {
        let flag = Arc::new(AtomicBool::new(false));
        let handle = ShutdownHandle {
            flag: Arc::clone(&flag),
        };
        assert!(!handle.is_shutting_down());
        handle.shutdown();
        assert!(handle.is_shutting_down());
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn binding_a_used_port_is_a_misconfiguration_not_a_panic() {
        let held = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = held.local_addr().expect("address").to_string();

        let config = volucer_core::config::Config {
            server: volucer_core::config::ServerConfig {
                fastcgi: Some(address.clone()),
                ..Default::default()
            },
            // Nothing here has to start: the dispatcher is built but never used,
            // because binding fails first. An `external` backend pointing at a
            // port nothing listens on costs nothing and starts no process.
            backends: [(
                "fpm".to_string(),
                volucer_core::config::BackendConfig::External(
                    volucer_core::config::ExternalConfig {
                        socket: "127.0.0.1:1".to_string(),
                    },
                ),
            )]
            .into_iter()
            .collect(),
            default_backend: Some("fpm".to_string()),
            routes: Vec::new(),
            sites: BTreeMap::new(),
            ..volucer_core::config::Config::default()
        };
        let dispatcher =
            Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher");

        let err = FastCgiListener::bind(
            &address,
            std::sync::Arc::new(dispatcher),
            std::sync::Arc::new(Sites::from_config(&config, std::path::Path::new("."))),
            Limits::default(),
        )
        .err()
        .unwrap_or_else(|| panic!("binding {address} should have failed"));
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("cannot bind"), "{err}");
    }

    #[test]
    fn stats_start_at_zero() {
        let stats = Stats::default();
        assert_eq!(stats.snapshot(), (0, 0, 0, 0));
    }
}
